//! Linked companions and bounded claim scope (slice LINKED, row `t-0dcbb1a9`)
//! at the compiled-process boundary.
//!
//! Spec: `docs/specs/linked.md` acceptance A1-A4 (`LINKED-01`..`LINKED-08`,
//! `LINKED-10`..`LINKED-14`). Every test spawns the real `kanban` binary
//! against real SQLite files in a temp `KANBAN_DATA_DIR`, following
//! `tests/cross_board_e2e.rs` (two-board estate) and
//! `tests/identity_e2e.rs` (binary invocation, refusal helpers).
//!
//! Out of scope here and owned elsewhere: contributions and delivery evidence
//! (`t-9eff9257`, `LINKED-15`..`LINKED-21`) and CLI/MCP agreement, recovery,
//! and the sibling-slice complement (`t-db6937ba`, `LINKED-22`, `LINKED-24`,
//! `LINKED-25`).

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const OP: &str = "op";
const WORKER: &str = "w";
const LANE: &str = "driver";
const SESSION: &str = "s1";

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
            "kanban-linked-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        Self { root, data }
    }

    /// Re-address the same root after every process has exited: the restart
    /// half of A1. Double `Drop` is safe; the second removal is discarded.
    fn reopen(&self) -> Self {
        Self {
            root: self.root.clone(),
            data: self.data.clone(),
        }
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
            .env_remove("KANBAN_WORKER_CREDENTIAL")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
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

    /// A refused command: nonzero, and its one error line — the `--json`
    /// `error` field when the command answers in JSON, else the raw output.
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

    fn add(&self, board: &str, id: &str, extra: &[&str]) {
        self.add_titled(board, id, id, extra);
    }

    /// `task add TITLE --id ID`: the title is one argv entry, whatever it holds.
    fn add_titled(&self, board: &str, id: &str, title: &str, extra: &[&str]) {
        let work = self.workspace(board);
        let mut args = vec!["task", "add", title, "--id", id, "--as", OP, "--json"];
        args.extend_from_slice(extra);
        let output = self.run(&work, &args);
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

fn user_version(path: &Path) -> i64 {
    readonly(path)
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn row_count(path: &Path, sql: &str) -> i64 {
    readonly(path).query_row(sql, [], |row| row.get(0)).unwrap()
}

/// The sorted endpoint set of one `link show` answer, for side-to-side
/// comparison: `((boardID, id, incarnation), ...)` plus the pairing id.
fn pairing_shape(shown: &Value) -> (String, Vec<(String, String, String)>) {
    let pairing = shown.as_array().unwrap().first().unwrap();
    let mut endpoints = pairing["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|endpoint| {
            (
                endpoint["boardID"].as_str().unwrap().to_owned(),
                endpoint["id"].as_str().unwrap().to_owned(),
                endpoint["incarnation"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    endpoints.sort();
    (pairing["pairingID"].as_str().unwrap().to_owned(), endpoints)
}

/// The incarnation a pairing receipt pins for the endpoint on `board_id`:
/// endpoints are stored in `(boardID, id)` order, so either board may be A.
fn incarnation_on(receipt: &Value, board_id: &str) -> String {
    ["boardA", "boardB"]
        .iter()
        .map(|side| &receipt[*side])
        .find(|endpoint| endpoint["boardID"] == board_id)
        .unwrap_or_else(|| panic!("no endpoint on board {board_id}: {receipt}"))["incarnation"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A1 — Pair two disposable boards; both directions read identical after a
/// restart (`LINKED-01`, `LINKED-02`, `LINKED-03`, `LINKED-05`).
#[test]
fn pairing_roundtrip_reads_identical_from_both_sides_after_restart() {
    let estate = Estate::new("a1");
    let unum = estate.board("unum");
    let acies = estate.board("acies");
    estate.add("unum", "t-u1", &[]);
    estate.add("acies", "t-a1", &[]);
    assert_eq!(user_version(&estate.data.join("registry.db")), 18);

    let added = estate.ok_json(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    let pairing_id = added["pairingID"].as_str().unwrap().to_owned();

    // Either side projects the same pair with the same attribution.
    let from_unum = estate.ok_json(
        &estate.root,
        &["link", "show", "--board", &unum, "--id", "t-u1", "--json"],
    );
    let from_acies = estate.ok_json(
        &estate.root,
        &["link", "show", "--board", &acies, "--id", "t-a1", "--json"],
    );
    assert_eq!(pairing_shape(&from_unum), pairing_shape(&from_acies));
    assert_eq!(pairing_shape(&from_unum).0, pairing_id);

    // The identical write retried answers the stored record: still one row.
    let replay = estate.ok_json(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &acies,
            "--a-id",
            "t-a1",
            "--b-board",
            &unum,
            "--b-id",
            "t-u1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert_eq!(replay["pairingID"].as_str().unwrap(), pairing_id);
    assert_eq!(
        row_count(
            &estate.data.join("registry.db"),
            "SELECT COUNT(*) FROM linked_companions"
        ),
        1,
        "retrying the identical pairing writes no second row"
    );

    // After every process has exited and the estate is reopened, both reads
    // are byte-identical to before: the pairing is durable state.
    let reopened = estate.reopen();
    let after_unum = reopened.ok_json(
        &reopened.root,
        &["link", "show", "--board", &unum, "--id", "t-u1", "--json"],
    );
    let after_acies = reopened.ok_json(
        &reopened.root,
        &["link", "show", "--board", &acies, "--id", "t-a1", "--json"],
    );
    assert_eq!(after_unum, from_unum);
    assert_eq!(after_acies, from_acies);
}

/// A2 — Unknown, denied, renamed, and recreated endpoints cannot move a
/// pairing (`LINKED-02`, `LINKED-04`, `LINKED-06`).
#[test]
fn refusals_hide_cause_and_content_and_pins_survive_recreation() {
    let estate = Estate::new("a2");
    let unum = estate.board("unum");
    let acies = estate.board("acies");
    estate.add("unum", "t-u1", &[]);
    estate.add_titled("acies", "t-a1", "classified-alpha-title", &[]);
    let added = estate.ok_json(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    let pairing_id = added["pairingID"].as_str().unwrap().to_owned();
    let pinned = incarnation_on(&added, &acies);

    // A display name is not identity: refused naming the (boardID, id) shape.
    let shape = estate.refused(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            "unum",
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(
        shape.contains("(boardID, id)"),
        "display-name write names the required shape: {shape}"
    );

    // Unknown board, unknown item, and foreign registry answer the one
    // sentence — and none of them carries a content byte.
    let unknown_board = estate.refused(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            "00000000-0000-0000-0000-000000000000",
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    let unknown_item = estate.refused(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-nope",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    let foreign = Estate::new("a2-foreign");
    let other = foreign.board("other");
    let foreign_board = estate.refused(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &other,
            "--a-id",
            "t-x",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert_eq!(
        unknown_board, unknown_item,
        "unknown and denied refuse indistinguishably"
    );
    assert!(
        unknown_board.contains("unavailable to this caller"),
        "one uniform refusal: {unknown_board}"
    );
    for refusal in [&unknown_board, &unknown_item, &foreign_board] {
        assert!(
            !refusal.contains("classified-alpha-title"),
            "no content byte leaks into a refusal: {refusal}"
        );
    }
    assert_eq!(
        row_count(
            &estate.data.join("registry.db"),
            "SELECT COUNT(*) FROM linked_companions"
        ),
        1,
        "refused writes land no row"
    );

    // An item cannot pair with itself.
    let selfie = estate.refused(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &unum,
            "--b-id",
            "t-u1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(selfie.contains("itself"), "self-pairing refuses: {selfie}");

    // Retiring the Acies item leaves the pairing pointing at the pinned
    // incarnation: it reads retired, never retargeted, and nothing new.
    let work_acies = estate.workspace("acies");
    estate.ok_json(
        &work_acies,
        &["task", "remove", "t-a1", "--as", OP, "--json"],
    );
    let retired = estate.ok_json(
        &estate.root,
        &["link", "show", "--board", &unum, "--id", "t-u1", "--json"],
    );
    let (_, endpoints) = pairing_shape(&retired);
    assert_eq!(endpoints.len(), 2);
    let acies_end = endpoints
        .iter()
        .find(|(board, _, _)| board == &acies)
        .unwrap();
    assert_eq!(acies_end.2, pinned, "the pin never moves");
    let states: Vec<&str> = retired[0]["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|endpoint| endpoint["state"].as_str().unwrap())
        .collect();
    assert!(
        states.contains(&"retired"),
        "the retired endpoint reads retired: {retired}"
    );

    // Recreating an item under the retired ID does not retarget the pairing.
    // The board now refuses to reuse a removed id, so the recreation is
    // planted the way history left it (as `tests/e2e.rs` plants its reused
    // id): a raw insert under the removed id, which the board mints a fresh
    // incarnation for. The new row reads recreated, and re-pairing while the
    // old row lives is refused as a conflicting duplicate.
    let planted = Connection::open(estate.board_file(&acies)).unwrap();
    planted
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             DELETE FROM search_documents WHERE task_id='t-a1';
             INSERT INTO tasks(id,type,title,status,created_at,updated_at)
               VALUES('t-a1','task','replacement-title','todo',1,1);",
        )
        .unwrap();
    drop(planted);
    let recreated = estate.ok_json(
        &estate.root,
        &["link", "show", "--board", &unum, "--id", "t-u1", "--json"],
    );
    let (_, endpoints) = pairing_shape(&recreated);
    let acies_end = endpoints
        .iter()
        .find(|(board, _, _)| board == &acies)
        .unwrap();
    assert_eq!(
        acies_end.2, pinned,
        "the pin never moves onto the recreated row"
    );
    let replacement: String = readonly(&estate.board_file(&acies))
        .query_row("SELECT incarnation FROM tasks WHERE id='t-a1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_ne!(
        replacement, pinned,
        "the recreated item carries a new incarnation"
    );
    let states: Vec<&str> = recreated[0]["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|endpoint| endpoint["state"].as_str().unwrap())
        .collect();
    assert!(
        states.contains(&"recreated"),
        "the reused ID reads recreated, never as its replacement: {recreated}"
    );
    assert!(
        !recreated.to_string().contains("replacement-title"),
        "no replacement content is projected: {recreated}"
    );
    let conflict = estate.refused(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(
        conflict.contains("already pairs"),
        "re-pairing over a live row refuses: {conflict}"
    );

    // The explicit re-pair — retire, then pair — lands on the new incarnation.
    estate.ok_json(
        &estate.root,
        &[
            "link",
            "remove",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--reason",
            "re-pairing",
            "--json",
        ],
    );
    let repaired = estate.ok_json(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert_ne!(
        repaired["pairingID"].as_str().unwrap(),
        pairing_id,
        "the re-pair is a new stored row"
    );
    assert_eq!(
        incarnation_on(&repaired, &acies),
        replacement,
        "the explicit re-pair pins the recreated incarnation"
    );
}

/// A3 — A bound worker takes its selected task and is refused everywhere else
/// (`LINKED-07`, `LINKED-08`, `LINKED-10`).
#[test]
fn bound_worker_takes_only_selected_tasks_on_every_path() {
    let estate = Estate::new("a3");
    let unum = estate.board("unum");
    estate.board("acies");
    let work = estate.workspace("unum");
    // A task contains nothing, so `t-in` sits in a story beside an unselected
    // story-mate: selecting a task selects neither its parent nor its
    // parent's other children (LINKED-10).
    estate.add("unum", "s-par", &["--type", "story"]);
    estate.add("unum", "t-in", &["--parent", "s-par"]);
    estate.add("unum", "t-out", &[]);
    estate.add("unum", "t-mate", &["--parent", "s-par"]);
    // The neighbour carries a depends-on edge whose gate is satisfied, so the
    // scope refusal — not a readiness gate — is what fires on it (LINKED-10:
    // a relation never implies membership).
    estate.add("unum", "t-base", &["--status", "done"]);
    estate.add("unum", "t-nbr", &["--depends-on", "t-base"]);
    // A second neighbour whose edge is still open keeps refusing on the
    // existing readiness gate beside the new scope gate (LINKED-09).
    estate.add("unum", "t-gated", &["--depends-on", "t-in"]);

    estate.ok_json(
        &estate.root,
        &["scope", "create", "--set", "joint-3", "--as", OP, "--json"],
    );
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "add",
            "--set",
            "joint-3",
            "--board",
            &unum,
            "--id",
            "t-in",
            "--expect-revision",
            "1",
            "--as",
            OP,
            "--json",
        ],
    );
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "bind",
            "--set",
            "joint-3",
            "--actor",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--expect-revision",
            "2",
            "--as",
            OP,
            "--json",
        ],
    );

    // Candidates offer the selected task and none of the others — not the
    // sibling, not the unselected story-mate, not the depends-on neighbour.
    let candidates = estate.ok_json(
        &work,
        &[
            "claim",
            "--candidates",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--json",
        ],
    );
    let offered = candidates
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(offered, vec!["t-in".to_owned()]);

    // The selected claim succeeds, and its event records the binding and the
    // revision it was checked against.
    let claimed = estate.ok_json(
        &work,
        &[
            "claim",
            "t-in",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    );
    let lease = claimed["leaseToken"].as_str().unwrap().to_owned();
    let payload: Value = readonly(&estate.board_file(&unum))
        .query_row(
            "SELECT payload FROM events WHERE kind='task_claimed' ORDER BY seq DESC LIMIT 1",
            [],
            |row| {
                let raw: String = row.get(0)?;
                Ok(serde_json::from_str(&raw).unwrap())
            },
        )
        .unwrap();
    assert_eq!(payload["scopeSet"], Value::from("joint-3"));
    assert_eq!(payload["scopeRevision"], Value::from(3));

    // Every other path refuses with the same sentence: the sibling, the
    // story-mate and the neighbour even while the selected task is held.
    let mut same_sentence = Vec::new();
    for task in ["t-out", "t-mate", "t-nbr"] {
        same_sentence.push(estate.refused(
            &work,
            &[
                "claim",
                task,
                "--as",
                WORKER,
                "--lane",
                LANE,
                "--session",
                SESSION,
                "--json",
            ],
        ));
    }
    // `--next` with an emptied pool reports the same gate in pool wording: it
    // names the set, but there is no task for the named-task sentence.
    let next_empty = estate.refused(
        &work,
        &[
            "claim",
            "--next",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    );
    assert!(
        next_empty.contains("selected set joint-3"),
        "an emptied pool names the set: {next_empty}"
    );
    // The still-open neighbour keeps refusing on its existing readiness gate,
    // unchanged beside the scope gate (LINKED-09).
    let gated = estate.refused(
        &work,
        &[
            "claim",
            "t-gated",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    );
    assert!(
        gated.contains("gated on work that is not done"),
        "existing gates keep refusing: {gated}"
    );
    // A lease-taking handoff for the sibling: take it as an unbound lane,
    // offer it over, and watch the bound worker's accept stay pending.
    let held = estate.ok_json(&work, &["claim", "t-out", "--as", "op2", "--json"]);
    let op_lease = held["leaseToken"].as_str().unwrap().to_owned();
    let handoff = estate.ok_json(
        &work,
        &[
            "handoff",
            "create",
            "t-out",
            "--lease",
            &op_lease,
            "--as",
            "op2",
            "--summary",
            "take it",
            "--intent",
            "cover",
            "--next-action",
            "claim",
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
    let handoff_id = handoff["id"].as_str().unwrap().to_owned();
    same_sentence.push(estate.refused(
        &work,
        &[
            "handoff",
            "accept",
            &handoff_id,
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    ));
    let listed = estate.ok_json(&work, &["handoff", "list", "--json"]);
    let status = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == handoff_id)
        .unwrap()["status"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(status, "pending", "an out-of-set accept grants no lease");
    // Both resumption entry points refuse onto the sibling with the same words.
    // The pending handoff leaves `t-out` assigned to op2; `--allow-reassign`
    // lifts that existing gate (CLAIM-07) so the scope gate is the one that
    // answers, with the same words as every other path.
    same_sentence.push(estate.refused(
        &work,
        &[
            "claim",
            "t-out",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--allow-reassign",
            "--json",
        ],
    ));
    same_sentence.push(estate.refused(
        &work,
        &[
            "handoff",
            "accept",
            &handoff_id,
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    ));
    for refusal in &same_sentence {
        assert!(
            refusal.contains("selected set joint-3"),
            "every path names the set: {refusal}"
        );
    }
    let normalized = same_sentence
        .iter()
        .map(|refusal| {
            refusal
                .replace("t-out", "T")
                .replace("t-mate", "T")
                .replace("t-nbr", "T")
        })
        .collect::<Vec<_>>();
    assert!(
        normalized.windows(2).all(|pair| pair[0] == pair[1]),
        "one gate, one sentence on every named path: {normalized:#?}"
    );

    // The live lease is untouched: heartbeat answers, then release ends it.
    estate.ok_json(&work, &["heartbeat", "t-in", "--lease", &lease, "--json"]);
    estate.ok_json(&work, &["release", "t-in", "--lease", &lease, "--json"]);
}

/// A4 — Revocation, races, freezes, and rebinds serialize under authority
/// (`LINKED-11`, `LINKED-12`, `LINKED-13`, `LINKED-14`).
#[test]
fn revocation_freeze_and_rebind_serialize_under_authority() {
    let estate = Estate::new("a4");
    let unum = estate.board("unum");
    let work = estate.workspace("unum");
    for task in ["t-1", "t-2", "t-3", "t-4", "t-5"] {
        estate.add("unum", task, &[]);
    }

    estate.ok_json(
        &estate.root,
        &["scope", "create", "--set", "joint-4", "--as", OP, "--json"],
    );
    for (id, revision) in [("t-1", "1"), ("t-2", "2")] {
        estate.ok_json(
            &estate.root,
            &[
                "scope",
                "add",
                "--set",
                "joint-4",
                "--board",
                &unum,
                "--id",
                id,
                "--expect-revision",
                revision,
                "--as",
                OP,
                "--json",
            ],
        );
    }
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "bind",
            "--set",
            "joint-4",
            "--actor",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--expect-revision",
            "3",
            "--as",
            OP,
            "--json",
        ],
    );

    // The claim granted before the revocation lands keeps its lease.
    let claimed = estate.ok_json(
        &work,
        &[
            "claim",
            "t-1",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    );
    let lease = claimed["leaseToken"].as_str().unwrap().to_owned();
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "revoke",
            "--set",
            "joint-4",
            "--actor",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--reason",
            "misuse",
            "--expect-revision",
            "4",
            "--as",
            OP,
            "--json",
        ],
    );
    // ...but the revoked triple takes nothing further, by any path.
    let second = estate.refused(
        &work,
        &[
            "claim",
            "t-2",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    );
    assert!(
        second.contains("was revoked") && second.contains("joint-4"),
        "revocation ends taking: {second}"
    );
    estate.ok_json(&work, &["heartbeat", "t-1", "--lease", &lease, "--json"]);
    // A revoked lease ends only via release, never by handoff onward.
    let onward = estate.refused(
        &work,
        &[
            "handoff",
            "create",
            "t-1",
            "--lease",
            &lease,
            "--as",
            WORKER,
            "--summary",
            "pass",
            "--intent",
            "pass",
            "--next-action",
            "pass",
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
    assert!(
        onward.contains("ends only via `release"),
        "revoked leases cannot be handed on: {onward}"
    );

    // Concurrent membership revisions serialize on the revision check: the
    // stale one is refused whole and names the current revision.
    let stale = estate.refused(
        &estate.root,
        &[
            "scope",
            "add",
            "--set",
            "joint-4",
            "--board",
            &unum,
            "--id",
            "t-3",
            "--expect-revision",
            "4",
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(
        stale.contains("at revision 5, not 4"),
        "stale revision refuses: {stale}"
    );
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "add",
            "--set",
            "joint-4",
            "--board",
            &unum,
            "--id",
            "t-3",
            "--expect-revision",
            "5",
            "--as",
            OP,
            "--json",
        ],
    );
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "add",
            "--set",
            "joint-4",
            "--board",
            &unum,
            "--id",
            "t-4",
            "--expect-revision",
            "6",
            "--as",
            OP,
            "--json",
        ],
    );
    let raced = estate.refused(
        &estate.root,
        &[
            "scope",
            "add",
            "--set",
            "joint-4",
            "--board",
            &unum,
            "--id",
            "t-5",
            "--expect-revision",
            "6",
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(
        raced.contains("at revision 7, not 6"),
        "exactly one racing revision wins: {raced}"
    );

    // A revoked actor takes nothing under a relabeled lane either: the
    // revocation stands actor-wide until an audited rebind re-arms it.
    let relabeled = estate.refused(
        &work,
        &[
            "claim",
            "t-2",
            "--as",
            WORKER,
            "--lane",
            "other",
            "--session",
            SESSION,
            "--json",
        ],
    );
    assert!(
        relabeled.contains("was revoked"),
        "revocation bars relabeled triples: {relabeled}"
    );

    // Rebinding the revoked triple re-arms it through a new audited revision.
    let rearmed = estate.ok_json(
        &estate.root,
        &[
            "scope",
            "bind",
            "--set",
            "joint-4",
            "--actor",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--expect-revision",
            "7",
            "--as",
            OP,
            "--json",
        ],
    );
    assert_eq!(rearmed["kind"], Value::from("rebind"));

    // Freezing parks taking while holding runs on; only an audited unfreeze
    // thaws it. The worker is bound again, so the frozen sentence is what fires.
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "freeze",
            "--set",
            "joint-4",
            "--expect-revision",
            "8",
            "--as",
            OP,
            "--json",
        ],
    );
    let frozen = estate.refused(
        &work,
        &[
            "claim",
            "t-3",
            "--as",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--json",
        ],
    );
    assert!(
        frozen.contains("is frozen"),
        "post-freeze claims name the freeze: {frozen}"
    );
    estate.ok_json(&work, &["heartbeat", "t-1", "--lease", &lease, "--json"]);
    estate.ok_json(&work, &["release", "t-1", "--lease", &lease, "--json"]);
    estate.ok_json(
        &estate.root,
        &[
            "scope",
            "unfreeze",
            "--set",
            "joint-4",
            "--expect-revision",
            "9",
            "--as",
            OP,
            "--json",
        ],
    );

    // A different lane naming the same agent inherits nothing.
    let lookalike = estate.refused(
        &work,
        &[
            "claim",
            "t-2",
            "--as",
            WORKER,
            "--lane",
            "other",
            "--session",
            SESSION,
            "--json",
        ],
    );
    assert!(
        lookalike.contains("matches no live binding"),
        "the triple mismatch names itself: {lookalike}"
    );

    // Every change sits in the hash-chained log with author, reason, and the
    // full member list.
    let shown = estate.ok_json(
        &estate.root,
        &["scope", "show", "--set", "joint-4", "--json"],
    );
    assert_eq!(shown["revision"], Value::from(10));
    assert_eq!(shown["members"].as_array().unwrap().len(), 4);
    let audited = estate.ok_json(&estate.root, &["audit", "verify", "--json"]);
    assert_eq!(
        audited["healthy"],
        Value::from(true),
        "audit verify stays healthy across every revision"
    );
}
