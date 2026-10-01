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
/// left behind: registry v15 without registration tokens, board v38 without
/// item incarnations, foreign-edge table, file token, mint trigger or the
/// v39 guard on `search_tasks_au` (restored to its v38 body, so nothing still
/// references the column SQLite is asked to drop).
fn make_pre_cross(estate: &Estate, board_ids: &[&str]) {
    let registry = Connection::open(estate.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            "DROP INDEX idx_boards_registration_token;
             ALTER TABLE boards DROP COLUMN registration_token;
             PRAGMA user_version=15;",
        )
        .unwrap();
    for board_id in board_ids {
        let board = Connection::open(estate.board_file(board_id)).unwrap();
        board
            .execute_batch(
                "DROP TRIGGER tasks_mint_incarnation;
                 DROP TRIGGER search_tasks_au;
                 CREATE TRIGGER search_tasks_au AFTER UPDATE ON tasks BEGIN
                  DELETE FROM search_documents WHERE task_id=old.id;
                  INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
                  SELECT * FROM search_source_rows WHERE task_id=new.id;
                  INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
                  SELECT * FROM search_deployment_event_rows WHERE task_id=new.id;
                 END;
                 DROP INDEX idx_tasks_incarnation;
                 ALTER TABLE tasks DROP COLUMN incarnation;
                 DROP INDEX idx_task_foreign_dependencies_source;
                 DROP TABLE task_foreign_dependencies;
                 DELETE FROM board_meta WHERE key='registration_token';
                 PRAGMA user_version=38;",
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
    assert_eq!(user_version(&registry), 16);
    let file = readonly(&estate.board_file(&board));
    assert_eq!(user_version(&file), 39);
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
    assert_eq!(user_version(&file), 39, "scratch takes the whole ladder");
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
        15
    );
    assert_eq!(user_version(&readonly(&estate.board_file(&target))), 38);
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
    assert_eq!(user_version(&readonly(&estate.board_file(&target))), 38);
    drop(lock);

    // The owner's init upgrades the registry and the one addressed board.
    estate.ok_json(
        &estate.workspace("Target"),
        &["init", "--name", "Target", "--json"],
    );
    assert_eq!(
        user_version(&readonly(&estate.data.join("registry.db"))),
        16
    );
    let target_file = readonly(&estate.board_file(&target));
    assert_eq!(user_version(&target_file), 39);
    assert_eq!(board_token(&target_file), registry_token(&estate, &target));
    assert_eq!(
        user_version(&readonly(&estate.board_file(&source))),
        38,
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

/// A managed estate in the canonical root (`XDG_DATA_HOME/kanban`), the only
/// root a managed caller's authority is minted from, addressed by working
/// directory alone — the route managed enforcement does not refuse.
struct Managed {
    root: PathBuf,
    xdg: PathBuf,
}

impl Managed {
    fn new(label: &str) -> Self {
        let uid = Command::new("id").arg("-u").output().unwrap();
        assert_ne!(
            String::from_utf8_lossy(&uid.stdout).trim(),
            "0",
            "managed enforcement mints no authority for root by design; run as a non-root user \
             or in the Linux gate container"
        );
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-cross-managed-{label}-{}-{unique}",
            std::process::id()
        ));
        let xdg = root.join("xdg");
        fs::create_dir_all(&xdg).unwrap();
        Self { root, xdg }
    }

    fn work(&self, name: &str) -> PathBuf {
        let work = self.root.join("work").join(name);
        fs::create_dir_all(&work).unwrap();
        work
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanban"))
            .current_dir(cwd)
            .args(args)
            .env("XDG_DATA_HOME", &self.xdg)
            .env_remove("KANBAN_DATA_DIR")
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn ok_json(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refused(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(!output.status.success(), "{args:?} must be refused");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn registry(&self) -> Connection {
        Connection::open(self.xdg.join("kanban").join("registry.db")).unwrap()
    }

    /// Bind this process's own identity to `principal` (once), then grant it
    /// each `(capability, atoms)` scope.
    fn grant(&self, principal: &str, scopes: &[(&str, Vec<String>)]) {
        let username = Command::new("id").arg("-un").output().unwrap();
        let uid = Command::new("id").arg("-u").output().unwrap();
        let registry = self.registry();
        registry
            .execute(
                "INSERT OR IGNORE INTO principals(id,username,uid,enabled,bound_at_epoch,bound_by_event_id) \
                 VALUES(?1,?2,?3,1,0,'pe-00000000')",
                rusqlite::params![
                    principal,
                    String::from_utf8_lossy(&username.stdout).trim(),
                    String::from_utf8_lossy(&uid.stdout).trim().parse::<u32>().unwrap()
                ],
            )
            .unwrap();
        for (index, (capability, atoms)) in scopes.iter().enumerate() {
            registry
                .execute(
                    "INSERT INTO grants(id,principal_id,capability,scope,state,origin,\
                     granted_at_epoch,granted_by_event_id) \
                     VALUES(?1,?2,?3,?4,'active','grant',0,'pe-00000000')",
                    rusqlite::params![
                        format!(
                            "g-{principal}-{index}-{}",
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

    fn pins(&self, board_path: &str, task: &str) -> Vec<String> {
        let board = readonly(Path::new(board_path));
        let mut statement = board
            .prepare(
                "SELECT source_item_id FROM task_foreign_dependencies WHERE task_id=? \
                 ORDER BY source_item_id",
            )
            .unwrap();
        statement
            .query_map([task], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_managed_declaration_needs_source_row_read_and_refuses_without_confirming() {
    let estate = Managed::new("authority");
    let source_work = estate.work("Source");
    let target_work = estate.work("Target");
    // Built while the estate is still direct, as a real estate reaches managed.
    let source = estate.ok_json(&source_work, &["init", "--name", "Source", "--json"]);
    let target = estate.ok_json(&target_work, &["init", "--name", "Target", "--json"]);
    let source_path = source["boardPath"].as_str().unwrap().to_owned();
    let target_path = target["boardPath"].as_str().unwrap().to_owned();
    let source_id = Path::new(&source_path)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let target_id = Path::new(&target_path)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    estate.ok_json(
        &source_work,
        &["tag", "add", "secret", "--as", ACTOR, "--json"],
    );
    estate.ok_json(
        &source_work,
        &[
            "task", "add", "open", "--id", "t-open", "--as", ACTOR, "--json",
        ],
    );
    estate.ok_json(
        &source_work,
        &[
            "task", "add", "hidden", "--id", "t-hidden", "--tag", "secret", "--as", ACTOR, "--json",
        ],
    );
    let owner = |board: &str| -> Vec<(&str, Vec<String>)> {
        vec![
            ("read", vec![format!("board:{board}")]),
            ("write", vec![format!("board:{board}")]),
            ("read", vec![format!("board:{board}"), "*".to_owned()]),
            ("write", vec![format!("board:{board}"), "*".to_owned()]),
        ]
    };
    estate.grant("p-target-owner", &owner(&target_id));
    estate
        .registry()
        .execute(
            "UPDATE enforcement_state SET state='managed' WHERE id=1",
            [],
        )
        .unwrap();
    let declare = |task: &str, item: &str| -> Vec<String> {
        vec![
            "task".into(),
            "add".into(),
            task.into(),
            "--id".into(),
            task.into(),
            "--as".into(),
            ACTOR.into(),
            "--depends-on-json".into(),
            edge_json(&source_id, item),
            "--json".into(),
        ]
    };

    // No authority on the source board at all: the one sentence, nothing
    // written — exactly what an unknown board UUID answers.
    let denied = estate.refused(&target_work, &strs(&declare("t-a", "t-open")));
    assert_eq!(
        denied,
        format!("prerequisite boardID {source_id} id t-open {UNAVAILABLE}")
    );
    let unknown = "0f8fad5b-d9cb-469f-a165-70867728950e";
    let mut probe = declare("t-a", "t-open");
    probe[8] = edge_json(unknown, "t-open");
    assert_eq!(
        estate.refused(&target_work, &strs(&probe)),
        format!("prerequisite boardID {unknown} id t-open {UNAVAILABLE}"),
        "denied and unknown differ only in the caller's own input"
    );
    assert!(estate.pins(&target_path, "t-a").is_empty());

    // Board read without the tag: the untagged row pins, the tagged one is
    // as unavailable as a row that does not exist.
    estate.grant(
        "p-target-owner",
        &[("read", vec![format!("board:{source_id}")])],
    );
    estate.ok_json(&target_work, &strs(&declare("t-b", "t-open")));
    assert_eq!(estate.pins(&target_path, "t-b"), ["t-open"]);
    assert_eq!(
        estate.refused(&target_work, &strs(&declare("t-c", "t-hidden"))),
        format!("prerequisite boardID {source_id} id t-hidden {UNAVAILABLE}")
    );
    assert_eq!(
        estate.refused(&target_work, &strs(&declare("t-c", "t-absent"))),
        format!("prerequisite boardID {source_id} id t-absent {UNAVAILABLE}")
    );

    // Granting the tag reveals the tagged row.
    estate.grant(
        "p-target-owner",
        &[(
            "read",
            vec![format!("board:{source_id}"), "tag:secret".to_owned()],
        )],
    );
    estate.ok_json(&target_work, &strs(&declare("t-c", "t-hidden")));
    assert_eq!(estate.pins(&target_path, "t-c"), ["t-hidden"]);
}

fn strs(owned: &[String]) -> Vec<&str> {
    owned.iter().map(String::as_str).collect()
}

fn incarnation(estate: &Estate, board_id: &str, task: &str) -> String {
    readonly(&estate.board_file(board_id))
        .query_row("SELECT incarnation FROM tasks WHERE id=?", [task], |row| {
            row.get(0)
        })
        .unwrap()
}

fn event_count(estate: &Estate, board: &str, task: &str) -> usize {
    estate
        .ok_json(
            &estate.workspace(board),
            &["events", "--task", task, "--json"],
        )
        .as_array()
        .unwrap()
        .len()
}

#[test]
fn a_reconcile_import_that_replaces_content_rotates_the_incarnation() {
    let estate = Estate::new("reconcile");
    let source = estate.board("Source");
    let target = estate.board("Target");
    let file = estate.root.join("atmux.json");
    let write = |subject: &str, status: &str| {
        fs::write(
            &file,
            serde_json::to_vec(&json!({
                "epics": [],
                "stories": [],
                "tasks": [{
                    "id": "t-imp", "subject": subject, "status": status,
                    "createdAt": 1700000000
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    };
    let path = file.to_str().unwrap().to_owned();
    let import = |reconcile: bool| {
        let mut args = vec![
            "import",
            "atmux-json",
            path.as_str(),
            "--as",
            ACTOR,
            "--json",
        ];
        if reconcile {
            args.push("--reconcile");
        }
        estate.ok_json(&estate.workspace("Source"), &args);
    };
    write("Original", "todo");
    import(false);
    estate.add_ok(
        "Target",
        "t-w",
        &["--depends-on-json", &edge_json(&source, "t-imp")],
    );
    let pinned = pins(&estate, &target, "t-w")[0].3.clone();
    assert_eq!(incarnation(&estate, &source, "t-imp"), pinned);

    // A state-only reconcile is the same item: the token stays.
    write("Original", "done");
    import(true);
    assert_eq!(incarnation(&estate, &source, "t-imp"), pinned);

    // Replacing the content makes a different item: a fresh token, and the
    // stored pin keeps naming the old one, so it can never match again.
    write("Replaced", "done");
    import(true);
    let rotated = incarnation(&estate, &source, "t-imp");
    assert_ne!(rotated, pinned);
    assert_eq!(rotated.len(), 32);
    assert_eq!(
        pins(&estate, &target, "t-w")[0].3,
        pinned,
        "the pin never rebinds"
    );
}

#[test]
fn a_refused_foreign_entry_leaves_an_update_completely_unwritten() {
    let estate = Estate::new("update-atomic");
    let target = estate.board("Target");
    estate.add_ok("Target", "t-a", &[]);
    estate.add_ok("Target", "t-b", &[]);
    estate.add_ok("Target", "t-row", &["--depends-on", "t-a"]);
    let before = event_count(&estate, "Target", "t-row");
    let unknown = "0f8fad5b-d9cb-469f-a165-70867728950e";
    // The own-UUID half is valid on its own; the foreign half is not.
    let set = json!([
        { "boardID": target, "id": "t-b" },
        { "boardID": unknown, "id": "t-x" }
    ])
    .to_string();
    let refusal = estate.refused(
        &estate.workspace("Target"),
        &[
            "task",
            "update",
            "t-row",
            "--as",
            ACTOR,
            "--title",
            "renamed",
            "--depends-on-json",
            &set,
            "--json",
        ],
    );
    assert_eq!(
        refusal,
        format!("prerequisite boardID {unknown} id t-x {UNAVAILABLE}")
    );
    assert_eq!(local_edges(&estate, &target, "t-row"), ["t-a"]);
    assert!(pins(&estate, &target, "t-row").is_empty());
    let title: String = readonly(&estate.board_file(&target))
        .query_row("SELECT title FROM tasks WHERE id='t-row'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(title, "t-row", "no half of the update landed");
    assert_eq!(event_count(&estate, "Target", "t-row"), before);
}
