//! Delegated-worker identity, Part A: the claim attempt counter and
//! idempotent claim-path requests (docs/specs/identity.md IDENT-09,
//! IDENT-14, IDENT-17, IDENT-18), at the compiled-process boundary.
//!
//! Every test spawns the real `kanban` binary against a real SQLite board in
//! a private data root. The counter and the receipts live in the store the
//! binary writes through, so an in-process call would not establish what is
//! asserted here.

use rusqlite::Connection;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const ACTOR: &str = "@:t/b/driver";
const SUCCESSOR: &str = "@:t/b/driver-2";

/// One board in a temp data root, addressed by working directory.
struct Estate {
    root: PathBuf,
    data: PathBuf,
    work: PathBuf,
    board_path: PathBuf,
}

impl Estate {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-identity-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let work = root.join("work");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&work).unwrap();
        let mut estate = Self {
            root,
            data,
            work,
            board_path: PathBuf::new(),
        };
        estate.ok_json(&["init", "--name", "Identity", "--json"]);
        estate.board_path = estate.resolve_board_path();
        estate
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanban"))
            .current_dir(&self.work)
            .args(args)
            .env("KANBAN_DATA_DIR", &self.data)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .env_remove("KANBAN_WORKER_CREDENTIAL")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn ok_json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.ok(args).stdout).unwrap()
    }

    fn refused(&self, args: &[&str], sentence: &str) {
        let output = self.run(args);
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

    fn add_task(&self, title: &str) -> String {
        self.ok_json(&["task", "add", title, "--as", ACTOR, "--json"])["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn board(&self) -> Connection {
        Connection::open(&self.board_path).unwrap()
    }

    /// The board file, read once from the registry right after `init` so a
    /// test that rewinds the file is not migrated by the lookup.
    fn resolve_board_path(&self) -> PathBuf {
        let workspaces = self.ok_json(&["workspace", "list", "--json"]);
        workspaces[0]["boardPath"]
            .as_str()
            .expect("workspace list names the board file")
            .into()
    }

    fn count(&self, sql: &str, task: &str) -> i64 {
        self.board()
            .query_row(sql, [task], |row| row.get(0))
            .unwrap()
    }

    fn task_attempt(&self, task: &str) -> i64 {
        self.count("SELECT attempt FROM tasks WHERE id=?", task)
    }
}

impl Drop for Estate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A1's direct-mode half (IDENT-09, IDENT-18): a claim reports the task's
/// attempt and no worker; a release does not lower the counter, and the next
/// claim gets the next attempt. A refused claim does not move it.
#[test]
fn a_direct_claim_reports_its_attempt_and_no_worker() {
    let estate = Estate::new("attempt");
    let task = estate.add_task("count my claims");

    let first = estate.ok_json(&["claim", &task, "--as", ACTOR, "--json"]);
    assert_eq!(first["attempt"], 1, "{first}");
    assert!(first.get("workerId").is_none(), "{first}");
    assert!(first.get("principalId").is_none(), "{first}");
    let lease = first["leaseToken"].as_str().unwrap().to_owned();

    // A claim the live lease refuses writes nothing, the counter included.
    estate.refused(
        &["claim", &task, "--as", SUCCESSOR],
        &format!("task {task} is already claimed by {ACTOR}"),
    );
    assert_eq!(estate.task_attempt(&task), 1);

    estate.ok(&["release", &task, "--lease", &lease]);
    assert_eq!(
        estate.task_attempt(&task),
        1,
        "a release lowered the counter"
    );

    let second = estate.ok_json(&["claim", &task, "--as", ACTOR, "--json"]);
    assert_eq!(second["attempt"], 2, "{second}");
}

/// IDENT-09 through a checkpoint and a handoff: each row records the attempt
/// of the lease it was written under, and accepting the handoff opens the
/// next attempt.
#[test]
fn checkpoints_and_handoffs_record_the_lease_attempt_and_accept_counts_one() {
    let estate = Estate::new("handoff");
    let task = estate.add_task("hand me over");
    let claim = estate.ok_json(&["claim", &task, "--as", ACTOR, "--json"]);
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();

    let checkpoint = estate.ok_json(&[
        "checkpoint",
        &task,
        "--lease",
        &lease,
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
    ]);
    assert_eq!(checkpoint["attempt"], 1, "{checkpoint}");
    assert!(checkpoint.get("workerId").is_none(), "{checkpoint}");

    let handoff = estate.ok_json(&[
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
        "--json",
    ]);
    assert_eq!(handoff["attempt"], 1, "{handoff}");
    let id = handoff["id"].as_str().unwrap().to_owned();

    let accepted = estate.ok_json(&["handoff", "accept", &id, "--as", SUCCESSOR, "--json"]);
    assert_eq!(accepted["claim"]["attempt"], 2, "{accepted}");
    assert_eq!(estate.task_attempt(&task), 2);
}

/// A session handoff holds no lease and so records no attempt.
#[test]
fn a_session_handoff_records_no_attempt() {
    let estate = Estate::new("session");
    let handoff = estate.ok_json(&[
        "handoff",
        "create",
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
        "--json",
    ]);
    assert!(handoff.get("attempt").is_none(), "{handoff}");
}

/// A7 (IDENT-14), direct mode: the same request id with the same arguments
/// replays the first answer byte for byte and writes nothing; with other
/// arguments it is refused and writes nothing.
#[test]
fn a_claim_request_id_replays_byte_identically_and_refuses_other_arguments() {
    let estate = Estate::new("replay");
    let task = estate.add_task("retry me");
    let key = "req-000000000001";

    let first = estate.ok(&["claim", &task, "--as", ACTOR, "--request-id", key, "--json"]);
    let second = estate.ok(&["claim", &task, "--as", ACTOR, "--request-id", key, "--json"]);
    assert!(!first.stdout.is_empty());
    assert_eq!(
        first.stdout, second.stdout,
        "the replay is not byte-identical"
    );
    assert_eq!(
        estate.count("SELECT count(*) FROM task_claims WHERE task_id=?", &task),
        1
    );
    assert_eq!(estate.task_attempt(&task), 1);
    let claimed_events = estate.count(
        "SELECT count(*) FROM events WHERE task_id=? AND kind='task_claimed'",
        &task,
    );
    assert_eq!(claimed_events, 1, "the replay wrote a second claim event");

    estate.refused(
        &[
            "claim",
            &task,
            "--as",
            ACTOR,
            "--lease-minutes",
            "30",
            "--request-id",
            key,
        ],
        &format!("request {key} was already used for a different claim request"),
    );
    assert_eq!(estate.task_attempt(&task), 1);

    // The replay still answers after the lease it named has ended.
    let lease = serde_json::from_slice::<Value>(&first.stdout).unwrap()["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned();
    estate.ok(&["release", &task, "--lease", &lease]);
    let after = estate.ok(&["claim", &task, "--as", ACTOR, "--request-id", key, "--json"]);
    assert_eq!(first.stdout, after.stdout);
    assert_eq!(
        estate.count("SELECT count(*) FROM task_claims WHERE task_id=?", &task),
        0,
        "a replay took a new lease"
    );
}

/// IDENT-14: a refused request stores no receipt, so the same key can be
/// used again once the refusal's cause is gone; a malformed key is refused
/// before anything is read.
#[test]
fn a_refused_request_stores_no_receipt_and_a_malformed_key_is_refused() {
    let estate = Estate::new("refused");
    let task = estate.add_task("contended");
    let key = "req-000000000002";
    let held = estate.ok_json(&["claim", &task, "--as", SUCCESSOR, "--json"]);

    estate.refused(
        &["claim", &task, "--as", ACTOR, "--request-id", key],
        "is already claimed by",
    );
    assert_eq!(
        estate
            .board()
            .query_row("SELECT count(*) FROM request_receipts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );

    // The holder releases; the task stays assigned to it, so the same key
    // now succeeds for that holder. Had the refusal stored a receipt, this
    // call would be refused as a different request instead.
    let lease = held["leaseToken"].as_str().unwrap();
    estate.ok(&["release", &task, "--lease", lease]);
    let retried = estate.ok_json(&[
        "claim",
        &task,
        "--as",
        SUCCESSOR,
        "--request-id",
        key,
        "--json",
    ]);
    assert_eq!(retried["agentID"], SUCCESSOR, "{retried}");

    for bad in ["short-key", "has a space in it!!", &"x".repeat(129)] {
        estate.refused(
            &["claim", &task, "--as", ACTOR, "--request-id", bad],
            "--request-id must be 16 to 128 ASCII letters",
        );
    }
}

/// IDENT-14 on checkpoint: a replayed checkpoint prints the same bytes and
/// adds no second checkpoint row.
#[test]
fn a_checkpoint_request_id_replays_without_a_second_row() {
    let estate = Estate::new("cp-replay");
    let task = estate.add_task("checkpoint me");
    let claim = estate.ok_json(&["claim", &task, "--as", ACTOR, "--json"]);
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();
    let args = [
        "checkpoint",
        &task,
        "--lease",
        &lease,
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
        "--request-id",
        "cp-0000000000001",
        "--json",
    ];
    let first = estate.ok(&args);
    let second = estate.ok(&args);
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(
        estate.count("SELECT count(*) FROM checkpoints WHERE task_id=?", &task),
        1
    );
}

/// A9 (IDENT-09, IDENT-17), this binary's half: a board written at schema 37
/// with one leased and one unleased task migrates to 38; the leased task and
/// its lease read attempt 1, the unleased task's next claim gets attempt 1,
/// and the old lease still heartbeats without a credential. A board from a
/// newer schema is refused with the version sentence a baseline binary gives
/// this one's boards.
#[test]
fn a_schema_37_board_migrates_with_leases_at_attempt_one() {
    let estate = Estate::new("migrate");
    let leased = estate.add_task("leased before the upgrade");
    let unleased = estate.add_task("never claimed");
    let claim = estate.ok_json(&["claim", &leased, "--as", ACTOR, "--json"]);
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();

    // Rewind the file to schema 37 exactly: drop the v38 columns and table.
    {
        let board = estate.board();
        board
            .execute_batch(
                "DROP TABLE request_receipts;
                 ALTER TABLE tasks DROP COLUMN attempt;
                 ALTER TABLE task_claims DROP COLUMN attempt;
                 ALTER TABLE task_claims DROP COLUMN worker_id;
                 ALTER TABLE task_claims DROP COLUMN principal_id;
                 ALTER TABLE checkpoints DROP COLUMN attempt;
                 ALTER TABLE checkpoints DROP COLUMN worker_id;
                 ALTER TABLE checkpoints DROP COLUMN principal_id;
                 ALTER TABLE handoffs DROP COLUMN attempt;
                 ALTER TABLE handoffs DROP COLUMN worker_id;
                 ALTER TABLE handoffs DROP COLUMN principal_id;
                 PRAGMA user_version=37;",
            )
            .unwrap();
    }

    let shown = estate.ok_json(&["task", "show", &leased, "--json"]);
    assert_eq!(shown["claim"]["attempt"], 1, "{shown}");
    assert_eq!(estate.task_attempt(&leased), 1);
    assert_eq!(estate.task_attempt(&unleased), 0);
    let version: i64 = estate
        .board()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 38);

    estate.ok(&["heartbeat", &leased, "--lease", &lease]);
    let next = estate.ok_json(&["claim", &unleased, "--as", ACTOR, "--json"]);
    assert_eq!(next["attempt"], 1, "{next}");

    estate
        .board()
        .execute_batch("PRAGMA user_version=40;")
        .unwrap();
    estate.refused(
        &["task", "list", "--json"],
        "database version 40 is newer than supported version 39",
    );
}
