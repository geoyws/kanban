//! The epic's bypass matrix, at the compiled-process boundary (ADR-006,
//! ADR-038 clauses 5 and 9).
//!
//! Seven classes, one test each: cross-board, retagging, history, projection,
//! actor, selector, and revocation. Every one spawns
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

//! Non-root only: the managed broker refuses root pairs by design
//! (`routing::local_authority` mints no authority for euid 0, and
//! `policy.rs` refuses root bootstrap/prove-rebind pairs), so as uid 0
//! `bind_self` binds a principal the guard can never resolve and every
//! managed command answers `denied-or-not-found`. `ManagedEstate::new`
//! panics fast with that sentence instead of failing seventeen tests
//! confusingly — and never skips. Run as a normal user or in the Linux
//! gate container.

use rusqlite::Connection;
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The one refusal every denial in this file must be, byte for byte. A second
/// wording anywhere would be an oracle.
const SEED_HEAD: &str = "0000000000000000000000000000000000000000";
const DENIED: &str = "denied or not found";

/// How long a `watch --follow` assertion waits for a line that should arrive,
/// and how long it waits before being satisfied that a line will NOT arrive.
/// The `watch --follow` server polls once per 250ms (`rust/watch.rs`
/// `POLL_INTERVAL`), so both are many polls wide.
const APPEAR: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_secs(4);
/// Hard cap for a should-arrive wait that keeps seeing stream output. Under
/// load the delivery can lag well past APPEAR while the stream is visibly
/// alive (heartbeats keep arriving); the cap bounds that grace period.
const APPEAR_MAX: Duration = Duration::from_secs(30);

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
        require_non_root();
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

/// Fail fast as root instead of failing every test confusingly. The managed
/// broker refuses root pairs by design (`routing::local_authority` mints no
/// authority for euid 0; `policy.rs` refuses root bootstrap/prove-rebind
/// pairs), so `bind_self` would bind a principal the guard can never resolve
/// and every managed command would answer `denied-or-not-found`. This panics
/// with that sentence — it never skips, so a root run stays red until it is
/// re-run as a non-root user or in the Linux gate container.
fn require_non_root() {
    if self_uid() == 0 {
        panic!(
            "authz_bypass_matrix_e2e requires a non-root user: the managed broker refuses root pairs by design \
             (routing::local_authority mints no authority for euid 0; policy.rs refuses root bootstrap/prove-rebind pairs), \
             so as uid 0 every managed command answers `denied-or-not-found`. \
             Re-run as a non-root user or in the Linux gate container."
        );
    }
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
        &["tag", "add", "geoyws/private", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_b,
        &[
            "task",
            "add",
            "beta secret",
            "--tag",
            "geoyws/private",
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

    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/alpha", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/beta", "--as", "seed", "--json"],
    );
    let row = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "movable subject",
            "--tag",
            "geoyws/alpha",
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
            tag_scope("read", &estate.id_a, "geoyws/alpha"),
            tag_scope("write", &estate.id_a, "geoyws/alpha"),
        ],
    );
    estate.enforce("managed");

    estate.denied(
        &work_a,
        &[
            "task",
            "update",
            &row_id,
            "--tag",
            "geoyws/beta",
            "--as",
            "actor",
        ],
    );

    // The refused write did not partly apply: the row is still `alpha`, which
    // this caller can still see.
    let after = estate.ok_json(&work_a, &["task", "show", &row_id, "--json"]);
    assert_eq!(after["tags"], serde_json::json!(["geoyws/alpha"]));

    // Now the read half. Retag the row to `beta` through the direct estate —
    // the guard is not what is under test here — then desynchronise the index
    // so its copy of the tags is empty.
    estate.enforce("direct");
    estate.ok_json(
        &work_a,
        &[
            "task",
            "update",
            &row_id,
            "--tag",
            "geoyws/beta",
            "--as",
            "seed",
        ],
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
        "geoyws/beta",
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
        !hits.contains("geoyws/beta"),
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

    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
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
            "task",
            "update",
            &hidden_id,
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
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
            "geoyws/secret",
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
            frozen != &serde_json::json!(["geoyws/secret"])
                && !frozen.to_string().contains("geoyws/secret"),
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
    estate.grant(
        "p-board-only",
        &[tag_scope("read", &estate.id_a, "geoyws/secret")],
    );
    estate.enforce("managed");
    assert!(
        estate
            .ok(&work_a, &["events", "--task", &hidden_id, "--json"])
            .contains(&hidden_id)
    );
}

/// ACC-14, removed-task tails: every event of a task that was tagged and
/// then removed stays tag-gated on each tail that serves it, and the tails
/// agree with search.
///
/// CLI-process port of the HTTP-era `..._on_every_tail_over_http`: serve is
/// retired, so the served routes are gone and board-wide `events`,
/// `events --task`, a `watch --follow` stream and CLI `search` carry the
/// claim instead. The caller holds only board read on Alpha — no tag scope
/// at all — and full ownership of Beta so the whole-estate reads answer.
///
/// The fixture creates a task untagged, edits its body so a `task_updated`
/// event carries the draft in `previousBody` while its snapshot still reads
/// `tags: []`, tags the task `secret`, then removes it. The `events` rows
/// survive the delete, so the pre-tag event is reachable through every tail.
/// The draft token appears nowhere else on either board, so its absence
/// proves each tail withheld the event rather than merely ranking it below
/// something.
///
/// The denied half reads board-wide `events`, `events --task`, one
/// `watch --follow` stream and `search`: none carries the token, and the
/// `--task` refusal answers exactly like a never-created id. The owner half
/// grants that same principal `secret` read and reads again: `events`,
/// `watch --follow` and `search` all carry the token. A fix that simply hid
/// every removed-task event would fail here, so what is proved is gating,
/// not hiding.
#[test]
fn a_removed_tasks_trail_stays_tag_gated_on_every_tail() {
    let estate = ManagedEstate::new("acc14-removed-task-tails");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "removed tail",
            "--id",
            "t-evttail",
            "--body",
            "draft cinderquorum notes",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "update",
            "t-evttail",
            "--body",
            "final wording",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "update",
            "t-evttail",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok(&work_a, &["task", "remove", "t-evttail", "--as", "seed"]);

    estate.bind_self("p-tail-reader", &[board_scope("read", &estate.id_a)]);
    estate.grant("p-tail-reader", &owner_of(&estate.id_b));
    estate.enforce("managed");

    // The denied half: no tail carries the removed task's pre-tag draft.
    let events = estate.ok(&work_a, &["events", "--json"]);
    assert!(
        !events.contains("cinderquorum"),
        "board-wide events handed over a removed task's pre-tag draft: {events}"
    );
    let rows: Vec<Value> = serde_json::from_str(&events).unwrap();
    assert!(
        rows.iter()
            .all(|event| event["taskID"] != serde_json::json!("t-evttail")),
        "board-wide events served a removed task's trail: {events}"
    );
    // `events --task` on the gone row refuses exactly like a never-created
    // id — the same exit code and the same stderr modulo the id — and leaks
    // nothing either way.
    let named = estate.run(&work_a, &["events", "--task", "t-evttail", "--json"]);
    let unknown = estate.run(&work_a, &["events", "--task", "t-never-created", "--json"]);
    assert!(
        !named.status.success(),
        "events --task on a removed task should fail, not serve its trail"
    );
    assert_eq!(
        named.status.code(),
        unknown.status.code(),
        "the gone row's --task refusal exits differently from an unknown id"
    );
    let named_stderr = String::from_utf8_lossy(&named.stderr).into_owned();
    let unknown_stderr = String::from_utf8_lossy(&unknown.stderr).into_owned();
    assert_eq!(
        named_stderr.replace("t-evttail", "t-never-created"),
        unknown_stderr,
        "the gone row's --task refusal reads differently from an unknown id"
    );
    assert!(
        !named_stderr.contains("cinderquorum") && !unknown_stderr.contains("cinderquorum"),
        "events --task leaked the removed task's draft in its refusal: {named_stderr}"
    );
    // CLI search drops the gone row's pre-tag event for the tag-less reader:
    // the receipt echoes the query, so what must be empty is the results.
    let search: Value =
        serde_json::from_str(&estate.ok(&work_a, &["search", "cinderquorum", "--json"])).unwrap();
    assert!(
        search["results"].as_array().is_some_and(Vec::is_empty),
        "search handed over a removed task's pre-tag event: {search}"
    );
    // The live tail withholds it too. The stream replays from cursor 0, so
    // the pre-tag event is offered to the filter; heartbeats prove the
    // stream stayed alive while it stayed quiet.
    {
        let mut stream = Stream::start(
            estate
                .command(&work_a)
                .args(["watch", "--follow", "--json"]),
        );
        thread::sleep(SETTLE);
        let quiet = stream.drain();
        assert!(
            !quiet.join("\n").contains("cinderquorum"),
            "watch --follow handed over a removed task's pre-tag draft:\n{}",
            quiet.join("\n")
        );
        assert!(
            stream.running(),
            "the watch process exited instead of withholding the row"
        );
        assert!(
            !quiet.is_empty(),
            "the stream produced nothing at all, so nothing was measured"
        );
    }

    // The owner half: the same principal with `secret` read keeps the trail
    // on every tail, so the rule gates rather than hides.
    estate.enforce("direct");
    estate.grant(
        "p-tail-reader",
        &[tag_scope("read", &estate.id_a, "geoyws/secret")],
    );
    estate.enforce("managed");
    let events = estate.ok(&work_a, &["events", "--json"]);
    assert!(
        events.contains("cinderquorum"),
        "the tag holder lost the removed task's audit trail in events: {events}"
    );
    let search: Value =
        serde_json::from_str(&estate.ok(&work_a, &["search", "cinderquorum", "--json"])).unwrap();
    assert!(
        !search["results"].as_array().is_some_and(Vec::is_empty)
            && search.to_string().contains("cinderquorum"),
        "the tag holder lost the removed task's audit trail in search: {search}"
    );
    let mut stream = Stream::start(
        estate
            .command(&work_a)
            .args(["watch", "--follow", "--json"]),
    );
    let deadline = Instant::now() + APPEAR;
    loop {
        let seen = stream.drain().join("\n");
        if seen.contains("cinderquorum") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the tag holder never received the removed task's trail on watch --follow"
        );
        thread::sleep(Duration::from_millis(100));
    }
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

    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
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
            "geoyws/secret",
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
            tag_scope("read", &estate.id_a, "geoyws/secret"),
            tag_scope("write", &estate.id_a, "geoyws/secret"),
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
/// arrive, retires the tag grant while the stream is still open — the next
/// attach is refused outright, because a caller who cannot read the row
/// cannot write to it either — then restores the grant just long enough to
/// write one event, revokes again, and observes that event NOT arrive. The
/// process is asserted still running and still producing output, so what is
/// measured is a live stream that stopped delivering — not a crashed one, and
/// not a reconnect. Restoring the grant then makes the withheld row appear on
/// the SAME stream, which is only possible if the authority is re-read per poll.
#[test]
fn revoking_authority_stops_a_live_watch_stream_without_a_reconnect() {
    let estate = ManagedEstate::new("revocation");
    let work_a = estate.work_a.clone();

    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/live", "--as", "seed", "--json"],
    );
    let watched = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "watched row",
            "--tag",
            "geoyws/live",
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
            tag_scope("read", &estate.id_a, "geoyws/live"),
            tag_scope("write", &estate.id_a, "geoyws/live"),
        ],
    );
    estate.enforce("managed");

    let mut stream = Stream::start(estate.command(&work_a).args([
        "watch",
        "--tag",
        "geoyws/live",
        "--follow",
        "--json",
    ]));

    // Before revocation: a note on the watched row is delivered as an event
    // envelope. The envelope carries the event's identity, not the note body,
    // so what is counted is `"type":"event"` — a heartbeat is the other kind.
    estate.ok_json(&work_a, &note_on(&watched_id, "before-revocation"));
    stream.wait_for_event(APPEAR);

    // Revoke, with the stream still open and the process untouched.
    estate.revoke_atom("tag:geoyws/live");

    // The write stops too: without the tag the caller cannot attach to the
    // row at all, so the refused note never reaches the ledger.
    estate.denied(&work_a, &note_on(&watched_id, "after-revocation"));

    // To observe what the LIVE stream does with an event it must not deliver,
    // that event has to be written by someone who still may: restore the tag,
    // write the note, and revoke again — all with the stream still open.
    // Anything the stream delivered while the grant was back is drained, so
    // what follows measures only the revoked state.
    estate.enforce("direct");
    estate.grant(
        "p-watcher",
        &[
            tag_scope("read", &estate.id_a, "geoyws/live"),
            tag_scope("write", &estate.id_a, "geoyws/live"),
        ],
    );
    estate.enforce("managed");
    stream.drain();
    estate.ok_json(&work_a, &note_on(&watched_id, "after-revocation"));
    estate.revoke_atom("tag:geoyws/live");
    // After revocation: the next event on the same row must not be delivered.
    stream.drain();
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
    estate.grant(
        "p-watcher",
        &[
            tag_scope("read", &estate.id_a, "geoyws/live"),
            tag_scope("write", &estate.id_a, "geoyws/live"),
        ],
    );
    estate.enforce("managed");
    stream.drain();
    estate.ok_json(&work_a, &note_on(&watched_id, "after-restore"));
    stream.wait_for_event(APPEAR);
}

/// ACC-14 (A11): a checked row stays non-enumerating to an unauthorized
/// actor. The caller owns board B and nothing on board A: the same-key check
/// post and every read of A's checked row get the existing generic denial
/// with no check metadata anywhere, and flipping back to direct afterwards
/// shows the check still unanswered with no result from the denied attempts.
#[test]
fn checked_row_stays_non_enumerating_to_an_unauthorized_actor() {
    let estate = ManagedEstate::new("checked-non-enumerating");
    let work_a = estate.work_a.clone();
    // Raised in direct mode, the way a real estate reaches managed with
    // boards already in it. Sentinels stand in for every check secret.
    let raised = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "Choose after demonstrating the Store boundary.",
            "--as",
            "seed",
            "--check",
            "Which layer owns traversal QUIZSENTINEL?",
            "--check-choice",
            "store=Label SENTINELLABEL one",
            "--check-choice",
            "client=Label SENTINELLABEL two",
            "--check-answer",
            "store",
            "--check-explain",
            "Explanation SENTINEL EXPLAIN holds the answer.",
            "--check-about",
            "src/store.rs",
            "--json",
        ],
    );
    let id = raised["id"].as_str().unwrap().to_owned();

    // The caller owns board B and nothing on board A.
    estate.bind_self("p-b-owner", &owner_of(&estate.id_b));
    estate.enforce("managed");

    let sentinels = [
        "QUIZSENTINEL",
        "SENTINELLABEL",
        "SENTINEL EXPLAIN",
        "src/store.rs",
    ];
    let mut refused_any = false;
    for args in [
        vec!["attention", "show", id.as_str(), "--json"],
        vec![
            "attention",
            "check",
            id.as_str(),
            "--as",
            "seed",
            "--key",
            "store",
            "--json",
        ],
        vec![
            "attention",
            "resolve",
            id.as_str(),
            "--as",
            "seed",
            "--check-answered",
            "store",
            "--json",
        ],
    ] {
        let output = estate.run(&work_a, &args);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "{args:?} succeeded for an actor with no authority on this board\nstdout: {stdout}"
        );
        assert!(
            stderr.contains(DENIED),
            "{args:?} was refused without the generic denial\nstderr: {stderr}"
        );
        for sentinel in sentinels {
            assert!(
                !stdout.contains(sentinel) && !stderr.contains(sentinel),
                "{args:?} leaked check metadata {sentinel:?}\nstdout: {stdout}\nstderr: {stderr}"
            );
        }
        refused_any = true;
    }
    assert!(refused_any, "no denied attempt ran");

    // Back to direct: the check is still unanswered with no result from the
    // denied attempts, so nothing was written on the way out.
    estate.enforce("direct");
    let shown = estate.ok_json(&work_a, &["attention", "show", id.as_str(), "--json"]);
    assert!(
        shown["check"]["answered"].is_null(),
        "a denied post recorded an answer: {}",
        shown["check"]
    );
    assert!(
        shown["check"].get("result").is_none(),
        "a denied post recorded a result: {}",
        shown["check"]
    );
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
    ///
    /// The base budget is `budget` (APPEAR at both call sites). Past it, the
    /// wait is granted grace ONLY while the stream keeps producing output —
    /// each newly arrived line (heartbeat or envelope) moves the deadline out
    /// by another `budget`, capped at APPEAR_MAX past the start. Under load
    /// the server-side 250ms poll plus scheduling can lag a delivery well
    /// past the base budget while the stream is visibly alive; that is the
    /// flake this absorbs. Total silence still fails at the base budget,
    /// because with no output at all there is no evidence the stream is even
    /// up.
    ///
    /// Why this cannot mask a real failure: the exit condition is unchanged —
    /// an `"type":"event"` envelope must still arrive, and a heartbeat is
    /// never counted as one (`events_in`). A genuinely broken delivery
    /// produces zero envelopes no matter how long the wait runs, so it still
    /// fails, only at the cap instead of at the base budget; grace merely
    /// withholds the verdict while the stream proves it is alive and polling.
    /// The must-NOT-appear half of the revocation test is untouched — it
    /// still sleeps a fixed SETTLE and asserts zero envelopes — so extra
    /// patience here can never turn a leaked delivery into a pass.
    fn wait_for_event(&mut self, budget: Duration) {
        let start = Instant::now();
        let hard = start + APPEAR_MAX;
        let mut deadline = start + budget;
        let mut arrived = Vec::new();
        let mut settled = 0usize;
        loop {
            for line in self.drain() {
                settled = 0;
                arrived.push(line);
                // Observed progress: the stream is alive, so grant another
                // full budget window, never past the hard cap.
                deadline = (Instant::now() + budget).min(hard);
            }
            if events_in(&arrived) > 0 {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                panic!(
                    "no event envelope arrived within {budget:?} (+ progress-tied grace to {APPEAR_MAX:?}, \
                     {settled} silent polls at the end):\n{}",
                    arrived.join("\n")
                );
            }
            settled += 1;
            thread::sleep(Duration::from_millis(100));
        }
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
// ---------------------------------------------------------------------------
// Search scores: a denied document must not move a permitted hit's scores.
// ---------------------------------------------------------------------------

/// The lexical normalisation oracle (ACC-14): `lexical_scores` divided every
/// bm25 by the strongest match on the whole board index, denied documents
/// included, and served the quotient as `lexicalScore` (folded into `score`).
/// A principal with board read could plant an anchor task carrying a unique
/// token once in a long body, then read a denied token's prefix off the
/// anchor's served scores: `lexicalScore < 1.0` exactly when a denied
/// document matches the guessed prefix more strongly.
///
/// The anchor's served `lexicalScore` and `score` must therefore be identical
/// with and without the denied document. The denied document is created after
/// the first measurement, through the direct estate, so the principal's own
/// authority never changes between the two reads.
#[test]
fn search_scores_are_a_function_of_permitted_documents_only() {
    let estate = ManagedEstate::new("search-oracle");
    let work_a = estate.work_a.clone();

    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );

    // Board read plus board write: untagged rows, and nothing tagged.
    estate.bind_self(
        "p-oracle",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");

    // The anchor: a long body carrying the unique token `anchorqq` exactly
    // once, so its bm25 is low. The filler carries no `zzqxj9`-prefixed
    // token, so nothing visible matches the guessed prefix.
    let filler = "The harbour ledger records each crossing in turn, noting the vessel name, \
        the tide mark and the pilot on duty. Clerks copy the manifest lines into the day book \
        before the evening bell, and the night watch signs every page. "
        .repeat(8);
    let anchor_body =
        format!("{filler}The one token this probe carries is anchorqq, written exactly once.");
    let anchor = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "Anchor probe document",
            "--body",
            &anchor_body,
            "--as",
            "seed",
            "--json",
        ],
    );
    let anchor_id = anchor["id"].as_str().unwrap().to_owned();

    fn anchor_scores(receipt: &Value, anchor_id: &str, what: &str) -> (f64, f64) {
        let hit = receipt["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{what}: the receipt has no results: {receipt}"))
            .iter()
            .find(|hit| hit["sourceKind"] == "task" && hit["sourceId"] == anchor_id)
            .unwrap_or_else(|| panic!("{what}: the anchor is missing: {receipt}"));
        (
            hit["lexicalScore"].as_f64().unwrap(),
            hit["score"].as_f64().unwrap(),
        )
    }

    let query = "anchorqq zzqxj9";
    let before = estate.ok_json(&work_a, &["search", query, "--limit", "50", "--json"]);
    let (lexical_before, score_before) = anchor_scores(&before, &anchor_id, "before");

    // A tag-denied document matching the guessed prefix far more strongly:
    // sixty repetitions in a short body. The principal holds no `secret`
    // grant, so this row is invisible to them — but on the unfixed code its
    // bm25 becomes the normalisation divisor.
    estate.enforce("direct");
    let denied_body = format!("A short vault note. {}", "zzqxj9vlt ".repeat(60));
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "Vault note",
            "--body",
            &denied_body,
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.enforce("managed");

    let after = estate.ok_json(&work_a, &["search", query, "--limit", "50", "--json"]);
    // The denied row itself stays hidden.
    assert!(
        !after.to_string().contains("Vault note") && !after.to_string().contains("zzqxj9vlt"),
        "the denied row leaked into the receipt: {after}"
    );
    let (lexical_after, score_after) = anchor_scores(&after, &anchor_id, "after");
    assert_eq!(
        lexical_after, lexical_before,
        "the anchor's lexicalScore moved from {lexical_before} to {lexical_after} when a \
         denied document matching the guessed prefix appeared: the denied row is a score oracle"
    );
    assert_eq!(
        score_after, score_before,
        "the anchor's score moved from {score_before} to {score_after} when a denied document \
         matching the guessed prefix appeared: the denied row is a score oracle"
    );
}

/// The IDF follow-up to the divisor oracle (ACC-14, `t-e9c0127a` M1):
/// filtering only the normalisation divisor is not enough, because FTS5's
/// per-row bm25 folds whole-index statistics — per-term document frequency,
/// row count, average length — into every score. A denied row containing a
/// guessed prefix moves the IDF of that term, and with it the served
/// `lexicalScore`, `score` and possibly the order of the permitted hits.
///
/// Two anchors each carry one side of the query: A the unique token
/// `anchorqq` once in a long body, B the token `zzqxj9aaa` (matched by the
/// guessed prefix `zzqxj9`) once in a long body. Both hits' served scores
/// AND their relative order must be identical with and without a tag-denied
/// document strongly matching the prefix.
#[test]
fn search_scores_and_order_are_a_function_of_permitted_documents_only() {
    let estate = ManagedEstate::new("search-oracle-idf");
    let work_a = estate.work_a.clone();

    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    estate.bind_self(
        "p-oracle-idf",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");

    // The shared filler carries neither query token, so each anchor matches
    // exactly one side of the query.
    let filler = "The harbour ledger records each crossing in turn, noting the vessel name, \
        the tide mark and the pilot on duty. Clerks copy the manifest lines into the day book \
        before the evening bell, and the night watch signs every page. "
        .repeat(8);
    let add_anchor = |title: &str, body: &str| -> String {
        estate.ok_json(
            &work_a,
            &[
                "task", "add", title, "--body", body, "--as", "seed", "--json",
            ],
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let anchor_a = add_anchor(
        "Anchor probe alpha",
        &format!("{filler}The one token this probe carries is anchorqq, written exactly once."),
    );
    let anchor_b = add_anchor(
        "Anchor probe beta",
        &format!("{filler}The one token this probe carries is zzqxj9aaa, written exactly once."),
    );

    fn task_hit<'a>(receipt: &'a Value, anchor_id: &str, what: &str) -> &'a Value {
        receipt["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{what}: the receipt has no results: {receipt}"))
            .iter()
            .find(|hit| hit["sourceKind"] == "task" && hit["sourceId"] == anchor_id)
            .unwrap_or_else(|| panic!("{what}: the anchor is missing: {receipt}"))
    }

    fn order(receipt: &Value, anchor_a: &str, anchor_b: &str, what: &str) -> Vec<String> {
        receipt["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{what}: the receipt has no results: {receipt}"))
            .iter()
            .filter(|hit| {
                hit["sourceKind"] == "task"
                    && (hit["sourceId"] == anchor_a || hit["sourceId"] == anchor_b)
            })
            .map(|hit| hit["sourceId"].as_str().unwrap().to_owned())
            .collect()
    }

    let query = "anchorqq zzqxj9";
    let before = estate.ok_json(&work_a, &["search", query, "--limit", "50", "--json"]);
    let order_before = order(&before, &anchor_a, &anchor_b, "before");
    assert_eq!(
        order_before.len(),
        2,
        "both anchors should be served before the denied row exists: {before}"
    );

    estate.enforce("direct");
    let denied_body = format!("A short vault note. {}", "zzqxj9vlt ".repeat(60));
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "Vault note",
            "--body",
            &denied_body,
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.enforce("managed");

    let after = estate.ok_json(&work_a, &["search", query, "--limit", "50", "--json"]);
    assert!(
        !after.to_string().contains("Vault note") && !after.to_string().contains("zzqxj9vlt"),
        "the denied row leaked into the receipt: {after}"
    );
    for (anchor_id, name) in [(&anchor_a, "geoyws/alpha"), (&anchor_b, "geoyws/beta")] {
        let hit_before = task_hit(&before, anchor_id, "before");
        let hit_after = task_hit(&after, anchor_id, "after");
        assert_eq!(
            hit_after["lexicalScore"], hit_before["lexicalScore"],
            "anchor {name}'s lexicalScore moved when a denied document matching the guessed \
             prefix appeared: {before} vs {after}"
        );
        assert_eq!(
            hit_after["score"], hit_before["score"],
            "anchor {name}'s score moved when a denied document matching the guessed prefix \
             appeared: {before} vs {after}"
        );
    }
    assert_eq!(
        order(&after, &anchor_a, &anchor_b, "after"),
        order_before,
        "the anchors' result order moved when a denied document appeared: {before} vs {after}"
    );
}

// ---------------------------------------------------------------------------
// Task-attach writes: note, attention raise --task, sitrep post --task.
// ---------------------------------------------------------------------------

/// ACC-14, task-attach writes (`t-d2fd604a` port of `9626f1f`, CLI paths
/// only): a managed caller holding board read and write (plus `visible` at
/// both capabilities) but no `secret` scope must not attach a note, an
/// attention row or a sitrep to a `secret` task, and a denied task id must
/// answer byte-identically to a never-created id on each of those three
/// writes — with nothing recorded.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. The pre-serve fix covered further attach paths
/// (checkpoint, handoff with a task, parent and dependency edges,
/// subscription subjects and relations, `deploy start --task`); their store
/// paths are unchanged by this port, so no test pins them here.
#[test]
fn note_attention_raise_and_sitrep_refuse_a_tag_denied_task_like_an_unknown_id() {
    fn sitrep_on_task(task: &str) -> Vec<&str> {
        vec![
            "sitrep",
            "post",
            "attach probe body",
            "--as",
            "probe",
            "--lane",
            "driver-1",
            "--repo",
            "/tmp/attach-probe",
            "--branch",
            "main",
            "--head",
            "0123456789abcdef0123456789abcdef01234567",
            "--dirty",
            "clean",
            "--task",
            task,
            "--json",
        ]
    }
    fn raise_on_task(task: &str) -> Vec<&str> {
        vec![
            "attention",
            "raise",
            "attach probe body",
            "--kind",
            "decision",
            "--as",
            "probe",
            "--task",
            task,
            "--json",
        ]
    }
    let estate = ManagedEstate::new("acc14-task-attach");
    let work_a = estate.work_a.clone();
    for tag in ["geoyws/visible", "geoyws/secret"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    // Unmanaged first: where no guard can deny, an unknown id keeps its plain
    // message on the paths that previously answered it.
    for (args, what) in [
        (note_on("t-never-created", "unmanaged probe"), "note add"),
        (raise_on_task("t-never-created"), "attention raise --task"),
        (sitrep_on_task("t-never-created"), "sitrep post --task"),
    ] {
        let plain = estate.run(&work_a, &args);
        assert!(!plain.status.success(), "{what} should fail unmanaged");
        let plain_stderr = String::from_utf8_lossy(&plain.stderr).into_owned();
        assert!(
            plain_stderr.contains("task t-never-created not found"),
            "{what} lost its plain unmanaged message: {plain_stderr}"
        );
        assert!(
            plain_stderr.find(DENIED).is_none(),
            "{what} answers a denial where no guard can deny: {plain_stderr}"
        );
    }
    for (id, title, tag) in [
        ("s-attach-secret", "the attach secret task", "geoyws/secret"),
        (
            "s-attach-visible",
            "the attach visible task",
            "geoyws/visible",
        ),
    ] {
        estate.ok_json(
            &work_a,
            &[
                "task", "add", title, "--id", id, "--type", "story", "--tag", tag, "--as", "seed",
                "--json",
            ],
        );
    }
    estate.bind_self(
        "p-attach",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "geoyws/visible"),
            tag_scope("write", &estate.id_a, "geoyws/visible"),
        ],
    );
    estate.enforce("managed");
    // Each task-attach write: the denied id and the never-created id exit
    // with the same code and byte-identical stderr — the generic denial —
    // and the denied form must not succeed.
    let assert_write_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            denied.status.code() != Some(0),
            "{what} with a denied id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_write_identical(
        &note_on("s-attach-secret", "attach probe body"),
        &note_on("t-never-created", "attach probe body"),
        "note add",
    );
    assert_write_identical(
        &raise_on_task("s-attach-secret"),
        &raise_on_task("t-never-created"),
        "attention raise --task",
    );
    assert_write_identical(
        &sitrep_on_task("s-attach-secret"),
        &sitrep_on_task("t-never-created"),
        "sitrep post --task",
    );
    // Nothing was written. As the board owner the same caller re-reads every
    // row the refused writes could have touched: each task-scoped ledger
    // holds only its birth event, and no attention row or sitrep was created.
    estate.grant("p-attach", &owner_of(&estate.id_a));
    for task in ["s-attach-secret", "s-attach-visible"] {
        let ledger = estate.ok(&work_a, &["events", "--task", task, "--all", "--json"]);
        let events: Vec<Value> = serde_json::from_str(&ledger).unwrap();
        assert_eq!(
            events.len(),
            1,
            "a refused attach wrote to {task}: {ledger}"
        );
        assert!(
            ledger.contains("task_added"),
            "{task} lost its birth event: {ledger}"
        );
    }
    let raised = estate.ok_json(&work_a, &["attention", "list", "--all", "--json"]);
    assert!(
        raised.as_array().unwrap().is_empty(),
        "a refused attention raise was recorded: {raised}"
    );
    let sitreps = estate.ok_json(&work_a, &["sitrep", "list", "--all", "--json"]);
    assert!(
        sitreps.as_array().unwrap().is_empty(),
        "a refused sitrep post was recorded: {sitreps}"
    );
    // The control path works: as the owner the same caller attaches a note
    // to the secret task, so the denials above came from the tag and not a
    // broken write path.
    estate.ok(&work_a, &note_on("s-attach-secret", "control note"));
}

/// ACC-14, removed-task links (A23): removing a `secret` task keeps its
/// sitrep, handoff, untagged attention row and deployment attempt linked to
/// the orphaned id, and every one of them stays gated on the task's
/// last-known removal tags — on every listing, on the event tail, on search,
/// on lane-filtered sitrep listings (the stand-in for the retired
/// `/api/v1/lanes` surface), on the deployment listing, in a `watch --follow`
/// stream, and on `handoff retire`, `handoff accept`, `attention show`,
/// `deploy show` and `deploy finish` — while a removed id and a
/// never-created id answer byte-identically everywhere.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. The owner seeds rows on a `secret` task carrying a marker
/// no other row carries, plus genuinely taskless control rows (a lanewide
/// sitrep, a session handoff, a taskless deployment, an untagged attention
/// row) carrying a second marker, then removes the task; principal P holds
/// board read and write but no `secret` scope. P sees no trace of the
/// orphaned rows anywhere yet still reads every control row — the denials
/// come from the guard gating the orphaned id, not from missing rows —
/// while the owner still sees every row once enforcement is lifted.
#[test]
fn removed_task_links_stay_tag_gated_on_every_listing_search_and_lane() {
    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    let estate = ManagedEstate::new("acc14-removed-task-links");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the removed secret task",
            "--id",
            "t-gone-secret",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    // Genuinely taskless controls: board scope before and after the removal.
    estate.ok_json(
        &work_a,
        &[
            "sitrep",
            "post",
            "lanewide control qxkcontrol body",
            "--as",
            "seed",
            "--lane",
            "driver-1",
            "--repo",
            "/tmp/gone-links",
            "--branch",
            "main",
            "--head",
            HEAD,
            "--dirty",
            "clean",
            "--json",
        ],
    );
    let session_handoff = estate.ok_json(
        &work_a,
        &[
            "handoff",
            "create",
            "--as",
            "seed",
            "--to",
            "watcher",
            "--summary",
            "session control qxkcontrol summary",
            "--intent",
            "session control intent",
            "--next-action",
            "session control next",
            "--reason",
            "manual",
            "--repo",
            "/tmp/gone-links",
            "--branch",
            "main",
            "--head",
            HEAD,
            "--dirty",
            "clean",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let free_attention = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "free control qxkcontrol question",
            "--kind",
            "decision",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let free_attempt = estate.ok_json(
        &work_a,
        &[
            "deploy",
            "start",
            "--repo",
            "kanban",
            "--commit",
            HEAD,
            "--tier",
            "@_bdt",
            "--environment",
            "branch-dev-testing",
            "--host",
            "geoywsMBP",
            "--url",
            "http://localhost:9999",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The secret task's rows, each carrying the marker no other row carries.
    estate.ok_json(
        &work_a,
        &[
            "sitrep",
            "post",
            "gone secret sitrep zxqpayroll body",
            "--as",
            "seed",
            "--lane",
            "driver-1",
            "--repo",
            "/tmp/gone-links",
            "--branch",
            "main",
            "--head",
            HEAD,
            "--dirty",
            "clean",
            "--task",
            "t-gone-secret",
            "--json",
        ],
    );
    let token = estate.ok_json(
        &work_a,
        &["claim", "t-gone-secret", "--as", "seed", "--json"],
    )["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned();
    estate.ok_json(
        &work_a,
        &[
            "handoff",
            "create",
            "t-gone-secret",
            "--lease",
            &token,
            "--as",
            "seed",
            "--summary",
            "gone secret handoff zxqpayroll summary",
            "--intent",
            "gone secret handoff intent",
            "--next-action",
            "gone secret handoff next",
            "--repo",
            "/tmp/gone-links",
            "--branch",
            "main",
            "--head",
            HEAD,
            "--dirty",
            "clean",
            "--json",
        ],
    );
    let attention_id = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "gone secret attention zxqpayroll question",
            "--kind",
            "decision",
            "--as",
            "seed",
            "--task",
            "t-gone-secret",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let attempt_id = estate.ok_json(
        &work_a,
        &[
            "deploy",
            "start",
            "--repo",
            "kanban",
            "--commit",
            HEAD,
            "--tier",
            "@_bdt",
            "--environment",
            "branch-dev-testing",
            "--host",
            "geoywsMBP",
            "--url",
            "http://localhost:9999",
            "--task",
            "t-gone-secret",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let secret_handoff = estate
        .ok_json(
            &work_a,
            &["handoff", "list", "--task", "t-gone-secret", "--json"],
        )
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["status"] == "pending")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    estate.ok(
        &work_a,
        &["task", "remove", "t-gone-secret", "--as", "seed"],
    );
    estate.bind_self(
        "p-gone-rows",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");
    // 1. The `--task` listings succeed for the removed id and the unknown id
    //    alike and hand over nothing: the removed filter withholds exactly
    //    what the unknown filter cannot match.
    for (base, what) in [
        (&["sitrep", "list", "--task"][..], "sitrep list --task"),
        (&["handoff", "list", "--task"][..], "handoff list --task"),
        (
            &["attention", "list", "--task"][..],
            "attention list --task",
        ),
    ] {
        let mut removed_args = base.to_vec();
        removed_args.extend_from_slice(&["t-gone-secret", "--json"]);
        let mut unknown_args = base.to_vec();
        unknown_args.extend_from_slice(&["t-never-created", "--json"]);
        let removed = estate.run(&work_a, &removed_args);
        let unknown = estate.run(&work_a, &unknown_args);
        assert!(
            removed.status.success(),
            "{what} with a removed id should succeed: {}",
            String::from_utf8_lossy(&removed.stderr)
        );
        assert!(
            unknown.status.success(),
            "{what} with an unknown id should succeed: {}",
            String::from_utf8_lossy(&unknown.stderr)
        );
        assert_eq!(
            removed.stdout, unknown.stdout,
            "{what} output differs between a removed id and an unknown id"
        );
        let rows: Value = serde_json::from_slice(&removed.stdout).unwrap();
        assert_eq!(
            rows.as_array().unwrap().len(),
            0,
            "{what} with the removed id handed over a row the caller may not read: {rows}"
        );
    }
    // 2. The unfiltered listings, the event tail, search and the deployment
    //    listing carry no trace of the removed task's rows — neither their
    //    markers nor the orphaned id (event envelopes carry the id even where
    //    their payloads carry no row text).
    for (args, what) in [
        (&["sitrep", "list", "--json"][..], "sitrep list"),
        (&["handoff", "list", "--json"][..], "handoff list"),
        (&["attention", "list", "--json"][..], "attention list"),
        (&["deploy", "list", "--json"][..], "deploy list"),
        (&["events", "--json"][..], "events"),
        (&["search", "zxqpayroll", "--json"][..], "search"),
    ] {
        let listed = estate.run(&work_a, args);
        assert!(
            listed.status.success(),
            "{what} should succeed: {}",
            String::from_utf8_lossy(&listed.stderr)
        );
        let body = String::from_utf8_lossy(&listed.stdout).into_owned();
        // The search receipt echoes the query itself, so absence is pinned
        // on the rows' content markers and the orphaned id, never on the
        // query string.
        assert!(
            !body.contains("gone secret"),
            "{what} served a row on the removed denied task: {body}"
        );
        assert!(
            !body.contains("t-gone-secret"),
            "{what} served the removed task's id: {body}"
        );
    }
    // 3. The lane-filtered sitrep listing — the stand-in for the retired
    //    `/api/v1/lanes` surface — still serves the lanewide control while
    //    withholding the removed task's sitrep.
    let lane = estate.ok(&work_a, &["sitrep", "list", "--lane", "driver-1", "--json"]);
    assert!(
        lane.contains("lanewide control qxkcontrol body"),
        "the lane listing lost the board-scope control row: {lane}"
    );
    assert!(
        !lane.contains("gone secret") && !lane.contains("t-gone-secret"),
        "the lane listing served the removed task's sitrep: {lane}"
    );
    // 4. The live tail withholds the removed rows too. The stream replays
    //    from cursor 0, so history is offered to the filter; heartbeats prove
    //    the stream stayed alive while it stayed quiet about the removed task.
    {
        let mut stream = Stream::start(
            estate
                .command(&work_a)
                .args(["watch", "--follow", "--json"]),
        );
        thread::sleep(SETTLE);
        let quiet = stream.drain();
        // Event envelopes carry the task id even where their payloads carry
        // no row text, so both the markers and the orphaned id must be absent.
        assert!(
            !quiet.join("\n").contains("zxqpayroll"),
            "watch --follow handed over a removed task's rows:\n{}",
            quiet.join("\n")
        );
        assert!(
            !quiet.join("\n").contains("t-gone-secret"),
            "watch --follow served the removed task's id:\n{}",
            quiet.join("\n")
        );
        assert!(
            stream.running(),
            "the watch process exited instead of withholding the rows"
        );
        assert!(
            !quiet.is_empty(),
            "the stream produced nothing at all, so nothing was measured"
        );
    }
    // 5. Every by-id surface answers the removed row exactly like a
    //    never-created id — the same exit code and byte-identical stderr
    //    carrying the generic denial — and records nothing.
    let assert_by_id_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a removed id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a removed id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a removed id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_by_id_identical(
        &[
            "handoff",
            "retire",
            &secret_handoff,
            "--as",
            "seed",
            "--note",
            "gone probe",
        ],
        &[
            "handoff",
            "retire",
            "h-never-created",
            "--as",
            "seed",
            "--note",
            "gone probe",
        ],
        "handoff retire",
    );
    assert_by_id_identical(
        &["handoff", "accept", &secret_handoff, "--as", "seed"],
        &["handoff", "accept", "h-never-created", "--as", "seed"],
        "handoff accept",
    );
    assert_by_id_identical(
        &["attention", "show", &attention_id, "--json"],
        &["attention", "show", "a-never-raised", "--json"],
        "attention show",
    );
    assert_by_id_identical(
        &["deploy", "show", &attempt_id, "--json"],
        &["deploy", "show", "d-never-started", "--json"],
        "deploy show",
    );
    assert_by_id_identical(
        &[
            "deploy",
            "finish",
            &attempt_id,
            "--token",
            "token-bogus",
            "--result",
            "failed",
            "--phase",
            "build",
            "--receipt",
            "gone probe",
            "--as",
            "seed",
        ],
        &[
            "deploy",
            "finish",
            "d-never-started",
            "--token",
            "token-bogus",
            "--result",
            "failed",
            "--phase",
            "build",
            "--receipt",
            "gone probe",
            "--as",
            "seed",
        ],
        "deploy finish",
    );
    // 6. The negative control: every genuinely taskless row stays readable
    //    to the same caller, so the withholds above came from the guard
    //    gating the orphaned id and not from missing rows.
    for (args, marker, what) in [
        (
            &["sitrep", "list", "--json"][..],
            "lanewide control qxkcontrol body",
            "lanewide sitrep",
        ),
        (
            &["handoff", "list", "--json"][..],
            "session control qxkcontrol summary",
            "session handoff",
        ),
        (
            &["attention", "list", "--json"][..],
            "free control qxkcontrol question",
            "untagged attention row",
        ),
    ] {
        let visible = estate.ok(&work_a, args);
        assert!(
            visible.contains(marker),
            "the caller lost the {what} control row it may read: {visible}"
        );
    }
    let deployments = estate.ok(&work_a, &["deploy", "list", "--json"]);
    assert!(
        deployments.contains(&free_attempt),
        "the caller lost the taskless deployment it may read: {deployments}"
    );
    let free_shown = estate.ok_json(&work_a, &["attention", "show", &free_attention, "--json"]);
    assert_eq!(free_shown["id"], serde_json::json!(free_attention));
    // 7. The owner still sees every row, so the denials above came from the
    //    guard and not from missing rows — and the refused writes recorded
    //    nothing.
    estate.enforce("direct");
    // A `--task` filter naming a removed id cannot run even unenforced —
    // there is no row left to filter on — so the owner reads the unfiltered
    // listings, where the orphaned rows still carry their markers and id.
    for (args, marker, what) in [
        (
            &["sitrep", "list", "--json"][..],
            "gone secret sitrep zxqpayroll body",
            "sitrep list",
        ),
        (
            &["handoff", "list", "--json"][..],
            "gone secret handoff zxqpayroll summary",
            "handoff list",
        ),
        (
            &["attention", "list", "--json"][..],
            "gone secret attention zxqpayroll question",
            "attention list",
        ),
        (
            &["deploy", "list", "--json"][..],
            attempt_id.as_str(),
            "deploy list",
        ),
    ] {
        let owned = estate.ok(&work_a, args);
        assert!(
            owned.contains(marker),
            "the owner lost {what} on the removed task: {owned}"
        );
    }
    let owned_search = estate.ok(&work_a, &["search", "zxqpayroll", "--json"]);
    assert!(
        owned_search.contains("zxqpayroll"),
        "the owner lost the removed task's rows in search: {owned_search}"
    );
    let owned_attempt = estate.ok_json(&work_a, &["deploy", "show", &attempt_id, "--json"]);
    assert_eq!(
        owned_attempt["taskID"].as_str(),
        Some("t-gone-secret"),
        "the removed attempt lost its task link: {owned_attempt}"
    );
    let handoffs = estate.ok_json(&work_a, &["handoff", "list", "--json"]);
    assert!(
        handoffs
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == secret_handoff && row["status"] == "pending"),
        "a refused retire or accept moved a row: {handoffs}"
    );
    assert!(
        handoffs
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == session_handoff),
        "the session control handoff went missing: {handoffs}"
    );
}

/// ACC-14 id-reuse contract (A24), at the layer `process`: a removed task's
/// id is never reusable, and probing one answers exactly like probing a live
/// denied id.
///
/// The fixture tags two tasks `secret`, posts a sitrep carrying a unique
/// token on `t-sec`, and removes `t-sec`. The caller holds board read and
/// untagged write only — the `task add --id` precondition. Re-adding `t-sec`
/// and re-adding the still-live `t-sec-live` then answer with the same exit
/// code and byte-identical stderr carrying the one generic denial, and the
/// caller sees the sitrep on no listing. Granting the same principal `secret`
/// read shows the row survived: gating, not deletion.
#[test]
fn removed_task_ids_are_never_reused_and_probe_like_live_denied_ids() {
    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    let estate = ManagedEstate::new("acc14-id-reuse");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    for (id, title) in [
        ("t-sec", "the reuse secret task"),
        ("t-sec-live", "the live secret task"),
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
                "geoyws/secret",
                "--as",
                "seed",
                "--json",
            ],
        );
    }
    estate.ok_json(
        &work_a,
        &[
            "sitrep",
            "post",
            "quorumidreuse body on the removed secret task",
            "--as",
            "seed",
            "--lane",
            "driver-1",
            "--repo",
            "/tmp/id-reuse-probe",
            "--branch",
            "main",
            "--head",
            HEAD,
            "--dirty",
            "clean",
            "--task",
            "t-sec",
            "--json",
        ],
    );
    estate.ok(&work_a, &["task", "remove", "t-sec", "--as", "seed"]);

    estate.bind_self(
        "p-id-reuse",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");

    let removed = estate.run(
        &work_a,
        &[
            "task", "add", "probe", "--id", "t-sec", "--as", "probe", "--json",
        ],
    );
    let live = estate.run(
        &work_a,
        &[
            "task",
            "add",
            "probe",
            "--id",
            "t-sec-live",
            "--as",
            "probe",
            "--json",
        ],
    );
    for (output, what) in [
        (&removed, "re-adding a removed secret id"),
        (&live, "re-adding a live secret id"),
    ] {
        assert!(
            !output.status.success(),
            "{what} succeeded but must be refused\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    assert_eq!(
        removed.status.code(),
        live.status.code(),
        "re-adding a removed id exits differently from re-adding a live denied one"
    );
    assert_eq!(
        removed.stderr, live.stderr,
        "re-adding a removed id answers differently from re-adding a live denied one"
    );
    assert!(
        String::from_utf8_lossy(&removed.stderr).contains(DENIED),
        "re-add refusal is not the generic denial: {}",
        String::from_utf8_lossy(&removed.stderr)
    );

    for args in [
        vec!["sitrep", "list", "--json"],
        vec!["sitrep", "list", "--task", "t-sec", "--json"],
    ] {
        let listing = estate.ok(&work_a, &args);
        assert!(
            !listing.contains("quorumidreuse"),
            "`kanban {args:?}` handed over a removed task's sitrep after a refused re-add: {listing}"
        );
    }

    estate.grant(
        "p-id-reuse",
        &[
            tag_scope("read", &estate.id_a, "geoyws/secret"),
            tag_scope("write", &estate.id_a, "geoyws/secret"),
        ],
    );
    let owner = estate.ok(&work_a, &["sitrep", "list", "--task", "t-sec", "--json"]);
    assert!(
        owner.contains("quorumidreuse"),
        "the refused re-add destroyed the row it must only gate: {owner}"
    );
}

/// ACC-14 id-reuse contract (A24) where no guard can deny: re-adding a
/// removed id and re-adding a live id are plain, stable refusals — never the
/// raw `UNIQUE constraint failed`, never the generic denial.
#[test]
fn reusing_a_task_id_is_refused_with_a_plain_message_where_no_guard_can_deny() {
    let estate = ManagedEstate::new("acc14-id-reuse-direct");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &[
            "task", "add", "live", "--id", "t-live", "--as", "seed", "--json",
        ],
    );
    let live = estate.run(
        &work_a,
        &[
            "task", "add", "again", "--id", "t-live", "--as", "seed", "--json",
        ],
    );
    assert!(
        !live.status.success(),
        "re-adding a live id succeeded but must be refused"
    );
    let live_stderr = String::from_utf8_lossy(&live.stderr).into_owned();
    assert!(
        live_stderr.contains("task t-live already exists"),
        "re-adding a live id lost its plain refusal: {live_stderr}"
    );
    assert!(
        !live_stderr.contains(DENIED),
        "re-adding a live id answers a denial where no guard can deny: {live_stderr}"
    );
    assert!(
        !live_stderr.contains("UNIQUE constraint failed"),
        "re-adding a live id leaks the raw constraint: {live_stderr}"
    );

    estate.ok_json(
        &work_a,
        &[
            "task", "add", "gone", "--id", "t-gone", "--as", "seed", "--json",
        ],
    );
    estate.ok(&work_a, &["task", "remove", "t-gone", "--as", "seed"]);
    let gone = estate.run(
        &work_a,
        &[
            "task", "add", "again", "--id", "t-gone", "--as", "seed", "--json",
        ],
    );
    assert!(
        !gone.status.success(),
        "re-adding a removed id succeeded but must be refused"
    );
    let gone_stderr = String::from_utf8_lossy(&gone.stderr).into_owned();
    assert!(
        gone_stderr.contains("task t-gone was removed and its id cannot be reused"),
        "re-adding a removed id lost its plain refusal: {gone_stderr}"
    );
    assert!(
        !gone_stderr.contains(DENIED),
        "re-adding a removed id answers a denial where no guard can deny: {gone_stderr}"
    );
}

/// ACC-14 orphaned-handoff follow-through (A23), at the layer `process`: a
/// pending handoff on a removed `secret` task stays deniable to a tag-less
/// caller — `handoff accept` answers exactly like a never-created id — yet
/// the secret holder can still accept it as a lease-free acknowledgement
/// that the archive sweep files away, and `doctor` stays healthy throughout
/// because rows with a removal record are not orphans.
#[test]
fn an_orphaned_handoff_stays_deniable_yet_acceptable_and_archivable() {
    let estate = ManagedEstate::new("acc14-accept-orphan");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the doomed secret task",
            "--id",
            "t-orphan-secret",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    let token = estate.ok_json(
        &work_a,
        &["claim", "t-orphan-secret", "--as", "seed", "--json"],
    )["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let handoff_id = estate.ok_json(
        &work_a,
        &[
            "handoff",
            "create",
            "t-orphan-secret",
            "--lease",
            &token,
            "--as",
            "seed",
            "--summary",
            "orphaned acknowledgement summary",
            "--intent",
            "orphaned intent",
            "--next-action",
            "orphaned next",
            "--reason",
            "manual",
            "--repo",
            "/tmp/accept-orphan",
            "--branch",
            "main",
            "--head",
            "0123456789abcdef0123456789abcdef01234567",
            "--dirty",
            "clean",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    estate.ok(
        &work_a,
        &["task", "remove", "t-orphan-secret", "--as", "seed"],
    );
    estate.bind_self(
        "p-orphan",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");
    // The tag-less caller is denied the orphan exactly like a never-created
    // handoff, and sees no trace of it on any listing.
    let denied = estate.run(&work_a, &["handoff", "accept", &handoff_id, "--as", "seed"]);
    let unknown = estate.run(
        &work_a,
        &["handoff", "accept", "h-never-created", "--as", "seed"],
    );
    assert!(
        !denied.status.success(),
        "accepting a removed task's handoff succeeded but must be refused"
    );
    assert_eq!(
        denied.status.code(),
        unknown.status.code(),
        "the orphan accept exits differently from an unknown id"
    );
    assert_eq!(
        denied.stderr, unknown.stderr,
        "the orphan accept answers differently from an unknown id"
    );
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains(DENIED),
        "the orphan accept is not the generic denial: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let listed = estate.ok(&work_a, &["handoff", "list", "--json"]);
    assert!(
        !listed.contains(&handoff_id),
        "the caller reads an orphaned handoff it may not read: {listed}"
    );
    let filtered = estate.ok(
        &work_a,
        &["handoff", "list", "--task", "t-orphan-secret", "--json"],
    );
    assert_eq!(
        filtered.trim(),
        "[]",
        "the removed-task filter handed over a row the caller may not read: {filtered}"
    );
    // The secret holder accepts the orphan as an acknowledgement: no lease
    // is minted on the missing task.
    estate.grant(
        "p-orphan",
        &[
            tag_scope("read", &estate.id_a, "geoyws/secret"),
            tag_scope("write", &estate.id_a, "geoyws/secret"),
        ],
    );
    let accepted = estate.ok_json(
        &work_a,
        &["handoff", "accept", &handoff_id, "--as", "seed", "--json"],
    );
    assert_eq!(accepted["handoff"]["status"], "accepted");
    assert_eq!(
        accepted["handoff"]["acceptedBy"],
        serde_json::json!("seed"),
        "the acknowledgement lost its acceptor: {accepted}"
    );
    assert_eq!(
        accepted["claim"],
        Value::Null,
        "accepting a removed task minted a lease on a missing task: {accepted}"
    );
    let claims: i64 = Connection::open(&estate.board_a)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM task_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(claims, 0, "accepting a removed task left a lease behind");
    // Once old, the archive sweep files the orphaned acknowledgement away.
    Connection::open(&estate.board_a)
        .unwrap()
        .execute(
            "UPDATE handoffs SET created_at=1,accepted_at=1 WHERE id=?",
            [&handoff_id],
        )
        .unwrap();
    let swept = estate.ok_json(
        &work_a,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "seed",
            "--json",
        ],
    );
    assert_eq!(
        swept["handoffs"], 1,
        "the sweep left the orphan pending: {swept}"
    );
    assert_eq!(
        estate.ok_json(&work_a, &["handoff", "list", "--json"]),
        serde_json::json!([])
    );
    let all = estate.ok_json(&work_a, &["handoff", "list", "--all", "--json"]);
    assert_eq!(all[0]["id"], serde_json::json!(handoff_id));
    assert_eq!(all[0]["archived"], true);
    // And the board is still consistent: rows with a removal record behind
    // them are linked rows, not orphans, so doctor stays healthy.
    estate.enforce("direct");
    let doctor = estate.ok_json(&work_a, &["doctor", "--json"]);
    assert_eq!(doctor["healthy"], true, "{doctor}");
    assert_eq!(
        doctor["projects"][0]["orphanedTaskLinks"],
        serde_json::json!([])
    );
}

/// George a-daa231b3: a child ID in a readable story's advance refusal is
/// not secret, but the child row's content remains tag-gated. Exercise the
/// compiled CLI across process boundaries with one visible and one hidden
/// open child, then prove the refused advance left the story unchanged.
#[test]
fn story_advance_names_tag_denied_child_id_without_exposing_its_row() {
    let estate = ManagedEstate::new("story-hidden-child-id");
    let work = estate.work_a.clone();
    for tag in ["geoyws/visible", "geoyws/secret"] {
        estate.ok_json(&work, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    estate.ok_json(
        &work,
        &[
            "task",
            "add",
            "visible story",
            "--id",
            "s-visible",
            "--type",
            "story",
            "--tag",
            "geoyws/visible",
            "--as",
            "seed",
            "--json",
        ],
    );
    for (id, title, tag) in [
        ("t-visible-child", "visible child", "geoyws/visible"),
        ("t-secret-child", "secret payload headline", "geoyws/secret"),
    ] {
        estate.ok_json(
            &work,
            &[
                "task",
                "add",
                title,
                "--id",
                id,
                "--parent",
                "s-visible",
                "--type",
                "task",
                "--lane",
                "driver",
                "--tag",
                tag,
                "--as",
                "seed",
                "--json",
            ],
        );
    }
    // planning -> ready -> in-progress; the next step is gated by open children.
    for _ in 0..2 {
        estate.ok_json(
            &work,
            &["story", "advance", "s-visible", "--as", "seed", "--json"],
        );
    }
    estate.bind_self(
        "visible-reader",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "geoyws/visible"),
            tag_scope("write", &estate.id_a, "geoyws/visible"),
        ],
    );
    estate.enforce("managed");
    assert_eq!(
        estate.ok_json(&work, &["task", "show", "t-visible-child", "--json"])["id"],
        "t-visible-child"
    );
    estate.denied(&work, &["task", "show", "t-secret-child", "--json"]);

    let refused = estate.run(
        &work,
        &[
            "story",
            "advance",
            "s-visible",
            "--as",
            "visible-reader",
            "--json",
        ],
    );
    assert!(!refused.status.success(), "the open children were ignored");
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("non-test-lane tasks still open:"),
        "{message}"
    );
    assert!(message.contains("t-visible-child"), "{message}");
    assert!(message.contains("t-secret-child"), "{message}");
    assert!(!message.contains("secret payload headline"), "{message}");
    assert!(!message.contains("geoyws/secret"), "{message}");
    assert_eq!(
        estate.ok_json(&work, &["task", "show", "s-visible", "--json"])["metadata"]["workflowStatus"],
        "in-progress",
        "the refused advance changed the story"
    );
}

/// ACC-14, dependency replacement keeps the edges the caller cannot read — and
/// the ones it can read but not write: the owner gates `t-visible` on
/// `t-secret` (unreadable to the caller) and on `t-ops` (readable but
/// read-only), and a managed caller holding board read and write (plus
/// `visible` at both capabilities and `ops` read only) replaces the list with
/// `t-other`. The write succeeds — refusing would confirm a hidden edge
/// exists — but both kept edges survive: the caller's own listing still shows
/// `t-ops` beside `t-other` while withholding `t-secret`, the gate still names
/// both kept prerequisites, and the claim is still refused while either is
/// open. Re-listing the read-only edge needs no new authority, clearing the
/// list keeps both, and the owner can still drop any edge. Re-listing the
/// hidden edge is refused byte-identically to naming an id that was never an
/// edge — the re-list skip applies only to edges the caller can read — with
/// nothing written either way.
#[test]
fn dependency_replacement_keeps_a_tag_denied_prerequisite() {
    let estate = ManagedEstate::new("acc14-dep-replace");
    let work_a = estate.work_a.clone();
    for tag in ["geoyws/visible", "geoyws/secret", "geoyws/ops"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    for (id, title, tag) in [
        ("t-secret", "secret prerequisite", "geoyws/secret"),
        ("t-ops", "ops prerequisite", "geoyws/ops"),
        ("t-other", "other prerequisite", "geoyws/visible"),
    ] {
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
            "--tag",
            "geoyws/visible",
            "--depends-on",
            "t-secret",
            "--depends-on",
            "t-ops",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.bind_self(
        "p-visible-only",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "geoyws/visible"),
            tag_scope("write", &estate.id_a, "geoyws/visible"),
            tag_scope("read", &estate.id_a, "geoyws/ops"),
        ],
    );
    estate.enforce("managed");
    // The replacement succeeds: refusing would tell the caller an edge it
    // cannot see exists. The receipt carries the row, never the edges, so it
    // names no prerequisite either way.
    let receipt = estate.ok(
        &work_a,
        &[
            "task",
            "update",
            "t-visible",
            "--depends-on",
            "t-other",
            "--as",
            "p",
        ],
    );
    assert!(
        !receipt.contains("t-secret"),
        "the update receipt revealed the hidden edge: {receipt}"
    );
    // The caller's own view still shows the edge it named beside the
    // readable kept edge, and still withholds the hidden one.
    let shown = estate.ok_json(&work_a, &["task", "show", "t-visible", "--json"]);
    assert_eq!(
        shown["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["t-ops".to_owned(), "t-other".to_owned()],
        "the listing lost the read-only edge or handed over the hidden one: {shown}"
    );
    // Both gates still hold: each id and status stays, the hidden title stays
    // blanked while the readable one stays visible.
    assert_eq!(
        shown["blockingGates"]
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
            .collect::<Vec<_>>(),
        vec![
            (
                "t-ops".to_owned(),
                "todo".to_owned(),
                serde_json::json!("ops prerequisite"),
            ),
            (
                "t-other".to_owned(),
                "todo".to_owned(),
                serde_json::json!("other prerequisite"),
            ),
            ("t-secret".to_owned(), "todo".to_owned(), Value::Null),
        ],
        "the gates lost a kept edge: {shown}"
    );
    // Re-listing the hidden edge answers exactly like naming an id that was
    // never an edge: the re-list skip applies only to edges the caller can
    // read, so guessing the hidden prerequisite confirms nothing. Same exit
    // code, byte-identical stderr carrying the generic denial, nothing
    let hidden = estate.run(
        &work_a,
        &[
            "task",
            "update",
            "t-visible",
            "--depends-on",
            "t-secret",
            "--as",
            "p",
        ],
    );
    let unknown = estate.run(
        &work_a,
        &[
            "task",
            "update",
            "t-visible",
            "--depends-on",
            "t-nonexistent",
            "--as",
            "p",
        ],
    );
    assert!(
        !hidden.status.success(),
        "re-listing the hidden edge succeeded but must be refused"
    );
    assert_eq!(
        hidden.status.code(),
        unknown.status.code(),
        "re-listing a hidden edge exits differently from naming an unknown id"
    );
    assert_eq!(
        hidden.stderr, unknown.stderr,
        "re-listing a hidden edge reads differently from naming an unknown id"
    );
    assert!(
        String::from_utf8_lossy(&hidden.stderr).contains(DENIED),
        "the hidden-edge probe did not answer the non-enumerating denial: {}",
        String::from_utf8_lossy(&hidden.stderr)
    );
    let reread = estate.ok_json(&work_a, &["task", "show", "t-visible", "--json"]);
    assert_eq!(
        reread, shown,
        "a refused edge probe wrote to the row: {reread}"
    );
    // Re-listing the read-only edge alongside the new one is accepted:
    // a readable edge already on the row needs no new authority.
    estate.ok(
        &work_a,
        &[
            "task",
            "update",
            "t-visible",
            "--depends-on",
            "t-ops",
            "--depends-on",
            "t-other",
            "--as",
            "p",
        ],
    );
    // Clearing the list keeps both the hidden and the read-only edges: only
    // the writable `t-other` edge is dropped.
    estate.ok(
        &work_a,
        &[
            "task",
            "update",
            "t-visible",
            "--clear-dependencies",
            "--as",
            "p",
        ],
    );
    let cleared = estate.ok_json(&work_a, &["task", "show", "t-visible", "--json"]);
    assert_eq!(
        cleared["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["t-ops".to_owned()],
        "clearing dropped a kept edge or handed over the hidden one: {cleared}"
    );
    let mut gate_ids = cleared["blockingGates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gate| gate["prerequisiteID"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    gate_ids.sort();
    assert_eq!(
        gate_ids,
        vec!["t-ops".to_owned(), "t-secret".to_owned()],
        "clearing lost a kept gate: {cleared}"
    );
    // So the claim is still refused while a kept prerequisite is open — and
    // the refusal still names the hidden edge.
    let refused = estate.run(&work_a, &["claim", "t-visible", "--as", "p"]);
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        !refused.status.success(),
        "the kept gates let a claim through: {stderr}"
    );
    assert!(
        stderr.contains("t-secret is todo"),
        "the claim refusal no longer names the hidden edge: {stderr}"
    );
    // The owner sees every surviving edge, so the assertions above came from
    // the tag scopes and not from missing rows — and the owner can still drop
    // any edge, kept or not.
    estate.grant("p-visible-only", &owner_of(&estate.id_a));
    let owned = estate.ok_json(&work_a, &["task", "show", "t-visible", "--json"]);
    let mut owned_ids = owned["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    owned_ids.sort();
    assert_eq!(
        owned_ids,
        vec!["t-ops".to_owned(), "t-secret".to_owned()],
        "the owner lost an edge: {owned}"
    );
    estate.ok(
        &work_a,
        &[
            "task",
            "update",
            "t-visible",
            "--depends-on",
            "t-other",
            "--as",
            "owner",
        ],
    );
    let dropped = estate.ok_json(&work_a, &["task", "show", "t-visible", "--json"]);
    assert_eq!(
        dropped["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["t-other".to_owned()],
        "the owner could not drop the kept edges: {dropped}"
    );
}

/// ACC-14, task-attach writes: a managed caller holding board read and write
/// (plus `visible` at both capabilities) but no `secret` scope must not
/// attach rows to a `secret` task, and a denied task id must answer
/// byte-identically to a never-created id on every such write — with nothing
/// recorded.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. Covers `note`, `attention raise --task`, `sitrep post
/// --task`, `checkpoint`, `handoff create` with a task, `task add --parent` /
/// `--depends-on`, `task update --parent` / `--depends-on`, `subscription
/// add --subject` / `--relation`, and `deploy start --task`.
#[test]
fn task_attach_writes_refuse_a_tag_denied_task_like_an_unknown_id() {
    fn sitrep_on_task(task: &str) -> Vec<&str> {
        vec![
            "sitrep",
            "post",
            "attach probe body",
            "--as",
            "probe",
            "--lane",
            "driver-1",
            "--repo",
            "/tmp/attach-probe",
            "--branch",
            "main",
            "--head",
            SEED_HEAD,
            "--dirty",
            "clean",
            "--task",
            task,
            "--json",
        ]
    }
    fn raise_on_task(task: &str) -> Vec<&str> {
        vec![
            "attention",
            "raise",
            "attach probe body",
            "--kind",
            "decision",
            "--as",
            "probe",
            "--task",
            task,
            "--json",
        ]
    }
    fn checkpoint_on_task(task: &str) -> Vec<&str> {
        vec![
            "checkpoint",
            task,
            "--lease",
            "lease-bogus",
            "--as",
            "probe",
            "--summary",
            "attach probe summary",
            "--intent",
            "attach probe intent",
            "--next-action",
            "attach probe next",
            "--repo",
            "/tmp/attach-probe",
            "--branch",
            "main",
            "--head",
            SEED_HEAD,
            "--dirty",
            "clean",
            "--json",
        ]
    }
    fn handoff_on_task(task: &str) -> Vec<&str> {
        vec![
            "handoff",
            "create",
            task,
            "--lease",
            "lease-bogus",
            "--as",
            "probe",
            "--summary",
            "attach probe summary",
            "--intent",
            "attach probe intent",
            "--next-action",
            "attach probe next",
            "--repo",
            "/tmp/attach-probe",
            "--branch",
            "main",
            "--head",
            SEED_HEAD,
            "--dirty",
            "clean",
            "--json",
        ]
    }
    fn subscription_base<'a>() -> Vec<&'a str> {
        vec![
            "subscription",
            "add",
            "--consumer",
            "probe-consumer",
            "--action",
            "probe-action",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "probe",
        ]
    }
    fn subscription_on_subject(task: &str) -> Vec<&str> {
        let mut args = subscription_base();
        args.extend_from_slice(&["--subject", task, "--json"]);
        args
    }
    fn subscription_on_relation(target: &str) -> Vec<&str> {
        let mut args = subscription_base();
        args.extend_from_slice(&["--relation", target, "--json"]);
        args
    }
    fn deploy_on_task(task: &str) -> Vec<&str> {
        vec![
            "deploy",
            "start",
            "--repo",
            "kanban",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "--tier",
            "@_uat",
            "--environment",
            "probe-env",
            "--host",
            "probe-host",
            "--url",
            "http://localhost:9999",
            "--as",
            "probe",
            "--task",
            task,
            "--json",
        ]
    }
    let estate = ManagedEstate::new("acc14-task-attach");
    let work_a = estate.work_a.clone();
    for tag in ["geoyws/visible", "geoyws/secret"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    // Unmanaged first: where no guard can deny, an unknown id keeps its plain
    // message on the paths that previously answered it.
    for (args, what) in [
        (note_on("t-never-created", "unmanaged probe"), "note add"),
        (sitrep_on_task("t-never-created"), "sitrep post --task"),
    ] {
        let plain = estate.run(&work_a, &args);
        assert!(!plain.status.success(), "{what} should fail unmanaged");
        let plain_stderr = String::from_utf8_lossy(&plain.stderr).into_owned();
        assert!(
            plain_stderr.contains("task t-never-created not found"),
            "{what} lost its plain unmanaged message: {plain_stderr}"
        );
        assert!(
            !plain_stderr.contains(DENIED),
            "{what} answers a denial where no guard can deny: {plain_stderr}"
        );
    }
    for (id, title, tag) in [
        ("s-attach-secret", "the attach secret task", "geoyws/secret"),
        (
            "s-attach-visible",
            "the attach visible task",
            "geoyws/visible",
        ),
    ] {
        estate.ok_json(
            &work_a,
            &[
                "task", "add", title, "--id", id, "--type", "story", "--tag", tag, "--as", "seed",
                "--json",
            ],
        );
    }
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the attach child",
            "--id",
            "t-attach-child",
            "--parent",
            "s-attach-visible",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.bind_self(
        "p-attach",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "geoyws/visible"),
            tag_scope("write", &estate.id_a, "geoyws/visible"),
        ],
    );
    estate.enforce("managed");
    // Each task-attach write: the denied id and the never-created id exit
    // with the same code and byte-identical stderr — the generic denial —
    // and the denied form must not succeed.
    let assert_write_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a denied id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_write_identical(
        &note_on("s-attach-secret", "attach probe body"),
        &note_on("t-never-created", "attach probe body"),
        "note add",
    );
    assert_write_identical(
        &raise_on_task("s-attach-secret"),
        &raise_on_task("t-never-created"),
        "attention raise --task",
    );
    assert_write_identical(
        &sitrep_on_task("s-attach-secret"),
        &sitrep_on_task("t-never-created"),
        "sitrep post --task",
    );
    assert_write_identical(
        &checkpoint_on_task("s-attach-secret"),
        &checkpoint_on_task("t-never-created"),
        "checkpoint",
    );
    assert_write_identical(
        &handoff_on_task("s-attach-secret"),
        &handoff_on_task("t-never-created"),
        "handoff create --task",
    );
    assert_write_identical(
        &[
            "task",
            "add",
            "attach child probe",
            "--id",
            "t-attach-child-probe",
            "--parent",
            "s-attach-secret",
            "--as",
            "probe",
            "--json",
        ],
        &[
            "task",
            "add",
            "attach child probe",
            "--id",
            "t-attach-child-probe",
            "--parent",
            "t-never-created",
            "--as",
            "probe",
            "--json",
        ],
        "task add --parent",
    );
    assert_write_identical(
        &[
            "task",
            "add",
            "attach dep probe",
            "--id",
            "t-attach-dep-probe",
            "--depends-on",
            "s-attach-secret",
            "--as",
            "probe",
            "--json",
        ],
        &[
            "task",
            "add",
            "attach dep probe",
            "--id",
            "t-attach-dep-probe",
            "--depends-on",
            "t-never-created",
            "--as",
            "probe",
            "--json",
        ],
        "task add --depends-on",
    );
    assert_write_identical(
        &[
            "task",
            "update",
            "t-attach-child",
            "--parent",
            "s-attach-secret",
            "--as",
            "probe",
        ],
        &[
            "task",
            "update",
            "t-attach-child",
            "--parent",
            "t-never-created",
            "--as",
            "probe",
        ],
        "task update --parent",
    );
    assert_write_identical(
        &[
            "task",
            "update",
            "t-attach-child",
            "--depends-on",
            "s-attach-secret",
            "--as",
            "probe",
        ],
        &[
            "task",
            "update",
            "t-attach-child",
            "--depends-on",
            "t-never-created",
            "--as",
            "probe",
        ],
        "task update --depends-on",
    );
    assert_write_identical(
        &subscription_on_subject("task:s-attach-secret"),
        &subscription_on_subject("task:t-never-created"),
        "subscription add --subject",
    );
    assert_write_identical(
        &subscription_on_relation("depends-on:s-attach-secret"),
        &subscription_on_relation("depends-on:t-never-created"),
        "subscription add --relation",
    );
    assert_write_identical(
        &deploy_on_task("s-attach-secret"),
        &deploy_on_task("t-never-created"),
        "deploy start --task",
    );
    // Nothing was written. As the board owner the same caller re-reads every
    // row the refused writes could have touched: each task-scoped ledger
    // holds only its birth event, the phantom children do not exist, and no
    // subscription was created.
    estate.grant("p-attach", &owner_of(&estate.id_a));
    for task in ["s-attach-secret", "s-attach-visible", "t-attach-child"] {
        let ledger = estate.ok(&work_a, &["events", "--task", task, "--all", "--json"]);
        let events: Vec<Value> = serde_json::from_str(&ledger).unwrap();
        assert_eq!(
            events.len(),
            1,
            "a refused attach wrote to {task}: {ledger}"
        );
        assert!(
            ledger.contains("task_added"),
            "{task} lost its birth event: {ledger}"
        );
        for marker in ["probe-host", "t-attach-child-probe", "t-attach-dep-probe"] {
            assert!(
                !ledger.contains(marker),
                "a refused attach left {marker} on {task}: {ledger}"
            );
        }
    }
    for phantom in ["t-attach-child-probe", "t-attach-dep-probe"] {
        let shown = estate.run(&work_a, &["task", "show", phantom]);
        assert!(
            !shown.status.success(),
            "a refused relation add created {phantom}"
        );
    }
    let subscriptions = estate.ok_json(&work_a, &["subscription", "list", "--all", "--json"]);
    assert!(
        subscriptions.as_array().unwrap().is_empty(),
        "a refused subscription was created: {subscriptions}"
    );
    // The control path works: as the owner the same caller attaches a note
    // to the secret task, so the denials above came from the tag and not a
    // broken write path.
    estate.ok(&work_a, &note_on("s-attach-secret", "control note"));
}

/// Paged listings are bound by readable rows, not raw rows (ACC-14).
///
/// Under enforcement the SQL `LIMIT` used to run before the tag filter, so a
/// page came back short when denied rows fell in range — and the callers'
/// `+1` truncation probes with it: `events --task t-visible --limit 1`
/// answered `[]` when the newest events concerned a denied attention row,
/// and `deploy list --limit 1` came back empty while readable attempts
/// waited behind a denied one. A managed reader holding board read but no
/// `secret` tag now gets a full page of visible rows with a true probe,
/// while the owner still sees every row.
#[test]
fn managed_pages_fill_past_denied_rows_with_a_true_truncation_probe() {
    let estate = ManagedEstate::new("page-fill");
    let work_a = estate.work_a.clone();

    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/visible", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "visible row",
            "--id",
            "t-visible",
            "--tag",
            "geoyws/visible",
            "--as",
            "seed",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "secret row",
            "--id",
            "t-secret",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
        ],
    );

    // Fifty-five visible events about the readable task, beneath the denied
    // rows the CLI writes below.
    // Inserted directly: the filter reads live task tags, so plain
    // task-scoped rows with no frozen snapshot are visible to anyone who can
    // read the task, exactly like the CLI-written ones below them. The
    // placeholder hashes keep the audit head initialized for the CLI writes
    // that follow; chain verification is not what this test measures (the
    // direct-estate `UPDATE events` precedent in `tests/e2e.rs` mutates rows
    // the same way).
    let board = Connection::open(&estate.board_a).unwrap();
    for index in 0..55 {
        board
            .execute(
                "INSERT INTO events(task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) \
                 VALUES('t-visible','task_updated','seed','{}',?1,0,'0000000000000000000000000000000000000000000000000000000000000000',?2)",
                rusqlite::params![
                    2_000_000_i64 + index,
                    format!("{:064x}", 1_000_000 + index),
                ],
            )
            .unwrap();
    }
    let newest_visible: i64 = board
        .query_row("SELECT max(seq) FROM events", [], |row| row.get(0))
        .unwrap();
    for index in 0..101 {
        board
            .execute(
                "INSERT INTO deployments(id,task_id,repo,identity_mode,commit_sha,tier,environment,\
                 host,url,status,actor,capability_token,created_at,updated_at,completed_at) \
                 VALUES(?1,'t-visible','kanban','git',\
                 '0123456789abcdef0123456789abcdef01234567','@_bdt','branch-dev-testing',\
                 'geoywsMBP','http://localhost:9999','started','seed',?2,?3,?3,NULL)",
                rusqlite::params![
                    format!("d-s{index:03}"),
                    format!("token-s{index:03}"),
                    3_000_000_i64 + index,
                ],
            )
            .unwrap();
    }
    for index in 0..31 {
        board
            .execute(
                "INSERT INTO deployments(id,task_id,repo,identity_mode,commit_sha,tier,environment,\
                 host,url,status,actor,capability_token,created_at,updated_at,completed_at) \
                 VALUES(?1,'t-visible','kanban','git',\
                 '0123456789abcdef0123456789abcdef01234567','@_bdt','branch-dev-testing',\
                 'geoywsMBP','http://localhost:9999','failed','seed',?2,?3,?3,?3)",
                rusqlite::params![
                    format!("d-f{index:02}"),
                    format!("token-f{index:02}"),
                    4_000_000_i64 + index,
                ],
            )
            .unwrap();
    }
    // The denied failure is newer than every visible one, so it sits in
    // range of the failures page.
    board
        .execute(
            "INSERT INTO deployments(id,task_id,repo,identity_mode,commit_sha,tier,environment,\
             host,url,status,actor,capability_token,created_at,updated_at,completed_at) \
             VALUES('d-fsec','t-secret','kanban','git',\
             '0123456789abcdef0123456789abcdef01234567','@_bdt','branch-dev-testing',\
             'geoywsMBP','http://localhost:9999','failed','seed','token-fsec',4_000_031,4_000_031,\
             4_000_031)",
            [],
        )
        .unwrap();
    // Three hundred denied events about the `secret` task, newer than every
    // visible one: plain task-scoped rows carry their task's live tags, so a
    // caller without `secret` sees none of them. Together with the two
    // denied attention envelopes raised below, the visible history sits more
    // than one 256-row scan chunk down — every page that fills past them
    // crosses a chunk boundary.
    for index in 0..300 {
        board
            .execute(
                "INSERT INTO events(task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) \
                 VALUES('t-secret','task_updated','seed','{}',?1,0,'0000000000000000000000000000000000000000000000000000000000000000',?2)",
                rusqlite::params![
                    2_100_000_i64 + index,
                    format!("{:064x}", 2_000_000 + index),
                ],
            )
            .unwrap();
    }
    drop(board);

    // Two newest events, both denied: a `secret` attention row raised on the
    // readable task, then settled. Each envelope unions the row's live tags,
    // so a caller without `secret` sees neither — but the task-scoped read
    // must still answer the visible history beneath them.
    let secret_attention = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "the secret question",
            "--as",
            "seed",
            "--kind",
            "decision",
            "--task",
            "t-visible",
            "--tag",
            "geoyws/secret",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "resolve",
            &secret_attention,
            "--as",
            "seed",
            "--choice",
            "approve",
        ],
    );
    // The denied start is newer than every visible one, so it sits in range
    // of the starts page.
    let denied_start = estate.ok_json(
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
            "t-secret",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The owner's view, taken while enforcement is still direct: the newest
    // rows are the denied ones, on every listing.
    let owner_task_page = estate.ok(
        &work_a,
        &["events", "--task", "t-visible", "--limit", "1", "--json"],
    );
    assert!(
        owner_task_page.contains(&secret_attention),
        "the owner lost the newest task event before enforcement: {owner_task_page}"
    );
    let owner_starts = estate.ok(
        &work_a,
        &[
            "deploy", "list", "--status", "started", "--limit", "1", "--all", "--json",
        ],
    );
    assert!(
        owner_starts.contains(&denied_start),
        "the owner lost the newest start before enforcement: {owner_starts}"
    );
    let owner_failures = estate.ok(
        &work_a,
        &[
            "deploy", "list", "--status", "failed", "--limit", "1", "--all", "--json",
        ],
    );
    assert!(
        owner_failures.contains("d-fsec"),
        "the owner lost the newest failure before enforcement: {owner_failures}"
    );

    estate.bind_self(
        "p-page",
        &[
            board_scope("read", &estate.id_a),
            tag_scope("read", &estate.id_a, "geoyws/visible"),
        ],
    );
    estate.enforce("managed");

    // The CLI pages: full, visible, and honest about the remainder. Before
    // the fix each of these came back short — the task page empty — because
    // the SQL bound ran ahead of the tag test.
    let task_page = estate.run(
        &work_a,
        &["events", "--task", "t-visible", "--limit", "1", "--json"],
    );
    assert!(
        task_page.status.success(),
        "task page failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&task_page.stdout),
        String::from_utf8_lossy(&task_page.stderr)
    );
    let task_stdout = String::from_utf8_lossy(&task_page.stdout).into_owned();
    let task_rows: Vec<Value> = serde_json::from_str(&task_stdout).unwrap();
    assert_eq!(
        task_rows.len(),
        1,
        "the task page came back short behind denied rows: {task_stdout}"
    );
    assert_eq!(
        task_rows[0]["seq"].as_i64().unwrap(),
        newest_visible,
        "the task page skipped the newest visible event: {task_stdout}"
    );
    assert!(
        !task_stdout.contains(&secret_attention) && !task_stdout.contains("secret"),
        "the task page carried the hidden row: {task_stdout}"
    );
    assert!(
        String::from_utf8_lossy(&task_page.stderr).contains("showing 1 of more than 1"),
        "the task page hid its truncation: {task_stdout}"
    );
    let board_page = estate.run(&work_a, &["events", "--limit", "1", "--all", "--json"]);
    assert!(board_page.status.success());
    let board_stdout = String::from_utf8_lossy(&board_page.stdout).into_owned();
    let board_rows: Vec<Value> = serde_json::from_str(&board_stdout).unwrap();
    assert_eq!(
        board_rows.len(),
        1,
        "the board page came back short behind denied rows: {board_stdout}"
    );
    assert_eq!(
        board_rows[0]["seq"].as_i64().unwrap(),
        newest_visible,
        "the board page skipped the newest visible event: {board_stdout}"
    );
    assert!(
        String::from_utf8_lossy(&board_page.stderr).contains("showing 1 of more than 1"),
        "the board page hid its truncation: {board_stdout}"
    );
    // A wider board page across the same stretch: five visible rows, no
    // duplicates across the chunk boundary, still newest-first from the
    // newest visible event, and an honest truncation flag. Three hundred
    // denied rows sit ahead of it, so filling this page crosses two chunk
    // boundaries.
    let wide_page = estate.run(&work_a, &["events", "--limit", "5", "--all", "--json"]);
    assert!(wide_page.status.success());
    let wide_stdout = String::from_utf8_lossy(&wide_page.stdout).into_owned();
    let wide_rows: Vec<Value> = serde_json::from_str(&wide_stdout).unwrap();
    let wide_seqs: Vec<i64> = wide_rows
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(
        wide_seqs,
        vec![
            newest_visible,
            newest_visible - 1,
            newest_visible - 2,
            newest_visible - 3,
            newest_visible - 4,
        ],
        "the wide board page duplicated or skipped visible rows across chunks: {wide_stdout}"
    );
    assert!(
        !wide_stdout.contains(&secret_attention) && !wide_stdout.contains("secret"),
        "the wide board page carried the hidden row: {wide_stdout}"
    );
    assert!(
        String::from_utf8_lossy(&wide_page.stderr).contains("showing 5 of more than 5"),
        "the wide board page hid its truncation: {wide_stdout}"
    );
    let starts = estate.ok(
        &work_a,
        &[
            "deploy", "list", "--status", "started", "--limit", "1", "--all", "--json",
        ],
    );
    let starts_rows: Vec<Value> = serde_json::from_str(&starts).unwrap();
    assert_eq!(
        starts_rows
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["d-s100"],
        "the starts page came back short behind a denied attempt: {starts}"
    );
    let failures = estate.ok(
        &work_a,
        &[
            "deploy", "list", "--status", "failed", "--limit", "1", "--all", "--json",
        ],
    );
    let failures_rows: Vec<Value> = serde_json::from_str(&failures).unwrap();
    assert_eq!(
        failures_rows
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["d-f30"],
        "the failures page came back short behind a denied attempt: {failures}"
    );

    // Granting `secret` makes exactly the withheld rows appear, which is
    // what proves the tag was the reason — and the owner's newest-first
    // order is unchanged.
    estate.enforce("direct");
    estate.grant(
        "p-page",
        &[
            tag_scope("read", &estate.id_a, "geoyws/secret"),
            tag_scope("write", &estate.id_a, "geoyws/secret"),
        ],
    );
    estate.enforce("managed");
    let granted_task_page = estate.ok(
        &work_a,
        &["events", "--task", "t-visible", "--limit", "1", "--json"],
    );
    assert!(
        granted_task_page.contains(&secret_attention),
        "granting the tag did not restore the newest task event: {granted_task_page}"
    );
    let granted_starts = estate.ok(
        &work_a,
        &[
            "deploy", "list", "--status", "started", "--limit", "1", "--all", "--json",
        ],
    );
    assert!(
        granted_starts.contains(&denied_start),
        "granting the tag did not restore the newest start: {granted_starts}"
    );
    let granted_failures = estate.ok(
        &work_a,
        &[
            "deploy", "list", "--status", "failed", "--limit", "1", "--all", "--json",
        ],
    );
    assert!(
        granted_failures.contains("d-fsec"),
        "granting the tag did not restore the newest failure: {granted_failures}"
    );
}

/// ACC-14 on every CLI by-id surface: a tag-denied id and a never-created id
/// answer byte-identically on `attention show`, `attention resolve`,
/// `attention reopen`, `attention check` and `task show`.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. The caller holds board read AND write plus `tag:visible` at both
/// capabilities on Alpha — write as well, so every denial below is the ROW's
/// and not the guard's blanket refusal of a principal who can write nothing.
///
/// The fixture raises one `secret` decision row (open, no check), one
/// `secret` checked row with a known key, one `visible` checked row (the
/// control that proves a past-guard refusal still names its reason), and one
/// `secret` task. The unknown ids `att-never-raised` and `t-never-created`
/// were never raised or added. Refusals past the guard — no check, already
/// answered, undeclared key, not the raiser — keep the store's own sentence,
/// and the unmanaged estate keeps its plain `not found` message.
#[test]
fn denied_and_unknown_ids_answer_identically_on_every_by_id_attention_surface() {
    let estate = ManagedEstate::new("acc14-by-id-oracle");
    let work_a = estate.work_a.clone();
    for tag in ["geoyws/visible", "geoyws/secret"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    // Unmanaged first: where no guard can deny, an unknown id keeps its plain
    // message. This fix must not change that UX.
    let plain = estate.run(&work_a, &["attention", "show", "att-never-raised"]);
    assert!(
        !plain.status.success(),
        "an unknown show should fail unmanaged"
    );
    let plain_stderr = String::from_utf8_lossy(&plain.stderr).into_owned();
    assert!(
        plain_stderr.contains("attention att-never-raised not found"),
        "unmanaged show lost its plain message: {plain_stderr}"
    );
    assert!(
        !plain_stderr.contains(DENIED),
        "unmanaged show answers a denial where no guard can deny: {plain_stderr}"
    );
    let secret_task = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the oracle secret task",
            "--id",
            "t-oracle-secret",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let secret = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "the oracle secret body",
            "--kind",
            "decision",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let raise_checked = |body: &str, tag: &str, key_a: &str, key_b: &str| -> String {
        estate.ok_json(
            &work_a,
            &[
                "attention",
                "raise",
                body,
                "--kind",
                "decision",
                "--tag",
                tag,
                "--check",
                "Oracle check question quorum?",
                "--check-choice",
                &format!("{key_a}=First label"),
                "--check-choice",
                &format!("{key_b}=Second label"),
                "--check-answer",
                key_a,
                "--check-explain",
                "Oracle check explanation quorum",
                "--check-about",
                "rust/store.rs",
                "--as",
                "seed",
                "--json",
            ],
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let checked = raise_checked(
        "the oracle secret check body",
        "geoyws/secret",
        "okey-alpha",
        "okey-beta",
    );
    let visible = raise_checked(
        "the oracle visible check body",
        "geoyws/visible",
        "vkey-alpha",
        "vkey-beta",
    );
    estate.bind_self(
        "p-oracle",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "geoyws/visible"),
            tag_scope("write", &estate.id_a, "geoyws/visible"),
        ],
    );
    estate.enforce("managed");
    // 1. Each CLI by-id surface: the denied id and the never-created id exit
    //    with the same code and byte-identical stderr — the generic denial.
    let assert_cli_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a denied id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_cli_identical(
        &["attention", "show", &secret],
        &["attention", "show", "att-never-raised"],
        "attention show",
    );
    assert_cli_identical(
        &[
            "attention",
            "resolve",
            &secret,
            "--as",
            "seed",
            "--choice",
            "custom",
            "--outcome",
            "other",
            "--note",
            "done",
        ],
        &[
            "attention",
            "resolve",
            "att-never-raised",
            "--as",
            "seed",
            "--choice",
            "custom",
            "--outcome",
            "other",
            "--note",
            "done",
        ],
        "attention resolve",
    );
    assert_cli_identical(
        &[
            "attention",
            "reopen",
            &secret,
            "--as",
            "seed",
            "--note",
            "undo probe",
        ],
        &[
            "attention",
            "reopen",
            "att-never-raised",
            "--as",
            "seed",
            "--note",
            "undo probe",
        ],
        "attention reopen",
    );
    assert_cli_identical(
        &[
            "attention",
            "check",
            &checked,
            "--as",
            "seed",
            "--key",
            "okey-alpha",
        ],
        &[
            "attention",
            "check",
            "att-never-raised",
            "--as",
            "seed",
            "--key",
            "okey-alpha",
        ],
        "attention check",
    );
    assert_cli_identical(
        &["task", "show", &secret_task],
        &["task", "show", "t-never-created"],
        "task show",
    );
    // 2. A refusal past the guard keeps the store's own sentence: an
    //    undeclared key on the VISIBLE row is refused by name, not as denied.
    let named = estate.run(
        &work_a,
        &[
            "attention",
            "check",
            &visible,
            "--as",
            "seed",
            "--key",
            "no-such-key",
        ],
    );
    assert!(!named.status.success(), "an undeclared key should fail");
    let named_stderr = String::from_utf8_lossy(&named.stderr).into_owned();
    assert!(
        named_stderr.contains("names no --check-choice"),
        "the past-guard refusal lost its sentence: {named_stderr}"
    );
    assert!(
        !named_stderr.contains(DENIED),
        "the past-guard refusal collapsed into the generic denial: {named_stderr}"
    );
    // 3. The control path works: the visible row's correct key records, so
    //    the denials above came from the guard and not a broken route.
    let recorded = estate.run(
        &work_a,
        &[
            "attention",
            "check",
            &visible,
            "--as",
            "seed",
            "--key",
            "vkey-alpha",
        ],
    );
    assert!(
        recorded.status.success(),
        "the control check answer should record: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );
}

/// ACC-14 over the CLI, every task route that names a row: a tag-denied task
/// id and a never-created one answer with identical stderr and exit code on
/// `task move`, `task update`, `task remove`, `claim`, `events --task` and
/// `deploy show`, and a task-filtered listing (`attention list --task`)
/// succeeds for both alike — the unknown filter proceeds exactly as the
/// denied one does. `notes` and `checkpoints` have no standalone CLI read —
/// every command reaches them only past `require_task`, which already denies
/// both alike — so they are pinned at store level instead
/// (`managed_notes_checkpoints_and_named_claim_deny_denied_and_unknown_tasks_identically`).
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate, running as the same identity the grants were bound for.
/// The caller holds board read AND write on Alpha and no tag scope at all,
/// so every denial below is the ROW's and not the guard's blanket refusal of
/// a principal who can write nothing. The fixture holds one `secret` task
/// (with a deployment started against it before enforcement, so the attempt
/// is a projection of a denied subject) and one untagged control task. The
/// unknown ids `t-never-created` and `dep-never-started` were never added or
/// started. Outside enforcement an unknown task keeps its plain `not found`
/// message.
#[test]
fn denied_and_unknown_task_ids_answer_identically_on_task_routes() {
    let estate = ManagedEstate::new("acc14-task-routes");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    let visible = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the oracle visible task",
            "--id",
            "t-oracle-visible",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let secret = estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the oracle secret task",
            "--id",
            "t-oracle-secret",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
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
            &secret,
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Unmanaged first: where no guard can deny, an unknown task keeps its
    // plain message. This fix must not change that UX.
    let plain = estate.run(
        &work_a,
        &["task", "move", "t-never-created", "todo", "--as", "seed"],
    );
    assert!(
        !plain.status.success(),
        "an unknown move should fail unmanaged"
    );
    let plain_stderr = String::from_utf8_lossy(&plain.stderr).into_owned();
    assert!(
        plain_stderr.contains("task t-never-created not found"),
        "unmanaged move lost its plain message: {plain_stderr}"
    );
    assert!(
        !plain_stderr.contains(DENIED),
        "unmanaged move answers a denial where no guard can deny: {plain_stderr}"
    );
    estate.bind_self(
        "p-oracle-tasks",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");
    // 1. Every task mutation and named task read: the denied id and the
    //    never-created id fail with the same code and byte-identical stderr —
    //    the existing non-enumerating denial.
    let assert_cli_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a denied id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_cli_identical(
        &["task", "move", &secret, "todo", "--as", "seed"],
        &["task", "move", "t-never-created", "todo", "--as", "seed"],
        "task move",
    );
    assert_cli_identical(
        &[
            "task",
            "update",
            &secret,
            "--title",
            "oracle rename",
            "--as",
            "seed",
        ],
        &[
            "task",
            "update",
            "t-never-created",
            "--title",
            "oracle rename",
            "--as",
            "seed",
        ],
        "task update",
    );
    assert_cli_identical(
        &["task", "remove", &secret, "--as", "seed"],
        &["task", "remove", "t-never-created", "--as", "seed"],
        "task remove",
    );
    assert_cli_identical(
        &["claim", &secret, "--as", "seed"],
        &["claim", "t-never-created", "--as", "seed"],
        "claim",
    );
    assert_cli_identical(
        &["events", "--task", &secret],
        &["events", "--task", "t-never-created"],
        "events --task",
    );
    assert_cli_identical(
        &["deploy", "show", &attempt],
        &["deploy", "show", "dep-never-started"],
        "deploy show",
    );
    // 2. A task-filtered listing succeeds for both alike: the unknown filter
    //    proceeds exactly as the denied one does, tag-filtered per row.
    for task in [secret.as_str(), "t-never-created"] {
        let listed = estate.run(&work_a, &["attention", "list", "--task", task, "--json"]);
        assert!(
            listed.status.success(),
            "attention list --task {} should succeed: {}",
            task,
            String::from_utf8_lossy(&listed.stderr)
        );
        assert!(
            listed.stderr.is_empty(),
            "attention list --task {} wrote to stderr: {}",
            task,
            String::from_utf8_lossy(&listed.stderr)
        );
    }
    // 3. The control paths work: the visible task's history reads and its
    //    filtered listing answers, so the denials above came from the guard
    //    and not a broken route.
    let history = estate.ok(&work_a, &["events", "--task", &visible]);
    assert!(
        history.contains(&visible),
        "the control task's history lost its rows: {history}"
    );
    let control = estate.ok_json(
        &work_a,
        &["attention", "list", "--task", &visible, "--json"],
    );
    assert_eq!(
        control.as_array().unwrap().len(),
        0,
        "the control listing should be empty, not failed: {control}"
    );
    // 4. Nothing was removed: with enforcement lifted the refused remove's
    //    target still shows, so the denials above recorded nothing.
    estate.enforce("direct");
    let survived = estate.ok_json(&work_a, &["task", "show", &secret, "--json"]);
    assert_eq!(
        survived["id"].as_str(),
        Some(secret.as_str()),
        "a refused remove moved a row: {survived}"
    );
}

/// ACC-14, task-linked rows: a sitrep, a handoff, an untagged attention row or
/// a subscription naming a `secret` task is visible only to a caller who can
/// read that task — on every listing and on `handoff retire` — while a denied task id and a never-created id answer
/// byte-identically everywhere.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. The owner seeds rows on a `secret` task and on a visible
/// task; principal P holds board read and write but no `secret` scope, so the
/// visible rows are the positive control: P sees those, never the secret
/// ones, and the owner still sees all of them.
#[test]
fn task_linked_rows_withhold_a_tag_denied_task_on_every_listing() {
    let estate = ManagedEstate::new("acc14-task-linked-rows");
    let work_a = estate.work_a.clone();
    let repo = work_a.to_string_lossy().into_owned();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the linked secret task",
            "--id",
            "t-linked-secret",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    // The control task carries no tag, so principal P — board read and write,
    // no `secret` scope — reads it and its rows: P's listings are non-empty
    // by construction, and an answer that hid everything could not pass.
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the linked visible task",
            "--id",
            "t-linked-visible",
            "--as",
            "seed",
            "--json",
        ],
    );
    // The owner seeds one row of each kind on each task, all carrying a
    // marker no other row carries, so "no trace" is a string search.
    for task in ["t-linked-secret", "t-linked-visible"] {
        let marker = task.strip_prefix("t-linked-").unwrap().to_owned();
        let sitrep_args = vec![
            "sitrep".to_owned(),
            "post".to_owned(),
            format!("linked {marker} sitrep body"),
            "--as".to_owned(),
            "seed".to_owned(),
            "--lane".to_owned(),
            "driver-1".to_owned(),
            "--repo".to_owned(),
            repo.clone(),
            "--branch".to_owned(),
            "main".to_owned(),
            "--head".to_owned(),
            SEED_HEAD.to_owned(),
            "--dirty".to_owned(),
            "clean".to_owned(),
            "--task".to_owned(),
            task.to_owned(),
            "--json".to_owned(),
        ];
        let sitrep_refs: Vec<&str> = sitrep_args.iter().map(String::as_str).collect();
        estate.ok_json(&work_a, &sitrep_refs);
        let token =
            estate.ok_json(&work_a, &["claim", task, "--as", "seed", "--json"])["leaseToken"]
                .as_str()
                .unwrap()
                .to_owned();
        estate.ok_json(
            &work_a,
            &[
                "handoff",
                "create",
                task,
                "--lease",
                &token,
                "--as",
                "seed",
                "--summary",
                &format!("linked {marker} handoff summary"),
                "--intent",
                &format!("linked {marker} handoff intent"),
                "--next-action",
                &format!("linked {marker} handoff next"),
                "--repo",
                &repo,
                "--branch",
                "main",
                "--head",
                SEED_HEAD,
                "--dirty",
                "clean",
                "--json",
            ],
        );
        estate.ok_json(
            &work_a,
            &[
                "attention",
                "raise",
                &format!("linked {marker} attention question"),
                "--kind",
                "decision",
                "--as",
                "seed",
                "--task",
                task,
                "--json",
            ],
        );
        estate.ok_json(
            &work_a,
            &[
                "subscription",
                "add",
                "--consumer",
                &format!("linked-{marker}-consumer"),
                "--action",
                "linked-action",
                "--timeout-ms",
                "100",
                "--max-retries",
                "1",
                "--rate-per-minute",
                "60",
                "--max-concurrency",
                "1",
                "--as",
                "seed",
                "--subject",
                &format!("task:{task}"),
                "--json",
            ],
        );
    }
    let secret_handoff = estate
        .ok_json(
            &work_a,
            &["handoff", "list", "--task", "t-linked-secret", "--json"],
        )
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["status"] == "pending")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let seeded_subs = estate.ok_json(&work_a, &["subscription", "list", "--all", "--json"]);
    let sub_id = |consumer: &str| {
        seeded_subs
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["consumerID"] == consumer)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let secret_sub = sub_id("linked-secret-consumer");
    let visible_sub = sub_id("linked-visible-consumer");
    assert_ne!(secret_sub, visible_sub, "the two seeds collided on one row");
    estate.bind_self(
        "p-linked-rows",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");
    // 1. The `--task` listings succeed for both ids alike and hand over
    //    nothing: the denied filter withholds exactly what the unknown filter
    //    cannot match.
    for (base, what) in [
        (&["sitrep", "list", "--task"][..], "sitrep list --task"),
        (&["handoff", "list", "--task"][..], "handoff list --task"),
        (
            &["attention", "list", "--task"][..],
            "attention list --task",
        ),
    ] {
        let mut denied_args = base.to_vec();
        denied_args.extend_from_slice(&["t-linked-secret", "--json"]);
        let mut unknown_args = base.to_vec();
        unknown_args.extend_from_slice(&["t-never-created", "--json"]);
        let denied = estate.run(&work_a, &denied_args);
        let unknown = estate.run(&work_a, &unknown_args);
        assert!(
            denied.status.success(),
            "{what} with a denied id should succeed: {}",
            String::from_utf8_lossy(&denied.stderr)
        );
        assert!(
            unknown.status.success(),
            "{what} with an unknown id should succeed: {}",
            String::from_utf8_lossy(&unknown.stderr)
        );
        assert_eq!(
            denied.stdout, unknown.stdout,
            "{what} output differs between a denied id and an unknown id"
        );
        assert!(
            denied.stderr.is_empty() && unknown.stderr.is_empty(),
            "{what} wrote to stderr: {} / {}",
            String::from_utf8_lossy(&denied.stderr),
            String::from_utf8_lossy(&unknown.stderr)
        );
        let rows: Value = serde_json::from_slice(&denied.stdout).unwrap();
        assert_eq!(
            rows.as_array().unwrap().len(),
            0,
            "{what} with the denied id handed over a row the caller may not read: {rows}"
        );
    }
    // 2. The unfiltered listings carry the visible rows and no trace of the
    //    secret ones — neither their markers nor their task id.
    for (args, visible_marker, what) in [
        (
            &["sitrep", "list", "--json"][..],
            "linked visible sitrep body",
            "sitrep list",
        ),
        (
            &["handoff", "list", "--json"][..],
            "linked visible handoff summary",
            "handoff list",
        ),
        (
            &["attention", "list", "--json"][..],
            "linked visible attention question",
            "attention list",
        ),
        (
            &["subscription", "list", "--all", "--json"][..],
            "linked-visible-consumer",
            "subscription list",
        ),
    ] {
        let listed = estate.run(&work_a, args);
        assert!(
            listed.status.success(),
            "{what} should succeed: {}",
            String::from_utf8_lossy(&listed.stderr)
        );
        let body = String::from_utf8_lossy(&listed.stdout).into_owned();
        assert!(
            body.contains(visible_marker),
            "{what} withheld the visible row, so an empty answer proves nothing: {body}"
        );
        assert!(
            !body.contains("t-linked-secret"),
            "{what} served a row on the denied task: {body}"
        );
    }
    let unfiltered = estate.ok(&work_a, &["subscription", "list", "--all", "--json"]);
    assert!(
        !unfiltered.contains("linked secret"),
        "subscription list served the secret subject: {unfiltered}"
    );
    // 3. `handoff retire` answers the denied handoff exactly like an unknown
    //    one — the same exit code and byte-identical stderr — and records
    //    nothing.
    let denied_retire = estate.run(
        &work_a,
        &[
            "handoff",
            "retire",
            &secret_handoff,
            "--as",
            "seed",
            "--note",
            "linked probe",
        ],
    );
    let unknown_retire = estate.run(
        &work_a,
        &[
            "handoff",
            "retire",
            "h-never-created",
            "--as",
            "seed",
            "--note",
            "linked probe",
        ],
    );
    assert!(
        !denied_retire.status.success(),
        "retiring a handoff on the denied task succeeded but must be refused"
    );
    assert_eq!(
        denied_retire.status.code(),
        unknown_retire.status.code(),
        "handoff retire exit codes differ between a denied handoff and an unknown one"
    );
    assert_eq!(
        denied_retire.stderr, unknown_retire.stderr,
        "handoff retire stderr differs between a denied handoff and an unknown one"
    );
    assert!(
        String::from_utf8_lossy(&denied_retire.stderr).contains(DENIED),
        "handoff retire did not answer the non-enumerating denial: {}",
        String::from_utf8_lossy(&denied_retire.stderr)
    );
    // 4. The by-id surfaces on a linked row answer the denied id exactly like
    //    a never-created one — the same exit code and byte-identical stderr
    //    carrying the generic denial — and record nothing.
    let assert_by_id_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a denied id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_by_id_identical(
        &["subscription", "show", &secret_sub, "--json"],
        &["subscription", "show", "sub-never-created", "--json"],
        "subscription show",
    );
    assert_by_id_identical(
        &["subscription", "pause", &secret_sub, "--as", "seed"],
        &["subscription", "pause", "sub-never-created", "--as", "seed"],
        "subscription pause",
    );
    assert_by_id_identical(
        &["subscription", "resume", &secret_sub, "--as", "seed"],
        &[
            "subscription",
            "resume",
            "sub-never-created",
            "--as",
            "seed",
        ],
        "subscription resume",
    );
    assert_by_id_identical(
        &["handoff", "accept", &secret_handoff, "--as", "seed"],
        &["handoff", "accept", "h-never-created", "--as", "seed"],
        "handoff accept",
    );
    // The visible subscription stays readable and pausable, so the refusals
    // above came from the task gate and not a broken route.
    let visible_shown = estate.ok_json(&work_a, &["subscription", "show", &visible_sub, "--json"]);
    assert_eq!(
        visible_shown["consumerID"].as_str(),
        Some("linked-visible-consumer"),
        "the visible subscription lost its row: {visible_shown}"
    );
    // 5. The owner still sees every row, so the denials above came from the
    //    guard and not from missing rows — and the refused writes recorded
    //    nothing.
    estate.enforce("direct");
    for (args, secret_marker, what) in [
        (
            &["sitrep", "list", "--task", "t-linked-secret", "--json"][..],
            "linked secret sitrep body",
            "sitrep list --task",
        ),
        (
            &["handoff", "list", "--task", "t-linked-secret", "--json"][..],
            "linked secret handoff summary",
            "handoff list --task",
        ),
        (
            &["attention", "list", "--task", "t-linked-secret", "--json"][..],
            "linked secret attention question",
            "attention list --task",
        ),
    ] {
        let owned = estate.ok(&work_a, args);
        assert!(
            owned.contains(secret_marker),
            "the owner lost {what} on the secret task: {owned}"
        );
    }
    let owned_subs = estate.ok(&work_a, &["subscription", "list", "--all", "--json"]);
    assert!(
        owned_subs.contains("t-linked-secret"),
        "the owner lost the secret subscription: {owned_subs}"
    );
    let handoffs = estate.ok_json(
        &work_a,
        &["handoff", "list", "--task", "t-linked-secret", "--json"],
    );
    assert!(
        handoffs
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == secret_handoff && row["status"] == "pending"),
        "the refused retire moved a row: {handoffs}"
    );
    let secret_shown = estate.ok_json(&work_a, &["subscription", "show", &secret_sub, "--json"]);
    assert_eq!(
        secret_shown["status"].as_str(),
        Some("active"),
        "a refused pause or resume moved the secret subscription: {secret_shown}"
    );
}

/// ACC-14 residual oracles: heartbeat and release, sprint plan candidates and
/// parent epics, deploy finish/abandon/retry-of, and removed-task watch and
/// subscription subjects.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. Principal P holds board write only. A `secret` story is
/// leased to the seed holder, a `secret` epic stands by, two deployments prove
/// the secret story, and a second `secret` story is removed. P addresses each
/// denied id beside a never-created id: the same exit code and byte-identical
/// stderr carrying the existing non-enumerating denial, with nothing written.
/// The removed-then-denied id answers identically on `watch --task` and
/// `subscription add --subject`. Unmanaged boards keep their plain messages,
/// and the holder's own heartbeat, release, watch, subscription, sprint plan
/// and deploy finish/abandon still work.
#[test]
fn residual_lease_sprint_and_deployment_ids_answer_identically_under_enforcement() {
    fn subscription_on_subject(task: &str) -> Vec<&str> {
        vec![
            "subscription",
            "add",
            "--consumer",
            "residual-consumer",
            "--action",
            "residual-action",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "probe",
            "--subject",
            task,
            "--json",
        ]
    }
    fn deploy_start_commit() -> &'static str {
        "0123456789abcdef0123456789abcdef01234567"
    }
    let estate = ManagedEstate::new("acc14-residual-oracle");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    estate.ok_json(
        &work_a,
        &[
            "task",
            "add",
            "the residual visible task",
            "--id",
            "s-resid-visible",
            "--type",
            "story",
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
            "the residual secret task",
            "--id",
            "t-resid-secret",
            "--type",
            "task",
            "--tag",
            "geoyws/secret",
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
            "the residual secret epic",
            "--id",
            "e-resid-secret",
            "--type",
            "epic",
            "--tag",
            "geoyws/secret",
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
            "the residual removed task",
            "--id",
            "s-resid-gone",
            "--type",
            "story",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "sprint",
            "new",
            "Residual sprint",
            "--id",
            "sp-resid",
            "--target-version",
            "9.9.0",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "seed",
            "--json",
        ],
    );
    let lease = estate.ok_json(
        &work_a,
        &["claim", "t-resid-secret", "--as", "seed", "--json"],
    )["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut deployments = Vec::new();
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let started = estate.ok_json(
            &work_a,
            &[
                "deploy",
                "start",
                "--repo",
                "kanban",
                "--commit",
                deploy_start_commit(),
                "--tier",
                "@_bdt",
                "--environment",
                "branch-dev-testing",
                "--host",
                "geoywsMBP",
                "--url",
                "http://localhost:9999",
                "--task",
                "t-resid-secret",
                "--as",
                "seed",
                "--json",
            ],
        );
        deployments.push(started["id"].as_str().unwrap().to_owned());
        tokens.push(started["capabilityToken"].as_str().unwrap().to_owned());
    }
    estate.ok(&work_a, &["task", "remove", "s-resid-gone", "--as", "seed"]);
    for (args, plain, what) in [
        (
            vec!["heartbeat", "t-never-created", "--lease", "lease-bogus"],
            "has no active lease",
            "heartbeat",
        ),
        (
            vec!["release", "t-never-created", "--lease", "lease-bogus"],
            "has no active lease",
            "release",
        ),
        (
            vec![
                "sprint",
                "plan",
                "sp-resid",
                "--body",
                "residual probe",
                "--candidate",
                "t-never-created",
                "--as",
                "seed",
            ],
            "task t-never-created not found",
            "sprint plan --candidate",
        ),
        (
            vec![
                "sprint",
                "plan",
                "sp-resid",
                "--body",
                "residual probe",
                "--parent-epic",
                "t-never-created",
                "--as",
                "seed",
            ],
            "task t-never-created not found",
            "sprint plan --parent-epic",
        ),
        (
            vec![
                "deploy",
                "finish",
                "d-never-started",
                "--token",
                "token-bogus",
                "--result",
                "failed",
                "--phase",
                "build",
                "--receipt",
                "residual probe",
                "--as",
                "seed",
            ],
            "deployment d-never-started not found",
            "deploy finish",
        ),
        (
            vec![
                "deploy",
                "abandon",
                "d-never-started",
                "--as",
                "seed",
                "--note",
                "residual probe",
                "--token",
                "token-bogus",
            ],
            "deployment d-never-started not found",
            "deploy abandon",
        ),
        (
            vec![
                "deploy",
                "start",
                "--repo",
                "kanban",
                "--commit",
                deploy_start_commit(),
                "--tier",
                "@_bdt",
                "--environment",
                "branch-dev-testing",
                "--host",
                "geoywsMBP",
                "--url",
                "http://localhost:9999",
                "--retry-of",
                "d-never-started",
                "--as",
                "seed",
                "--json",
            ],
            "deployment d-never-started not found",
            "deploy start --retry-of",
        ),
        (
            vec!["watch", "--task", "t-never-created", "--json"],
            "not present in this board or its event history",
            "watch --task",
        ),
    ] {
        let output = estate.run(&work_a, &args);
        assert!(!output.status.success(), "{what} should fail unmanaged");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(plain),
            "{what} lost its plain unmanaged message: {stderr}"
        );
        assert!(
            !stderr.contains(DENIED),
            "{what} answers a denial where no guard can deny: {stderr}"
        );
    }
    let plain_subscription = estate.run(&work_a, &subscription_on_subject("task:t-never-created"));
    assert!(
        !plain_subscription.status.success(),
        "subscription add should fail unmanaged"
    );
    let plain_stderr = String::from_utf8_lossy(&plain_subscription.stderr).into_owned();
    assert!(
        plain_stderr.contains("not found in current or historical board state"),
        "subscription add lost its plain unmanaged message: {plain_stderr}"
    );
    assert!(
        !plain_stderr.contains(DENIED),
        "subscription add answers a denial where no guard can deny: {plain_stderr}"
    );
    estate.bind_self("p-residual", &[board_scope("write", &estate.id_a)]);
    estate.enforce("managed");
    // Each denied id answers exactly like the never-created one: the same
    // exit code, byte-identical stderr, and the generic denial.
    let assert_cli_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a denied id succeeded but must be refused"
        );
        assert!(
            !unknown.status.success(),
            "{what} with an unknown id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_cli_identical(
        &["heartbeat", "t-resid-secret", "--lease", "lease-bogus"],
        &["heartbeat", "t-never-created", "--lease", "lease-bogus"],
        "heartbeat",
    );
    assert_cli_identical(
        &["release", "t-resid-secret", "--lease", "lease-bogus"],
        &["release", "t-never-created", "--lease", "lease-bogus"],
        "release",
    );
    assert_cli_identical(
        &[
            "sprint",
            "plan",
            "sp-resid",
            "--body",
            "residual probe",
            "--candidate",
            "t-resid-secret",
            "--as",
            "seed",
        ],
        &[
            "sprint",
            "plan",
            "sp-resid",
            "--body",
            "residual probe",
            "--candidate",
            "t-never-created",
            "--as",
            "seed",
        ],
        "sprint plan --candidate",
    );
    assert_cli_identical(
        &[
            "sprint",
            "plan",
            "sp-resid",
            "--body",
            "residual probe",
            "--parent-epic",
            "e-resid-secret",
            "--as",
            "seed",
        ],
        &[
            "sprint",
            "plan",
            "sp-resid",
            "--body",
            "residual probe",
            "--parent-epic",
            "t-never-created",
            "--as",
            "seed",
        ],
        "sprint plan --parent-epic",
    );
    assert_cli_identical(
        &[
            "deploy",
            "finish",
            &deployments[0],
            "--token",
            "token-bogus",
            "--result",
            "failed",
            "--phase",
            "build",
            "--receipt",
            "residual probe",
            "--as",
            "seed",
        ],
        &[
            "deploy",
            "finish",
            "d-never-started",
            "--token",
            "token-bogus",
            "--result",
            "failed",
            "--phase",
            "build",
            "--receipt",
            "residual probe",
            "--as",
            "seed",
        ],
        "deploy finish",
    );
    assert_cli_identical(
        &[
            "deploy",
            "abandon",
            &deployments[0],
            "--as",
            "seed",
            "--note",
            "residual probe",
            "--token",
            "token-bogus",
        ],
        &[
            "deploy",
            "abandon",
            "d-never-started",
            "--as",
            "seed",
            "--note",
            "residual probe",
            "--token",
            "token-bogus",
        ],
        "deploy abandon",
    );
    assert_cli_identical(
        &[
            "deploy",
            "start",
            "--repo",
            "kanban",
            "--commit",
            deploy_start_commit(),
            "--tier",
            "@_bdt",
            "--environment",
            "branch-dev-testing",
            "--host",
            "geoywsMBP",
            "--url",
            "http://localhost:9999",
            "--retry-of",
            &deployments[0],
            "--as",
            "seed",
            "--json",
        ],
        &[
            "deploy",
            "start",
            "--repo",
            "kanban",
            "--commit",
            deploy_start_commit(),
            "--tier",
            "@_bdt",
            "--environment",
            "branch-dev-testing",
            "--host",
            "geoywsMBP",
            "--url",
            "http://localhost:9999",
            "--retry-of",
            "d-never-started",
            "--as",
            "seed",
            "--json",
        ],
        "deploy start --retry-of",
    );
    assert_cli_identical(
        &["watch", "--task", "s-resid-gone", "--json"],
        &["watch", "--task", "t-never-created", "--json"],
        "watch --task on a removed task",
    );
    assert_cli_identical(
        &subscription_on_subject("task:s-resid-gone"),
        &subscription_on_subject("task:t-never-created"),
        "subscription add --subject on a removed task",
    );
    // Nothing was written. As the board owner the same caller re-reads every
    // row the refused commands could have touched.
    estate.grant("p-residual", &owner_of(&estate.id_a));
    let shown = estate.ok_json(&work_a, &["task", "show", "t-resid-secret", "--json"]);
    assert_eq!(
        shown["claim"]["agentID"].as_str(),
        Some("seed"),
        "a refused heartbeat or release moved the lease: {shown}"
    );
    let ledger = estate.ok(
        &work_a,
        &["events", "--task", "t-resid-secret", "--all", "--json"],
    );
    for marker in [
        "task_sprint_changed",
        "claim_released",
        "deployment_finished",
        "deployment_abandoned",
    ] {
        assert!(
            !ledger.contains(marker),
            "a refused command wrote {marker}: {ledger}"
        );
    }
    assert!(
        ledger.contains("task_added"),
        "the secret task lost its birth event: {ledger}"
    );
    let listed = estate.ok_json(&work_a, &["deploy", "list", "--json"]);
    assert_eq!(
        listed.as_array().unwrap().len(),
        2,
        "a refused retry started a deployment: {listed}"
    );
    let subscriptions = estate.ok_json(&work_a, &["subscription", "list", "--all", "--json"]);
    assert!(
        subscriptions.as_array().unwrap().is_empty(),
        "a refused subscription was created: {subscriptions}"
    );
    let gone = estate.run(&work_a, &["task", "show", "s-resid-gone", "--json"]);
    assert!(
        !gone.status.success(),
        "a refused command restored the removed task"
    );
    // The control paths work: the holder's own heartbeat and release still
    // move the lease, the owner still watches the removed task's history,
    // subscribes to it, plans the sprint and finishes the deployments — so
    // the denials above came from the tag and not a broken path.
    estate.ok(
        &work_a,
        &["heartbeat", "t-resid-secret", "--lease", &lease, "--json"],
    );
    estate.ok(&work_a, &["release", "t-resid-secret", "--lease", &lease]);
    let released = estate.ok_json(&work_a, &["task", "show", "t-resid-secret", "--json"]);
    assert!(
        released["claim"].is_null(),
        "the holder's release kept the lease: {released}"
    );
    assert_eq!(
        released["status"].as_str(),
        Some("todo"),
        "the holder's release kept the status: {released}"
    );
    estate.ok(&work_a, &["watch", "--task", "s-resid-gone", "--json"]);
    estate.ok_json(&work_a, &subscription_on_subject("task:s-resid-gone"));
    let subscriptions = estate.ok_json(&work_a, &["subscription", "list", "--all", "--json"]);
    assert_eq!(
        subscriptions.as_array().unwrap().len(),
        1,
        "the owner's subscription to the removed task was not created: {subscriptions}"
    );
    estate.ok_json(
        &work_a,
        &[
            "sprint",
            "plan",
            "sp-resid",
            "--body",
            "residual control",
            "--candidate",
            "s-resid-visible",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "deploy",
            "finish",
            &deployments[0],
            "--token",
            &tokens[0],
            "--result",
            "failed",
            "--phase",
            "build",
            "--receipt",
            "residual control",
            "--as",
            "seed",
            "--json",
        ],
    );
    estate.ok_json(
        &work_a,
        &[
            "deploy",
            "abandon",
            &deployments[1],
            "--as",
            "seed",
            "--note",
            "residual control",
            "--token",
            &tokens[1],
            "--json",
        ],
    );
}

/// ACC-14, attention by-id reads through the linked task: a managed caller
/// holding board read and write but no `secret` scope must answer an untagged
/// attention row raised on a `secret` task exactly like a never-created id on
/// `attention show`, `resolve`, `reopen` and `check` — the same exit code and byte-identical stderr carrying the existing
/// non-enumerating denial, with nothing recorded — while the owner still runs
/// every one of those operations.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. Sibling of
/// `task_linked_rows_withhold_a_tag_denied_task_on_every_listing`, which pins
/// the listings; this one pins the by-id paths that used to check only the
/// row's own tags.
#[test]
fn attention_by_id_withholds_rows_on_a_tag_denied_task() {
    let estate = ManagedEstate::new("acc14-attention-byid-task");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    for (id, title) in [
        ("t-byid-secret", "the by-id secret task"),
        ("t-byid-visible", "the by-id visible task"),
    ] {
        let mut args = vec!["task", "add", title, "--id", id, "--as", "seed"];
        if id == "t-byid-secret" {
            args.extend_from_slice(&["--tag", "geoyws/secret"]);
        }
        args.push("--json");
        estate.ok_json(&work_a, &args);
    }
    let raise = |body: &str, task: &str| -> String {
        estate.ok_json(
            &work_a,
            &[
                "attention",
                "raise",
                body,
                "--kind",
                "decision",
                "--as",
                "seed",
                "--task",
                task,
                "--json",
            ],
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // Four untagged rows on the secret task, one per operation under test, so
    // a refusal on one cannot consume the state another needs.
    let show_row = raise("by-id secret show body", "t-byid-secret");
    let resolve_row = raise("by-id secret resolve body", "t-byid-secret");
    let reopen_row = raise("by-id secret reopen body", "t-byid-secret");
    let check_row = estate.ok_json(
        &work_a,
        &[
            "attention",
            "raise",
            "by-id secret check body",
            "--kind",
            "decision",
            "--task",
            "t-byid-secret",
            "--check",
            "By-id check question quorum?",
            "--check-choice",
            "bkey-alpha=First label",
            "--check-choice",
            "bkey-beta=Second label",
            "--check-answer",
            "bkey-alpha",
            "--check-explain",
            "By-id check explanation quorum",
            "--check-about",
            "rust/store.rs",
            "--as",
            "seed",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The control row lives on the visible task, so P's denials below prove a
    // guard and not a broken route.
    let visible_row = raise("by-id visible show body", "t-byid-visible");
    // The owner settles the reopen row while still direct, so P's reopen
    // meets a resolved row it may not read.
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "resolve",
            &reopen_row,
            "--as",
            "seed",
            "--choice",
            "custom",
            "--outcome",
            "other",
            "--note",
            "done",
            "--json",
        ],
    );
    estate.bind_self(
        "p-byid-task",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");
    // 1. Each CLI by-id surface: the denied row and the never-created id exit
    //    with the same code and byte-identical stderr — the generic denial.
    let assert_cli_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a denied row succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied row and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied row and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_cli_identical(
        &["attention", "show", &show_row],
        &["attention", "show", "att-never-raised"],
        "attention show",
    );
    assert_cli_identical(
        &[
            "attention",
            "resolve",
            &resolve_row,
            "--as",
            "seed",
            "--choice",
            "custom",
            "--outcome",
            "other",
            "--note",
            "done",
        ],
        &[
            "attention",
            "resolve",
            "att-never-raised",
            "--as",
            "seed",
            "--choice",
            "custom",
            "--outcome",
            "other",
            "--note",
            "done",
        ],
        "attention resolve",
    );
    assert_cli_identical(
        &[
            "attention",
            "reopen",
            &reopen_row,
            "--as",
            "seed",
            "--note",
            "undo probe",
        ],
        &[
            "attention",
            "reopen",
            "att-never-raised",
            "--as",
            "seed",
            "--note",
            "undo probe",
        ],
        "attention reopen",
    );
    assert_cli_identical(
        &[
            "attention",
            "check",
            &check_row,
            "--as",
            "seed",
            "--key",
            "bkey-alpha",
        ],
        &[
            "attention",
            "check",
            "att-never-raised",
            "--as",
            "seed",
            "--key",
            "bkey-alpha",
        ],
        "attention check",
    );
    // 2. The control row stays readable, so the refusals above came from the
    //    task gate and not a broken route.
    let control = estate.ok_json(&work_a, &["attention", "show", &visible_row, "--json"]);
    assert_eq!(
        control["id"].as_str(),
        Some(visible_row.as_str()),
        "the visible row lost its read: {control}"
    );
    // 3. The owner still runs every operation, which also proves the refused
    //    writes recorded nothing: a recorded resolve would meet "already
    //    resolved", a recorded reopen an open row, a recorded check "exactly
    //    one answer".
    estate.enforce("direct");
    let owned = estate.ok_json(&work_a, &["attention", "show", &show_row, "--json"]);
    assert_eq!(
        owned["status"].as_str(),
        Some("open"),
        "the owner lost the denied show row: {owned}"
    );
    let settled = estate.ok_json(
        &work_a,
        &[
            "attention",
            "resolve",
            &resolve_row,
            "--as",
            "seed",
            "--choice",
            "custom",
            "--outcome",
            "other",
            "--note",
            "done",
            "--json",
        ],
    );
    assert_eq!(
        settled["status"].as_str(),
        Some("resolved"),
        "the owner lost the denied resolve row: {settled}"
    );
    let undone = estate.ok_json(
        &work_a,
        &[
            "attention",
            "reopen",
            &reopen_row,
            "--as",
            "seed",
            "--note",
            "owner undo",
            "--json",
        ],
    );
    assert_eq!(
        undone["status"].as_str(),
        Some("open"),
        "the owner lost the denied reopen row: {undone}"
    );
    estate.ok_json(
        &work_a,
        &[
            "attention",
            "check",
            &check_row,
            "--as",
            "seed",
            "--key",
            "bkey-alpha",
            "--json",
        ],
    );
}

/// ACC-14, subscription relation targets on read: the owner adds a
/// subscription with no subject and `--relation parent:t-secret`, which the
/// add path already gates like a subject. A managed caller holding board read
/// and write but no `secret` scope must not see it in `subscription list`,
/// and `show`, `pause` and `resume` must answer it exactly like a
/// never-created id — the same exit code and byte-identical stderr carrying
/// the existing non-enumerating denial, with nothing recorded — while the
/// owner still sees it.
///
/// INTEGRATION, at the layer `process`: the real binary against a real
/// managed estate. Sibling of
/// `task_linked_rows_withhold_a_tag_denied_task_on_every_listing`, which pins
/// subject-gated subscriptions; this one pins the relation-target half the
/// read paths used to skip.
#[test]
fn subscription_relation_targets_withhold_a_tag_denied_task_on_read() {
    let estate = ManagedEstate::new("acc14-subscription-relation");
    let work_a = estate.work_a.clone();
    estate.ok_json(
        &work_a,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    for (id, title) in [
        ("t-rel-secret", "the relation secret task"),
        ("t-rel-visible", "the relation visible task"),
    ] {
        let mut args = vec!["task", "add", title, "--id", id, "--as", "seed"];
        if id == "t-rel-secret" {
            args.extend_from_slice(&["--tag", "geoyws/secret"]);
        }
        args.push("--json");
        estate.ok_json(&work_a, &args);
    }
    // The owner subscribes with no subject, filtering only on the relation —
    // the shape whose target the read paths used to ignore — once against the
    // secret task and once against the visible control task.
    let add_relation = |consumer: &str, target: &str| {
        estate.ok_json(
            &work_a,
            &[
                "subscription",
                "add",
                "--consumer",
                consumer,
                "--action",
                "relation-action",
                "--timeout-ms",
                "100",
                "--max-retries",
                "1",
                "--rate-per-minute",
                "60",
                "--max-concurrency",
                "1",
                "--as",
                "seed",
                "--relation",
                target,
                "--json",
            ],
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let secret_sub = add_relation("rel-secret-consumer", "parent:t-rel-secret");
    let visible_sub = add_relation("rel-visible-consumer", "parent:t-rel-visible");
    assert_ne!(secret_sub, visible_sub, "the two seeds collided on one row");
    estate.bind_self(
        "p-rel-sub",
        &[
            board_scope("read", &estate.id_a),
            board_scope("write", &estate.id_a),
        ],
    );
    estate.enforce("managed");
    // 1. The list carries the visible subscription and no trace of the secret
    //    one — neither its consumer nor its relation target.
    let listed = estate.run(&work_a, &["subscription", "list", "--all", "--json"]);
    assert!(
        listed.status.success(),
        "subscription list should succeed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let body = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(
        body.contains("rel-visible-consumer"),
        "subscription list withheld the visible row, so an empty answer proves nothing: {body}"
    );
    assert!(
        !body.contains("rel-secret-consumer") && !body.contains("t-rel-secret"),
        "subscription list served the denied relation target: {body}"
    );
    // 2. The by-id surfaces answer the denied subscription exactly like a
    //    never-created one — the same exit code and byte-identical stderr
    //    carrying the generic denial — and record nothing.
    let assert_by_id_identical = |denied_args: &[&str], unknown_args: &[&str], what: &str| {
        let denied = estate.run(&work_a, denied_args);
        let unknown = estate.run(&work_a, unknown_args);
        assert!(
            !denied.status.success(),
            "{what} with a denied id succeeded but must be refused"
        );
        assert_eq!(
            denied.status.code(),
            unknown.status.code(),
            "{what} exit codes differ between a denied id and an unknown id"
        );
        assert_eq!(
            denied.stderr, unknown.stderr,
            "{what} stderr differs between a denied id and an unknown id"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains(DENIED),
            "{what} did not answer the non-enumerating denial: {stderr}"
        );
    };
    assert_by_id_identical(
        &["subscription", "show", &secret_sub, "--json"],
        &["subscription", "show", "sub-never-created", "--json"],
        "subscription show",
    );
    assert_by_id_identical(
        &["subscription", "pause", &secret_sub, "--as", "seed"],
        &["subscription", "pause", "sub-never-created", "--as", "seed"],
        "subscription pause",
    );
    assert_by_id_identical(
        &["subscription", "resume", &secret_sub, "--as", "seed"],
        &[
            "subscription",
            "resume",
            "sub-never-created",
            "--as",
            "seed",
        ],
        "subscription resume",
    );
    // 3. The owner still sees the subscription untouched, so the refusals
    //    above came from the relation gate and not from missing rows — and
    //    the refused pause and resume recorded nothing.
    estate.enforce("direct");
    let owned = estate.ok(&work_a, &["subscription", "list", "--all", "--json"]);
    assert!(
        owned.contains("rel-secret-consumer") && owned.contains("t-rel-secret"),
        "the owner lost the secret subscription: {owned}"
    );
    let secret_shown = estate.ok_json(&work_a, &["subscription", "show", &secret_sub, "--json"]);
    assert_eq!(
        secret_shown["status"].as_str(),
        Some("active"),
        "a refused pause or resume moved the secret subscription: {secret_shown}"
    );
    let visible_shown = estate.ok_json(&work_a, &["subscription", "show", &visible_sub, "--json"]);
    assert_eq!(
        visible_shown["consumerID"].as_str(),
        Some("rel-visible-consumer"),
        "the visible subscription lost its row: {visible_shown}"
    );
}

/// ACC-14 import gate, at the layer `process`: an import rewrites arbitrary
/// rows, deletes claims and dependencies with `--reconcile`, and seizes live
/// leases with `--force`, so only a principal holding the whole board may run
/// it — including its previews, whose overlap listing names existing ids.
///
/// The fixture holds a live `secret` task beside an untagged-visible one. A
/// read-only caller and a caller holding board write plus the `visible` tag
/// scope — both without whole-board authority — are refused the one generic
/// denial on `import atmux-json` with no flags and with `--reconcile`,
/// `--dry-run` and `--verify`, and on one `atmux-sqlite` source, write
/// nothing, and see neither live id on stderr. Granting the same principal
/// the board tag wildcard turns the same source into a successful reconcile.
#[test]
fn import_requires_whole_board_write_and_names_no_denied_id() {
    let estate = ManagedEstate::new("acc14-import-gate");
    let work_a = estate.work_a.clone();
    for tag in ["geoyws/secret", "geoyws/visible"] {
        estate.ok_json(&work_a, &["tag", "add", tag, "--as", "seed", "--json"]);
    }
    for (id, title, tag) in [
        ("t-imp-secret", "the import secret task", "geoyws/secret"),
        ("t-imp-visible", "the import visible task", "geoyws/visible"),
    ] {
        estate.ok_json(
            &work_a,
            &[
                "task", "add", title, "--id", id, "--tag", tag, "--as", "seed", "--json",
            ],
        );
    }
    let source = estate.root.join("import-source.json");
    fs::write(
        &source,
        serde_json::to_vec(&serde_json::json!({
            "epics": [],
            "stories": [],
            "tasks": [
                {"id": "t-imp-fresh", "subject": "a fresh import row", "status": "todo"},
                {"id": "t-imp-secret", "subject": "an overwrite attempt", "status": "todo"},
            ],
        }))
        .unwrap(),
    )
    .unwrap();
    let source_arg = source.to_string_lossy().into_owned();
    // The same rows through the sqlite mapping, so the refused loop below pins
    // the gate for the second source kind as well: the tolerant mapper needs
    // only these columns.
    let sqlite_source = estate.root.join("import-source.db");
    Connection::open(&sqlite_source)
        .unwrap()
        .execute_batch(
            "CREATE TABLE tasks(id TEXT,subject TEXT,status TEXT);
             INSERT INTO tasks VALUES('t-imp-fresh','a fresh import row','todo');
             INSERT INTO tasks VALUES('t-imp-secret','an overwrite attempt','todo');",
        )
        .unwrap();
    let sqlite_arg = sqlite_source.to_string_lossy().into_owned();
    let attempt = |sub: &str, path: &str, extra: &[&str]| {
        let mut args = vec!["import", sub, path, "--as", "seed", "--json"];
        args.extend_from_slice(extra);
        estate.run(&work_a, &args)
    };
    let refused_silently = |output: &std::process::Output, what: &str| {
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "{what} succeeded but must be refused\nstdout: {stdout}"
        );
        assert!(
            stderr.contains(DENIED),
            "{what} was refused with the wrong message\nstderr: {stderr}"
        );
        for id in ["t-imp-secret", "t-imp-visible", "t-imp-fresh"] {
            assert!(
                !stderr.contains(id),
                "{what} named an existing id on stderr: {stderr}"
            );
        }
    };
    // What "nothing is written" means here: the board file itself is
    // byte-identical and holds no new row or event. The registry is out of
    // scope for the byte comparison — resolving a board by working directory
    // stamps `last_used_at` on every command, refused or not.
    let board_bytes = || fs::read(&estate.board_a).unwrap();
    let board_row_count = |sql: &str| -> i64 {
        Connection::open(&estate.board_a)
            .unwrap()
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    };
    estate.bind_self("p-import", &[board_scope("read", &estate.id_a)]);
    estate.enforce("managed");
    let pristine = board_bytes();
    // `--verify` stands alone: the CLI refuses to combine it with the write
    // flags, so it is its own case rather than another flag in the loop.
    for extra in [
        &[][..],
        &["--reconcile"][..],
        &["--dry-run"][..],
        &["--verify"][..],
    ] {
        let output = attempt("atmux-json", &source_arg, extra);
        refused_silently(&output, &format!("read-only import {extra:?}"));
    }
    let output = attempt("atmux-sqlite", &sqlite_arg, &[]);
    refused_silently(&output, "read-only import atmux-sqlite");
    assert_eq!(
        board_bytes(),
        pristine,
        "a refused read-only import wrote to the board file"
    );

    estate.enforce("direct");
    estate.grant(
        "p-import",
        &[
            board_scope("write", &estate.id_a),
            tag_scope("read", &estate.id_a, "geoyws/visible"),
            tag_scope("write", &estate.id_a, "geoyws/visible"),
        ],
    );
    estate.enforce("managed");
    for extra in [
        &[][..],
        &["--reconcile"][..],
        &["--dry-run"][..],
        &["--verify"][..],
    ] {
        let output = attempt("atmux-json", &source_arg, extra);
        refused_silently(&output, &format!("tag-scoped import {extra:?}"));
    }
    assert_eq!(
        board_bytes(),
        pristine,
        "a refused tag-scoped import wrote to the board file"
    );
    assert_eq!(
        board_row_count("SELECT COUNT(*) FROM tasks"),
        2,
        "a refused import added a task row"
    );
    assert_eq!(
        board_row_count(
            "SELECT COUNT(*) FROM events WHERE kind IN ('tasks_imported','lease_seized')"
        ),
        0,
        "a refused import left an audit event"
    );

    estate.enforce("direct");
    estate.grant(
        "p-import",
        &[
            (
                "read",
                vec![format!("board:{}", estate.id_a), "*".to_owned()],
            ),
            (
                "write",
                vec![format!("board:{}", estate.id_a), "*".to_owned()],
            ),
        ],
    );
    estate.enforce("managed");
    assert_eq!(
        estate.ok_json(&work_a, &["task", "show", "t-imp-secret", "--json"])["title"],
        serde_json::json!("the import secret task"),
        "a refused import overwrote the secret row before the owner ran",
    );
    let receipt = estate.ok_json(
        &work_a,
        &[
            "import",
            "atmux-json",
            &source_arg,
            "--as",
            "seed",
            "--reconcile",
            "--json",
        ],
    );
    assert_eq!(receipt["created"], serde_json::json!(1), "{receipt}");
    assert_eq!(receipt["updated"], serde_json::json!(1), "{receipt}");
    assert_eq!(
        estate.ok_json(&work_a, &["task", "show", "t-imp-secret", "--json"])["title"],
        serde_json::json!("an overwrite attempt"),
    );
    assert_eq!(
        estate.ok_json(&work_a, &["task", "show", "t-imp-fresh", "--json"])["title"],
        serde_json::json!("a fresh import row"),
    );
}
