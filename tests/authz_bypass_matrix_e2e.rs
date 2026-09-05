//! The epic's bypass matrix, at the compiled-process boundary (ADR-006,
//! ADR-038 clauses 5 and 9).
//!
//! Seven classes, one test each: cross-board, retagging, history, projection,
//! actor, selector, and revocation. Every one spawns the real `kanban` binary
//! against a real SQLite estate — a library call in this process would not
//! establish the thing being asserted, because the authority under test is
//! minted from the SPAWNED process's own kernel identity and nothing else.
//!
//! How a managed estate is reached without the broker. The broker's socket hop
//! is a separate slice, and `routing::board_authz` stands in for it exactly as
//! `local_actor` already does for the `access` command family: the process's
//! own effective UID, ADR-033's two-way passwd check, the frozen
//! `{username, uid}` principal, and that principal's active grants. So the
//! fixture binds a principal for the UID the test process — and therefore the
//! binary it spawns — actually runs as, and grants it scopes. Nothing the
//! command line can say contributes to that decision, which is the point of
//! several of these tests.
//!
//! The grants are inserted into the registry directly rather than through
//! `access grant`, because clause 6's bootstrap is root-only and these tests
//! are not. The rows are the same rows `access grant` writes, read back by the
//! same `active_grants_for_principal_on` query, and retired by the same state
//! transition `access revoke` performs.

use rusqlite::Connection;
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The one refusal every denial in this file must be, byte for byte. A second
/// wording anywhere would be an oracle.
const DENIED: &str = "denied or not found";

/// How long a `watch --follow` assertion waits for a line that should arrive,
/// and how long it waits before being satisfied that a line will NOT arrive.
/// The poll interval is 250ms, so both are many polls wide.
const APPEAR: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_secs(4);

/// One scope grant: a capability and the ADR-033 atom list it applies to.
type Scope = (&'static str, Vec<String>);

fn board_scope(capability: &'static str, board: &str) -> Scope {
    (capability, vec![format!("board:{board}")])
}

fn tag_scope(capability: &'static str, board: &str, tag: &str) -> Scope {
    (
        capability,
        vec![format!("board:{board}"), format!("tag:{tag}")],
    )
}

/// Everything a full owner of one board holds: the board and its tag wildcard,
/// at both capabilities.
fn owner_of(board: &str) -> Vec<Scope> {
    vec![
        board_scope("read", board),
        board_scope("write", board),
        ("read", vec![format!("board:{board}"), "*".to_owned()]),
        ("write", vec![format!("board:{board}"), "*".to_owned()]),
    ]
}

/// Two boards in one canonical estate, plus a working directory that resolves
/// to each, so every command can be addressed by CWD alone — the one route
/// managed enforcement does not refuse as a selector bypass.
struct ManagedEstate {
    root: PathBuf,
    xdg: PathBuf,
    work_a: PathBuf,
    work_b: PathBuf,
    board_a: String,
    board_b: String,
    id_a: String,
    id_b: String,
}

impl ManagedEstate {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-authz-matrix-{label}-{}-{unique}",
            std::process::id()
        ));
        let xdg = root.join("xdg");
        let work_a = root.join("work-a");
        let work_b = root.join("work-b");
        for directory in [&xdg, &work_a, &work_b] {
            fs::create_dir_all(directory).unwrap();
        }
        let mut estate = Self {
            root,
            xdg,
            work_a,
            work_b,
            board_a: String::new(),
            board_b: String::new(),
            id_a: String::new(),
            id_b: String::new(),
        };
        // Both boards are created while enforcement is still `direct`, which
        // is how a real estate reaches `managed`: the one-way transition
        // happens to a registry that already has boards in it.
        let work_a = estate.work_a.clone();
        let work_b = estate.work_b.clone();
        let alpha = estate.ok_json(&work_a, &["init", "--name", "Alpha", "--json"]);
        let beta = estate.ok_json(&work_b, &["init", "--name", "Beta", "--json"]);
        estate.board_a = alpha["boardPath"].as_str().unwrap().to_owned();
        estate.board_b = beta["boardPath"].as_str().unwrap().to_owned();
        estate.id_a = board_id(&estate.board_a);
        estate.id_b = board_id(&estate.board_b);
        estate
    }

    /// A spawned binary with the canonical root pinned and every selector
    /// default cleared, so `present_bypasses` sees nothing and the command is
    /// resolved from the working directory.
    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(cwd)
            .env("XDG_DATA_HOME", &self.xdg)
            .env_remove("KANBAN_DATA_DIR")
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd).args(args).output().unwrap()
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn ok_json(&self, cwd: &Path, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(cwd, args)).unwrap()
    }

    /// Assert one command is refused with exactly the generic denial.
    fn denied(&self, cwd: &Path, args: &[&str]) {
        let output = self.run(cwd, args);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "{args:?} succeeded but must be denied\nstdout: {stdout}"
        );
        assert!(
            stderr.contains(DENIED),
            "{args:?} was refused with the wrong message\nstderr: {stderr}"
        );
    }

    fn registry(&self) -> Connection {
        Connection::open(self.xdg.join("kanban").join("registry.db")).unwrap()
    }

    /// Bind a principal for the identity the spawned binary resolves for
    /// ITSELF, and give it exactly `scopes`.
    fn bind_self(&self, principal: &str, scopes: &[Scope]) {
        self.bind(principal, &self_username(), self_uid(), scopes);
    }

    /// Bind a principal that is NOT this process: another username, and a UID
    /// that cannot be ours. It exists so a test can put a genuinely
    /// well-authorized principal's name in `--as` and watch it make no
    /// difference.
    fn bind_other(&self, principal: &str, username: &str, scopes: &[Scope]) {
        self.bind(principal, username, self_uid() + 4242, scopes);
    }

    fn bind(&self, principal: &str, username: &str, uid: u32, scopes: &[Scope]) {
        self.registry()
            .execute(
                "INSERT INTO principals(id,username,uid,enabled,bound_at_epoch,bound_by_event_id) \
                 VALUES(?1,?2,?3,1,0,'pe-00000000')",
                rusqlite::params![principal, username, uid],
            )
            .unwrap();
        self.grant(principal, scopes);
    }

    fn grant(&self, principal: &str, scopes: &[Scope]) {
        let connection = self.registry();
        for (capability, atoms) in scopes {
            connection
                .execute(
                    "INSERT INTO grants(id,principal_id,capability,scope,state,origin,\
                     granted_at_epoch,granted_by_event_id) \
                     VALUES(?1,?2,?3,?4,'active','grant',0,'pe-00000000')",
                    rusqlite::params![
                        // Unique per row: re-granting a scope that was revoked earlier in a
                        // test inserts a SECOND grant row rather than reviving the first,
                        // which is what `access grant` does too.
                        format!(
                            "g-{principal}-{capability}-{}",
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_nanos()
                        ),
                        principal,
                        capability,
                        serde_json::to_string(atoms).unwrap(),
                    ],
                )
                .unwrap();
        }
    }

    /// Retire every active grant whose atom list contains `atom` — the same
    /// state transition `access revoke` performs.
    fn revoke_atom(&self, atom: &str) {
        let retired = self
            .registry()
            .execute(
                "UPDATE grants SET state='retired' \
                 WHERE state='active' AND EXISTS (\
                   SELECT 1 FROM json_each(grants.scope) WHERE json_each.value=?1)",
                [atom],
            )
            .unwrap();
        assert!(retired > 0, "no active grant named {atom}");
    }

    fn enforce(&self, state: &str) {
        self.registry()
            .execute("UPDATE enforcement_state SET state=? WHERE id=1", [state])
            .unwrap();
    }
}

impl Drop for ManagedEstate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The board UUID a board path names — the `<uuid>.db` stem ADR-032 mints and
/// `board_id_from_path` reads back.
fn board_id(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// This process's effective UID, from the same kernel the guard asks.
fn self_uid() -> u32 {
    id_output(&["-u"]).parse().unwrap()
}

/// This process's effective user name — `getpwuid(geteuid())->pw_name`, which
/// is exactly what the guard resolves and then puts through the two-way
/// passwd check.
fn self_username() -> String {
    id_output(&["-un"])
}

fn id_output(args: &[&str]) -> String {
    let output = Command::new("id").args(args).output().unwrap();
    assert!(output.status.success(), "id {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

// ---------------------------------------------------------------------------
// 1. Cross-board: authority on one board reaches no surface of another.
// ---------------------------------------------------------------------------

/// A caller who fully owns board A reaches NOTHING on board B — not through
/// search, not through the event ledger, not through a deployment projection,
/// and not through a whole-file copy. Board B is addressed the only way
/// managed enforcement permits, by working directory, so this is the guard
/// answering and not the selector gate.
#[test]
fn cross_board_authority_reaches_no_derived_surface_of_another_board() {
    let estate = ManagedEstate::new("cross-board");
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();

    estate.ok_json(
        &work_a,
        &["task", "add", "alpha row", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_b,
        &["task", "add", "beta row", "--as", "seed", "--json"],
    );

    estate.bind_self("p-a-owner", &owner_of(&estate.id_a));
    estate.enforce("managed");

    let snapshot = estate.root.join("snap-b").to_string_lossy().into_owned();
    for args in [
        vec!["search", "beta", "--json"],
        vec!["events", "--json"],
        vec!["deploy", "list", "--json"],
        vec!["deploy", "current", "--json"],
        vec!["archive", "--older-than-days", "1", "--as", "x", "--json"],
        vec!["search-rebuild", "--as", "x", "--json"],
        vec!["backup", "--output", &snapshot, "--json"],
    ] {
        estate.denied(&work_b, &args);
    }

    // The positive control matters as much as the refusals: the same commands
    // on the caller's OWN board still work, so what was measured above is the
    // board boundary and not a broken build.
    assert!(
        estate
            .ok(&work_a, &["search", "alpha", "--json"])
            .contains("alpha row")
    );
    estate.ok_json(&work_a, &["events", "--json"]);
    estate.ok_json(&work_a, &["deploy", "list", "--json"]);
}

// ---------------------------------------------------------------------------
// 2. Retagging: both scopes on a write, and the row's REAL tags on a read.
// ---------------------------------------------------------------------------

/// Two halves of the same class.
///
/// The write half: a caller who can see `alpha` but not `beta` cannot move a
/// row from one to the other, and the row is unchanged afterwards.
///
/// The read half is what this slice adds, and it is why the check cannot live
/// on the index. `search_documents.tags` is a projected copy, so the copy is
/// reset to empty behind the guard's back — which is exactly what a board
/// restored from an older snapshot, or one indexed before a retag, looks like.
/// A guard reading the copy would hand the row over. It stays hidden, because
/// the decision is made against `task_tags`.
#[test]
fn retagging_is_refused_and_a_stale_index_copy_does_not_reveal_the_row() {
    let estate = ManagedEstate::new("retag");
    let work_a = estate.work_a.clone();

    estate.ok_json(&work_a, &["tag", "add", "alpha", "--as", "seed", "--json"]);
    estate.ok_json(&work_a, &["tag", "add", "beta", "--as", "seed", "--json"]);
    let row = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "movable subject",
            "--tag",
            "alpha",
            "--as",
            "seed",
            "--json",
        ],
    );
    let row_id = row["id"].as_str().unwrap().to_owned();

    // Board scope at both capabilities, plus `alpha` — and never `beta`.
    estate.bind_self(
        "p-alpha-only",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "alpha"),
            tag_scope("write", &estate.id_a, "alpha"),
        ],
    );
    estate.enforce("managed");

    estate.denied(
        &work_a,
        &["task", "update", &row_id, "--tag", "beta", "--as", "actor"],
    );

    // The refused write did not partly apply: the row is still `alpha`, which
    // this caller can still see.
    let after = estate.ok_json(&work_a, &["task", "show", &row_id, "--json"]);
    assert_eq!(after["tags"], serde_json::json!(["alpha"]));

    // Now the read half. Retag the row to `beta` through the direct estate —
    // the guard is not what is under test here — then desynchronise the index
    // so its copy of the tags is empty.
    estate.enforce("direct");
    estate.ok_json(
        &work_a,
        &["task", "update", &row_id, "--tag", "beta", "--as", "seed"],
    );
    let board = Connection::open(&estate.board_a).unwrap();
    let stale = board
        .execute(
            "UPDATE search_documents SET tags='' WHERE task_id=?",
            [&row_id],
        )
        .unwrap();
    assert!(stale > 0, "no search document to make stale");
    assert_eq!(
        board
            .query_row(
                "SELECT tags FROM search_documents WHERE task_id=? AND source_kind='task'",
                [&row_id],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "",
        "the index copy should now claim the row has no tags"
    );
    assert_eq!(
        board
            .query_row(
                "SELECT group_concat(tag) FROM task_tags WHERE task_id=?",
                [&row_id],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "beta",
        "the row itself should really carry beta"
    );
    drop(board);
    estate.enforce("managed");

    // The index says "untagged", the row says `beta`, and the caller holds
    // neither `beta` nor the wildcard. The row must not come back.
    let hits = estate.ok(&work_a, &["search", "movable", "--json"]);
    assert!(
        !hits.contains(&row_id) && !hits.contains("movable subject"),
        "a stale index copy revealed a row the caller may not see: {hits}"
    );
    assert!(
        !hits.contains("beta"),
        "the hidden row's tag leaked through the receipt: {hits}"
    );
}

// ---------------------------------------------------------------------------
// 3. History: a row's past is not a second read path to the row.
// ---------------------------------------------------------------------------

/// A task is created untagged and later tagged `secret`. The caller holds
/// board read and not `secret`.
///
/// Two things must hold. The row's trail must not be readable, because
/// `task_added` and `task_updated` carry titles, bodies and previous bodies —
/// history would otherwise be a complete second copy of an invisible row. And
/// the trail must not NAME the tag: the retag event's semantic snapshot froze
/// `["secret"]`, so an events listing filtered only on the row's current tags
/// would still print the word that hid it.
#[test]
fn history_does_not_reconstruct_a_row_or_name_the_tag_that_hid_it() {
    let estate = ManagedEstate::new("history");
    let work_a = estate.work_a.clone();

    estate.ok_json(&work_a, &["tag", "add", "secret", "--as", "seed", "--json"]);
    let hidden = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "classified subject matter",
            "--body",
            "the body a trail would leak",
            "--as",
            "seed",
            "--json",
        ],
    );
    let hidden_id = hidden["id"].as_str().unwrap().to_owned();
    let open = estate.ok_json(
        &work_a,
        &["task", "add", "ordinary row", "--as", "seed", "--json"],
    );
    let open_id = open["id"].as_str().unwrap().to_owned();
    // The retag itself: this is the event whose snapshot names `secret`.
    estate.ok_json(
        &work_a,
        &[
            "task", "update", &hidden_id, "--tag", "secret", "--as", "seed",
        ],
    );

    // The row whose tags have MOVED ON. It carried `secret` and no longer
    // does, so its live tag set is empty and board scope alone would show its
    // whole trail — including the two events whose frozen snapshots still say
    // `["secret"]`. Only the snapshot half of the check catches this one, and
    // without it the trail of a row that used to be confidential is readable
    // by anyone holding the board.
    let former = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "formerly classified",
            "--tag",
            "secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    let former_id = former["id"].as_str().unwrap().to_owned();
    estate.ok_json(
        &work_a,
        &["task", "update", &former_id, "--clear-tags", "--as", "seed"],
    );

    estate.bind_self(
        "p-board-only",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");

    // Naming the row gives the generic denial, not `task <id> not found`.
    estate.denied(&work_a, &["events", "--task", &hidden_id, "--json"]);

    // The board-wide ledger simply does not contain it — and the listing must
    // not refuse mid-enumeration either, because that would announce that
    // there is history here the caller cannot have.
    let ledger = estate.ok(&work_a, &["events", "--limit", "100", "--all", "--json"]);
    assert!(
        !ledger.contains(&hidden_id),
        "the hidden row's history was readable: {ledger}"
    );
    assert!(
        !ledger.contains("classified subject matter"),
        "the hidden row's title leaked through its trail: {ledger}"
    );
    assert!(
        !ledger.contains("the body a trail would leak"),
        "the hidden row's body leaked through its trail: {ledger}"
    );
    // The retag's own frozen snapshot named `["secret"]`, and no event this
    // caller is handed may carry it. Asserted on the parsed rows rather than
    // the raw text, because `tag add secret` also wrote a BOARD-level
    // `tag_added` event whose `taskID` is null: a tag's existence is board
    // vocabulary, exactly as `tag list` already treats it, and the tag-scope
    // rule is about tagged ROWS. That distinction is the claim being made
    // here, so it is made precisely.
    let events: Vec<Value> = serde_json::from_str(&ledger).unwrap();
    for event in &events {
        let frozen = &event["payload"]["_semanticV1"]["tags"];
        assert!(
            frozen != &serde_json::json!(["secret"]) && !frozen.to_string().contains("secret"),
            "an event's semantic snapshot named the tag the caller lacks: {event}"
        );
        assert!(
            event["taskID"] != serde_json::json!(hidden_id),
            "an event about the hidden row was delivered: {event}"
        );
    }
    // The visible row's history is still there, so this is a filter and not an
    // empty result.
    assert!(
        ledger.contains(&open_id),
        "the visible row's history disappeared too: {ledger}"
    );

    // Granting the tag makes exactly the withheld trail appear, which is what
    // proves the tag was the reason.
    estate.enforce("direct");
    estate.grant("p-board-only", &[tag_scope("read", &estate.id_a, "secret")]);
    estate.enforce("managed");
    assert!(
        estate
            .ok(&work_a, &["events", "--task", &hidden_id, "--json"])
            .contains(&hidden_id)
    );
}

// ---------------------------------------------------------------------------
// 4. Projection: no bulk path is the one way to read everything.
// ---------------------------------------------------------------------------

/// A deployment attempt is a projection of a task; a backup is a projection of
/// the whole board. Neither may hand over what the row surfaces withhold.
///
/// The deployment view is checked against the subject task's REAL tags, read
/// at query time rather than copied onto the immutable attempt, so retagging
/// the subject AFTER the attempt was recorded still hides the attempt.
#[test]
fn bulk_projections_do_not_hand_over_rows_the_row_surfaces_withhold() {
    let estate = ManagedEstate::new("projection");
    let work_a = estate.work_a.clone();

    estate.ok_json(&work_a, &["tag", "add", "secret", "--as", "seed", "--json"]);
    let subject = estate.ok_json(
        &work_a,
        &["task", "add", "deployed subject", "--as", "seed", "--json"],
    );
    let subject_id = subject["id"].as_str().unwrap().to_owned();
    let attempt = estate.ok_json(
        &work_a,
        &[
            "deploy",
            "start",
            "--repo",
            "kanban",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "--tier",
            "@_bdt",
            "--environment",
            "branch-dev-testing",
            "--host",
            "geoywsMBP",
            "--url",
            "http://localhost:9999",
            "--task",
            &subject_id,
            "--as",
            "seed",
            "--json",
        ],
    );
    let attempt_id = attempt["id"].as_str().unwrap().to_owned();
    // The subject becomes invisible only AFTER the attempt was recorded, so a
    // guard reading a tag set frozen at deploy time would still show it.
    estate.ok_json(
        &work_a,
        &[
            "task",
            "update",
            &subject_id,
            "--tag",
            "secret",
            "--as",
            "seed",
        ],
    );

    estate.bind_self(
        "p-board-only",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");

    estate.denied(&work_a, &["deploy", "show", &attempt_id, "--json"]);
    let listed = estate.ok(&work_a, &["deploy", "list", "--all", "--json"]);
    assert!(
        !listed.contains(&attempt_id) && !listed.contains(&subject_id),
        "a deployment projection revealed an invisible subject: {listed}"
    );

    // The whole-board paths: a caller who cannot see one tagged row cannot
    // take the file that contains it, cannot sweep it into the archive, and
    // cannot rewrite its index entry.
    let snapshot = estate.root.join("snap-a").to_string_lossy().into_owned();
    estate.denied(&work_a, &["backup", "--output", &snapshot, "--json"]);
    estate.denied(
        &work_a,
        &["archive", "--older-than-days", "1", "--as", "x", "--json"],
    );
    estate.denied(&work_a, &["search-rebuild", "--as", "x", "--json"]);

    // With the tag granted every one of them works, so the refusals above were
    // the tag and not the command.
    estate.enforce("direct");
    estate.grant(
        "p-board-only",
        &[
            tag_scope("read", &estate.id_a, "secret"),
            tag_scope("write", &estate.id_a, "secret"),
        ],
    );
    // `backup` is an ESTATE-wide command: it walks every registered board, so
    // its positive control needs authority over the other board too. That is
    // itself the observation — a cross-board bulk command fails closed on the
    // first board the caller cannot read, rather than quietly omitting it.
    estate.grant("p-board-only", &owner_of(&estate.id_b));
    estate.enforce("managed");
    assert!(
        estate
            .ok(&work_a, &["deploy", "show", &attempt_id, "--json"])
            .contains(&attempt_id)
    );
    // A fresh destination: the refused attempt above had already written the
    // registry half of the snapshot before the board's gate stopped it, and
    // `backup` refuses to overwrite an existing destination.
    let permitted = estate
        .root
        .join("snap-a-permitted")
        .to_string_lossy()
        .into_owned();
    estate.ok_json(&work_a, &["backup", "--output", &permitted, "--json"]);
}

// ---------------------------------------------------------------------------
// 5. Actor: a claimed identity is a label, never authority.
// ---------------------------------------------------------------------------

/// `--as` is the audit actor and nothing else. ADR-038 clause 10 says plainly
/// that a claimed actor "never resolves to a principal or affects
/// authorization", so naming a principal that DOES own the board changes
/// nothing.
///
/// The name passed is not a fiction: a real, fully-authorized principal is
/// bound under another username first, so what is being offered is the exact
/// username of a principal that could do this, from a process that is not it.
#[test]
fn a_claimed_actor_never_becomes_authority() {
    let estate = ManagedEstate::new("actor");
    let work_a = estate.work_a.clone();

    estate.ok_json(
        &work_a,
        &["task", "add", "owned row", "--as", "seed", "--json"],
    );

    // This other principal really does own board A.
    estate.bind_other(
        "p-real-owner",
        "kanban-board-owner",
        &owner_of(&estate.id_a),
    );
    // This process's own principal owns board B, and nothing on A.
    estate.bind_self("p-elsewhere", &owner_of(&estate.id_b));
    estate.enforce("managed");

    // Every surface that ACCEPTS an audit actor, offered the username of a
    // principal that really could do this. `task list`, `events` and `search`
    // are absent on purpose: they do not take `--as` at all, so there is no
    // field on a read command an actor claim could even be written into.
    for actor in ["kanban-board-owner", "seed", "root", "p-real-owner"] {
        estate.denied(&work_a, &["task", "add", "forged", "--as", actor, "--json"]);
        estate.denied(&work_a, &note_on("t-nonexistent", "forged note"));
        estate.denied(
            &work_a,
            &["archive", "--older-than-days", "1", "--as", actor, "--json"],
        );
        estate.denied(&work_a, &["search-rebuild", "--as", actor, "--json"]);
        estate.denied(
            &work_a,
            &[
                "deploy",
                "start",
                "--repo",
                "kanban",
                "--commit",
                "0123456789abcdef0123456789abcdef01234567",
                "--tier",
                "@_bdt",
                "--environment",
                "branch-dev-testing",
                "--host",
                "geoywsMBP",
                "--url",
                "http://localhost:9999",
                "--as",
                actor,
                "--json",
            ],
        );
    }
    // A read command cannot carry an actor claim at all, and is denied on the
    // authority the process actually has.
    estate.denied(&work_a, &["task", "list", "--json"]);
    estate.denied(&work_a, &["events", "--json"]);
    estate.denied(&work_a, &["search", "owned", "--json"]);

    // And nothing was written by any of those attempts.
    estate.enforce("direct");
    let rows = estate.ok(&work_a, &["task", "list", "--json"]);
    assert!(
        !rows.contains("forged"),
        "a claimed actor wrote a row: {rows}"
    );
}

// ---------------------------------------------------------------------------
// 6. Selector: no selector reaches a board the caller lacks.
// ---------------------------------------------------------------------------

/// Under managed enforcement every route around the broker is refused BY NAME
/// before a board is opened, so a selector cannot be used to reach another
/// tenant's board — and, just as important, the refusal is a refusal rather
/// than a silent downgrade to a direct open of a file this caller can still
/// read on this host.
///
/// Each attempt names board B, which this caller has no authority over at all,
/// so both layers are present: the selector gate refuses the route, and the
/// guard would have refused the rows.
#[test]
fn no_selector_reaches_a_board_the_caller_has_no_authority_over() {
    let estate = ManagedEstate::new("selector");
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();

    estate.ok_json(
        &work_b,
        &["task", "add", "beta secret row", "--as", "seed", "--json"],
    );
    estate.bind_self("p-a-owner", &owner_of(&estate.id_a));
    estate.enforce("managed");

    // The typed flags, each refused by the name the caller typed.
    for (flag, value) in [
        ("--db", estate.board_b.as_str()),
        ("--project", "Beta"),
        ("--workspace", work_b.to_str().unwrap()),
    ] {
        let output = estate.run(&work_a, &["task", "list", flag, value, "--json"]);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(!output.status.success(), "{flag} was not refused");
        assert!(
            stderr.contains("managed mode refuses") && stderr.contains(flag),
            "{flag} was refused without naming the bypass: {stderr}"
        );
        assert!(
            !stdout.contains("beta secret row"),
            "{flag} returned rows from the other board: {stdout}"
        );
    }

    // The environment defaults, same treatment.
    for (key, value) in [
        ("KANBAN_DB", estate.board_b.as_str()),
        ("KANBAN_PROJECT", "Beta"),
    ] {
        let output = estate
            .command(&work_a)
            .env(key, value)
            .args(["task", "list", "--json"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(!output.status.success(), "{key} was not refused");
        assert!(
            stderr.contains(key),
            "{key} was refused without naming it: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("beta secret row"),
            "{key} returned rows from the other board"
        );
    }

    // And a repointed data root cannot make a managed estate look direct.
    let output = estate
        .command(&work_a)
        .env("KANBAN_DATA_DIR", estate.xdg.join("kanban"))
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("KANBAN_DATA_DIR"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// 7. Revocation: authority loss lands on a LIVE stream.
// ---------------------------------------------------------------------------

/// The subtle one. `watch --follow` is cursor-native and long-lived, so a
/// guard that resolved its authority once at startup would keep delivering
/// rows for as long as the subscriber stayed connected — and a subscriber
/// stays connected for hours. Revocation would then mean "revoked at the next
/// reconnect", which is not revocation.
///
/// This starts a real `watch --follow` process, observes a tagged row's event
/// arrive, retires the tag grant while the stream is still open, and then
/// observes the next event on that same row NOT arrive. The process is
/// asserted still running and still producing output, so what is measured is a
/// live stream that stopped delivering — not a crashed one, and not a
/// reconnect. Restoring the grant then makes the withheld row appear on the
/// SAME stream, which is only possible if the authority is re-read per poll.
#[test]
fn revoking_authority_stops_a_live_watch_stream_without_a_reconnect() {
    let estate = ManagedEstate::new("revocation");
    let work_a = estate.work_a.clone();

    estate.ok_json(&work_a, &["tag", "add", "live", "--as", "seed", "--json"]);
    let watched = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "watched row",
            "--tag",
            "live",
            "--as",
            "seed",
            "--json",
        ],
    );
    let watched_id = watched["id"].as_str().unwrap().to_owned();

    estate.bind_self(
        "p-watcher",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "live"),
            tag_scope("write", &estate.id_a, "live"),
        ],
    );
    estate.enforce("managed");

    let mut stream = Stream::start(
        estate
            .command(&work_a)
            .args(["watch", "--tag", "live", "--follow", "--json"]),
    );

    // Before revocation: a note on the watched row is delivered as an event
    // envelope. The envelope carries the event's identity, not the note body,
    // so what is counted is `"type":"event"` — a heartbeat is the other kind.
    estate.ok_json(&work_a, &note_on(&watched_id, "before-revocation"));
    stream.wait_for_event(APPEAR);

    // Revoke, with the stream still open and the process untouched.
    estate.revoke_atom("tag:live");

    // After revocation: the next event on the same row must not be delivered.
    stream.drain();
    estate.ok_json(&work_a, &note_on(&watched_id, "after-revocation"));
    thread::sleep(SETTLE);
    let after = stream.drain();
    assert_eq!(
        events_in(&after),
        0,
        "a revoked watcher was still delivered its row:\n{}",
        after.join("\n")
    );

    // It is a LIVE stream that went quiet, not a dead one: the process is
    // still running and still producing output, because the board tail still
    // sees the rows the filter now rejects and keeps the cursor moving.
    assert!(
        stream.running(),
        "the watch process exited instead of withholding the row"
    );
    assert!(
        !after.is_empty(),
        "the stream produced nothing at all after revocation, so nothing was measured"
    );

    // Restore the grant and write a THIRD note. The withheld one is not
    // replayed — the tail advanced the cursor past it, which is deliberate:
    // the alternative is a cursor stalled behind a row the subscriber may
    // never be allowed to see. What matters is that delivery resumes on the
    // SAME process, which is only possible because the authority is re-minted
    // once per poll rather than cached for the life of the stream.
    estate.enforce("direct");
    estate.grant("p-watcher", &[tag_scope("read", &estate.id_a, "live")]);
    estate.enforce("managed");
    stream.drain();
    estate.ok_json(&work_a, &note_on(&watched_id, "after-restore"));
    stream.wait_for_event(APPEAR);
}

/// How many of these lines are event envelopes rather than heartbeats.
fn events_in(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| line.contains("\"type\":\"event\""))
        .count()
}

/// `kanban note ID TEXT --as AGENT` — the body is positional.
fn note_on<'a>(task: &'a str, body: &'a str) -> Vec<&'a str> {
    vec![
        "note", task, body, "--as", "seed", "--kind", "progress", "--json",
    ]
}

/// A spawned `watch --follow`, with its stdout drained by a reader thread into
/// a channel so the test can assert about what has arrived so far without
/// blocking on what has not.
struct Stream {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Stream {
    fn start(command: &mut Command) -> Self {
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().expect("watch stdout is piped");
        let (sender, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Everything that has arrived since the last drain.
    fn drain(&mut self) -> Vec<String> {
        let mut fresh = Vec::new();
        loop {
            match self.lines.try_recv() {
                Ok(line) => fresh.push(line),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        self.seen.extend(fresh.iter().cloned());
        fresh
    }

    /// Block until an event envelope arrives, or fail with everything seen.
    fn wait_for_event(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        let mut arrived = Vec::new();
        while Instant::now() < deadline {
            arrived.extend(self.drain());
            if events_in(&arrived) > 0 {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "no event envelope arrived within {budget:?}:\n{}",
            arrived.join("\n")
        );
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
