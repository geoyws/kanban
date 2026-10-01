//! Cross-board prerequisite identities (slice CROSS, row `t-e87d4704`), at
//! the compiled-process boundary.
//!
//! Spec: `docs/specs/cross-board-gates.md` (`CROSS-01`, `CROSS-02`,
//! `CROSS-05`, `CROSS-06`, `CROSS-11`); decisions ADR-056 §5 and ADR-057 §3.
//! Every test spawns the real `kanban` binary against real SQLite files in a
//! temp `KANBAN_DATA_DIR`. The pins themselves have no read surface yet (that
//! is `t-d8cc65c9`), so they are observed by opening the board file directly,
//! read-only, after the process has exited.
//!
//! Out of scope here and owned elsewhere: gate enforcement over a foreign
//! edge and the qualified cycle check (`t-a31b8d4f`), and the qualified read
//! projections (`t-d8cc65c9`).

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const ACTOR: &str = "lane-x";
const UNAVAILABLE: &str = "cannot be used as a prerequisite: it is unavailable to this caller. \
     Nothing was written";

struct Estate {
    root: PathBuf,
    data: PathBuf,
}

impl Estate {
    /// An empty data root; boards are added with [`Estate::board`].
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-cross-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        Self { root, data }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        let work = self.root.join("work").join(name);
        fs::create_dir_all(&work).unwrap();
        work
    }

    /// `init --name NAME` in its own workspace; returns the board UUID.
    fn board(&self, name: &str) -> String {
        let work = self.workspace(name);
        self.ok_json(&work, &["init", "--name", name, "--json"]);
        self.board_id(name)
    }

    fn board_id(&self, name: &str) -> String {
        let listing = self.ok_json(&self.root, &["workspace", "list", "--all", "--json"]);
        let path = listing
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("board {name} is not listed: {listing}"))["boardPath"]
            .as_str()
            .unwrap()
            .to_owned();
        Path::new(&path)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn board_file(&self, board_id: &str) -> PathBuf {
        self.data.join("boards").join(format!("{board_id}.db"))
    }

    fn run<S: AsRef<OsStr>>(&self, cwd: &Path, args: &[S]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanban"))
            .current_dir(cwd)
            .args(args)
            .env("KANBAN_DATA_DIR", &self.data)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn ok_json<S: AsRef<OsStr> + std::fmt::Debug>(&self, cwd: &Path, args: &[S]) -> Value {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout is not JSON: {error}\nstdout: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// A refused `--json` command: nonzero, and its one error line.
    fn refused<S: AsRef<OsStr> + std::fmt::Debug>(&self, cwd: &Path, args: &[S]) -> String {
        let output = self.run(cwd, args);
        assert!(
            !output.status.success(),
            "command should have been refused: {args:?}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        serde_json::from_str::<Value>(&stdout)
            .ok()
            .and_then(|value| value["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{stdout}{stderr}"))
    }

    fn add(&self, board: &str, id: &str, extra: &[&str]) -> Output {
        let work = self.workspace(board);
        let mut args = vec!["task", "add", id, "--id", id, "--as", ACTOR, "--json"];
        args.extend_from_slice(extra);
        self.run(&work, &args)
    }

    fn add_ok(&self, board: &str, id: &str, extra: &[&str]) {
        let output = self.add(board, id, extra);
        assert!(
            output.status.success(),
            "add {id} on {board} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for Estate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn readonly(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn user_version(connection: &Connection) -> i64 {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn board_token(board: &Connection) -> Option<String> {
    board
        .query_row(
            "SELECT value FROM board_meta WHERE key='registration_token'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
}

fn registry_token(estate: &Estate, board_id: &str) -> Option<String> {
    let registry = readonly(&estate.data.join("registry.db"));
    let mut statement = registry
        .prepare("SELECT board_path,registration_token FROM boards")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows.into_iter()
        .find(|(path, _)| path.ends_with(&format!("{board_id}.db")))
        .and_then(|(_, token)| token)
}

/// `(source_board_id, source_registration, source_item_id,
/// source_item_incarnation)` for every foreign edge on `task`.
fn pins(estate: &Estate, board_id: &str, task: &str) -> Vec<(String, String, String, String)> {
    let board = readonly(&estate.board_file(board_id));
    let mut statement = board
        .prepare(
            "SELECT source_board_id,source_registration,source_item_id,source_item_incarnation \
             FROM task_foreign_dependencies WHERE task_id=? ORDER BY source_board_id,source_item_id",
        )
        .unwrap();
    statement
        .query_map([task], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn local_edges(estate: &Estate, board_id: &str, task: &str) -> Vec<String> {
    let board = readonly(&estate.board_file(board_id));
    let mut statement = board
        .prepare("SELECT depends_on FROM task_dependencies WHERE task_id=? ORDER BY depends_on")
        .unwrap();
    statement
        .query_map([task], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn task_exists(estate: &Estate, board_id: &str, task: &str) -> bool {
    readonly(&estate.board_file(board_id))
        .query_row("SELECT 1 FROM tasks WHERE id=?", [task], |_| Ok(()))
        .optional()
        .unwrap()
        .is_some()
}

fn edge_json(board_id: &str, id: &str) -> String {
    json!([{ "boardID": board_id, "id": id }]).to_string()
}

/// Downgrade a freshly created estate to the pre-CROSS shape an old binary
/// left behind: registry v14 without registration tokens, board v37 without
/// item incarnations, foreign-edge table or file token.
fn make_pre_cross(estate: &Estate, board_ids: &[&str]) {
    let registry = Connection::open(estate.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            "DROP INDEX idx_boards_registration_token;
             ALTER TABLE boards DROP COLUMN registration_token;
             PRAGMA user_version=14;",
        )
        .unwrap();
    for board_id in board_ids {
        let board = Connection::open(estate.board_file(board_id)).unwrap();
        board
            .execute_batch(
                "DROP INDEX idx_tasks_incarnation;
                 ALTER TABLE tasks DROP COLUMN incarnation;
                 DROP INDEX idx_task_foreign_dependencies_source;
                 DROP TABLE task_foreign_dependencies;
                 DELETE FROM board_meta WHERE key='registration_token';
                 PRAGMA user_version=37;",
            )
            .unwrap();
    }
}

#[test]
fn a_new_estate_is_born_cross_aware_with_one_token_in_registry_and_file() {
    let estate = Estate::new("born");
    let board = estate.board("Alpha");
    estate.add_ok("Alpha", "t-1", &[]);
    let registry = readonly(&estate.data.join("registry.db"));
    assert_eq!(user_version(&registry), 15);
    let file = readonly(&estate.board_file(&board));
    assert_eq!(user_version(&file), 38);
    let token = registry_token(&estate, &board).expect("the registry minted a token");
    assert_eq!(token.len(), 32);
    assert_eq!(board_token(&file).as_deref(), Some(token.as_str()));
    let incarnation: Option<String> = file
        .query_row("SELECT incarnation FROM tasks WHERE id='t-1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(incarnation.map(|value| value.len()), Some(32));
}

#[test]
fn a_json_entry_naming_this_board_is_the_legacy_local_edge() {
    let estate = Estate::new("own");
    let board = estate.board("Alpha");
    let id = "weird/id:with\"quotes";
    estate.add_ok("Alpha", id, &[]);
    estate.add_ok(
        "Alpha",
        "t-wait",
        &["--depends-on-json", &edge_json(&board, id)],
    );
    assert_eq!(local_edges(&estate, &board, "t-wait"), [id]);
    assert!(pins(&estate, &board, "t-wait").is_empty());
    // A scalar naming the same opaque id is the same edge, never parsed.
    estate.add_ok("Alpha", "t-scalar", &["--depends-on", id]);
    assert_eq!(local_edges(&estate, &board, "t-scalar"), [id]);
}

#[test]
fn a_foreign_edge_pins_the_source_registration_and_item_incarnation() {
    let estate = Estate::new("pin");
    let source = estate.board("Source");
    let target = estate.board("Target");
    estate.add_ok("Source", "t-same", &[]);
    estate.add_ok("Target", "t-same", &[]);
    estate.add_ok(
        "Target",
        "t-wait",
        &["--depends-on-json", &edge_json(&source, "t-same")],
    );
    let source_file = readonly(&estate.board_file(&source));
    let incarnation: String = source_file
        .query_row(
            "SELECT incarnation FROM tasks WHERE id='t-same'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let registration = registry_token(&estate, &source).unwrap();
    assert_eq!(
        pins(&estate, &target, "t-wait"),
        [(
            source.clone(),
            registration,
            "t-same".to_owned(),
            incarnation
        )]
    );
    // Equal ids on two boards stay distinct: the local t-same is no edge.
    assert!(local_edges(&estate, &target, "t-wait").is_empty());
    let events = estate.ok_json(
        &estate.workspace("Target"),
        &["events", "--task", "t-wait", "--json"],
    );
    let added = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "task_added")
        .expect("task_added event");
    let payload: Value = match &added["payload"] {
        Value::String(text) => serde_json::from_str(text).unwrap(),
        other => other.clone(),
    };
    assert_eq!(
        payload["foreignDependencies"],
        json!([{ "boardID": source, "id": "t-same" }])
    );
}

#[test]
fn every_unusable_source_gives_one_refusal_and_writes_nothing() {
    let estate = Estate::new("unavailable");
    let source = estate.board("Source");
    estate.board("Target");
    estate.add_ok("Source", "t-there", &[]);
    let elsewhere = Estate::new("elsewhere");
    let foreign_registry = elsewhere.board("Faraway");
    elsewhere.add_ok("Faraway", "t-there", &[]);
    let unknown = "0f8fad5b-d9cb-469f-a165-70867728950e";
    let target_work = estate.workspace("Target");
    for (board, item) in [
        (unknown, "t-there"),
        (source.as_str(), "t-missing"),
        (foreign_registry.as_str(), "t-there"),
    ] {
        let refusal = estate.refused(
            &target_work,
            &[
                "task",
                "add",
                "t-wait",
                "--id",
                "t-wait",
                "--as",
                ACTOR,
                "--depends-on-json",
                &edge_json(board, item),
                "--json",
            ],
        );
        assert_eq!(
            refusal,
            format!("prerequisite boardID {board} id {item} {UNAVAILABLE}"),
            "one sentence for every cause, varying only by the caller's own input"
        );
    }
    let target = estate.board_id("Target");
    assert!(
        !task_exists(&estate, &target, "t-wait"),
        "a refused add leaves no row behind"
    );
    // A retired source is unavailable the same way.
    estate.ok_json(
        &estate.root,
        &[
            "workspace",
            "retire",
            "Source",
            "--as",
            ACTOR,
            "--note",
            "gone",
        ],
    );
    let refusal = estate.refused(
        &target_work,
        &[
            "task",
            "add",
            "t-wait",
            "--id",
            "t-wait",
            "--as",
            ACTOR,
            "--depends-on-json",
            &edge_json(&source, "t-there"),
            "--json",
        ],
    );
    assert_eq!(
        refusal,
        format!("prerequisite boardID {source} id t-there {UNAVAILABLE}")
    );
}

#[test]
fn mixed_or_malformed_forms_refuse_before_any_write() {
    let estate = Estate::new("mixed");
    let board = estate.board("Alpha");
    estate.add_ok("Alpha", "t-pre", &[]);
    let work = estate.workspace("Alpha");
    let json_edge = edge_json(&board, "t-pre");
    for extra in [
        vec![
            "--depends-on-json",
            json_edge.as_str(),
            "--depends-on",
            "t-pre",
        ],
        vec![
            "--depends-on-json",
            "[{\"boardID\":\"Alpha\",\"id\":\"t-pre\"}]",
        ],
        vec!["--depends-on-json", "[\"t-pre\"]"],
        vec!["--depends-on-json", "{\"boardID\":\"x\"}"],
    ] {
        let mut args = vec![
            "task", "add", "t-new", "--id", "t-new", "--as", ACTOR, "--json",
        ];
        args.extend(extra);
        estate.refused(&work, &args);
        assert!(!task_exists(&estate, &board, "t-new"));
    }
    estate.add_ok("Alpha", "t-row", &["--depends-on", "t-pre"]);
    for extra in [
        vec!["--depends-on-json", "[]", "--clear-dependencies"],
        vec!["--depends-on-json", "[]", "--depends-on", "t-pre"],
    ] {
        let mut args = vec!["task", "update", "t-row", "--as", ACTOR, "--json"];
        args.extend(extra);
        estate.refused(&work, &args);
        assert_eq!(local_edges(&estate, &board, "t-row"), ["t-pre"]);
    }
}

#[test]
fn clearing_a_pin_needs_no_source_and_carries_only_identities() {
    let estate = Estate::new("clear");
    let source = estate.board("Source");
    let target = estate.board("Target");
    estate.add_ok("Source", "t-secret-title", &[]);
    estate.add_ok("Target", "t-local", &[]);
    estate.add_ok(
        "Target",
        "t-wait",
        &["--depends-on-json", &edge_json(&source, "t-secret-title")],
    );
    assert_eq!(pins(&estate, &target, "t-wait").len(), 1);
    // The source goes away entirely; removal still succeeds.
    estate.ok_json(
        &estate.root,
        &[
            "workspace",
            "retire",
            "Source",
            "--as",
            ACTOR,
            "--note",
            "gone",
        ],
    );
    let work = estate.workspace("Target");
    estate.ok_json(
        &work,
        &[
            "task",
            "update",
            "t-wait",
            "--as",
            ACTOR,
            "--depends-on-json",
            "[]",
            "--json",
        ],
    );
    assert!(pins(&estate, &target, "t-wait").is_empty());
    let events = estate.ok_json(&work, &["events", "--task", "t-wait", "--json"]);
    let updated = events
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["kind"] == "task_updated")
        .expect("task_updated event");
    let text = updated.to_string();
    assert!(
        text.contains("foreignDependenciesRemoved") && text.contains(&source),
        "the removal is audited by qualified identity: {text}"
    );
    // A local replacement is the same set operation: it drops a pin too.
    let fresh = estate.board("Fresh");
    estate.add_ok("Fresh", "t-src", &[]);
    estate.ok_json(
        &work,
        &[
            "task",
            "update",
            "t-wait",
            "--as",
            ACTOR,
            "--depends-on-json",
            &edge_json(&fresh, "t-src"),
            "--json",
        ],
    );
    assert_eq!(pins(&estate, &target, "t-wait").len(), 1);
    estate.ok_json(
        &work,
        &[
            "task",
            "update",
            "t-wait",
            "--as",
            ACTOR,
            "--depends-on",
            "t-local",
            "--json",
        ],
    );
    assert!(pins(&estate, &target, "t-wait").is_empty());
    assert_eq!(local_edges(&estate, &target, "t-wait"), ["t-local"]);
}

#[test]
fn a_scratch_db_refuses_json_and_keeps_local_scalars() {
    let estate = Estate::new("scratch");
    let scratch = estate.root.join("scratch.db");
    let db = scratch.to_string_lossy().into_owned();
    let cwd = estate.root.clone();
    estate.ok_json(
        &cwd,
        &["--db", &db, "task", "add", "t-a", "--id", "t-a", "--json"],
    );
    let refusal = estate.refused(
        &cwd,
        &[
            "--db",
            &db,
            "task",
            "add",
            "t-b",
            "--id",
            "t-b",
            "--depends-on-json",
            &edge_json("0f8fad5b-d9cb-469f-a165-70867728950e", "t-a"),
            "--json",
        ],
    );
    assert!(refusal.contains("scratch"), "{refusal}");
    estate.ok_json(
        &cwd,
        &[
            "--db",
            &db,
            "task",
            "add",
            "t-b",
            "--id",
            "t-b",
            "--depends-on",
            "t-a",
            "--json",
        ],
    );
    let file = readonly(&scratch);
    assert_eq!(user_version(&file), 38, "scratch takes the whole ladder");
    assert_eq!(board_token(&file), None, "and binds no registration token");
}

#[test]
fn owner_init_is_the_only_upgrade_boundary_and_refuses_a_live_holder() {
    let estate = Estate::new("upgrade");
    let source = estate.board("Source");
    let target = estate.board("Target");
    estate.add_ok("Source", "t-src", &[]);
    make_pre_cross(&estate, &[&source, &target]);

    // Ordinary commands keep the pre-CROSS schema and keep local work going.
    estate.add_ok("Target", "t-local", &[]);
    estate.add_ok("Target", "t-wait", &["--depends-on", "t-local"]);
    assert_eq!(
        user_version(&readonly(&estate.data.join("registry.db"))),
        14
    );
    assert_eq!(user_version(&readonly(&estate.board_file(&target))), 37);
    let refusal = String::from_utf8_lossy(
        &estate
            .add(
                "Target",
                "t-x",
                &["--depends-on-json", &edge_json(&source, "t-src")],
            )
            .stdout,
    )
    .into_owned();
    assert!(refusal.contains("cross-board upgrade"), "{refusal}");

    // A live holder of the data root makes the pending upgrade refuse.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(estate.data.join(".lock"))
        .unwrap();
    lock.lock_shared().unwrap();
    let held = estate.refused(
        &estate.workspace("Target"),
        &["init", "--name", "Target", "--json"],
    );
    assert!(held.contains("another kanban process"), "{held}");
    assert_eq!(user_version(&readonly(&estate.board_file(&target))), 37);
    drop(lock);

    // The owner's init upgrades the registry and the one addressed board.
    estate.ok_json(
        &estate.workspace("Target"),
        &["init", "--name", "Target", "--json"],
    );
    assert_eq!(
        user_version(&readonly(&estate.data.join("registry.db"))),
        15
    );
    let target_file = readonly(&estate.board_file(&target));
    assert_eq!(user_version(&target_file), 38);
    assert_eq!(board_token(&target_file), registry_token(&estate, &target));
    assert_eq!(
        user_version(&readonly(&estate.board_file(&source))),
        37,
        "another board is never upgraded as a side effect"
    );
    assert_eq!(local_edges(&estate, &target, "t-wait"), ["t-local"]);

    // The source is unavailable until its own owner upgrades it.
    let still = estate.refused(
        &estate.workspace("Target"),
        &[
            "task",
            "add",
            "t-x",
            "--id",
            "t-x",
            "--as",
            ACTOR,
            "--depends-on-json",
            &edge_json(&source, "t-src"),
            "--json",
        ],
    );
    assert_eq!(
        still,
        format!("prerequisite boardID {source} id t-src {UNAVAILABLE}")
    );
    estate.ok_json(
        &estate.workspace("Source"),
        &["init", "--name", "Source", "--json"],
    );
    estate.add_ok(
        "Target",
        "t-x",
        &["--depends-on-json", &edge_json(&source, "t-src")],
    );
    assert_eq!(pins(&estate, &target, "t-x").len(), 1);
}

#[test]
fn owner_init_never_overwrites_a_different_incarnation() {
    let estate = Estate::new("incarnation");
    let target = estate.board("Target");
    let registered = registry_token(&estate, &target).unwrap();
    {
        let file = Connection::open(estate.board_file(&target)).unwrap();
        file.execute(
            "UPDATE board_meta SET value='ffffffffffffffffffffffffffffffff' \
             WHERE key='registration_token'",
            [],
        )
        .unwrap();
    }
    let refusal = estate.refused(
        &estate.workspace("Target"),
        &["init", "--name", "Target", "--json"],
    );
    assert!(refusal.contains("another incarnation"), "{refusal}");
    let file = readonly(&estate.board_file(&target));
    assert_eq!(
        board_token(&file).as_deref(),
        Some("ffffffffffffffffffffffffffffffff"),
        "the present token is never rewritten"
    );
    assert_eq!(registry_token(&estate, &target), Some(registered));
}
