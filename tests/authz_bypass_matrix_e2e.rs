//! The epic's bypass matrix, at the compiled-process boundary (ADR-006,
//! ADR-038 clauses 5 and 9).
//!
//! Eight classes, one test each: cross-board, retagging, history, projection,
//! actor, selector, revocation, and the web write surface. Every one spawns
//! the real `kanban` binary against a real SQLite estate — a library call in
//! this process would not establish the thing being asserted, because the
//! authority under test is minted from the SPAWNED process's own kernel
//! identity and nothing else.
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
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
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

    fn live_bytes(&self) -> Vec<Vec<u8>> {
        [
            self.xdg.join("kanban").join("registry.db"),
            PathBuf::from(&self.board_a),
            PathBuf::from(&self.board_b),
        ]
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect()
    }

    fn pre_restore_snapshots(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.xdg.join("kanban").join("backups")) else {
            return Vec::new();
        };
        let mut paths = entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .starts_with("pre-restore-")
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
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

/// Restore's undo snapshot includes registered boards the incoming snapshot
/// will not overwrite. That rescue-only copy is a whole-board read: missing
/// board authority or authority that omits one row's tag must refuse before
/// any pre-restore artifact exists, never fall through to a raw file copy.
#[test]
fn restore_requires_whole_board_read_for_every_rescue_only_board() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = env::temp_dir().join(format!(
        "kanban-authz-matrix-restore-rescue-{}-{unique}",
        std::process::id()
    ));
    let xdg = root.join("xdg");
    let work_a = root.join("work-a");
    let work_b = root.join("work-b");
    for directory in [&xdg, &work_a, &work_b] {
        fs::create_dir_all(directory).unwrap();
    }
    let mut estate = ManagedEstate {
        root,
        xdg,
        work_a,
        work_b,
        board_a: String::new(),
        board_b: String::new(),
        id_a: String::new(),
        id_b: String::new(),
    };
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();
    let alpha = estate.ok_json(&work_a, &["init", "--name", "Alpha", "--json"]);
    estate.board_a = alpha["boardPath"].as_str().unwrap().to_owned();
    estate.id_a = board_id(&estate.board_a);
    let snapshot = estate.root.join("alpha-snapshot");
    let snapshot_arg = snapshot.to_string_lossy().into_owned();
    estate.ok_json(&work_a, &["backup", "--output", &snapshot_arg, "--json"]);

    let beta = estate.ok_json(&work_b, &["init", "--name", "Beta", "--json"]);
    estate.board_b = beta["boardPath"].as_str().unwrap().to_owned();
    estate.id_b = board_id(&estate.board_b);
    estate.ok_json(
        &work_b,
        &["tag", "add", "private", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_b,
        &[
            "task",
            "add",
            "beta secret",
            "--tag",
            "private",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "post-snapshot alpha",
            "--as",
            "seed",
            "--json",
        ],
    );

    estate.bind_self("p-restore", &owner_of(&estate.id_a));
    estate.enforce("managed");
    let restore_args = ["restore", "--from", &snapshot_arg, "--force", "--json"];

    let before = estate.live_bytes();
    estate.denied(&work_a, &restore_args);
    assert_eq!(
        estate.live_bytes(),
        before,
        "denied restore changed live files"
    );
    assert!(
        estate.pre_restore_snapshots().is_empty(),
        "denied restore created a pre-restore snapshot"
    );

    estate.grant("p-restore", &[board_scope("read", &estate.id_b)]);
    let before = estate.live_bytes();
    estate.denied(&work_a, &restore_args);
    assert_eq!(
        estate.live_bytes(),
        before,
        "partially authorized restore changed live files"
    );
    assert!(
        estate.pre_restore_snapshots().is_empty(),
        "partially authorized restore created a pre-restore snapshot"
    );

    estate.grant(
        "p-restore",
        &[(
            "read",
            vec![format!("board:{}", estate.id_b), "*".to_owned()],
        )],
    );
    let restored = estate.ok_json(&work_a, &restore_args);
    let rescue = PathBuf::from(restored["rescueSnapshot"].as_str().unwrap());
    let beta_name = Path::new(&estate.board_b).file_name().unwrap();
    assert!(
        rescue.join("boards").join(beta_name).is_file(),
        "authorized restore did not rescue registered non-overwrite Beta"
    );
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

// ---------------------------------------------------------------------------
// 8. Web edge: a request header and a same-origin POST are not authority.
// ---------------------------------------------------------------------------

/// `kanban serve --actor-header NAME` threads one trusted edge header into the
/// AUDIT actor, and the POST also has to pass the same-origin check. Neither
/// is an authorization input: a perfectly-formed same-origin write, carrying
/// the username of a principal that really does own this board, is refused
/// with the one generic denial while the SERVING process holds nothing on it —
/// and the byte-identical request lands once that process's OWN principal is
/// granted the board.
///
/// The refusal is `409` rather than `403`, which is the load-bearing detail:
/// the same-origin gate PASSED and the guard refused anyway. The audit actor
/// offered is `kanban-board-owner`, the real owner bound under another UID, so
/// what the header carries is not a fiction the edge could be blamed for.
#[test]
fn a_trusted_edge_header_and_a_same_origin_post_are_not_authority() {
    let estate = ManagedEstate::new("web-edge");
    let work_a = estate.work_a.clone();

    let raised = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "needs a decision",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--json",
        ],
    );
    let attention = raised["id"].as_str().unwrap().to_owned();

    // A principal that really owns board A — under another username and UID.
    // This process's own principal owns board B and nothing on A.
    estate.bind_other(
        "p-real-owner",
        "kanban-board-owner",
        &owner_of(&estate.id_a),
    );
    estate.bind_self("p-elsewhere", &owner_of(&estate.id_b));
    estate.enforce("managed");

    let server = WebServer::start(&estate, &work_a, Some("X-Kanban-Actor"));
    let reply = format!("/attention/Alpha/{attention}/reply");
    let refused = server.post(
        &reply,
        &[("X-Kanban-Actor", "kanban-board-owner")],
        "decision=approve&reply=forged",
    );
    assert_eq!(
        refused.status, 409,
        "the guard must refuse this write: {}",
        refused.body
    );
    assert!(
        refused.body.contains(DENIED),
        "the refusal must be the generic denial: {}",
        refused.body
    );

    // Read the row back with enforcement lifted, so the read is not itself
    // the thing being refused: the item is untouched.
    estate.enforce("direct");
    let open = estate.ok_json(
        &work_a,
        &["attention", "list", "--status", "open", "--json"],
    );
    assert_eq!(
        open.as_array().unwrap().len(),
        1,
        "a refused web write must leave the item open"
    );
    estate.enforce("managed");

    // Now grant the SERVING process's own principal the board. Nothing about
    // the request changes — not the header, not the origin, not the body.
    estate.grant("p-elsewhere", &owner_of(&estate.id_a));
    let recorded = server.post(
        &reply,
        &[("X-Kanban-Actor", "kanban-board-owner")],
        "decision=approve&reply=recorded",
    );
    assert_eq!(
        recorded.status, 303,
        "the principal's own authority must permit this write: {}",
        recorded.body
    );

    estate.enforce("direct");
    let resolved = estate.ok_json(
        &work_a,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    let rows = resolved.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["resolvedBy"].as_str(),
        Some("kanban-board-owner"),
        "the edge identity is the AUDIT actor, recorded verbatim"
    );
}

// ---------------------------------------------------------------------------
// 9. The JSON projection over real HTTP (SPA-06, SPA-08..SPA-13).
// ---------------------------------------------------------------------------
//
// INTEGRATION coverage, at the layer `http` (rule g-ffbd95f5): every case
// below spawns the compiled binary, speaks HTTP to it over a real socket and
// reads a real SQLite estate, and no browser is involved in any of it.
//
// It lives in this file rather than in one of its own because the estate is
// this file's: `ManagedEstate` is the only fixture in the suite that can put
// the serving process under managed enforcement with a named set of grants,
// which is what several of these cases are about and what the first wave's
// cases in `tests/e2e.rs` could not seed. `tests/access_refusals_e2e.rs` has
// no server and no board authority in it at all.
//
// Where a case asserts the API and the CLI agree, the CLI read is run as the
// SAME process identity against the SAME estate, so the only difference
// between the two answers is the surface.

/// The store's one generic denial as the JSON surface writes it, byte for
/// byte (`DENIED_OR_NOT_FOUND_JSON`, `rust/serve.rs`).
///
/// Held as a byte string rather than as a parsed object for the reason the
/// contract gives: the claim is that the refusal is byte-identical whichever
/// of the four reasons produced it, and a comparison through a parser would
/// pass on two bodies differing in spacing, field order, or an extra key.
const DENIED_JSON: &str = "{\"error\":\"denied or not found\"}";

/// The product's own sentence for a write that did not come from this site
/// (`rust/serve.rs`, and `#/components/responses/Refused`).
const NOT_FROM_THIS_SITE: &str = "The action did not come from this site.";

/// The three page bounds the projection reports, mirrored from
/// `rust/serve.rs` so a change there fails here rather than drifting.
const OPEN_ATTENTION_ROWS: usize = 1_000;
const LANE_UPDATE_ROWS: usize = 200;
const DETAIL_ROWS: usize = 50;
/// How many unarchived sitreps one lane keeps: posting archives all but the
/// last ten of that lane (`Store::post_sitrep`), so a fixture that needs
/// more live rows than this needs more lanes, not more rows.
const LIVE_SITREPS_PER_LANE: usize = 10;

/// A commit-shaped SHA for a sitrep's provenance.
const SEED_HEAD: &str = "0000000000000000000000000000000000000000";

/// `kanban sitrep post` outside a git checkout.
///
/// The write refuses blank provenance rather than storing it, so the fixture
/// supplies the four fields a checkout would have supplied.
fn sitrep_on<'a>(body: &'a str, lane: &'a str, repo: &'a str) -> Vec<&'a str> {
    vec![
        "sitrep", "post", body, "--as", "seed", "--lane", lane, "--repo", repo, "--branch", "main",
        "--head", SEED_HEAD, "--dirty", "clean", "--json",
    ]
}

/// Every read route `serve.rs`'s `api` answers that one board and one row of
/// it reach, which is every arm but the two that need an id of their own.
///
/// It was the five routes of the first wave until `t-bf255880` wave 2: the
/// cases below — the Managed tag grant, the lease token, the CLI
/// equivalence — were measuring a fifth of the surface while the rest of it
/// shipped. The two detail arms that need an attempt id or a sprint id are
/// [`detail_routes`], which a caller passes its own ids to.
fn every_read_route(board: &str, task: &str) -> Vec<String> {
    vec![
        "/api/v1/needs-you".to_owned(),
        "/api/v1/decided".to_owned(),
        "/api/v1/boards".to_owned(),
        "/api/v1/lanes".to_owned(),
        "/api/v1/deployments".to_owned(),
        "/api/v1/subscriptions".to_owned(),
        "/api/v1/sprints".to_owned(),
        "/api/v1/plans".to_owned(),
        format!("/api/v1/search?q={task}"),
        format!("/api/v1/sprints/{board}"),
        format!("/api/v1/board/{board}"),
        format!("/api/v1/task/{board}/{task}"),
        format!("/api/v1/preview/task/{board}/{task}"),
        format!("/api/v1/preview/board/{board}/{board}"),
    ]
}

/// The arms that name a row of their own: an attempt, a sprint, and the two
/// previews that read one. A caller that has seeded none passes ids that do
/// not exist, which is exactly what a denial case wants.
fn detail_routes(board: &str, deployment: &str, sprint: &str, attention: &str) -> Vec<String> {
    vec![
        format!("/api/v1/deployment/{board}/{deployment}"),
        format!("/api/v1/sprint/{board}/{sprint}"),
        format!("/api/v1/preview/deployment/{board}/{deployment}"),
        format!("/api/v1/preview/attention/{board}/{attention}"),
    ]
}

/// Compare one API object with one CLI object on the keys they share.
///
/// The shared set is asserted to contain `anchors` first: a comparison that
/// silently emptied because a field was renamed on one side would otherwise
/// pass forever. Nothing is sorted and nothing is normalised — each side is
/// read in the order its own surface returned it, because the order is the
/// page arm's own and the CLI read named beside it is the one that arm
/// shares, not a property of the data.
fn agrees_on_shared_keys(api: &Value, cli: &Value, anchors: &[&str], what: &str) {
    let api = api
        .as_object()
        .unwrap_or_else(|| panic!("{what}: the API row is not an object: {api}"));
    let cli = cli
        .as_object()
        .unwrap_or_else(|| panic!("{what}: the CLI row is not an object: {cli}"));
    for anchor in anchors {
        assert!(
            api.contains_key(*anchor) && cli.contains_key(*anchor),
            "{what}: both surfaces must carry {anchor}, or this comparison measures nothing"
        );
    }
    for (key, value) in api {
        if let Some(other) = cli.get(key) {
            assert_eq!(
                value, other,
                "{what}: {key} differs between the JSON route and the CLI"
            );
        }
    }
}

/// Every field a task carries on both surfaces and that a divergence would
/// show up in first.
const TASK_ANCHORS: &[&str] = &[
    "id",
    "title",
    "status",
    "priority",
    "tags",
    "type",
    "assignee",
    "lane",
    "createdAt",
    "updatedAt",
];

/// SPA-08: the same board and tag authority as the CLI, on all five routes.
///
/// The serving process owns board A and holds nothing at all on board B, and
/// the estate is `managed`, so the guard is live. Every named access to B is
/// refused with the one body; the whole-estate listings refuse exactly as the
/// CLI's own whole-estate listing refuses; and granting B afterwards makes
/// both surfaces answer, from the same unchanged request.
///
/// The tag half is asserted as an EQUIVALENCE rather than as a hiding rule:
/// SPA-08's claim is that the JSON route returns what the CLI would return
/// with the enforcement in the store, and a test that asserted the route
/// filtered what the CLI does not would be asserting a second rule in the
/// route — the one thing SPA-08 forbids. Since `t-34f6eed5` the store hides
/// a tag-denied row from BOTH listings, so the seeded `t-asec` is in neither
/// answer and the equivalence holds over a smaller set; the hiding itself is
/// asserted in `a_tag_denied_row_is_in_no_task_listing_and_in_no_count_over_http`.
#[test]
fn the_five_json_routes_enforce_the_same_board_and_tag_authority_as_the_cli_over_http() {
    let estate = ManagedEstate::new("json-authz");
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();
    let repo_a = work_a.to_string_lossy().into_owned();
    let repo_b = work_b.to_string_lossy().into_owned();

    estate.ok_json(&work_a, &["tag", "add", "secret", "--as", "seed", "--json"]);
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "alpha visible row",
            "--id",
            "t-avis",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "alpha tagged row",
            "--id",
            "t-asec",
            "--tag",
            "secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &sitrep_on("alpha lane update", "alpha-lane", &repo_a),
    );
    estate.ok_json(
        &work_b,
        &[
            "task",
            "add",
            "beta hidden row",
            "--id",
            "t-bhid",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_b,
        &sitrep_on("beta lane update", "beta-lane", &repo_b),
    );

    // Board scope at both capabilities on A, and the `secret` tag on
    // neither. Nothing at all on B.
    estate.bind_self(
        "p-alpha-only",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");
    let server = WebServer::start(&estate, &work_a, None);

    // A named read the CLI refuses is a named read the route refuses, with
    // the one non-enumerating body and the contract's content type.
    estate.denied(&work_b, &["task", "list", "--json"]);
    estate.denied(&work_a, &["task", "show", "t-asec", "--json"]);
    for path in [
        "/api/v1/board/Beta",
        "/api/v1/task/Beta/t-bhid",
        "/api/v1/task/Alpha/t-asec",
    ] {
        let answer = server.get(path);
        assert_eq!(answer.status, 404, "{path}: {}", answer.body);
        assert_eq!(
            answer.body, DENIED_JSON,
            "{path} answered a refusal of its own"
        );
        assert!(
            answer
                .head
                .contains("Content-Type: application/json; charset=utf-8"),
            "{path} refused as {}",
            answer.head
        );
    }

    // The whole-estate listings answer the same way the CLI's own
    // whole-estate listing answers: an estate holding a board this caller
    // cannot read is refused entire, on both surfaces, with one body. It is
    // the equivalence that is asserted here; that the contract asks instead
    // for the readable subset is a separate, named finding
    // (`a_whole_estate_listing_serves_the_boards_the_caller_may_read_over_http`).
    estate.denied(&work_a, &["dashboard", "--json"]);
    for path in ["/api/v1/boards", "/api/v1/needs-you"] {
        let answer = server.get(path);
        assert_eq!(answer.status, 404, "{path}: {}", answer.body);
        assert_eq!(answer.body, DENIED_JSON, "{path}");
    }

    // The caller's own board answers, and it answers exactly the rows the
    // CLI hands the same identity — the tagged row in neither, since both
    // read the store's one filtered listing and both refuse it by name.
    let alpha = server.get_json("/api/v1/board/Alpha");
    let listed = estate.ok_json(&work_a, &["task", "list", "--json"]);
    // A board row is `{task, openAttention}` since `t-bf255880`, so the
    // task the CLI lists is the row's `task` member.
    let api_rows = alpha["tasks"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["task"])
        .collect::<Vec<_>>();
    let cli_rows = listed.as_array().unwrap();
    assert_eq!(
        api_rows.len(),
        cli_rows.len(),
        "the route and the CLI disagree on how many rows this caller may list:\n{alpha}\n{listed}"
    );
    for (api, cli) in api_rows.iter().zip(cli_rows) {
        agrees_on_shared_keys(
            api,
            cli,
            TASK_ANCHORS,
            "board listing under managed authority",
        );
    }

    // Now the same request against a granted board. Nothing about the
    // request changes; only the grant does.
    estate.grant("p-alpha-only", &owner_of(&estate.id_b));
    let boards = server.get_json("/api/v1/boards");
    let names = boards["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["board"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        names.contains(&"Alpha".to_owned()) && names.contains(&"Beta".to_owned()),
        "a granted board did not appear: {boards}"
    );
    let beta = server.get_json("/api/v1/board/Beta");
    assert_eq!(beta["tasks"]["items"][0]["task"]["id"], "t-bhid", "{beta}");
    estate.ok_json(&work_b, &["task", "list", "--json"]);
}

/// SPA-08: a refusal names nothing that was seeded.
///
/// Every 4xx body an unauthorized caller can obtain is searched for every
/// distinctive string the invisible board carries — its name, its rows'
/// titles and ids, its tag, its lane, and the text of its open item. A
/// refusal that named any of them would confirm existence, which is exactly
/// what the single generic denial exists to prevent.
#[test]
fn no_json_refusal_names_a_board_row_or_tag_the_caller_may_not_see_over_http() {
    let estate = ManagedEstate::new("json-leak");
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();
    let repo_b = work_b.to_string_lossy().into_owned();

    let secrets = [
        "Beta",
        "harbour-wall-survey",
        "t-bsecret",
        "beta-only-tag",
        "beta-only-lane",
        "the pier decision nobody else may read",
    ];
    estate.ok_json(
        &work_b,
        &["tag", "add", "beta-only-tag", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_b,
        &[
            "task",
            "add",
            "harbour-wall-survey",
            "--id",
            "t-bsecret",
            "--tag",
            "beta-only-tag",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_b,
        &[
            "attention",
            "raise",
            "the pier decision nobody else may read",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--json",
        ],
    );
    estate.ok_json(
        &work_b,
        &sitrep_on("beta lane update", "beta-only-lane", &repo_b),
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "alpha row",
            "--id",
            "t-avis",
            "--as",
            "seed",
            "--json",
        ],
    );

    estate.bind_self("p-alpha-only", &[board_scope("read", &estate.id_a)]);
    estate.enforce("managed");
    let server = WebServer::start(&estate, &work_a, None);

    let mut refusals = Vec::new();
    for path in every_read_route("Beta", "t-bsecret")
        .into_iter()
        .chain(every_read_route("Alpha", "t-no-such-row"))
        .chain(detail_routes(
            "Beta",
            "d-bsecret",
            "sp-bsecret",
            "a-bsecret",
        ))
        .chain([
            "/api/v1/board/harbour-wall-survey".to_owned(),
            "/api/v1/task/Beta/t-bsecret".to_owned(),
            "/api/v1/deployments".to_owned(),
        ])
    {
        let answer = server.get(&path);
        if (400..500).contains(&answer.status) {
            refusals.push((path, answer));
        }
    }
    assert!(
        refusals.len() >= 5,
        "the fixture produced too few refusals to measure: {:?}",
        refusals.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    for (path, answer) in &refusals {
        assert_eq!(
            answer.body, DENIED_JSON,
            "{path} answered a refusal of its own"
        );
        for secret in secrets {
            assert!(
                !answer.body.contains(secret),
                "{path} named {secret} in a refusal: {}",
                answer.body
            );
        }
    }
}

/// SPA-09: no route serialises a lease token, on any of the five.
///
/// The token is taken from a real `kanban claim`, so what the bodies are
/// searched for is the capability's own bytes and not merely a field name —
/// a filter can be forgotten, and a test that only looked for `leaseToken`
/// would pass on a body that spelled the same secret differently.
#[test]
fn no_json_route_serialises_a_lease_token_over_http() {
    let estate = ManagedEstate::new("json-lease");
    let work_a = estate.work_a.clone();
    let repo_a = work_a.to_string_lossy().into_owned();
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "leased row",
            "--id",
            "t-leased",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "a decision on a leased row",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--task",
            "t-leased",
            "--json",
        ],
    );
    estate.ok_json(&work_a, &sitrep_on("lane update", "leased-lane", &repo_a));
    let lease = estate.ok_json(
        &work_a,
        &["claim", "t-leased", "--as", "driver-2", "--json"],
    );
    let token = lease["leaseToken"]
        .as_str()
        .unwrap_or_else(|| panic!("the CLI claim carries no lease token to look for: {lease}"))
        .to_owned();
    assert!(!token.is_empty(), "{lease}");

    let server = WebServer::start(&estate, &work_a, None);
    for path in every_read_route("Alpha", "t-leased") {
        let answer = server.get(&path);
        assert_eq!(answer.status, 200, "{path}: {}", answer.body);
        assert!(
            !answer.body.contains(&token),
            "{path} served the lease token itself: {}",
            answer.body
        );
        assert!(
            !answer.body.contains("leaseToken"),
            "{path} served a leaseToken field: {}",
            answer.body
        );
    }
    // The holder is still served — the token's absence is not the claim's.
    let detail = server.get_json("/api/v1/task/Alpha/t-leased");
    assert_eq!(detail["claim"]["agentID"], "driver-2", "{detail}");
}

/// SPA-06 / ADR-037 §4: every listing route says whether it was cut.
///
/// Each of the three bounds is crossed for real — one row past
/// `OPEN_ATTENTION_ROWS`, past `LANE_UPDATE_ROWS` and past `DETAIL_ROWS` —
/// because `truncated` is meant to be observed from an over-fetch rather
/// than inferred from `returned == limit`, and a fixture that stopped at the
/// bound could not tell the two apart. The two routes whose store call takes
/// no bound report `limit: null` rather than inventing one.
///
/// On `/api/v1/lanes` the envelope's `returned` counts LANE GROUPS while
/// `limit` is the per-board sitrep scan, which is what the contract says it
/// is (`getLanes`: "`limit` is the per-board scan, `200`"), so the cut is
/// asserted on the updates the groups carry. The rows are spread across
/// [`LIVE_SITREPS_PER_LANE`]-sized lanes rather than piled into one, because
/// posting a sitrep archives all but the last ten of its own lane: a single
/// lane can never reach the scan's bound however many rows are written to
/// it.
#[test]
fn every_json_listing_says_whether_it_was_capped_over_http() {
    let estate = ManagedEstate::new("json-capped");
    let work_a = estate.work_a.clone();
    let repo_a = work_a.to_string_lossy().into_owned();
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "noted row",
            "--id",
            "t-noted",
            "--as",
            "seed",
            "--json",
        ],
    );
    for index in 0..=DETAIL_ROWS {
        let body = format!("note {index}");
        estate.ok_json(&work_a, &note_on("t-noted", &body));
    }
    let lane_count = LANE_UPDATE_ROWS / LIVE_SITREPS_PER_LANE + 1;
    for lane in 0..lane_count {
        let lane_name = format!("capped-lane-{lane}");
        for index in 0..LIVE_SITREPS_PER_LANE {
            let body = format!("lane {lane} update {index}");
            estate.ok_json(&work_a, &sitrep_on(&body, &lane_name, &repo_a));
        }
    }
    for index in 0..=OPEN_ATTENTION_ROWS {
        let body = format!("bulk ask {index}");
        estate.ok_json(
            &work_a,
            &[
                "attention",
                "raise",
                &body,
                "--as",
                "seed",
                "--kind",
                "risk",
                "--json",
            ],
        );
    }

    let server = WebServer::start(&estate, &work_a, None);

    let queue = server.get_json("/api/v1/needs-you");
    assert_eq!(queue["limit"], OPEN_ATTENTION_ROWS, "{}", queue["limit"]);
    assert_eq!(
        queue["truncated"],
        Value::Bool(true),
        "the open queue was cut and did not say so"
    );
    assert_eq!(queue["returned"], OPEN_ATTENTION_ROWS);
    assert_eq!(
        queue["items"].as_array().unwrap().len(),
        OPEN_ATTENTION_ROWS,
        "the queue reported a count its rows do not support"
    );

    let lanes = server.get_json("/api/v1/lanes");
    assert_eq!(lanes["limit"], LANE_UPDATE_ROWS, "{lanes}");
    assert_eq!(
        lanes["truncated"],
        Value::Bool(true),
        "the lane scan was cut and did not say so"
    );
    let groups = lanes["items"].as_array().unwrap();
    assert_eq!(lanes["returned"], groups.len());
    let updates: usize = groups
        .iter()
        .map(|group| group["updates"].as_array().unwrap().len())
        .sum();
    assert_eq!(
        updates, LANE_UPDATE_ROWS,
        "the per-board scan handed over more or fewer rows than its bound"
    );

    let detail = server.get_json("/api/v1/task/Alpha/t-noted");
    for capped in ["notes", "events"] {
        let envelope = &detail[capped];
        assert_eq!(envelope["limit"], DETAIL_ROWS, "{capped}: {envelope}");
        assert_eq!(envelope["returned"], DETAIL_ROWS, "{capped}: {envelope}");
        assert_eq!(
            envelope["items"].as_array().unwrap().len(),
            DETAIL_ROWS,
            "{capped} reported a count its rows do not support: {envelope}"
        );
        assert_eq!(
            envelope["truncated"],
            Value::Bool(true),
            "{capped} was cut and did not say so: {envelope}"
        );
    }
    assert_eq!(detail["checkpoints"]["truncated"], Value::Bool(false));
    assert_eq!(detail["checkpoints"]["limit"], DETAIL_ROWS);

    // The two whose store call takes no bound report none rather than a
    // number an operator would plan around.
    for (path, pointer) in [
        ("/api/v1/boards", Vec::new()),
        ("/api/v1/board/Alpha", vec!["tasks"]),
    ] {
        let body = server.get_json(path);
        let envelope = pointer.iter().fold(&body, |value, key| &value[*key]);
        assert_eq!(envelope["limit"], Value::Null, "{path}: {envelope}");
        assert_eq!(
            envelope["truncated"],
            Value::Bool(false),
            "{path}: {envelope}"
        );
        assert_eq!(
            envelope["returned"],
            envelope["items"].as_array().unwrap().len(),
            "{path} reported a count its rows do not support: {envelope}"
        );
    }
}

/// SPA-11 and SPA-12: each of the four writes is refused without a
/// trusted-edge identity and refused across origins, and the board is
/// unchanged afterwards.
///
/// The order the handler checks in is load-bearing and is what the two
/// statuses distinguish: same-origin is decided BEFORE the actor and before
/// the board is opened, so a cross-origin write is `403` even when it names
/// a row that does not exist, and a same-origin write with no identity is
/// `400` rather than being recorded as the default operator.
///
/// The bodies are the shipped ones: a rendered page carrying the product's
/// own sentence. The contract declares these three refusals as
/// `application/json`, which the server does not do; that divergence is its
/// own named case below rather than a weakened assertion here.
#[test]
fn the_four_web_writes_are_refused_without_identity_and_across_origins_over_http() {
    let estate = ManagedEstate::new("json-writes");
    let work_a = estate.work_a.clone();
    let raised = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "needs a call",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--json",
        ],
    );
    let attention = raised["id"].as_str().unwrap().to_owned();
    let settled = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "already decided",
            "--as",
            "seed",
            "--kind",
            "approval",
            "--json",
        ],
    );
    let settled = settled["id"].as_str().unwrap().to_owned();
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "resolve",
            &settled,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "task", "add", "a plan", "--type", "epic", "--status", "draft", "--id", "e-plan",
            "--as", "seed", "--json",
        ],
    );

    let server = WebServer::start(&estate, &work_a, Some("X-Auth-Request-Email"));
    let writes = [
        (
            format!("/attention/Alpha/{attention}/reply"),
            "decision=approve&reply=done",
        ),
        (format!("/attention/Alpha/{settled}/reopen"), ""),
        ("/plan/Alpha/e-plan/open".to_owned(), ""),
        ("/subscription/Alpha/sub-none/pause".to_owned(), ""),
    ];
    for (path, body) in &writes {
        // No `Origin` at all: the gate requires one, it does not default.
        let absent = server.request(
            "POST",
            path,
            None,
            &[("X-Auth-Request-Email", "sso@edge.test")],
            Some(body),
        );
        assert_eq!(absent.status, 403, "{path} with no Origin: {}", absent.body);
        assert!(
            absent.body.contains(NOT_FROM_THIS_SITE),
            "{path} refused in words of its own: {}",
            absent.body
        );
        // Another origin, with a perfectly good identity behind it.
        let cross = server.request(
            "POST",
            path,
            Some("https://hostile.example"),
            &[("X-Auth-Request-Email", "sso@edge.test")],
            Some(body),
        );
        assert_eq!(cross.status, 403, "{path} cross-origin: {}", cross.body);
        assert!(
            cross.body.contains(NOT_FROM_THIS_SITE),
            "{path} refused in words of its own: {}",
            cross.body
        );
        // Same origin, no identity: fails closed rather than falling back to
        // the default operator.
        let anonymous = server.post(path, &[], body);
        assert_eq!(
            anonymous.status, 400,
            "{path} with no identity: {}",
            anonymous.body
        );
        assert!(
            anonymous
                .body
                .contains("actor header X-Auth-Request-Email is required"),
            "{path} did not say which identity was missing: {}",
            anonymous.body
        );
    }

    // Nothing was recorded by any of the twelve refusals.
    let open = estate.ok_json(
        &work_a,
        &["attention", "list", "--status", "open", "--json"],
    );
    let open_ids = open
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        open_ids,
        vec![attention],
        "a refused write moved an attention row"
    );
    let plan = estate.ok_json(&work_a, &["task", "show", "e-plan", "--json"]);
    assert_eq!(plan["status"], "draft", "a refused write opened the plan");
}

/// SPA-12: a client copy of the trusted-edge header cannot override the
/// proxy-set one, and neither can a form field.
///
/// nginx sets `X-Auth-Request-Email` with `proxy_set_header`, which
/// overwrites a client copy — so the case a client can actually create is a
/// SECOND copy of the header arriving beside the edge's. The server refuses
/// that outright rather than picking one, which is the only safe reading: on
/// a duplicated header there is no way to tell which copy the edge wrote.
/// The recorded actor on the write that IS accepted is asserted in the board
/// itself, not in the response.
#[test]
fn a_client_copy_of_the_actor_header_cannot_override_the_trusted_edge_value_over_http() {
    let estate = ManagedEstate::new("json-actor");
    let work_a = estate.work_a.clone();
    let first = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "first decision",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--json",
        ],
    );
    let first = first["id"].as_str().unwrap().to_owned();
    let second = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "second decision",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--json",
        ],
    );
    let second = second["id"].as_str().unwrap().to_owned();

    let server = WebServer::start(&estate, &work_a, Some("X-Auth-Request-Email"));

    // Two copies: the edge's, then the client's. Refused, and nothing is
    // recorded — not the first value, not the second.
    let doubled = server.post(
        &format!("/attention/Alpha/{first}/reply"),
        &[
            ("X-Auth-Request-Email", "sso@edge.test"),
            ("X-Auth-Request-Email", "client@spoofed.example"),
        ],
        "decision=approve&reply=two+identities",
    );
    assert_eq!(doubled.status, 400, "{}", doubled.body);
    assert!(
        doubled
            .body
            .contains("actor header X-Auth-Request-Email must appear exactly once"),
        "{}",
        doubled.body
    );

    // One copy, with a client-supplied actor in the form beside it. The
    // field is not an input to the identity at all.
    let recorded = server.post(
        &format!("/attention/Alpha/{second}/reply"),
        &[("X-Auth-Request-Email", "sso@edge.test")],
        "decision=approve&reply=done&actor=client%40spoofed.example",
    );
    assert_eq!(recorded.status, 303, "{}", recorded.body);

    let open = estate.ok_json(
        &work_a,
        &["attention", "list", "--status", "open", "--json"],
    );
    assert_eq!(
        open.as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec![first],
        "the refused write was recorded anyway"
    );
    let resolved = estate.ok_json(
        &work_a,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    let rows = resolved.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{resolved}");
    assert_eq!(rows[0]["id"].as_str(), Some(second.as_str()));
    assert_eq!(
        rows[0]["resolvedBy"].as_str(),
        Some("sso@edge.test"),
        "the recorded actor is not the header the server trusts: {resolved}"
    );
}

/// SPA-10 / ADR-042 §4 refusal 13: a card left open in a tab cannot answer a
/// key the row no longer carries.
///
/// The row's choices are rewritten between the render and the post, which is
/// exactly what happens to a card left open while someone else edits the
/// item. The old key is refused BY NAME rather than mapped onto whatever now
/// sits in that position — the mapping is the dangerous behaviour, because
/// the operator would be recording a decision they never read.
#[test]
fn a_reply_naming_a_choice_the_row_no_longer_carries_is_refused_by_name_over_http() {
    let estate = ManagedEstate::new("json-stale-choice");
    let work_a = estate.work_a.clone();
    let raised = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "ship or hold",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--question",
            "Ship tonight?",
            "--context",
            "The queue stays wrong until it lands.",
            "--choice",
            "ship=Ship it|approve",
            "--choice",
            "hold=Hold until morning|defer",
            "--consequence",
            "ship=It runs unwatched.",
            "--consequence",
            "hold=One more wrong day.",
            "--recommend",
            "ship",
            "--json",
        ],
    );
    let attention = raised["id"].as_str().unwrap().to_owned();
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "update",
            &attention,
            "--as",
            "seed",
            "--question",
            "Ship tonight?",
            "--context",
            "Rewritten while the card was open.",
            "--choice",
            "go=Go now|approve",
            "--choice",
            "wait=Wait for the morning|defer",
            "--consequence",
            "go=It runs unwatched.",
            "--consequence",
            "wait=One more wrong day.",
            "--recommend",
            "go",
            "--json",
        ],
    );

    let server = WebServer::start(&estate, &work_a, Some("X-Auth-Request-Email"));
    let path = format!("/attention/Alpha/{attention}/reply");
    let stale = server.post(
        &path,
        &[("X-Auth-Request-Email", "sso@edge.test")],
        "decision=ship&reply=from+a+card+left+open",
    );
    assert_eq!(stale.status, 409, "{}", stale.body);
    assert!(
        stale
            .body
            .contains(&format!("attention {attention} has no choice ship")),
        "the stale key was not refused by name: {}",
        stale.body
    );
    let open = estate.ok_json(
        &work_a,
        &["attention", "list", "--status", "open", "--json"],
    );
    assert_eq!(
        open.as_array().unwrap().len(),
        1,
        "the refused answer settled the row anyway"
    );

    // The rewritten key is accepted, so what was measured is the key and not
    // a broken write path.
    let fresh = server.post(
        &path,
        &[("X-Auth-Request-Email", "sso@edge.test")],
        "decision=go&reply=from+the+card+as+it+now+reads",
    );
    assert_eq!(fresh.status, 303, "{}", fresh.body);
}

/// SPA-06: THE PROJECTION INVARIANT — the JSON surface and the CLI answer the
/// same rows for the same board and the same caller.
///
/// This is the one case that keeps the second surface from becoming a second
/// implementation. Four routes are compared against the four CLI reads whose
/// store calls they share, field by field on the keys the two shapes have in
/// common, positionally, in the order each surface returned — nothing is
/// sorted first, because the order is part of what is being compared and a
/// sort would hide exactly the drift this exists to catch.
///
/// The open queue is seeded on ONE board on purpose: `needs_you` merges every
/// board and re-sorts, so a two-board fixture would be comparing the merge
/// rule against a single board's listing. The cross-board key is the card's
/// `board` field, asserted separately.
#[test]
fn the_json_routes_answer_the_same_rows_as_the_cli_over_http() {
    let estate = ManagedEstate::new("json-invariant");
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();
    let repo_a = work_a.to_string_lossy().into_owned();
    let repo_b = work_b.to_string_lossy().into_owned();

    for (index, title) in ["first row", "second row", "third row"].iter().enumerate() {
        estate.ok_json(
            &work_a,
            &[
                "task",
                "add",
                title,
                "--id",
                &format!("t-row{index}"),
                "--as",
                "seed",
                "--json",
            ],
        );
    }
    estate.ok_json(&work_a, &note_on("t-row0", "a note on the first row"));
    estate.ok_json(&work_a, &["claim", "t-row0", "--as", "driver-2", "--json"]);
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "a decision",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--task",
            "t-row0",
            "--priority",
            "P0",
            "--question",
            "Ship it?",
            "--context",
            "Context.",
            "--choice",
            "ship=Ship it|approve",
            "--choice",
            "hold=Hold|defer",
            "--consequence",
            "ship=x",
            "--consequence",
            "hold=y",
            "--recommend",
            "ship",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "a risk",
            "--as",
            "seed",
            "--kind",
            "risk",
            "--json",
        ],
    );
    for index in 0..3 {
        let body = format!("alpha update {index}");
        estate.ok_json(&work_a, &sitrep_on(&body, "alpha-lane", &repo_a));
    }
    let beta_update = "beta update";
    estate.ok_json(&work_b, &sitrep_on(beta_update, "beta-lane", &repo_b));
    estate.ok_json(
        &work_b,
        &[
            "task", "add", "beta row", "--id", "t-beta", "--as", "seed", "--json",
        ],
    );

    let server = WebServer::start(&estate, &work_a, None);

    // 1. The board listing against `task list`.
    let board = server.get_json("/api/v1/board/Alpha");
    let listed = estate.ok_json(&work_a, &["task", "list", "--json"]);
    let api_rows = board["tasks"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["task"])
        .collect::<Vec<_>>();
    let cli_rows = listed.as_array().unwrap();
    assert_eq!(
        api_rows.iter().map(|row| &row["id"]).collect::<Vec<_>>(),
        cli_rows.iter().map(|row| &row["id"]).collect::<Vec<_>>(),
        "the board route and `task list` disagree on the rows or their order"
    );
    for (api, cli) in api_rows.iter().zip(cli_rows) {
        agrees_on_shared_keys(api, cli, TASK_ANCHORS, "/api/v1/board/{project}");
    }

    // 2. The task detail against `task show`, including the claim summary
    //    and the trail the CLI serves beside it.
    let detail = server.get_json("/api/v1/task/Alpha/t-row0");
    let shown = estate.ok_json(&work_a, &["task", "show", "t-row0", "--json"]);
    agrees_on_shared_keys(
        &detail["task"],
        &shown,
        TASK_ANCHORS,
        "/api/v1/task/{project}/{id}",
    );
    agrees_on_shared_keys(
        &detail["claim"],
        &shown["claim"],
        &["taskID", "agentID", "claimedAt", "expiresAt", "heartbeatAt"],
        "the claim summary",
    );
    // A note row is `{note, bodyHtml}` since `t-bf255880` wave 1: the page
    // is the client now, so the server sends the note's own text and the
    // typeset copy of its body beside it. The CLI serves the note itself,
    // so the comparison is against the wrapper's `note`.
    let api_notes = detail["notes"]["items"].as_array().unwrap();
    let cli_notes = shown["notes"].as_array().unwrap();
    assert_eq!(api_notes.len(), cli_notes.len(), "{detail}\n{shown}");
    for (api, cli) in api_notes.iter().zip(cli_notes) {
        agrees_on_shared_keys(
            &api["note"],
            cli,
            &["seq", "author", "body", "createdAt"],
            "a note",
        );
        assert!(
            api["bodyHtml"].is_string(),
            "the task detail served a note without its typeset body: {api}"
        );
    }

    // 3. The lanes grouping against `sitrep list --lane`, per group, in the
    //    order each lane's own listing returns.
    let lanes = server.get_json("/api/v1/lanes");
    let groups = lanes["items"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{lanes}");
    for group in groups {
        let lane = group["lane"].as_str().unwrap();
        let cwd = if group["board"] == "Alpha" {
            &work_a
        } else {
            &work_b
        };
        let listed = estate.ok_json(
            cwd,
            &[
                "sitrep",
                "list",
                "--lane",
                lane,
                "--limit",
                &LANE_UPDATE_ROWS.to_string(),
                "--json",
            ],
        );
        let api_updates = group["updates"].as_array().unwrap();
        let cli_updates = listed.as_array().unwrap();
        assert_eq!(
            api_updates.iter().map(|row| &row["id"]).collect::<Vec<_>>(),
            cli_updates.iter().map(|row| &row["id"]).collect::<Vec<_>>(),
            "lane {lane}: the route and `sitrep list` disagree on the rows or their order"
        );
        for (api, cli) in api_updates.iter().zip(cli_updates) {
            agrees_on_shared_keys(
                api,
                cli,
                &["id", "lane", "author", "body", "createdAt"],
                "a lane update",
            );
        }
    }

    // 4. The open queue against `attention list --status open`.
    let queue = server.get_json("/api/v1/needs-you");
    let cards = queue["items"].as_array().unwrap();
    let open = estate.ok_json(
        &work_a,
        &[
            "attention",
            "list",
            "--status",
            "open",
            "--limit",
            &OPEN_ATTENTION_ROWS.to_string(),
            "--json",
        ],
    );
    let rows = open.as_array().unwrap();
    assert_eq!(rows.len(), 2, "the fixture seeded the wrong queue: {open}");
    assert_eq!(
        cards.len(),
        rows.len(),
        "the queue and `attention list` disagree on how many rows are open"
    );
    for (card, row) in cards.iter().zip(rows) {
        assert_eq!(card["board"], "Alpha", "{card}");
        agrees_on_shared_keys(
            &card["attention"],
            row,
            &["id", "kind", "status", "priority", "choices", "tags"],
            "a needs-you card",
        );
    }
}

// -- findings: what the contract promises and this server does not do -------

/// `t-c84850a1` (2026-09-19), was FINDING `t-4b9501b3`: `/api/v1/lanes` must
/// not hand over the sitreps of a board the caller may not read.
///
/// `lane_groups` (`rust/serve.rs`) iterates every ACTIVE board and calls
/// `Store::sitreps` on each. That method now takes the same `check_read(&[])`
/// guard its siblings take, so a board the same principal is refused by
/// `kanban sitrep list` is refused here too — and `lane_groups` skips it, the
/// way an enumeration must, rather than refusing the whole page. SPA-08 ("a
/// board the principal may not read is absent from every JSON body") is what
/// this asserts; the CLI refusal above is the positive control.
#[test]
fn the_lanes_route_withholds_the_sitreps_of_a_board_the_caller_may_not_read_over_http() {
    let estate = ManagedEstate::new("json-lane-leak");
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();
    let repo_a = work_a.to_string_lossy().into_owned();
    let repo_b = work_b.to_string_lossy().into_owned();
    estate.ok_json(&work_a, &sitrep_on("alpha update", "alpha-lane", &repo_a));
    estate.ok_json(
        &work_b,
        &sitrep_on("the pier survey nobody else may read", "beta-lane", &repo_b),
    );
    estate.bind_self("p-alpha-only", &[board_scope("read", &estate.id_a)]);
    estate.enforce("managed");

    // The control: the CLI refuses this caller board B's sitreps.
    estate.denied(&work_b, &["sitrep", "list", "--json"]);

    let server = WebServer::start(&estate, &work_a, None);
    let lanes = server.get_json("/api/v1/lanes");
    let served = lanes.to_string();
    // The positive control: the readable board's lane and sitrep are served,
    // so a route that answered nothing at all could not pass this case.
    assert!(
        served.contains("alpha-lane") && served.contains("alpha update"),
        "/api/v1/lanes withheld the board this caller may read: {lanes}"
    );
    assert!(
        !served.contains("beta-lane") && !served.contains("the pier survey nobody else may read"),
        "/api/v1/lanes served a board this caller may not read: {lanes}"
    );
}

/// FINDING (`t-4b9501b3`, 2026-09-19): a whole-estate listing refuses
/// entirely when ANY board is unreadable, rather than serving the readable
/// ones.
///
/// `board_summaries` and `needs_you` iterate every active board and propagate
/// the first refusal, so `/api/v1/boards` and `/api/v1/needs-you` answer `404
/// denied or not found` to a caller who fully owns one board out of two. The
/// CLI's `dashboard` does the same, so the two surfaces agree — but
/// `docs/api/README.md` ("The denial does not enumerate") says a caller who
/// asks for a list "is simply not handed the rows they may not see", and
/// SPA-06 requires the page's data to arrive rather than a refusal.
///
/// Ignored, not weakened: the equivalence that DOES hold is asserted in
/// `the_five_json_routes_enforce_the_same_board_and_tag_authority_as_the_cli_over_http`.
#[test]
#[ignore = "FINDING t-4b9501b3: a whole-estate listing refuses entire instead of serving the readable subset; the fix belongs to projection.rs/serve.rs"]
fn a_whole_estate_listing_serves_the_boards_the_caller_may_read_over_http() {
    let estate = ManagedEstate::new("json-listing-subset");
    let work_a = estate.work_a.clone();
    let work_b = estate.work_b.clone();
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "alpha row",
            "--id",
            "t-avis",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_b,
        &[
            "task", "add", "beta row", "--id", "t-bhid", "--as", "seed", "--json",
        ],
    );
    estate.bind_self("p-alpha-only", &owner_of(&estate.id_a));
    estate.enforce("managed");
    let server = WebServer::start(&estate, &work_a, None);

    let boards = server.get_json("/api/v1/boards");
    let names = boards["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["board"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["Alpha".to_owned()],
        "the index did not serve the readable board on its own: {boards}"
    );
    server.get_json("/api/v1/needs-you");
}

/// FINDING (`t-4b9501b3`, 2026-09-19): the four writes refuse in HTML, and
/// the contract says they refuse in JSON.
///
/// `#/components/responses/Refused`, `WriteRejected` and `WriteConflict` each
/// declare `application/json; charset=utf-8` with the `Error` schema, and
/// every one of the four POST operations references them. The server answers
/// a rendered `text/html` page for all three, which is the shipped behaviour
/// the browser deck depends on (it parses the page and reads `.error`), so
/// the divergence may well be the document's to fix rather than the server's
/// — but it is a divergence either way, and it is not among the ones
/// `docs/api/README.md` records.
///
/// Ignored, not weakened: the shipped shape is asserted in
/// `the_four_web_writes_are_refused_without_identity_and_across_origins_over_http`.
#[test]
#[ignore = "FINDING t-4b9501b3: the write refusals answer text/html; the contract declares application/json"]
fn the_write_refusals_answer_the_contracts_json_error_body_over_http() {
    let estate = ManagedEstate::new("json-write-shape");
    let work_a = estate.work_a.clone();
    let raised = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "needs a call",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--json",
        ],
    );
    let attention = raised["id"].as_str().unwrap().to_owned();
    let server = WebServer::start(&estate, &work_a, Some("X-Auth-Request-Email"));
    let path = format!("/attention/Alpha/{attention}/reply");
    let cross = server.request(
        "POST",
        &path,
        Some("https://hostile.example"),
        &[("X-Auth-Request-Email", "sso@edge.test")],
        Some("decision=approve&reply=done"),
    );
    assert_eq!(cross.status, 403);
    assert!(
        cross
            .head
            .contains("Content-Type: application/json; charset=utf-8"),
        "the same-origin refusal answered as {}",
        cross.head
    );
    assert_eq!(
        cross.body,
        format!("{{\"error\":\"{NOT_FROM_THIS_SITE}\"}}"),
        "the same-origin refusal is not the contract's Error body"
    );
}

/// A tag-denied row is in none of the four task listings, and in none of the
/// aggregates taken over them (`t-34f6eed5`).
///
/// INTEGRATION, at the layer `process`/`http`: the real binary, a real
/// managed estate, the CLI and the serving process running as the same
/// identity. The caller holds board read plus `tag:visible` read on Alpha,
/// and full ownership of Beta so the whole-estate `dashboard` answers at all
/// rather than refusing entire (that refusal is the separate finding
/// `a_whole_estate_listing_serves_the_boards_the_caller_may_read_over_http`).
///
/// `t-aboth` carries both tags. The read test is all-of-tag, so one of two
/// tags is not authority over the row, and a listing filtered as any-of
/// would hand it over while `task show` refused it.
///
/// Every assertion is paired with its positive control: `t-avis` IS listed
/// and IS counted, so a listing that had simply broken would fail here too.
#[test]
fn a_tag_denied_row_is_in_no_task_listing_and_in_no_count_over_http() {
    let estate = ManagedEstate::new("json-tag-listing");
    let work_a = estate.work_a.clone();

    for tag in ["visible", "secret"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    for (id, title, tags) in [
        ("t-avis", "alpha visible row", vec!["visible"]),
        ("t-asec", "alpha secret row", vec!["secret"]),
        (
            "t-aboth",
            "alpha row with both tags",
            vec!["visible", "secret"],
        ),
    ] {
        let mut args = vec!["task", "add", title, "--id", id];
        for tag in &tags {
            args.push("--tag");
            args.push(tag);
        }
        args.extend(["--as", "seed", "--json"]);
        estate.ok_json(&work_a, &args);
    }

    estate.bind_self(
        "p-visible-only",
        &[
            board_scope("read", &estate.id_a),
            tag_scope("read", &estate.id_a, "visible"),
        ],
    );
    estate.grant("p-visible-only", &owner_of(&estate.id_b));
    estate.enforce("managed");
    let server = WebServer::start(&estate, &work_a, None);

    // The control: the named read of each hidden row is refused, and the
    // visible one is not.
    estate.denied(&work_a, &["task", "show", "t-asec", "--json"]);
    estate.denied(&work_a, &["task", "show", "t-aboth", "--json"]);
    estate.ok_json(&work_a, &["task", "show", "t-avis", "--json"]);

    // 1. `task list`.
    let listed = estate.ok_json(&work_a, &["task", "list", "--json"]);
    let ids = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["t-avis".to_owned()],
        "task list handed over a row this caller may not read: {listed}"
    );

    // 2. `dashboard`: the aggregates over that listing.
    let dashboard = estate.ok_json(&work_a, &["dashboard", "--json"]);
    let alpha = dashboard
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "Alpha")
        .unwrap_or_else(|| panic!("the dashboard did not report Alpha: {dashboard}"));
    assert_eq!(
        alpha["totalTasks"], 1,
        "the dashboard total counts rows this caller may not read: {alpha}"
    );
    assert_eq!(
        alpha["taskCounts"]["todo"], 1,
        "the dashboard status counts are wrong: {alpha}"
    );

    // 3. The JSON board route, and its own summary count in the index.
    let board = server.get_json("/api/v1/board/Alpha");
    let api_ids = board["tasks"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["task"]["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        api_ids,
        vec!["t-avis".to_owned()],
        "/api/v1/board/Alpha handed over a row this caller may not read: {board}"
    );
    let boards = server.get_json("/api/v1/boards");
    let summary = boards["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["board"] == "Alpha")
        .unwrap_or_else(|| panic!("the index did not report Alpha: {boards}"));
    assert_eq!(
        summary["tasks"], 1,
        "the board index counts rows this caller may not read: {summary}"
    );

    // 4. The bytes the board PAGE is built from, searched for every string
    //    the hidden rows carry: bytes that named one would confirm its
    //    existence as surely as a rendering of them would. Since
    //    `t-bf255880` the page is the mounted application and `/board/Alpha`
    //    answers the shell, so the bytes that carry rows are the
    //    projection's — which is exactly what the client is handed.
    let shell = server.get("/board/Alpha");
    assert_eq!(shell.status, 200, "{}", shell.body);
    let page = server.get("/api/v1/board/Alpha");
    assert_eq!(page.status, 200, "{}", page.body);
    for needle in [
        "t-asec",
        "alpha secret row",
        "t-aboth",
        "alpha row with both tags",
    ] {
        assert!(
            !shell.body.contains(needle),
            "the board shell named {needle}, which this caller may not read"
        );
        assert!(
            !page.body.contains(needle),
            "the board projection named {needle}, which this caller may not read"
        );
    }
    assert!(
        page.body.contains("t-avis") && page.body.contains("alpha visible row"),
        "the board projection did not carry the row this caller MAY read"
    );
}

/// A tag-denied attention row is in no queue, in no decisions listing, in no
/// preview and in no open count (`t-8efced5b`).
///
/// INTEGRATION, at the layer `http`: the real binary, a real managed estate,
/// the CLI and the serving process running as the same identity. The caller
/// holds board read plus `tag:visible` read on Alpha, and full ownership of
/// Beta so the whole-estate listings answer at all rather than refusing
/// entire.
///
/// The attention row is the disclosure the task listing's fix did not cover:
/// a question, its body and its choices say what they are about. `a-both`
/// carries both tags, because the read test is all-of-tag and a filter
/// written as any-of would hand it over.
///
/// Every assertion is paired with its positive control — the `visible` row
/// IS queued, IS previewable and IS counted — so a surface that had simply
/// broken would fail here too.
#[test]
fn a_tag_denied_attention_row_is_in_no_queue_no_decision_and_no_count_over_http() {
    let estate = ManagedEstate::new("json-tag-attention");
    let work_a = estate.work_a.clone();

    for tag in ["visible", "secret"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "alpha visible row",
            "--id",
            "t-avis",
            "--tag",
            "visible",
            "--as",
            "seed",
            "--json",
        ],
    );

    let raise = |body: &str, priority: &str, tags: &[&str]| -> String {
        let mut args = vec![
            "attention",
            "raise",
            body,
            "--task",
            "t-avis",
            "--kind",
            "decision",
            "--priority",
            priority,
        ];
        for tag in tags {
            args.push("--tag");
            args.push(tag);
        }
        args.extend(["--as", "seed", "--json"]);
        estate.ok_json(&work_a, &args)["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // The denied row is the most urgent thing on Alpha. The board index
    // reads the urgent row with `limit = 1` (`board_summaries`), so a bound
    // applied before the tag test would hand that single slot to the row
    // this caller may not read, the filter would empty the page, and Alpha
    // would rank as though it held nothing urgent at all.
    let visible = raise("the visible question", "2", &["visible"]);
    let secret = raise("the secret question", "1", &["secret"]);
    let both = raise(
        "the question carrying both tags",
        "1",
        &["visible", "secret"],
    );
    // The decided row: resolved here, while the estate is still direct and
    // this caller is still a full owner of it, so what `/api/v1/decided`
    // withholds below is a real decision and not an unsettled row.
    let decided_secret = raise("the secret decision", "6", &["secret"]);
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "resolve",
            &decided_secret,
            "--choice",
            "approve",
            "--as",
            "seed",
            "--json",
        ],
    );
    let decided_visible = raise("the visible decision", "6", &["visible"]);
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "resolve",
            &decided_visible,
            "--choice",
            "approve",
            "--as",
            "seed",
            "--json",
        ],
    );

    // Beta, which this caller owns outright, holds one `P1`-shaped row: it
    // is the ruler the board index's urgency order is read against below.
    let work_b = estate.work_b.clone();
    estate.ok_json(
        &work_b,
        &[
            "task",
            "add",
            "beta row",
            "--id",
            "t-brow",
            "--priority",
            "3",
            "--as",
            "seed",
            "--json",
        ],
    );

    estate.bind_self(
        "p-visible-only",
        &[
            board_scope("read", &estate.id_a),
            tag_scope("read", &estate.id_a, "visible"),
        ],
    );
    estate.grant("p-visible-only", &owner_of(&estate.id_b));
    estate.enforce("managed");
    let server = WebServer::start(&estate, &work_a, None);

    let hidden = [secret.as_str(), both.as_str(), decided_secret.as_str()];
    let ids = |rows: &Value, what: &str| -> Vec<String> {
        rows.as_array()
            .unwrap_or_else(|| panic!("{what} is not a listing: {rows}"))
            .iter()
            .map(|row| {
                row.get("id")
                    .or_else(|| row.pointer("/attention/id"))
                    .unwrap_or_else(|| panic!("{what} row carries no id: {row}"))
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };

    // 1. `attention list` on the CLI — the surface `kb att list` reads.
    let listed = estate.ok_json(
        &work_a,
        &["attention", "list", "--status", "open", "--json"],
    );
    assert_eq!(
        ids(&listed, "attention list"),
        vec![visible.clone()],
        "att list handed over a card this caller may not read: {listed}"
    );

    // 2. `/api/v1/needs-you`, which reads the same store listing.
    let queue = server.get_json("/api/v1/needs-you");
    let queued = ids(&queue["items"], "needs-you");
    assert_eq!(
        queued,
        vec![visible.clone()],
        "the queue handed over a card this caller may not read: {queue}"
    );
    for (card, row) in queue["items"]
        .as_array()
        .unwrap()
        .iter()
        .zip(listed.as_array().unwrap())
    {
        agrees_on_shared_keys(
            &card["attention"],
            row,
            &["id", "kind", "status", "priority", "choices", "tags"],
            "a needs-you card",
        );
    }

    // 3. `/api/v1/decided`: a decision taken about a row this caller may not
    //    read is still about that row.
    let decided = server.get_json("/api/v1/decided");
    assert_eq!(
        ids(&decided["items"], "decided"),
        vec![decided_visible.clone()],
        "the decisions listing handed over a decision this caller may not read: {decided}"
    );

    // 4. The task detail's open items and its count.
    let detail = server.get_json("/api/v1/task/Alpha/t-avis");
    let open = detail["openAttention"].as_array().unwrap();
    assert_eq!(
        open.len(),
        1,
        "the task's open attention counts rows this caller may not read: {detail}"
    );
    assert_eq!(
        open[0]["attention"]["id"].as_str(),
        Some(visible.as_str()),
        "the task detail carried the wrong row: {detail}"
    );

    // 5. The hover preview names a row: the hidden ones answer the one
    //    non-enumerating refusal, and the readable one answers.
    for id in hidden {
        let path = format!("/api/v1/preview/attention/Alpha/{id}");
        let answer = server.get(&path);
        assert_eq!(answer.status, 404, "{path}: {}", answer.body);
        assert_eq!(
            answer.body, DENIED_JSON,
            "{path} answered a refusal of its own"
        );
    }
    let preview = server.get_json(&format!("/api/v1/preview/attention/Alpha/{visible}"));
    assert_eq!(
        preview["attention"]["id"].as_str(),
        Some(visible.as_str()),
        "the preview refused the row this caller MAY read: {preview}"
    );

    // 6. The board index's aggregate, which is `count_open_attention`.
    let boards = server.get_json("/api/v1/boards");
    let alpha = boards["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["board"] == "Alpha")
        .unwrap_or_else(|| panic!("the index did not report Alpha: {boards}"));
    assert_eq!(
        alpha["openAttention"], 1,
        "the open attention count counts rows this caller may not read: {alpha}"
    );

    //    …and its ORDER, which is the `limit = 1` urgency read. Alpha's most
    //    urgent READABLE row is priority 2 and Beta's ruler is 3, so Alpha
    //    ranks first. A bound applied before the tag test would have spent
    //    Alpha's single slot on the denied priority-1 row, left the urgency
    //    read empty, and dropped Alpha behind Beta on its tasks alone.
    assert_eq!(
        boards["items"][0]["board"], "Alpha",
        "the board index ranked Alpha as though its readable urgent row were not there: {boards}"
    );

    // 7. Nothing the hidden rows carry is anywhere in the served bodies —
    //    a body or a question names the row as surely as its id would. The
    //    task detail is swept WHOLE since `t-1de9c707`: its event stream
    //    used to carry the `attention_raised` and `attention_resolved`
    //    envelopes of rows this caller may not read, on the task's own
    //    `visible` tag, so the sweep had to be narrowed to `openAttention`.
    //    `event_tags` now unions the attention row's own tags in, and the
    //    whole body is clean.
    assert!(
        detail["events"]["returned"].as_u64().unwrap_or(0) > 0,
        "the task detail served no events at all, so the sweep below proves nothing: {detail}"
    );
    let served = format!("{queue}{decided}{boards}{detail}");
    for needle in [
        secret.as_str(),
        both.as_str(),
        decided_secret.as_str(),
        "the secret question",
        "the question carrying both tags",
        "the secret decision",
    ] {
        assert!(
            !served.contains(needle),
            "a served body named {needle}, which this caller may not read"
        );
    }
    assert!(
        served.contains("the visible question") && served.contains("the visible decision"),
        "the served bodies lost the rows this caller MAY read"
    );
    // The positive control on the tail itself: the readable rows' envelopes
    // are still in the task's events.
    let tail = detail["events"].to_string();
    assert!(
        tail.contains(visible.as_str()) && tail.contains(decided_visible.as_str()),
        "the event tail lost the attention rows this caller MAY read: {tail}"
    );
}

/// A tag-denied prerequisite, ancestor and child, read through a row the
/// caller MAY read (`t-3548303e`).
///
/// INTEGRATION, at the layer `process`/`http`: the real binary, a real
/// managed estate, the CLI and the serving process running as the same
/// identity. The caller holds board read AND write plus `tag:visible` at
/// both capabilities on Alpha — write as well, so the claim refusal below
/// is the GATE's and not the guard's — and full ownership of Beta so the
/// whole-estate commands answer at all.
///
/// The fixture hangs three unreadable rows off one readable story: a
/// prerequisite (`t-secret`), the parent epic (`t-epic`) and a child
/// (`t-child`). Each is a different question and gets a different answer:
/// the prerequisite leaves the dependency listing but keeps its gate with
/// the title blanked, the epic truncates the ancestry instead of refusing
/// the readable leaf, and the child was never in any relation listing.
///
/// Positive controls throughout: the readable prerequisite `t-open` is in
/// every answer WITH its title, the readable parent `t-visible` is in the
/// leaf's ancestry, and the gate still refuses the claim in its own
/// unchanged sentence.
#[test]
fn a_tag_denied_prerequisite_keeps_its_gate_and_loses_its_title_over_http() {
    let estate = ManagedEstate::new("json-tag-relations");
    let work_a = estate.work_a.clone();

    for tag in ["visible", "secret"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "secret epic",
            "--id",
            "t-epic",
            "--type",
            "epic",
            "--tag",
            "secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    for (id, title) in [
        ("t-secret", "secret prerequisite"),
        ("t-open", "open prerequisite"),
    ] {
        let tag = if id == "t-secret" {
            "secret"
        } else {
            "visible"
        };
        estate.ok_json(
            &work_a,
            &[
                "task", "add", title, "--id", id, "--tag", tag, "--as", "seed", "--json",
            ],
        );
    }
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "visible row",
            "--id",
            "t-visible",
            "--type",
            "story",
            "--tag",
            "visible",
            "--parent",
            "t-epic",
            "--depends-on",
            "t-secret",
            "--depends-on",
            "t-open",
            "--as",
            "seed",
            "--json",
        ],
    );
    for (id, title, tag) in [
        ("t-child", "secret child", "secret"),
        ("t-leaf", "visible leaf", "visible"),
    ] {
        estate.ok_json(
            &work_a,
            &[
                "task",
                "add",
                title,
                "--id",
                id,
                "--tag",
                tag,
                "--parent",
                "t-visible",
                "--as",
                "seed",
                "--json",
            ],
        );
    }

    estate.bind_self(
        "p-visible-only",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "visible"),
            tag_scope("write", &estate.id_a, "visible"),
        ],
    );
    estate.grant("p-visible-only", &owner_of(&estate.id_b));
    estate.enforce("managed");
    let server = WebServer::start(&estate, &work_a, None);

    /// Every string only an unreadable row carries. Ids are deliberately
    /// absent: `t-secret` is the gate's own answer and `t-epic` is on the
    /// readable row's `parentID` already.
    const HIDDEN_PROSE: [&str; 3] = ["secret prerequisite", "secret epic", "secret child"];

    // The control: each hidden row is refused by name, and the row whose
    // relations are read is not.
    for hidden in ["t-secret", "t-epic", "t-child"] {
        estate.denied(&work_a, &["task", "show", hidden, "--json"]);
    }
    estate.ok_json(&work_a, &["task", "show", "t-visible", "--json"]);

    // 1. `task show`: the denied prerequisite is out of `dependencies` and
    //    its title is out of `blockingGates`, which still names it.
    let shown = estate.ok_json(&work_a, &["task", "show", "t-visible", "--json"]);
    let dependency_ids = |value: &Value| {
        value["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        dependency_ids(&shown),
        vec!["t-open".to_owned()],
        "task show handed over a prerequisite this caller may not read: {shown}"
    );
    let gates = |value: &Value| {
        value["blockingGates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|gate| {
                (
                    gate["prerequisiteID"].as_str().unwrap().to_owned(),
                    gate["prerequisiteStatus"].as_str().unwrap().to_owned(),
                    gate["prerequisiteTitle"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let expected_gate = vec![
        (
            "t-open".to_owned(),
            "todo".to_owned(),
            serde_json::json!("open prerequisite"),
        ),
        ("t-secret".to_owned(), "todo".to_owned(), Value::Null),
    ];
    assert_eq!(
        gates(&shown),
        expected_gate,
        "the gate must keep the denied prerequisite and lose only its title: {shown}"
    );

    // 2. `context`: it ANSWERS for the readable row whose epic is denied —
    //    before this fix the chain walk refused the whole packet — and the
    //    chain stops at the boundary rather than gaining a hole.
    let packet = estate.ok_json(&work_a, &["context", "t-visible", "--json"]);
    assert!(
        packet["ancestors"].as_array().unwrap().is_empty(),
        "the context packet named the denied epic: {packet}"
    );
    assert_eq!(
        dependency_ids(&packet),
        vec!["t-open".to_owned()],
        "the context packet handed over the denied prerequisite: {packet}"
    );
    assert_eq!(
        gates(&packet),
        expected_gate,
        "the context packet's gate differs from task show's: {packet}"
    );
    let leaf_packet = estate.ok_json(&work_a, &["context", "t-leaf", "--json"]);
    assert_eq!(
        leaf_packet["ancestors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["t-visible".to_owned()],
        "the readable run of the chain must survive: {leaf_packet}"
    );

    // 3. `task list --with-relations`, which reads the gate of every row in
    //    one call rather than one at a time.
    let listed = estate.ok_json(&work_a, &["task", "list", "--with-relations", "--json"]);
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "t-visible")
        .unwrap_or_else(|| panic!("the listing dropped the readable row: {listed}"));
    assert_eq!(
        row["dependencies"],
        serde_json::json!(["t-open"]),
        "the listing handed over the denied prerequisite: {row}"
    );
    assert_eq!(
        gates(row),
        expected_gate,
        "the listing's gate differs from task show's: {row}"
    );

    // 4. No surface carries the prose of a row this caller may not read —
    //    the CLI's own rendered packet included.
    let rendered = estate.ok(&work_a, &["context", "t-visible"]);
    for surface in [
        listed.to_string(),
        shown.to_string(),
        packet.to_string(),
        rendered,
        server.get("/task/Alpha/t-visible").body,
        server.get_json("/api/v1/task/Alpha/t-visible").to_string(),
    ] {
        for needle in HIDDEN_PROSE {
            assert!(
                !surface.contains(needle),
                "a surface named {needle}, which this caller may not read"
            );
        }
    }

    // 5. The gate still holds, in its own unchanged sentence, and it names
    //    the denied prerequisite by id and status — which is the whole point
    //    of keeping the blocker: a row refused a claim must not read as
    //    ungated on the surfaces above.
    let refused = estate.run(&work_a, &["claim", "t-leaf", "--as", "agent"]);
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        !refused.status.success(),
        "the gate let a claim through: {stderr}"
    );
    assert_eq!(
        stderr.trim(),
        "Error: task t-leaf is gated on work that is not done: t-open is todo \
         (declared on ancestor t-visible); t-secret is todo (declared on ancestor \
         t-visible). A prerequisite satisfies a gate only at status done: finish it, \
         or drop the edge with `task update <owner> --depends-on ...` or \
         `--clear-dependencies`.",
        "the gate refusal wording changed"
    );

    // The positive control for the whole fixture: the readable prerequisite
    // is readable by name, with the title every answer above carried.
    let open = estate.ok_json(&work_a, &["task", "show", "t-open", "--json"]);
    assert_eq!(open["title"], "open prerequisite", "{open}");
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
        while let Ok(line) = self.lines.try_recv() {
            fresh.push(line);
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

/// One `kanban serve --port N --actor-header NAME` against this estate, on an
/// ephemeral loopback port.
///
/// Readiness is the process's own banner naming that port, drained through a
/// channel so a server that never binds fails on a deadline instead of parking
/// the test on a blocking read. `GET /` is deliberately NOT the gate: under
/// managed enforcement the operator page reads rows this principal may not
/// see, so a page error is the expected state, not a failure to start.
struct WebServer {
    child: Child,
    port: u16,
}

impl WebServer {
    fn start(estate: &ManagedEstate, cwd: &Path, header: Option<&str>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve a loopback port");
        let port = listener
            .local_addr()
            .expect("read the reserved port")
            .port();
        drop(listener);
        let mut command = estate.command(cwd);
        command.args(["serve", "--port", &port.to_string()]);
        // Without `--actor-header` the write surface records the default
        // operator, which is the configuration the read routes are served
        // under and the one a reader should be measured against.
        if let Some(header) = header {
            command.args(["--actor-header", header]);
        }
        let mut child = command.spawn().expect("spawn kanban serve");
        let stderr = child.stderr.take().expect("serve stderr is piped");
        let (sender, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let banner =
            format!("kanban serve: http://127.0.0.1:{port} (loopback only; front it with nginx)");
        let deadline = Instant::now() + APPEAR;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match lines.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    if line == banner {
                        return Self { child, port };
                    }
                    seen.push(line);
                }
                Err(_) => continue,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "kanban serve never announced {banner:?} within {APPEAR:?}:\n{}",
            seen.join("\n")
        );
    }

    /// One same-origin POST: `Host` and `Origin` name the same authority, so
    /// the CSRF gate passes and whatever happens next is the guard's answer.
    fn post(&self, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
        let origin = format!("http://127.0.0.1:{}", self.port);
        self.request("POST", path, Some(&origin), headers, Some(body))
    }

    /// One `GET`, with no `Origin`: a read carries no CSRF gate.
    fn get(&self, path: &str) -> Answer {
        self.request("GET", path, None, &[], None)
    }

    /// One `GET` that must have answered `200` with the contract's content
    /// type, parsed.
    fn get_json(&self, path: &str) -> Value {
        let answer = self.get(path);
        assert_eq!(answer.status, 200, "{path}: {}", answer.body);
        assert!(
            answer
                .head
                .contains("Content-Type: application/json; charset=utf-8"),
            "{path} answered as {}",
            answer.head
        );
        serde_json::from_str(&answer.body)
            .unwrap_or_else(|error| panic!("{path} is not JSON: {error}\n{}", answer.body))
    }

    /// One request, exactly as written: the method, the path, an `Origin` or
    /// deliberately none, headers in the order given — a repeated name is
    /// sent twice, which is the only way to offer a second copy of one
    /// header — and a body or none.
    fn request(
        &self,
        method: &str,
        path: &str,
        origin: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Answer {
        let port = self.port;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to kanban serve");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        )
        .unwrap();
        if let Some(origin) = origin {
            write!(stream, "Origin: {origin}\r\n").unwrap();
        }
        if body.is_some() {
            write!(
                stream,
                "Content-Type: application/x-www-form-urlencoded\r\n"
            )
            .unwrap();
        }
        write!(
            stream,
            "Content-Length: {}\r\nConnection: close\r\n",
            body.unwrap_or("").len()
        )
        .unwrap();
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        write!(stream, "\r\n").unwrap();
        stream.write_all(body.unwrap_or("").as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        Answer::decode(&raw)
    }
}

/// One response, cut into its head and the body the server meant to send.
///
/// The cut is on bytes, so the boundary cannot move, and the chunked framing
/// is removed rather than left in the body: tiny_http picks chunked for
/// anything at or above 32768 bytes, every page here carries an inline
/// stylesheet, and a bounded listing of rows clears that easily — so an
/// assertion about a needle would otherwise pass or fail on where a chunk
/// boundary happened to fall.
struct Answer {
    status: u16,
    head: String,
    body: String,
}

impl Answer {
    fn decode(raw: &[u8]) -> Self {
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|at| at + 4)
            .unwrap_or(raw.len());
        let (head, body) = raw.split_at(split);
        let head = String::from_utf8_lossy(head).into_owned();
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(body)
        } else {
            body.to_vec()
        };
        Self {
            status,
            head,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }
}

/// A chunked body put back together, byte for byte. Strict on purpose: a
/// short read or a malformed frame panics here rather than returning the
/// prefix it managed to decode, which is the one failure mode that would
/// leave assertions passing against bytes that never arrived.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    loop {
        let eol = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .unwrap_or_else(|| {
                panic!(
                    "a chunked body ended after {} bytes with no size line",
                    body.len()
                )
            });
        let line = String::from_utf8_lossy(&body[..eol]).into_owned();
        let size = usize::from_str_radix(line.split(';').next().unwrap_or(&line).trim(), 16)
            .unwrap_or_else(|error| panic!("chunk size {line:?} is not hexadecimal: {error}"));
        let rest = &body[eol + 2..];
        if size == 0 {
            return out;
        }
        assert!(
            rest.len() >= size + 2,
            "a chunk of {size} bytes arrived with {} bytes behind its size line",
            rest.len()
        );
        out.extend_from_slice(&rest[..size]);
        body = &rest[size + 2..];
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
