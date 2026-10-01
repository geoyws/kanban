//! Delegated-worker identity, Part B: worker records, credentials, effective
//! authority, lease binding and the closed operation list
//! (docs/specs/identity.md IDENT-01 .. IDENT-16, acceptance A1 .. A8, A10),
//! at the compiled-process boundary.
//!
//! Every test spawns the real `kanban` binary. A worker's authority is minted
//! from the spawned process's own kernel identity plus the credential in its
//! environment, so an in-process call would not establish what is asserted.
//!
//! The managed estate is reached exactly as `authz_bypass_matrix_e2e` reaches
//! it: the board is created under `direct`, a principal is bound for the UID
//! this process runs as, and enforcement is moved to `managed` in the
//! registry. Non-root only: root is not a policy principal, so as uid 0 every
//! managed call answers `denied or not found`. `Estate::managed` panics with
//! that sentence rather than skip.

use rusqlite::Connection;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const DENIED: &str = "denied or not found";
const ACTOR: &str = "@:t/b/driver";
const SUCCESSOR: &str = "@:t/b/driver-2";
const ALICE: &str = "p-alice";

/// One board in a canonical estate under `XDG_DATA_HOME`, addressed by its
/// working directory — the one route managed enforcement does not refuse.
struct Estate {
    root: PathBuf,
    xdg: PathBuf,
    work: PathBuf,
    board_id: String,
    board_path: PathBuf,
}

impl Estate {
    fn bare(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-worker-{label}-{}-{unique}",
            std::process::id()
        ));
        let xdg = root.join("xdg");
        let work = root.join("work");
        fs::create_dir_all(&xdg).unwrap();
        fs::create_dir_all(&work).unwrap();
        Self {
            root,
            xdg,
            work,
            board_id: String::new(),
            board_path: PathBuf::new(),
        }
    }

    /// A board, created under `direct`, with the registry in `state`.
    fn with_board(label: &str, state: &str) -> Self {
        let mut estate = Self::bare(label);
        let init = estate.ok_json(None, &["init", "--name", "Workers", "--json"]);
        estate.board_path = PathBuf::from(init["boardPath"].as_str().unwrap());
        estate.board_id = estate
            .board_path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if state != "direct" {
            estate.enforce(state);
        }
        estate
    }

    /// Managed, with this process bound as alice holding read and write on
    /// the board.
    fn managed(label: &str) -> Self {
        require_non_root();
        let estate = Self::with_board(label, "direct");
        estate.bind(ALICE, &self_username(), self_uid());
        estate.grant(ALICE, "read", &format!("board:{}", estate.board_id));
        estate.grant(ALICE, "write", &format!("board:{}", estate.board_id));
        estate.enforce("managed");
        estate
    }

    fn run(&self, credential: Option<&str>, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(&self.work)
            .args(args)
            .env("XDG_DATA_HOME", &self.xdg)
            .env_remove("KANBAN_DATA_DIR")
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .env_remove("KANBAN_WORKER_CREDENTIAL")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(credential) = credential {
            command.env("KANBAN_WORKER_CREDENTIAL", credential);
        }
        command.output().unwrap()
    }

    fn ok(&self, credential: Option<&str>, args: &[&str]) -> Output {
        let output = self.run(credential, args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn ok_json(&self, credential: Option<&str>, args: &[&str]) -> Value {
        serde_json::from_slice(&self.ok(credential, args).stdout).unwrap()
    }

    fn refused(&self, credential: Option<&str>, args: &[&str], sentence: &str) {
        let output = self.run(credential, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{args:?} succeeded but must be refused\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains(sentence),
            "{args:?} refusal does not say {sentence:?}\nstderr: {stderr}"
        );
    }

    fn registry(&self) -> Connection {
        Connection::open(self.xdg.join("kanban").join("registry.db")).unwrap()
    }

    fn board(&self) -> Connection {
        Connection::open(&self.board_path).unwrap()
    }

    fn bind(&self, principal: &str, username: &str, uid: u32) {
        self.registry()
            .execute(
                "INSERT INTO principals(id,username,uid,enabled,bound_at_epoch,bound_by_event_id) \
                 VALUES(?1,?2,?3,1,0,'pe-00000000')",
                rusqlite::params![principal, username, uid],
            )
            .unwrap();
    }

    fn grant(&self, principal: &str, capability: &str, atom: &str) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        self.registry()
            .execute(
                "INSERT INTO grants(id,principal_id,capability,scope,state,origin,\
                 granted_at_epoch,granted_by_event_id) \
                 VALUES(?1,?2,?3,?4,'active','grant',0,'pe-00000000')",
                rusqlite::params![
                    format!("g-{principal}-{capability}-{unique}"),
                    principal,
                    capability,
                    serde_json::to_string(&[atom]).unwrap(),
                ],
            )
            .unwrap();
    }

    /// The state transition `access revoke` performs, for one capability.
    fn revoke(&self, principal: &str, capability: &str) {
        let retired = self
            .registry()
            .execute(
                "UPDATE grants SET state='retired' \
                 WHERE state='active' AND principal_id=?1 AND capability=?2",
                [principal, capability],
            )
            .unwrap();
        assert!(retired > 0, "{principal} held no active {capability}");
    }

    fn enforce(&self, state: &str) {
        self.registry()
            .execute("UPDATE enforcement_state SET state=? WHERE id=1", [state])
            .unwrap();
    }

    fn epoch(&self) -> i64 {
        self.registry()
            .query_row(
                "SELECT COALESCE(MAX(epoch),0) FROM policy_epochs",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn audit_rows(&self) -> i64 {
        self.registry()
            .query_row("SELECT COUNT(*) FROM access_audit", [], |row| row.get(0))
            .unwrap()
    }

    fn add_task(&self, title: &str) -> String {
        self.ok_json(None, &["task", "add", title, "--as", ACTOR, "--json"])["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// `worker register` by `credential` (None: the coordinator).
    fn register(&self, credential: Option<&str>, lane_actor: &str, extra: &[&str]) -> Output {
        let mut args = vec![
            "worker",
            "register",
            "--run",
            "run-1",
            "--lane-actor",
            lane_actor,
        ];
        args.extend_from_slice(extra);
        args.push("--json");
        self.run(credential, &args)
    }

    /// A worker the coordinator registers with `write` on the board:
    /// (worker id, credential).
    fn worker(&self, lane_actor: &str) -> (String, String) {
        let grant = format!("write=board:{}", self.board_id);
        let output = self.register(None, lane_actor, &["--grant", &grant]);
        assert!(
            output.status.success(),
            "register failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        (
            receipt["workerId"].as_str().unwrap().to_owned(),
            receipt["credential"].as_str().unwrap().to_owned(),
        )
    }

    fn checkpoint(&self, credential: Option<&str>, task: &str, lease: &str) -> Output {
        self.run(
            credential,
            &[
                "checkpoint",
                task,
                "--lease",
                lease,
                "--as",
                ACTOR,
                "--summary",
                "s",
                "--intent",
                "i",
                "--next-action",
                "n",
                "--repo",
                "/r",
                "--branch",
                "b",
                "--head",
                "abc1234",
                "--dirty",
                "clean",
                "--json",
            ],
        )
    }
}

impl Drop for Estate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn id_output(flag: &str) -> String {
    let output = Command::new("id").arg(flag).output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn self_uid() -> u32 {
    id_output("-u").parse().unwrap()
}

fn self_username() -> String {
    id_output("-un")
}

fn require_non_root() {
    if self_uid() == 0 {
        panic!(
            "worker_identity_e2e requires a non-root user: root is not a policy principal, so as uid 0 \
             every managed command answers `denied or not found`. Re-run as a non-root user or in the \
             Linux gate container."
        );
    }
}

/// A credential's shape, exactly: `kwc_` and 64 lowercase hex digits.
fn credential_shaped(value: &str) -> bool {
    value.len() == 68
        && value.starts_with("kwc_")
        && value[4..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A1 (IDENT-01): no registry, or a registry short of `managed`, has no
/// worker identity — a worker verb and a credential-bearing call are each
/// refused naming the state, and write nothing. The plain claim is untouched.
#[test]
fn worker_identity_is_refused_outside_managed_enforcement() {
    let bare = Estate::bare("unregistered");
    for (credential, args) in [
        (None, &["worker", "list", "--json"][..]),
        (Some("kwc_anything"), &["task", "list"][..]),
    ] {
        bare.refused(
            credential,
            args,
            "worker identity needs managed enforcement; this installation is unregistered",
        );
    }
    assert!(
        !bare.xdg.join("kanban").join("registry.db").exists(),
        "a refused worker call created a registry"
    );

    let prepared = Estate::with_board("prepared", "prepared");
    let task = prepared.add_task("claim me plainly");
    let registry_before = fs::read(prepared.xdg.join("kanban").join("registry.db")).unwrap();
    let board_before = fs::read(&prepared.board_path).unwrap();
    for (credential, args) in [
        (None, &["worker", "list", "--json"][..]),
        (Some("kwc_anything"), &["task", "list"][..]),
    ] {
        prepared.refused(
            credential,
            args,
            "worker identity needs managed enforcement; this installation is prepared",
        );
    }
    assert_eq!(
        fs::read(prepared.xdg.join("kanban").join("registry.db")).unwrap(),
        registry_before,
        "a refused worker call wrote the registry"
    );
    assert_eq!(
        fs::read(&prepared.board_path).unwrap(),
        board_before,
        "a refused worker call wrote the board"
    );

    let claim = prepared.ok_json(None, &["claim", &task, "--as", ACTOR, "--json"]);
    assert_eq!(claim["attempt"], 1, "{claim}");
    assert!(claim.get("workerId").is_none(), "{claim}");
}

/// A2 (IDENT-02, IDENT-03, IDENT-05): registration mints a worker, prints its
/// credential once, advances the epoch with a credential-free event, and the
/// credential is bound to its principal.
#[test]
fn registration_mints_a_worker_and_binds_its_credential_to_the_principal() {
    let estate = Estate::managed("register");
    let epoch = estate.epoch();
    let grant = format!("write=board:{}", estate.board_id);

    let output = estate.register(None, ACTOR, &["--grant", &grant]);
    assert!(output.status.success(), "{}", stderr(&output));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    let worker = receipt["workerId"].as_str().unwrap().to_owned();
    let credential = receipt["credential"].as_str().unwrap().to_owned();
    assert!(worker.starts_with("w-"), "{receipt}");
    assert_eq!(receipt["parentWorkerId"], Value::Null, "{receipt}");
    assert_eq!(receipt["principalId"], ALICE, "{receipt}");
    assert_eq!(receipt["laneActor"], ACTOR, "{receipt}");
    assert_eq!(receipt["state"], "active", "{receipt}");
    assert!(credential_shaped(&credential), "{receipt}");

    assert_eq!(
        estate.epoch(),
        epoch + 1,
        "registration did not advance the epoch"
    );
    let (kind, payload): (String, String) = estate
        .registry()
        .query_row(
            "SELECT kind,payload FROM policy_events ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind, "worker_registered");
    assert!(payload.contains(&worker), "{payload}");
    let digest: String = estate
        .registry()
        .query_row(
            "SELECT credential_sha256 FROM worker_credentials WHERE worker_id=?",
            [&worker],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !payload.contains(&credential),
        "the event holds the credential"
    );
    assert!(
        !payload.contains(&digest),
        "the event holds the credential digest"
    );

    let shown = estate.ok(None, &["worker", "show", &worker, "--json"]);
    let shown_text = stdout(&shown);
    assert!(shown_text.contains(&worker), "{shown_text}");
    assert!(
        !shown_text.contains(&credential),
        "worker show printed the credential"
    );
    assert!(
        !shown_text.contains(&digest),
        "worker show printed the digest"
    );
    assert!(!shown_text.contains("kwc_"), "{shown_text}");

    // The credential works for alice ...
    estate.ok(Some(&credential), &["task", "list", "--json"]);
    // ... and is refused once this UID resolves to a different principal.
    estate
        .registry()
        .execute("UPDATE principals SET enabled=0 WHERE id=?", [ALICE])
        .unwrap();
    estate.bind("p-bob", &self_username(), self_uid());
    estate.grant("p-bob", "write", &format!("board:{}", estate.board_id));
    estate.refused(Some(&credential), &["task", "list"], DENIED);
    // A malformed or unknown credential is the same answer.
    estate.refused(Some("kwc_nope"), &["task", "list"], DENIED);
}

/// IDENT-02: a malformed label is refused naming the flag, writing nothing.
#[test]
fn a_malformed_worker_label_is_refused_by_flag() {
    let estate = Estate::managed("labels");
    let grant = format!("write=board:{}", estate.board_id);
    let epoch = estate.epoch();
    let bad_run = estate.run(
        None,
        &[
            "worker",
            "register",
            "--run",
            "-bad",
            "--lane-actor",
            ACTOR,
            "--grant",
            &grant,
        ],
    );
    assert!(!bad_run.status.success());
    assert!(
        stderr(&bad_run).contains("--run must be 1 to 128"),
        "{}",
        stderr(&bad_run)
    );
    let bad_actor = estate.register(None, "driver", &["--grant", &grant]);
    assert!(!bad_actor.status.success());
    assert!(
        stderr(&bad_actor).contains("--lane-actor must be @:<team>/<board>/<lane>"),
        "{}",
        stderr(&bad_actor)
    );
    assert_eq!(estate.epoch(), epoch);
}

/// A3 (IDENT-04, IDENT-06): a child worker only narrows; a refused
/// registration leaves an audit row and the epoch; a revoked principal grant
/// lands on the worker's next call.
#[test]
fn a_child_worker_only_narrows_and_a_revocation_lands_next_call() {
    let estate = Estate::managed("narrow");
    let (w1, k1) = estate.worker(ACTOR);
    let child_task = estate.add_task("child root");
    let ready = estate.add_task("ready work");
    let b = estate.board_id.clone();

    for grant in [format!("admin=board:{b}"), format!("write=board:{b},*")] {
        let epoch = estate.epoch();
        let audits = estate.audit_rows();
        let output = estate.register(Some(&k1), ACTOR, &["--grant", &grant]);
        assert!(!output.status.success(), "{grant} widened w1");
        assert!(stderr(&output).contains(DENIED), "{}", stderr(&output));
        assert_eq!(
            estate.epoch(),
            epoch,
            "{grant}: a refusal advanced the epoch"
        );
        assert_eq!(
            estate.audit_rows(),
            audits + 1,
            "{grant}: no denied audit row"
        );
    }

    let output = estate.register(
        Some(&k1),
        ACTOR,
        &["--grant", &format!("read=board:{b}"), "--task", &child_task],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let child: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(child["parentWorkerId"], w1.as_str(), "{child}");
    assert_eq!(child["taskRoot"]["taskId"], child_task.as_str(), "{child}");
    assert_eq!(child["taskRoot"]["boardId"], b.as_str(), "{child}");

    estate.revoke(ALICE, "write");
    estate.refused(Some(&k1), &["claim", &ready, "--as", ACTOR], DENIED);
    assert_eq!(
        estate
            .board()
            .query_row("SELECT COUNT(*) FROM task_claims", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

/// IDENT-04: a worker with a task root may confine a child only inside it.
#[test]
fn a_child_task_root_must_sit_inside_the_parent_root() {
    let estate = Estate::managed("root");
    let b = estate.board_id.clone();
    let root = estate.ok_json(
        None,
        &[
            "task", "add", "root", "--type", "story", "--as", ACTOR, "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let outside = estate.add_task("outside");
    let inside = estate.ok_json(
        None,
        &[
            "task", "add", "inside", "--parent", &root, "--as", ACTOR, "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let grant = format!("write=board:{b}");
    let parent: Value = serde_json::from_slice(
        &estate
            .register(None, ACTOR, &["--grant", &grant, "--task", &root])
            .stdout,
    )
    .unwrap();
    let k = parent["credential"].as_str().unwrap().to_owned();
    let w = parent["workerId"].as_str().unwrap().to_owned();

    let refused = estate.register(Some(&k), ACTOR, &["--grant", &grant, "--task", &outside]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains(&format!(
            "task {outside} is outside worker {w}'s task root {root}"
        )),
        "{}",
        stderr(&refused)
    );
    let accepted = estate.register(Some(&k), ACTOR, &["--grant", &grant, "--task", &inside]);
    assert!(accepted.status.success(), "{}", stderr(&accepted));

    // The root also bounds the parent's own claims.
    estate.refused(Some(&k), &["claim", &outside, "--as", ACTOR], DENIED);
    let claim = estate.ok_json(Some(&k), &["claim", &inside, "--as", ACTOR, "--json"]);
    assert_eq!(claim["workerId"], w.as_str(), "{claim}");
}

/// A4 (IDENT-08, IDENT-09, IDENT-10): a worker claims only as its lane
/// actor, the lease and its checkpoint are stamped, and a worker lease is
/// fenced from a call without the credential.
#[test]
fn a_worker_lease_is_bound_stamped_and_fenced() {
    let estate = Estate::managed("lease");
    let (w1, k1) = estate.worker(ACTOR);
    let task = estate.add_task("worker lease");

    estate.refused(
        Some(&k1),
        &["claim", &task, "--as", SUCCESSOR],
        &format!("worker {w1} acts as {ACTOR}, not {SUCCESSOR}"),
    );
    let claim = estate.ok_json(Some(&k1), &["claim", &task, "--as", ACTOR, "--json"]);
    assert_eq!(claim["workerId"], w1.as_str(), "{claim}");
    assert_eq!(claim["principalId"], ALICE, "{claim}");
    assert_eq!(claim["attempt"], 1, "{claim}");
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();

    let fenced = estate.checkpoint(None, &task, &lease);
    assert!(!fenced.status.success());
    assert!(
        stderr(&fenced).contains(&format!("lease belongs to worker {w1}, not no worker")),
        "{}",
        stderr(&fenced)
    );
    assert!(
        !stderr(&fenced).contains(&lease),
        "the refusal named the lease token"
    );
    for verb in [
        ["heartbeat", &task, "--lease", &lease],
        ["release", &task, "--lease", &lease],
    ] {
        estate.refused(
            None,
            &verb,
            &format!("lease belongs to worker {w1}, not no worker"),
        );
    }

    let written = estate.checkpoint(Some(&k1), &task, &lease);
    assert!(written.status.success(), "{}", stderr(&written));
    let checkpoint: Value = serde_json::from_slice(&written.stdout).unwrap();
    assert_eq!(checkpoint["workerId"], w1.as_str(), "{checkpoint}");
    assert_eq!(checkpoint["principalId"], ALICE, "{checkpoint}");
    assert_eq!(checkpoint["attempt"], 1, "{checkpoint}");
    let payload: String = estate
        .board()
        .query_row(
            "SELECT payload FROM events WHERE kind='checkpoint_added' ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        payload.contains(&w1),
        "the checkpoint event is not stamped: {payload}"
    );

    // The other direction: a coordinator lease is closed to a worker.
    let plain = estate.add_task("coordinator lease");
    let held = estate.ok_json(None, &["claim", &plain, "--as", ACTOR, "--json"]);
    assert!(held.get("workerId").is_none(), "{held}");
    assert_eq!(held["principalId"], ALICE, "{held}");
    let plain_lease = held["leaseToken"].as_str().unwrap().to_owned();
    estate.refused(
        Some(&k1),
        &["heartbeat", &plain, "--lease", &plain_lease],
        &format!("lease belongs to no worker, not {w1}"),
    );
}

/// A5 (IDENT-05, IDENT-07, IDENT-11, IDENT-12): coordinator-only commands are
/// refused before anything else, a read-only worker holds no lease, and a
/// retirement ends the worker and its descendants without releasing leases.
#[test]
fn coordinator_only_commands_read_only_workers_and_retirement() {
    let estate = Estate::managed("retire");
    let b = estate.board_id.clone();
    let (w1, k1) = estate.worker(ACTOR);
    let task = estate.add_task("held by w1");
    let ready = estate.add_task("ready");
    estate.ok(Some(&k1), &["claim", &task, "--as", ACTOR]);
    let child: Value = serde_json::from_slice(
        &estate
            .register(Some(&k1), ACTOR, &["--grant", &format!("read=board:{b}")])
            .stdout,
    )
    .unwrap();
    let k2 = child["credential"].as_str().unwrap().to_owned();

    let board_before = fs::read(&estate.board_path).unwrap();
    estate.refused(
        Some(&k1),
        &["task", "move", &task, "done", "--as", ACTOR],
        &format!("worker {w1} may not run task move: it is coordinator-only"),
    );
    estate.refused(
        Some(&k1),
        &["access", "principal", "list"],
        &format!("worker {w1} may not run access principal list: it is coordinator-only"),
    );
    assert_eq!(
        fs::read(&estate.board_path).unwrap(),
        board_before,
        "a coordinator-only refusal wrote the board"
    );

    // A worker with read only reads, and claims nothing.
    estate.ok(Some(&k2), &["task", "list", "--json"]);
    estate.refused(Some(&k2), &["claim", &ready, "--as", ACTOR], DENIED);
    let leases = |task: &str| -> i64 {
        estate
            .board()
            .query_row(
                "SELECT COUNT(*) FROM task_claims WHERE task_id=?",
                [task],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(leases(&ready), 0);

    let retired = estate.ok_json(None, &["worker", "retire", &w1, "--json"]);
    assert_eq!(retired["state"], "retired", "{retired}");
    estate.refused(
        None,
        &["worker", "retire", &w1],
        &format!("worker {w1} is already retired"),
    );
    estate.refused(Some(&k1), &["task", "list"], DENIED);
    estate.refused(Some(&k2), &["task", "list"], DENIED);
    let holder: Option<String> = estate
        .board()
        .query_row(
            "SELECT worker_id FROM task_claims WHERE task_id=?",
            [&task],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        holder.as_deref(),
        Some(w1.as_str()),
        "retirement released the lease"
    );
}

/// IDENT-07: retirement is the principal's, an ancestor's, or the worker's
/// own; a sibling is refused.
#[test]
fn only_the_principal_an_ancestor_or_the_worker_itself_retires_it() {
    let estate = Estate::managed("retirers");
    let b = estate.board_id.clone();
    let (_w1, k1) = estate.worker(ACTOR);
    let (w3, k3) = estate.worker(ACTOR);
    let child: Value = serde_json::from_slice(
        &estate
            .register(Some(&k1), ACTOR, &["--grant", &format!("read=board:{b}")])
            .stdout,
    )
    .unwrap();
    let w2 = child["workerId"].as_str().unwrap().to_owned();

    estate.refused(Some(&k3), &["worker", "retire", &w2], DENIED);
    estate.ok(Some(&k1), &["worker", "retire", &w2]);
    estate.ok(Some(&k3), &["worker", "retire", &w3]);
}

/// A6 (IDENT-13): leases stay per task. Two workers on one lane actor hold
/// distinct leases; two claims of one task produce exactly one lease.
#[test]
fn leases_stay_per_task_across_workers_on_one_lane() {
    let estate = Estate::managed("lanes");
    let (w3, k3) = estate.worker(ACTOR);
    let (w4, k4) = estate.worker(ACTOR);
    let t3 = estate.add_task("t3");
    let t4 = estate.add_task("t4");
    let t5 = estate.add_task("t5");

    let first = estate.ok_json(Some(&k3), &["claim", &t3, "--as", ACTOR, "--json"]);
    let second = estate.ok_json(Some(&k4), &["claim", &t4, "--as", ACTOR, "--json"]);
    assert_eq!(first["workerId"], w3.as_str());
    assert_eq!(second["workerId"], w4.as_str());
    assert_ne!(first["leaseToken"], second["leaseToken"]);

    let spawn = |credential: &str| {
        Command::new(env!("CARGO_BIN_EXE_kanban"))
            .current_dir(&estate.work)
            .args(["claim", &t5, "--as", ACTOR, "--json"])
            .env("XDG_DATA_HOME", &estate.xdg)
            .env_remove("KANBAN_DATA_DIR")
            .env("KANBAN_WORKER_CREDENTIAL", credential)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let racers = [spawn(&k3), spawn(&k4)];
    let outcomes: Vec<Output> = racers
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let won = outcomes.iter().filter(|o| o.status.success()).count();
    assert_eq!(won, 1, "exactly one T5 claim must win");
    let lost = outcomes.iter().find(|o| !o.status.success()).unwrap();
    assert!(
        stderr(lost).contains(&format!("task {t5} is already claimed by {ACTOR} until")),
        "{}",
        stderr(lost)
    );
}

/// A7 (IDENT-14) under a worker: the receipt is keyed by the worker, so the
/// replay is byte-identical and a different request under the key is refused.
#[test]
fn a_worker_claim_request_id_replays_byte_identically() {
    let estate = Estate::managed("replay");
    let (_w1, k1) = estate.worker(ACTOR);
    let task = estate.add_task("replayed");
    let args = [
        "claim",
        &task,
        "--as",
        ACTOR,
        "--request-id",
        "req-000000000001",
        "--json",
    ];
    let first = estate.ok(Some(&k1), &args);
    let second = estate.ok(Some(&k1), &args);
    assert_eq!(first.stdout, second.stdout);
    let leases: i64 = estate
        .board()
        .query_row("SELECT COUNT(*) FROM task_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(leases, 1);
    estate.refused(
        Some(&k1),
        &[
            "claim",
            &task,
            "--as",
            ACTOR,
            "--lease-minutes",
            "30",
            "--request-id",
            "req-000000000001",
        ],
        "request req-000000000001 was already used for a different claim request",
    );
}

/// A8 (IDENT-15, IDENT-16): `worker list` is bounded and secret-free, and
/// the MCP manifest carries the four worker tools.
#[test]
fn worker_list_is_bounded_and_the_manifest_carries_the_worker_tools() {
    let estate = Estate::managed("list");
    let grant = format!("read=board:{}", estate.board_id);
    for _ in 0..501 {
        let output = estate.register(None, ACTOR, &["--grant", &grant]);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let listed = estate.ok(None, &["worker", "list", "--json"]);
    let text = stdout(&listed);
    assert!(!text.contains("kwc_"), "worker list printed a credential");
    assert!(!text.contains("credential"), "{text}");
    let listed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(listed["workers"].as_array().unwrap().len(), 50);
    assert_eq!(listed["truncated"], true);
    estate.refused(
        None,
        &["worker", "list", "--limit", "501"],
        "--limit must be between 1 and 500, got 501",
    );
    let all: Value = estate.ok_json(None, &["worker", "list", "--limit", "500", "--json"]);
    assert_eq!(all["workers"].as_array().unwrap().len(), 500);
    assert_eq!(all["truncated"], true);

    let replies = mcp(
        &estate,
        None,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                   "params":{"name":"worker_list","arguments":{"limit":"2"}}}),
        ],
    );
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    for (name, read_only) in [
        ("worker_register", false),
        ("worker_show", true),
        ("worker_list", true),
        ("worker_retire", false),
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("the manifest has no {name}"));
        assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{name}");
    }
    assert_eq!(replies[2]["result"]["isError"], false, "{}", replies[2]);
    let called: Value =
        serde_json::from_str(replies[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(called["workers"].as_array().unwrap().len(), 2);
}

/// IDENT-16: an MCP server started with a credential acts as that worker for
/// every call, and a refusal is `isError` with the CLI's sentence.
#[test]
fn an_mcp_server_with_a_credential_acts_as_the_worker() {
    let estate = Estate::managed("mcp");
    let (w1, k1) = estate.worker(ACTOR);
    let replies = mcp(
        &estate,
        Some(&k1),
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                   "params":{"name":"task_add","arguments":{"title":"nope","as":ACTOR}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                   "params":{"name":"worker_list","arguments":{}}}),
        ],
    );
    assert_eq!(replies[1]["result"]["isError"], true, "{}", replies[1]);
    let message = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        message.contains(&format!(
            "worker {w1} may not run task add: it is coordinator-only"
        )),
        "{message}"
    );
    let listed: Value =
        serde_json::from_str(replies[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let ids: Vec<&str> = listed["workers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|worker| worker["workerId"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![w1.as_str()],
        "a worker sees itself and its descendants only"
    );
}

/// A10 (IDENT-08, IDENT-11, IDENT-14): a worker hands over from its own
/// lease, the create replays under its request id, and a handoff accept
/// follows the lane-actor rule before the target rule.
#[test]
fn a_worker_handoff_replays_and_its_accept_is_bound_to_the_lane_actor() {
    let estate = Estate::managed("handoff");
    let (_w1, k1) = estate.worker(ACTOR);
    let (w5, k5) = estate.worker(SUCCESSOR);
    let task = estate.add_task("hand over");
    let claim = estate.ok_json(Some(&k1), &["claim", &task, "--as", ACTOR, "--json"]);
    assert_eq!(claim["attempt"], 1);
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();
    let create = [
        "handoff",
        "create",
        &task,
        "--lease",
        &lease,
        "--as",
        ACTOR,
        "--to",
        SUCCESSOR,
        "--summary",
        "s",
        "--intent",
        "i",
        "--next-action",
        "n",
        "--repo",
        "/r",
        "--branch",
        "b",
        "--head",
        "abc1234",
        "--dirty",
        "clean",
        "--request-id",
        "hand-00000000001",
        "--json",
    ];
    let first = estate.ok(Some(&k1), &create);
    let second = estate.ok(Some(&k1), &create);
    assert_eq!(first.stdout, second.stdout);
    let handoffs: i64 = estate
        .board()
        .query_row("SELECT COUNT(*) FROM handoffs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(handoffs, 1);
    let handoff: Value = serde_json::from_slice(&first.stdout).unwrap();
    let id = handoff["id"].as_str().unwrap().to_owned();

    estate.refused(
        Some(&k5),
        &["handoff", "accept", &id, "--as", ACTOR],
        &format!("worker {w5} acts as {SUCCESSOR}, not {ACTOR}"),
    );
    let accepted = estate.ok_json(
        Some(&k5),
        &["handoff", "accept", &id, "--as", SUCCESSOR, "--json"],
    );
    assert_eq!(accepted["claim"]["workerId"], w5.as_str(), "{accepted}");
    assert_eq!(accepted["claim"]["attempt"], 2, "{accepted}");

    // A worker holds no lease for a session handoff, so it may not write one.
    estate.refused(
        Some(&k5),
        &[
            "handoff",
            "create",
            "--as",
            SUCCESSOR,
            "--to",
            ACTOR,
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--repo",
            "/r",
            "--branch",
            "b",
            "--head",
            "abc1234",
            "--dirty",
            "clean",
        ],
        &format!("worker {w5} may hand over only a task whose lease it holds"),
    );
}

/// A9's registry half (IDENT-17): a schema-14 registry — this estate's,
/// rewound by dropping the worker tables and the version — is brought to 15
/// on its next open, its principals and grants intact, and serves workers.
#[test]
fn a_schema_14_registry_migrates_to_15_and_serves_workers() {
    let estate = Estate::managed("migrate");
    {
        let registry = estate.registry();
        registry
            .execute_batch(
                "DROP TABLE worker_credentials; DROP TABLE workers; PRAGMA user_version=14;",
            )
            .unwrap();
    }
    let listed = estate.ok_json(None, &["worker", "list", "--json"]);
    assert_eq!(listed["workers"], json!([]), "{listed}");
    assert_eq!(listed["truncated"], false, "{listed}");
    let version: i64 = estate
        .registry()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 15);
    let (w1, k1) = estate.worker(ACTOR);
    let task = estate.add_task("after migration");
    let claim = estate.ok_json(Some(&k1), &["claim", &task, "--as", ACTOR, "--json"]);
    assert_eq!(claim["workerId"], w1.as_str(), "{claim}");
}

/// Run one MCP session to completion: send `requests`, close stdin, and read
/// one reply per request.
fn mcp(estate: &Estate, credential: Option<&str>, requests: &[Value]) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
    command
        .arg("mcp")
        .current_dir(&estate.work)
        .env("XDG_DATA_HOME", &estate.xdg)
        .env_remove("KANBAN_DATA_DIR")
        .env_remove("KANBAN_DB")
        .env_remove("KANBAN_PROJECT")
        .env_remove("KANBAN_WORKER_CREDENTIAL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(credential) = credential {
        command.env("KANBAN_WORKER_CREDENTIAL", credential);
    }
    let mut child = command.spawn().unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for request in requests {
            writeln!(stdin, "{request}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        replies.len(),
        requests.len(),
        "mcp answered {} of {} requests\nstderr: {}",
        replies.len(),
        requests.len(),
        stderr(&output)
    );
    replies
}

/// A worker confined to `root` (and its credential): `root` is a story with
/// one child; `outside` is a sibling task.
fn rooted(estate: &Estate, grant: &str) -> (String, String, String, String) {
    let root = estate.ok_json(
        None,
        &[
            "task",
            "add",
            "zebra root",
            "--type",
            "story",
            "--as",
            ACTOR,
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let inside = estate.ok_json(
        None,
        &[
            "task",
            "add",
            "zebra inside",
            "--parent",
            &root,
            "--as",
            ACTOR,
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let outside = estate.add_task("zebra outside");
    let output = estate.register(None, ACTOR, &["--grant", grant, "--task", &root]);
    assert!(output.status.success(), "{}", stderr(&output));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    (
        root,
        inside,
        outside,
        receipt["credential"].as_str().unwrap().to_owned(),
    )
}

/// IDENT-06: a task root bounds what the worker reads, not only what it
/// writes. Outside the root a single-row read is the generic refusal, and a
/// list, the event log and search carry nothing about the outside task.
#[test]
fn a_task_root_bounds_the_workers_reads() {
    let estate = Estate::managed("rootreads");
    let grant = format!("read=board:{}", estate.board_id);
    let (root, inside, outside, k) = rooted(&estate, &grant);

    estate.refused(Some(&k), &["task", "show", &outside, "--json"], DENIED);
    estate.refused(Some(&k), &["context", &outside], DENIED);
    estate.ok(Some(&k), &["task", "show", &inside, "--json"]);
    estate.ok(Some(&k), &["context", &inside]);

    let list = stdout(&estate.ok(Some(&k), &["task", "list", "--json"]));
    assert!(list.contains(&root) && list.contains(&inside), "{list}");
    assert!(
        !list.contains(&outside),
        "task list leaked {outside}: {list}"
    );

    let events = stdout(&estate.ok(Some(&k), &["events", "--json"]));
    assert!(events.contains(&inside), "{events}");
    assert!(
        !events.contains(&outside),
        "events leaked {outside}: {events}"
    );

    let hits = stdout(&estate.ok(Some(&k), &["search", "zebra", "--json"]));
    assert!(hits.contains(&inside), "{hits}");
    assert!(!hits.contains(&outside), "search leaked {outside}: {hits}");

    // The coordinator, unconfined, still sees the outside task.
    let all = stdout(&estate.ok(None, &["task", "list", "--json"]));
    assert!(all.contains(&outside), "{all}");
}

/// IDENT-08: accepting a handoff whose task was removed is still bound by
/// the task root; the handoff stays pending.
#[test]
fn a_removed_tasks_handoff_outside_the_root_is_not_accepted() {
    let estate = Estate::managed("rootaccept");
    let grant = format!("write=board:{}", estate.board_id);
    let (_root, _inside, outside, k) = rooted(&estate, &grant);
    let claim = estate.ok_json(None, &["claim", &outside, "--as", ACTOR, "--json"]);
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();
    let handoff = estate.ok_json(
        None,
        &[
            "handoff",
            "create",
            &outside,
            "--lease",
            &lease,
            "--as",
            ACTOR,
            "--to",
            ACTOR,
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--repo",
            "/r",
            "--branch",
            "b",
            "--head",
            "abc1234",
            "--dirty",
            "clean",
            "--json",
        ],
    );
    let id = handoff["id"].as_str().unwrap().to_owned();
    estate.ok(
        None,
        &["task", "remove", &outside, "--as", ACTOR, "--force"],
    );

    estate.refused(Some(&k), &["handoff", "accept", &id, "--as", ACTOR], DENIED);
    let status: String = estate
        .board()
        .query_row("SELECT status FROM handoffs WHERE id=?", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(status, "pending");
}

/// IDENT-04: a read-only worker cannot mint a child that writes.
#[test]
fn a_read_only_worker_cannot_register_a_writing_child() {
    let estate = Estate::managed("readchild");
    let b = estate.board_id.clone();
    let (_w1, k1) = estate.worker(ACTOR);
    let reader: Value = serde_json::from_slice(
        &estate
            .register(Some(&k1), ACTOR, &["--grant", &format!("read=board:{b}")])
            .stdout,
    )
    .unwrap();
    let k2 = reader["credential"].as_str().unwrap().to_owned();
    let epoch = estate.epoch();
    let output = estate.register(Some(&k2), ACTOR, &["--grant", &format!("write=board:{b}")]);
    assert!(
        !output.status.success(),
        "a read-only worker minted a writer"
    );
    assert!(stderr(&output).contains(DENIED), "{}", stderr(&output));
    assert_eq!(estate.epoch(), epoch, "a refusal advanced the epoch");
}

/// IDENT-03, IDENT-07: the `worker_retired` event names the worker's parent
/// and the worker that retired it.
#[test]
fn a_retirement_event_names_the_parent_and_the_retirer() {
    let estate = Estate::managed("retireevent");
    let b = estate.board_id.clone();
    let (w1, k1) = estate.worker(ACTOR);
    let child: Value = serde_json::from_slice(
        &estate
            .register(Some(&k1), ACTOR, &["--grant", &format!("read=board:{b}")])
            .stdout,
    )
    .unwrap();
    let w2 = child["workerId"].as_str().unwrap().to_owned();
    estate.ok(Some(&k1), &["worker", "retire", &w2]);
    let payload: Value = serde_json::from_str(
        &estate
            .registry()
            .query_row(
                "SELECT payload FROM policy_events WHERE kind='worker_retired' \
                 ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    let text = payload.to_string();
    assert!(
        text.contains(&format!("\"parentWorkerId\":\"{w1}\"")),
        "{payload}"
    );
    assert!(
        text.contains(&format!("\"retiredByWorkerId\":\"{w1}\"")),
        "{payload}"
    );
    assert!(
        text.contains(&format!("\"principalId\":\"{ALICE}\"")),
        "{payload}"
    );
}

/// IDENT-11: `watch` is long-running and has no generated MCP tool, so it is
/// not on a worker's read list.
#[test]
fn a_worker_may_not_run_watch() {
    let estate = Estate::managed("watch");
    let (w1, k1) = estate.worker(ACTOR);
    let output = estate.run(Some(&k1), &["watch", "--json"]);
    assert!(!output.status.success(), "a worker ran watch");
    assert!(
        stderr(&output).contains(&format!("worker {w1} may not run watch")),
        "{}",
        stderr(&output)
    );
}
