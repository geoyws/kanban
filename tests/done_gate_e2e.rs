//! The done-gate (slice DG), at the compiled-process boundary.
//!
//! Spec: `docs/specs/done-gate.md` (`SPEC-READY`, DG-01..DG-16, seven verbatim
//! refusal sentences in DG-08, acceptance A1..A16, trace plan in §8). Every
//! test below spawns the real `kanban` binary against a real SQLite board in a
//! temp `KANBAN_DATA_DIR`, following `tests/secret_guard_e2e.rs`: a library
//! call in this process would not establish the thing being asserted, because
//! the refusals under test live in the store the binary writes through.
//!
//! Test names follow the §8 verification plan; each test cites the A-case it
//! proves. DG-04's open/missing/reopened citation negatives and the DG-08
//! seven-sentence sweep have no dedicated A-case, so they get their own tests
//! (`done_gate_requires_resolved_decision_citations`,
//! `done_gate_refusals_carry_named_reasons`).
//!
//! Contract with the implementation (`IMPL-NOTES.md` in the impl worktree,
//! read 2026-09-29; where it and the spec disagree, the spec wins):
//! - Grammar `task verdict add ID --reviewer ACTOR --sha SHA [--sha ...]
//!   --evidence ATT-ID [--evidence ...] --as WRITER` (records `pass` only;
//!   `--evidence` may be empty at write time but never satisfies),
//!   `task verdict gate on|off --as ACTOR` (geoyws-only), plus the extra
//!   read-only `task verdict list ID` (no `--as`) answering the stored rows
//!   oldest-first — the process-layer observation point for the verdict list.
//! - Verdict record shape: `{"taskID","writer","reviewer","shas","verdict":
//!   "pass","evidence","createdAt"}`; toggle receipt shape:
//!   `{"board","doneGate","oldValue","changedBy"}`.
//! - Events: `done_gate_override` (task-scoped; payload
//!   `{"priorStatus","reason"}` with `reason` in
//!   `missing-verdict|self-review|stale`) and `done_gate_toggled`
//!   (board-scoped, `task_id` null; payload `{"board","oldValue","newValue"}`).
//!   No event for a stored verdict, none for a refused write or move.
//! - Publication (DG-13) is an allowlist: env `KANBAN_PUBLISHED_SHAS`
//!   (comma/whitespace-separated full SHAs; when set, membership decides;
//!   when unset, every citing write is refused sentence 6). Shape first:
//!   anything not exactly 40 lowercase-hex chars is sentence 5. The gate
//!   re-validates stored SHAs at move time, so every child — add and move —
//!   gets the same env. The single hook is [`Estate::run`]; SHAs are
//!   [`H1`]/[`H2`] (published) and [`UNPUB`] (well-formed, never listed).
//! - No-head moves are refused with sentence 1; only `geoyws --force` opens
//!   them. Sentence 2's `{old}` is the newest stored verdict's SHA list.
//! - DG-08 sentence 7's `{board}` is read off the toggle receipt's `board`
//!   field at runtime (it equals the `init --name` value here).
//!
//! Assumed refusal shape (pre-existing, verified against `task move` of a
//! story and of a missing task): with `--json`, a refusal exits nonzero with
//! stdout `{"error": "<line>"}` where `<line>` is byte-exactly the DG-08
//! sentence. Also assumed: `task show` carries `status`/`completedAt`,
//! `claim` carries `leaseToken`, attention resolve uses the default
//! Approve/Reject pair (`--choice approve`), `task add` defaults status to
//! `todo` (all verified against the current binary).
//!
//! Negative controls: every refusal test also proves the refused shape would
//! otherwise succeed — either on an ungated twin estate ([`Estate::twin`] +
//! [`ungated_twin_moves_done`]: flag never turned on, so DG-09 governs) or via
//! an accepted twin write/move on the same board. If the gate were absent,
//! the refusal assertions would fail because the moves would succeed.
//!
//! These tests do NOT pass in a tree without the implementation (the binary
//! here has no `task verdict` verb and no gate); they must COMPILE:
//! `cargo test --offline --test done_gate_e2e --no-run`.

use serde_json::Value;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Board name given to `init --name`; doubles as DG-08 sentence 7's `{board}`.
const BOARD: &str = "DoneGate";

const EXEC: &str = "lane-e";
const REVIEWER: &str = "lane-r";
const PLANNER: &str = "planner";
const THIRD: &str = "lane-q";
const GEO: &str = "geoyws";

/// Full 40-char lowercase-hex SHAs. H1/H2 are "published" (see header);
/// UNPUB is well-formed but never published; SHORT/MALFORMED fail the shape.
const H1: &str = "1111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222";
const UNPUB: &str = "3333333333333333333333333333333333333333";
const SHORT: &str = "abc123";
const MALFORMED: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";

/// Space-separated SHAs every child treats as published on origin (hook).
const PUBLISHED: &str =
    "1111111111111111111111111111111111111111 2222222222222222222222222222222222222222";

// ---------------------------------------------------------------------------
// DG-08 sentence builders, verbatim from the spec (only bracketed values vary).
// ---------------------------------------------------------------------------

fn s1_no_verdict(id: &str) -> String {
    format!(
        "task {id} has no foreign-actor pass verdict — record one with \
         `task verdict add {id} --reviewer <actor> --sha <sha> --evidence <a-id> --as <planner>` \
         before moving it to done"
    )
}

fn s2_stale(id: &str, old: &str, head: &str) -> String {
    format!(
        "task {id} verdict is stale: it covered {old} but the row now stands at {head} — \
         record a fresh verdict at the new head with `task verdict add {id} --as <planner>`"
    )
}

fn s3_self_review(id: &str, actor: &str) -> String {
    format!(
        "task {id} verdict is self-review: {actor} is the claim holder or the closing actor \
         and may be neither the writer nor the reviewer — have the planner loop record a foreign-actor verdict"
    )
}

fn s4_force(id: &str) -> String {
    format!(
        "task {id} done-move past the gate needs `--force --as geoyws` — \
         only geoyws may override, and the override is recorded"
    )
}

fn s5_bad_sha(id: &str, sha: &str) -> String {
    format!(
        "task {id} verdict refused: `{sha}` is not a full 40-character commit SHA — \
         cite the published commit the reviewer checked"
    )
}

fn s6_unpublished(id: &str, sha: &str) -> String {
    format!(
        "task {id} verdict refused: `{sha}` is not published on origin — \
         the verdict must cite a commit SHA published on origin"
    )
}

fn s7_toggle(board: &str) -> String {
    format!(
        "done_gate on board {board} unchanged — only geoyws may turn the gate on or off; \
         run `task verdict gate on|off --as geoyws`"
    )
}

// ---------------------------------------------------------------------------
// Harness: one board in a temp data root, addressed by working directory.
// ---------------------------------------------------------------------------

struct Estate {
    root: PathBuf,
    data: PathBuf,
    work: PathBuf,
}

impl Estate {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-done-gate-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let work = root.join("work");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&work).unwrap();
        let estate = Self { root, data, work };
        estate.ok_json(&["init", "--name", BOARD, "--json"]);
        estate
    }

    /// A twin board on which the gate is never turned on (DG-09 governs).
    fn twin(label: &str) -> Self {
        Self::new(label)
    }

    fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(&self.work)
            .args(args)
            .env("KANBAN_DATA_DIR", &self.data)
            .env("KANBAN_PUBLISHED_SHAS", PUBLISHED)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().unwrap()
    }

    fn ok_json<S: AsRef<OsStr> + std::fmt::Debug>(&self, args: &[S]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "success stdout is not JSON: {error}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    /// The refusal's `error` field (stdout JSON), asserting failure first.
    fn refusal<S: AsRef<OsStr> + std::fmt::Debug>(&self, args: &[S]) -> String {
        let output = self.run(args);
        assert!(
            !output.status.success(),
            "{args:?} succeeded but must be refused\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "refusal stdout is not JSON: {error}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        value["error"]
            .as_str()
            .unwrap_or_else(|| panic!("refusal has no error field: {value}"))
            .to_owned()
    }

    /// Byte-exact DG-08 assertion: the refusal line equals `expected` exactly.
    fn refuse_exact<S: AsRef<OsStr> + std::fmt::Debug>(&self, args: &[S], expected: &str) {
        let actual = self.refusal(args);
        assert_eq!(actual, expected, "refusal wording differs byte-for-byte");
    }

    /// Toggle the gate; returns the toggle receipt
    /// (`{"board","doneGate","oldValue","changedBy"}` per IMPL-NOTES.md).
    /// The `board` field is the source of DG-08 sentence 7's `{board}`.
    fn gate_on(&self) -> Value {
        self.ok_json(&["task", "verdict", "gate", "on", "--as", GEO, "--json"])
    }

    fn gate_off(&self) -> Value {
        self.ok_json(&["task", "verdict", "gate", "off", "--as", GEO, "--json"])
    }

    /// Stored verdicts for `id`, oldest first (`task verdict list`, an extra
    /// read-only verb per IMPL-NOTES.md; takes no `--as`).
    fn verdicts(&self, id: &str) -> Value {
        self.ok_json(&["task", "verdict", "list", id, "--json"])
    }

    fn add_task(&self, title: &str, id: &str, extra: &[&str]) {
        let mut args = vec!["task", "add", title, "--id", id, "--json"];
        args.extend_from_slice(extra);
        self.ok_json(&args);
    }

    fn claim(&self, id: &str, holder: &str) -> String {
        self.ok_json(&["claim", id, "--as", holder, "--json"])["leaseToken"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Record provenance head `head` for `id` via an explicit-flag checkpoint.
    fn checkpoint(&self, id: &str, lease: &str, author: &str, head: &str) {
        let repo = self.work.to_str().unwrap().to_owned();
        let args = vec![
            "checkpoint",
            id,
            "--lease",
            lease,
            "--as",
            author,
            "--summary",
            "worked the row",
            "--intent",
            "advance the task",
            "--next-action",
            "verify and close",
            "--state",
            "continue",
            "--repo",
            &repo,
            "--branch",
            "main",
            "--head",
            head,
            "--dirty",
            "clean",
            "--json",
        ];
        self.ok_json(&args);
    }

    fn release(&self, id: &str, lease: &str) {
        self.ok_json(&["release", id, "--lease", lease, "--json"]);
    }

    fn heartbeat(&self, id: &str, lease: &str) {
        self.ok_json(&["heartbeat", id, "--lease", lease, "--json"]);
    }

    /// Raise and resolve a decision attention row against `id`; returns its id.
    fn resolved_attention(&self, id: &str) -> String {
        let raised = self.ok_json(&[
            "attention",
            "raise",
            &format!("reviewed the pinned diff for {id}"),
            "--as",
            PLANNER,
            "--kind",
            "decision",
            "--task",
            id,
            "--json",
        ]);
        let aid = raised["id"].as_str().unwrap().to_owned();
        let settled = self.ok_json(&[
            "attention",
            "resolve",
            &aid,
            "--as",
            PLANNER,
            "--choice",
            "approve",
            "--note",
            "checked the pinned diff",
            "--json",
        ]);
        assert_eq!(settled["status"], "resolved");
        aid
    }

    fn raise_open_attention(&self, id: &str) -> String {
        self.ok_json(&[
            "attention",
            "raise",
            &format!("unreviewed question on {id}"),
            "--as",
            PLANNER,
            "--kind",
            "decision",
            "--task",
            id,
            "--json",
        ])["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// `task verdict add` (caller asserts ok vs refusal). Empty `shas` /
    /// `evidence` entries are omitted so A7 can pass no `--evidence` at all.
    fn verdict(
        &self,
        id: &str,
        reviewer: &str,
        shas: &[&str],
        evidence: &[&str],
        writer: &str,
    ) -> Output {
        let mut args: Vec<String> = vec![
            "task".into(),
            "verdict".into(),
            "add".into(),
            id.into(),
            "--reviewer".into(),
            reviewer.into(),
        ];
        for sha in shas {
            args.push("--sha".into());
            args.push((*sha).into());
        }
        for aid in evidence {
            args.push("--evidence".into());
            args.push((*aid).into());
        }
        args.push("--as".into());
        args.push(writer.into());
        args.push("--json".into());
        self.run(&args)
    }

    fn verdict_ok(
        &self,
        id: &str,
        reviewer: &str,
        shas: &[&str],
        evidence: &[&str],
        writer: &str,
    ) -> Value {
        let output = self.verdict(id, reviewer, shas, evidence, writer);
        assert!(
            output.status.success(),
            "verdict write should have succeeded\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn show(&self, id: &str) -> Value {
        self.ok_json(&["task", "show", id, "--json"])
    }

    fn task_events(&self, id: &str) -> Value {
        self.ok_json(&["events", "--task", id, "--json"])
    }

    fn all_events(&self) -> Value {
        self.ok_json(&["events", "--json"])
    }

    /// Row + task-scoped event history + stored verdict list, serialized: a
    /// refused write must leave all three byte-identical (DG-01: row, lease
    /// and event history unchanged; DG-11: a refused write stores nothing).
    fn snapshot(&self, id: &str) -> (String, String, String) {
        (
            serde_json::to_string(&self.show(id)).unwrap(),
            serde_json::to_string(&self.task_events(id)).unwrap(),
            serde_json::to_string(&self.verdicts(id)).unwrap(),
        )
    }

    fn assert_unchanged(&self, id: &str, before: &(String, String, String)) {
        let after = self.snapshot(id);
        assert_eq!(after.0, before.0, "refused write changed the row");
        assert_eq!(after.1, before.1, "refused write changed the event history");
        assert_eq!(after.2, before.2, "refused write changed the verdict list");
    }
}

impl Drop for Estate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
/// Newest-first event arrays: the entries `after` adds over `before`.
fn fresh_events<'a>(before: &Value, after: &'a Value) -> &'a [Value] {
    let previous = before.as_array().unwrap().len();
    let current = after.as_array().unwrap();
    assert!(
        current.len() >= previous,
        "event history shrank: {before} -> {after}"
    );
    &current[..current.len() - previous]
}

/// Negative control shared by the refusal tests: on a board where the gate was
/// never turned on, an ordinary claimed done-move succeeds exactly as at the
/// baseline (DG-09). If the gate were absent, the refusal under test would be
/// this success instead.
fn ungated_twin_moves_done(label: &str) {
    let twin = Estate::twin(label);
    twin.add_task("twin work", "t-twin", &[]);
    twin.claim("t-twin", EXEC);
    twin.ok_json(&["task", "move", "t-twin", "done", "--as", EXEC, "--json"]);
    assert_eq!(twin.show("t-twin")["status"], "done");
}

// ---------------------------------------------------------------------------
// A1 (DG-01, DG-08, DG-15): no verdict → sentence 1, nothing changes.
// ---------------------------------------------------------------------------

#[test]
fn a1_done_gate_refuses_done_move_without_verdict() {
    let estate = Estate::new("a1");
    estate.gate_on();
    estate.add_task("ungated work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    let before = estate.snapshot("t-x");
    let events_before = estate.task_events("t-x").as_array().unwrap().len();

    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-x"),
    );
    estate.assert_unchanged("t-x", &before);
    assert_eq!(estate.show("t-x")["status"], "todo");
    assert!(estate.show("t-x")["completedAt"].is_null());
    assert_eq!(
        estate.task_events("t-x").as_array().unwrap().len(),
        events_before
    );

    // Negative control: the same move succeeds with the gate off (DG-09).
    estate.gate_off();
    estate.ok_json(&["task", "move", "t-x", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-x")["status"], "done");
    ungated_twin_moves_done("a1-twin");
}

// ---------------------------------------------------------------------------
// A2 (DG-02, DG-04, DG-11, DG-12): foreign-actor pass → write stores one row,
// move succeeds, verdict stays attached.
// ---------------------------------------------------------------------------

#[test]
fn a2_done_gate_verdict_add_verb_records_pass_only() {
    let estate = Estate::new("a2");
    estate.gate_on();
    estate.add_task("reviewed work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    let a1 = estate.resolved_attention("t-x");
    let a2 = estate.resolved_attention("t-x");

    // The write answers the stored record with the exact field shape from
    // IMPL-NOTES.md, and `task verdict list` holds exactly that one row.
    let stored: Value = estate.verdict_ok("t-x", REVIEWER, &[H1], &[&a1, &a2], PLANNER);
    assert_eq!(stored["taskID"], "t-x");
    assert_eq!(stored["writer"], PLANNER);
    assert_eq!(stored["reviewer"], REVIEWER);
    assert_eq!(stored["shas"], serde_json::json!([H1]));
    assert_eq!(stored["verdict"], "pass");
    assert_eq!(stored["evidence"], serde_json::json!([&a1, &a2]));
    assert!(stored["createdAt"].is_number(), "no createdAt: {stored}");
    let listed = estate.verdicts("t-x");
    assert_eq!(
        listed.as_array().unwrap().len(),
        1,
        "expected one verdict row: {listed}"
    );
    assert_eq!(listed[0]["reviewer"], REVIEWER);
    assert_eq!(listed[0]["shas"], serde_json::json!([H1]));

    // Any actor may close once the gate is satisfied (the holder's lease is
    // released first so no lease rule stands in for the gate).
    estate.release("t-x", &lease);
    estate.ok_json(&["task", "move", "t-x", "done", "--as", THIRD, "--json"]);
    let done = estate.show("t-x");
    assert_eq!(done["status"], "done");
    assert!(!done["completedAt"].is_null(), "completed_at unset: {done}");
    // The verdict record is still attached to the row (the list is unchanged
    // by the close).
    let kept = estate.verdicts("t-x");
    assert_eq!(
        kept.as_array().unwrap().len(),
        1,
        "verdict lost on close: {kept}"
    );
    assert_eq!(kept[0]["reviewer"], REVIEWER);
    assert_eq!(kept[0]["shas"], serde_json::json!([H1]));

    ungated_twin_moves_done("a2-twin");
}

// ---------------------------------------------------------------------------
// A3 (DG-03, DG-08): reviewer is the (later) holder → sentence 3.
// The verdict is stored before the reviewer ever holds the claim: a
// holder-as-reviewer write is refused (DG-12), so only ordering makes the
// stored row possible, and the move must still refuse.
// ---------------------------------------------------------------------------

#[test]
fn a3_done_gate_refuses_self_review_verdict() {
    let estate = Estate::new("a3");
    estate.gate_on();
    estate.add_task("self-reviewed work", "t-x", &[]);
    let aid = estate.resolved_attention("t-x");
    // No claim exists yet, so the planner's write naming lane-e succeeds.
    estate.verdict_ok("t-x", EXEC, &[H1], &[&aid], PLANNER);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);

    let before = estate.snapshot("t-x");
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s3_self_review("t-x", EXEC),
    );
    estate.assert_unchanged("t-x", &before);

    // Negative controls: a foreign reviewer closes fine with the gate on, and
    // the self-review move succeeds with the gate off.
    estate.add_task("foreign work", "t-ok", &[]);
    let aid_ok = estate.resolved_attention("t-ok");
    let lease_ok = estate.claim("t-ok", EXEC);
    estate.checkpoint("t-ok", &lease_ok, EXEC, H1);
    estate.verdict_ok("t-ok", REVIEWER, &[H1], &[&aid_ok], PLANNER);
    estate.ok_json(&["task", "move", "t-ok", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-ok")["status"], "done");

    estate.gate_off();
    estate.ok_json(&["task", "move", "t-x", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-x")["status"], "done");
    ungated_twin_moves_done("a3-twin");
}

// ---------------------------------------------------------------------------
// A4 (DG-05, DG-06, DG-08): only geoyws --force overrides, audited.
// ---------------------------------------------------------------------------

#[test]
fn a4_done_gate_override_writes_audited_event() {
    let estate = Estate::new("a4");
    estate.gate_on();
    estate.add_task("unreviewed work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    estate.ok_json(&["task", "move", "t-x", "in_progress", "--as", EXEC, "--json"]);

    // A non-geoyws --force is refused with sentence 4 and changes nothing.
    let before = estate.snapshot("t-x");
    estate.refuse_exact(
        &[
            "task", "move", "t-x", "done", "--as", EXEC, "--force", "--json",
        ],
        &s4_force("t-x"),
    );
    estate.assert_unchanged("t-x", &before);

    // geoyws --force succeeds and writes exactly one audited override event
    // (IMPL-NOTES.md: kind `done_gate_override`, task-scoped, payload
    // `{"priorStatus","reason"}` with reason `missing-verdict` here).
    let events_before = estate.task_events("t-x");
    estate.ok_json(&[
        "task", "move", "t-x", "done", "--as", GEO, "--force", "--json",
    ]);
    assert_eq!(estate.show("t-x")["status"], "done");
    let events_after = estate.task_events("t-x");
    let fresh = fresh_events(&events_before, &events_after);
    let ovr: Vec<&Value> = fresh
        .iter()
        .filter(|e| e["kind"] == "done_gate_override")
        .collect();
    assert_eq!(
        ovr.len(),
        1,
        "expected one override event: {}",
        serde_json::to_string(&fresh).unwrap()
    );
    assert_eq!(ovr[0]["actor"], GEO);
    assert_eq!(ovr[0]["payload"]["priorStatus"], "in_progress");
    assert_eq!(ovr[0]["payload"]["reason"], "missing-verdict");
}

// ---------------------------------------------------------------------------
// A5 (DG-07, DG-12, DG-08): the executor (holder) cannot write the verdict.
// ---------------------------------------------------------------------------

#[test]
fn a5_done_gate_refuses_executor_written_verdict() {
    let estate = Estate::new("a5");
    estate.gate_on();
    estate.add_task("executor work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    let aid = estate.resolved_attention("t-x");
    let events_before = estate.snapshot("t-x");

    // Writer is the claim holder: refused with sentence 3, nothing stored.
    let refused = estate.verdict("t-x", REVIEWER, &[H1], &[&aid], EXEC);
    assert!(!refused.status.success());
    let value: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        value["error"].as_str().unwrap(),
        s3_self_review("t-x", EXEC)
    );
    estate.assert_unchanged("t-x", &events_before);

    // The gate is still closed afterwards.
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-x"),
    );

    // Negative control: the planner's identical write succeeds, the head never
    // advanced (the H1 verdict still satisfies), and the move goes through.
    estate.verdict_ok("t-x", REVIEWER, &[H1], &[&aid], PLANNER);
    estate.release("t-x", &lease);
    estate.ok_json(&["task", "move", "t-x", "done", "--as", THIRD, "--json"]);
    assert_eq!(estate.show("t-x")["status"], "done");
}

// ---------------------------------------------------------------------------
// A6 (DG-09, DG-10): gate off → baseline; gate on → only done is gated.
// ---------------------------------------------------------------------------

#[test]
fn a6_done_gate_off_leaves_move_unchanged() {
    // Gate off (flag absent): done and non-done moves behave as at baseline
    // with no verdict demanded and no new refusal reachable.
    let estate = Estate::new("a6");
    estate.add_task("plain work", "t-y1", &[]);
    estate.claim("t-y1", EXEC);
    let done_out = estate.ok_json(&["task", "move", "t-y1", "done", "--as", EXEC, "--json"]);
    assert_eq!(done_out["status"], "done");
    assert_eq!(estate.show("t-y1")["status"], "done");

    estate.add_task("more work", "t-y2", &[]);
    estate.claim("t-y2", EXEC);
    estate.ok_json(&["task", "move", "t-y2", "review", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-y2")["status"], "review");

    // With the gate on, non-done moves still never consult verdict state.
    estate.gate_on();
    estate.add_task("gated work", "t-g", &[]);
    let lease = estate.claim("t-g", EXEC);
    estate.checkpoint("t-g", &lease, EXEC, H1);
    estate.ok_json(&["task", "move", "t-g", "review", "--as", EXEC, "--json"]);
    estate.ok_json(&["task", "move", "t-g", "blocked", "--as", EXEC, "--json"]);
    estate.ok_json(&["task", "move", "t-g", "in_progress", "--as", EXEC, "--json"]);
    // ... while the done-move is refused (contrast proving the gate is on).
    estate.refuse_exact(
        &["task", "move", "t-g", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-g"),
    );

    ungated_twin_moves_done("a6-twin");
}

// ---------------------------------------------------------------------------
// A7 (DG-02, DG-08): a verdict with no evidence does not satisfy the gate.
// ---------------------------------------------------------------------------

#[test]
fn a7_done_gate_rejects_incomplete_verdict_record() {
    let estate = Estate::new("a7");
    estate.gate_on();
    estate.add_task("thin work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    let _aid = estate.resolved_attention("t-x");

    // A planner write with an empty evidence list is stored (IMPL-NOTES.md:
    // `--evidence` may be empty at write time) but never satisfies the gate.
    let stored: Value = estate.verdict_ok("t-x", REVIEWER, &[H1], &[], PLANNER);
    assert_eq!(stored["evidence"], serde_json::json!([]));
    let listed = estate.verdicts("t-x");
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["evidence"], serde_json::json!([]));

    let before = estate.snapshot("t-x");
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-x"),
    );
    estate.assert_unchanged("t-x", &before);

    ungated_twin_moves_done("a7-twin");
}

// ---------------------------------------------------------------------------
// A8 (DG-13, DG-08): short/malformed → sentence 5; unpublished → sentence 6.
// ---------------------------------------------------------------------------

#[test]
fn a8_done_gate_refuses_short_and_unpublished_shas() {
    let estate = Estate::new("a8");
    estate.gate_on();
    estate.add_task("sha work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    let aid = estate.resolved_attention("t-x");
    let before = estate.snapshot("t-x");

    for bad in [SHORT, MALFORMED] {
        let refused = estate.verdict("t-x", REVIEWER, &[bad], &[&aid], PLANNER);
        assert!(!refused.status.success(), "{bad} was accepted");
        let value: Value = serde_json::from_slice(&refused.stdout).unwrap();
        assert_eq!(value["error"].as_str().unwrap(), s5_bad_sha("t-x", bad));
    }
    let refused = estate.verdict("t-x", REVIEWER, &[UNPUB], &[&aid], PLANNER);
    assert!(!refused.status.success(), "unpublished SHA was accepted");
    let value: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        value["error"].as_str().unwrap(),
        s6_unpublished("t-x", UNPUB)
    );

    // Refused writes store nothing: row and event history are untouched.
    estate.assert_unchanged("t-x", &before);

    // Negative control: the same write citing a published SHA succeeds.
    estate.verdict_ok("t-x", REVIEWER, &[H1], &[&aid], PLANNER);
}

// ---------------------------------------------------------------------------
// A9 (DG-14, DG-08): a newer head stales the verdict → sentence 2; a fresh
// verdict at the new head reopens the gate.
// ---------------------------------------------------------------------------

#[test]
fn a9_done_gate_stale_verdict_does_not_open_gate() {
    let estate = Estate::new("a9");
    estate.gate_on();
    estate.add_task("moved work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    let a1 = estate.resolved_attention("t-x");
    let a2 = estate.resolved_attention("t-x");
    estate.verdict_ok("t-x", REVIEWER, &[H1], &[&a1, &a2], PLANNER);

    // Later work records head H2; the H1 verdict is now stale.
    std::thread::sleep(Duration::from_millis(30));
    estate.checkpoint("t-x", &lease, EXEC, H2);
    let before = estate.snapshot("t-x");
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s2_stale("t-x", H1, H2),
    );
    estate.assert_unchanged("t-x", &before);

    // A fresh pass at the new head, citing the still-resolved decisions,
    // reopens the gate.
    estate.verdict_ok("t-x", REVIEWER, &[H2], &[&a1, &a2], PLANNER);
    estate.ok_json(&["task", "move", "t-x", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-x")["status"], "done");

    ungated_twin_moves_done("a9-twin");
}

// ---------------------------------------------------------------------------
// A10 (DG-15, DG-09): flag absent reads off; geoyws-only audited toggles.
// ---------------------------------------------------------------------------

#[test]
fn a10_done_gate_flag_defaults_off_and_audits_changes() {
    let estate = Estate::new("a10");

    // Fresh board, no done_gate key: done-moves behave as at the baseline.
    estate.add_task("fresh work", "t-y", &[]);
    estate.claim("t-y", EXEC);
    estate.ok_json(&["task", "move", "t-y", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-y")["status"], "done");

    // geoyws turns the gate on: the receipt carries the exact toggle shape
    // (IMPL-NOTES.md) and one `done_gate_toggled` event audits off→on.
    let events_before = estate.all_events();
    let receipt = estate.gate_on();
    assert_eq!(receipt["doneGate"], "on");
    assert_eq!(receipt["oldValue"], "off");
    assert_eq!(receipt["changedBy"], GEO);
    let board = receipt["board"].as_str().unwrap().to_owned();
    let toggled: Vec<Value> = estate
        .ok_json(&["events", "--kind", "done_gate_toggled", "--json"])
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(toggled.len(), 1, "expected one toggle event: {toggled:?}");
    assert_eq!(toggled[0]["actor"], GEO);
    assert!(
        toggled[0]["taskID"].is_null(),
        "toggle is board-scoped: {}",
        toggled[0]
    );
    assert_eq!(toggled[0]["payload"]["board"], serde_json::json!(board));
    assert_eq!(toggled[0]["payload"]["oldValue"], "off");
    assert_eq!(toggled[0]["payload"]["newValue"], "on");
    let after_toggle = estate.all_events();
    assert!(!fresh_events(&events_before, &after_toggle).is_empty());

    // The gate now refuses with sentence 1.
    estate.add_task("gated work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-x"),
    );

    // A non-geoyws toggle is refused with sentence 7 (board from the receipt)
    // and changes nothing.
    let refused = estate.run(&["task", "verdict", "gate", "off", "--as", EXEC, "--json"]);
    assert!(!refused.status.success(), "non-geoyws toggle succeeded");
    let value: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(value["error"].as_str().unwrap(), s7_toggle(&board));
    // The flag is unchanged: the gate still refuses.
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-x"),
    );

    // geoyws turns it back off: done-moves behave as at the baseline again.
    let off = estate.gate_off();
    assert_eq!(off["doneGate"], "off");
    assert_eq!(off["oldValue"], "on");
    assert_eq!(off["changedBy"], GEO);
    estate.ok_json(&["task", "move", "t-x", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-x")["status"], "done");
}

// ---------------------------------------------------------------------------
// A13 (DG-02, DG-03, DG-08): the verdict writer may not close the row.
// ---------------------------------------------------------------------------

#[test]
fn a13_done_gate_refuses_writer_as_closer() {
    let estate = Estate::new("a13");
    estate.gate_on();
    estate.add_task("written work", "t-x", &[]);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    let aid = estate.resolved_attention("t-x");
    estate.verdict_ok("t-x", REVIEWER, &[H1], &[&aid], PLANNER);

    // The writer closing is self-review even with a foreign reviewer.
    let before = estate.snapshot("t-x");
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", PLANNER, "--json"],
        &s3_self_review("t-x", PLANNER),
    );
    estate.assert_unchanged("t-x", &before);

    // Negative control: the holder's own close succeeds with the gate on.
    estate.ok_json(&["task", "move", "t-x", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-x")["status"], "done");
}

// ---------------------------------------------------------------------------
// A11 (DG-16, DG-10): stories/epics project done from children, no verdict.
// ---------------------------------------------------------------------------

#[test]
fn a11_done_gate_story_and_epic_project_done_without_verdict() {
    let estate = Estate::new("a11");
    // Gate-off phase: build the hierarchy and run the story chain up to the
    // final advance (children reach done with no verdict anywhere).
    estate.add_task("Epic", "e-1", &["--type", "epic", "--status", "todo"]);
    estate.add_task(
        "Story",
        "s-1",
        &["--type", "story", "--parent", "e-1", "--status", "backlog"],
    );
    estate.add_task("Develop", "t-dev", &["--parent", "s-1", "--lane", "be"]);
    estate.add_task("Test", "t-test", &["--parent", "s-1", "--lane", "test"]);
    estate.add_task(
        "Other story",
        "s-2",
        &["--type", "story", "--parent", "e-1", "--status", "backlog"],
    );
    estate.add_task("Other child", "t-c2", &["--parent", "s-2"]);
    estate.add_task("Lone epic", "e-2", &["--type", "epic", "--status", "todo"]);

    assert_eq!(
        estate.ok_json(&["story", "advance", "s-1", "--as", "driver", "--json"])["to"],
        "ready"
    );
    assert_eq!(
        estate.ok_json(&["story", "advance", "s-1", "--as", "driver", "--json"])["to"],
        "in-progress"
    );
    estate.ok_json(&["task", "move", "t-dev", "done", "--as", "worker", "--json"]);
    assert_eq!(
        estate.ok_json(&["story", "advance", "s-1", "--as", "driver", "--json"])["to"],
        "testing"
    );
    estate.ok_json(&["task", "move", "t-test", "done", "--as", "tester", "--json"]);
    estate.ok_json(&[
        "story",
        "advance",
        "s-1",
        "--as",
        "driver",
        "--reviewer",
        "reviewer",
        "--json",
    ]);
    estate.ok_json(&["story", "signoff", "s-1", "--as", "reviewer", "--json"]);
    let merging = estate.ok_json(&[
        "story",
        "advance",
        "s-1",
        "--as",
        "driver",
        "--committer",
        "committer",
        "--json",
    ]);
    let merge_task = merging["dispatchedTaskID"].as_str().unwrap().to_owned();
    estate.ok_json(&[
        "task",
        "move",
        merge_task.as_str(),
        "done",
        "--as",
        "committer",
        "--json",
    ]);
    estate.ok_json(&["task", "move", "t-c2", "done", "--as", "worker", "--json"]);

    // Gate on: the final projection demands and checks no verdict. Note the
    // ledger performs no automatic epic-to-done flip when stories complete
    // (`advance_story` flips the parent only ready→in-progress); the epic
    // reaches done through its own baseline `task move` path, which DG-16
    // likewise leaves verdict-free. That is what the test drives below.
    estate.gate_on();
    let advanced = estate.ok_json(&["story", "advance", "s-1", "--as", "driver", "--json"]);
    assert_eq!(advanced["to"], "done");
    let advanced_raw = serde_json::to_string(&advanced).unwrap();
    assert!(
        !advanced_raw.contains("verdict"),
        "projection demanded a verdict: {advanced_raw}"
    );
    assert_eq!(estate.show("s-1")["status"], "done");

    // The parent epic reaches done with the gate on and no verdict anywhere.
    estate.ok_json(&["task", "move", "e-1", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("e-1")["status"], "done");

    // A direct story move stays refused by the projection rule exactly as at
    // the baseline — carrying no DG-08 sentence. Guard first: the story must
    // not already project to done, or the refusal premise is void.
    assert_ne!(
        estate.show("s-2")["status"],
        "done",
        "s-2 projected to done on its own"
    );
    let before = estate.snapshot("s-2");
    let direct = estate.run(&["task", "move", "s-2", "done", "--as", EXEC, "--json"]);
    assert!(!direct.status.success(), "direct story move succeeded");
    let value: Value = serde_json::from_slice(&direct.stdout).unwrap();
    let message = value["error"].as_str().unwrap().to_owned();
    for marker in ["verdict", "done_gate", "geoyws may override"] {
        assert!(
            !message.contains(marker),
            "projection refusal carries gate wording: {message}"
        );
    }
    estate.assert_unchanged("s-2", &before);

    // A direct epic move succeeds exactly as at the baseline, no verdict.
    estate.ok_json(&["task", "move", "e-2", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("e-2")["status"], "done");
}

// ---------------------------------------------------------------------------
// A12 (DG-11, DG-06): pre-gate rows gain no verdict; verdicts and overrides
// survive a backup/restore round-trip (the closest one-binary expression of
// "survives the migration": every command here already reopens the board in a
// fresh process, and restore replays the migration path onto live state).
// A same-binary test cannot open a board written by an older binary;
// downgrade-upgrade across two binaries is not expressible here.
// ---------------------------------------------------------------------------

#[test]
fn a12_done_gate_verdicts_table_is_append_only_across_migration() {
    let estate = Estate::new("a12");
    // Pre-gate row: done with no verdict anywhere.
    estate.add_task("old work", "t-old", &[]);
    estate.claim("t-old", EXEC);
    estate.ok_json(&["task", "move", "t-old", "done", "--as", EXEC, "--json"]);

    estate.gate_on();

    // The old row carries no verdict and gains none: the verdict list is
    // empty and no verdict-flavoured event exists for it.
    assert_eq!(estate.verdicts("t-old").as_array().unwrap().len(), 0);
    let old_events = serde_json::to_string(&estate.task_events("t-old")).unwrap();
    assert!(
        !old_events.contains("verdict"),
        "old row gained verdict history: {old_events}"
    );

    // A new done-move under the gate is refused until a verdict is earned.
    estate.add_task("new work", "t-new", &[]);
    let lease = estate.claim("t-new", EXEC);
    estate.checkpoint("t-new", &lease, EXEC, H1);
    let aid = estate.resolved_attention("t-new");
    estate.refuse_exact(
        &["task", "move", "t-new", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-new"),
    );
    estate.verdict_ok("t-new", REVIEWER, &[H1], &[&aid], PLANNER);
    estate.release("t-new", &lease);
    estate.ok_json(&["task", "move", "t-new", "done", "--as", THIRD, "--json"]);

    // An override event on a third row, then a backup/restore round-trip:
    // stored verdict rows and override events come back with payloads intact.
    estate.add_task("forced work", "t-ovr", &[]);
    let lease_ovr = estate.claim("t-ovr", EXEC);
    estate.checkpoint("t-ovr", &lease_ovr, EXEC, H1);
    estate.ok_json(&[
        "task", "move", "t-ovr", "done", "--as", GEO, "--force", "--json",
    ]);
    let snapshot = estate.ok_json(&["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();
    estate.ok_json(&["restore", "--from", &snapshot, "--force", "--json"]);

    // The stored verdict row comes back intact (append-only, never backfilled
    // onto the old row), and the override event keeps kind, actor, task and
    // payload.
    let kept = estate.verdicts("t-new");
    assert_eq!(
        kept.as_array().unwrap().len(),
        1,
        "verdict lost across restore: {kept}"
    );
    assert_eq!(kept[0]["reviewer"], REVIEWER);
    assert_eq!(kept[0]["shas"], serde_json::json!([H1]));
    let ovr = estate.ok_json(&[
        "events",
        "--kind",
        "done_gate_override",
        "--task",
        "t-ovr",
        "--json",
    ]);
    assert_eq!(
        ovr.as_array().unwrap().len(),
        1,
        "override lost across restore: {ovr}"
    );
    assert_eq!(ovr[0]["actor"], GEO);
    assert_eq!(ovr[0]["payload"]["priorStatus"], "todo");
    assert_eq!(ovr[0]["payload"]["reason"], "missing-verdict");
    assert_eq!(estate.verdicts("t-old").as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// A14 (DG-14): a heartbeat at an unchanged head stales nothing; a newer head
// stales; an older multi-head verdict satisfies again with no new write.
// ---------------------------------------------------------------------------

#[test]
fn a14_done_gate_heartbeat_at_same_head_stales_nothing() {
    let estate = Estate::new("a14");
    estate.gate_on();

    // Heartbeat re-records the claim at the same head: the verdict satisfies.
    estate.add_task("steady work", "t-s1", &[]);
    let lease = estate.claim("t-s1", EXEC);
    estate.checkpoint("t-s1", &lease, EXEC, H1);
    let aid = estate.resolved_attention("t-s1");
    estate.verdict_ok("t-s1", REVIEWER, &[H1], &[&aid], PLANNER);
    estate.heartbeat("t-s1", &lease);
    estate.ok_json(&["task", "move", "t-s1", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-s1")["status"], "done");

    // Back to todo, then a newer head: the same verdict is stale (sentence 2).
    estate.ok_json(&["task", "move", "t-s1", "todo", "--as", EXEC, "--json"]);
    std::thread::sleep(Duration::from_millis(30));
    estate.checkpoint("t-s1", &lease, EXEC, H2);
    estate.refuse_exact(
        &["task", "move", "t-s1", "done", "--as", EXEC, "--json"],
        &s2_stale("t-s1", H1, H2),
    );

    // An older verdict whose SHAs also include H2 satisfies again with no new
    // write: recorded early on a second row, current at H2.
    estate.add_task("ahead work", "t-s2", &[]);
    let lease2 = estate.claim("t-s2", EXEC);
    estate.checkpoint("t-s2", &lease2, EXEC, H1);
    let aid2 = estate.resolved_attention("t-s2");
    estate.verdict_ok("t-s2", REVIEWER, &[H1, H2], &[&aid2], PLANNER);
    std::thread::sleep(Duration::from_millis(30));
    estate.checkpoint("t-s2", &lease2, EXEC, H2);
    let events_before = estate.task_events("t-s2");
    estate.ok_json(&["task", "move", "t-s2", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-s2")["status"], "done");
    let after = estate.task_events("t-s2");
    let delta = serde_json::to_string(&fresh_events(&events_before, &after)).unwrap();
    assert!(
        !delta.contains("verdict"),
        "close wrote verdict state: {delta}"
    );
    assert_eq!(estate.verdicts("t-s2").as_array().unwrap().len(), 1);

    // Negative control for the stale refusal: absent the gate it succeeds.
    estate.gate_off();
    estate.ok_json(&["task", "move", "t-s1", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-s1")["status"], "done");
}

// ---------------------------------------------------------------------------
// A15 (DG-14, DG-06): no head anywhere → only the geoyws override opens it.
// ---------------------------------------------------------------------------

#[test]
fn a15_done_gate_no_head_opens_only_by_override() {
    let estate = Estate::new("a15");
    estate.gate_on();
    // Claimed but never checkpointed/handed-off/reported: no non-null head.
    estate.add_task("headless work", "t-x", &[]);
    estate.claim("t-x", EXEC);
    let aid = estate.resolved_attention("t-x");
    // A stored verdict citing a published SHA still satisfies nothing.
    estate.verdict_ok("t-x", REVIEWER, &[H1], &[&aid], PLANNER);

    let before = estate.snapshot("t-x");
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-x"),
    );
    estate.assert_unchanged("t-x", &before);

    // Only geoyws with --force opens it, writing exactly one override event
    // (reason `missing-verdict`: the sentence-1 category per IMPL-NOTES.md).
    let events_before = estate.task_events("t-x");
    estate.ok_json(&[
        "task", "move", "t-x", "done", "--as", GEO, "--force", "--json",
    ]);
    assert_eq!(estate.show("t-x")["status"], "done");
    let events_after = estate.task_events("t-x");
    let ovr: Vec<&Value> = fresh_events(&events_before, &events_after)
        .iter()
        .filter(|e| e["kind"] == "done_gate_override")
        .collect();
    assert_eq!(ovr.len(), 1, "expected one override event");
    assert_eq!(ovr[0]["actor"], GEO);
    assert_eq!(ovr[0]["payload"]["priorStatus"], "todo");
    assert_eq!(ovr[0]["payload"]["reason"], "missing-verdict");

    // Negative control: a headless twin moves fine with the gate off.
    let twin = Estate::twin("a15-twin");
    twin.add_task("headless twin", "t-twin", &[]);
    twin.claim("t-twin", EXEC);
    twin.ok_json(&["task", "move", "t-twin", "done", "--as", EXEC, "--json"]);
    assert_eq!(twin.show("t-twin")["status"], "done");
}

// ---------------------------------------------------------------------------
// A16 (DG-03, DG-08): a released holder is still the holder of record.
// ---------------------------------------------------------------------------

#[test]
fn a16_done_gate_released_holder_is_still_holder_of_record() {
    let estate = Estate::new("a16");
    estate.gate_on();
    estate.add_task("released work", "t-x", &[]);
    let aid = estate.resolved_attention("t-x");
    // lane-e has never held the claim, so this write succeeds.
    estate.verdict_ok("t-x", EXEC, &[H1], &[&aid], PLANNER);
    let lease = estate.claim("t-x", EXEC);
    estate.checkpoint("t-x", &lease, EXEC, H1);
    // Released: no live task_claims row, but the claim_released event keeps
    // lane-e the holder of record.
    estate.release("t-x", &lease);

    let before = estate.snapshot("t-x");
    estate.refuse_exact(
        &["task", "move", "t-x", "done", "--as", THIRD, "--json"],
        &s3_self_review("t-x", EXEC),
    );
    estate.assert_unchanged("t-x", &before);

    // Negative control: absent the gate the same move succeeds.
    estate.gate_off();
    estate.ok_json(&["task", "move", "t-x", "done", "--as", THIRD, "--json"]);
    assert_eq!(estate.show("t-x")["status"], "done");
    ungated_twin_moves_done("a16-twin");
}

// ---------------------------------------------------------------------------
// DG-04 (no dedicated A-case): open, missing and reopened citations fail the
// gate; resolving the cited row reopens it.
// ---------------------------------------------------------------------------

#[test]
fn done_gate_requires_resolved_decision_citations() {
    let estate = Estate::new("dg04");
    estate.gate_on();

    // Open citation: the write lands (write time does not judge evidence),
    // but the move is refused as no-verdict.
    estate.add_task("open cite", "t-open", &[]);
    let lease_o = estate.claim("t-open", EXEC);
    estate.checkpoint("t-open", &lease_o, EXEC, H1);
    let open_aid = estate.raise_open_attention("t-open");
    estate.verdict_ok("t-open", REVIEWER, &[H1], &[&open_aid], PLANNER);
    assert_eq!(estate.verdicts("t-open").as_array().unwrap().len(), 1);
    estate.refuse_exact(
        &["task", "move", "t-open", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-open"),
    );
    // Resolved at move time: once the cited row resolves, the same stored
    // row satisfies with no new write (DG-04 judges citations at the move).
    estate.ok_json(&[
        "attention",
        "resolve",
        &open_aid,
        "--as",
        PLANNER,
        "--choice",
        "approve",
        "--note",
        "checked late",
        "--json",
    ]);
    estate.ok_json(&["task", "move", "t-open", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-open")["status"], "done");
    assert_eq!(estate.verdicts("t-open").as_array().unwrap().len(), 1);

    // Missing citation: likewise refused.
    estate.add_task("missing cite", "t-missing", &[]);
    let lease_m = estate.claim("t-missing", EXEC);
    estate.checkpoint("t-missing", &lease_m, EXEC, H1);
    let _ = estate.verdict("t-missing", REVIEWER, &[H1], &[&"a-missing"], PLANNER);
    estate.refuse_exact(
        &["task", "move", "t-missing", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-missing"),
    );

    // Reopened citation: refused until re-resolved, then the move succeeds.
    estate.add_task("reopened cite", "t-re", &[]);
    let lease_r = estate.claim("t-re", EXEC);
    estate.checkpoint("t-re", &lease_r, EXEC, H1);
    let re_aid = estate.resolved_attention("t-re");
    estate.ok_json(&[
        "attention",
        "reopen",
        &re_aid,
        "--as",
        PLANNER,
        "--note",
        "needs another look",
        "--json",
    ]);
    estate.verdict_ok("t-re", REVIEWER, &[H1], &[&re_aid], PLANNER);
    assert_eq!(estate.verdicts("t-re").as_array().unwrap().len(), 1);
    estate.refuse_exact(
        &["task", "move", "t-re", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-re"),
    );
    estate.ok_json(&[
        "attention",
        "resolve",
        &re_aid,
        "--as",
        PLANNER,
        "--choice",
        "approve",
        "--note",
        "rechecked",
        "--json",
    ]);
    // The same stored row now satisfies: no second write, list still len 1.
    estate.ok_json(&["task", "move", "t-re", "done", "--as", EXEC, "--json"]);
    assert_eq!(estate.show("t-re")["status"], "done");
    assert_eq!(estate.verdicts("t-re").as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// DG-08: all seven refusal sentences, byte-exact, on one board.
// ---------------------------------------------------------------------------

#[test]
fn done_gate_refusals_carry_named_reasons() {
    let estate = Estate::new("dg08");
    let board = estate.gate_on()["board"].as_str().unwrap().to_owned();

    // 1. No pass verdict.
    estate.add_task("one", "t-1", &[]);
    let l1 = estate.claim("t-1", EXEC);
    estate.checkpoint("t-1", &l1, EXEC, H1);
    estate.refuse_exact(
        &["task", "move", "t-1", "done", "--as", EXEC, "--json"],
        &s1_no_verdict("t-1"),
    );

    // 2. Stale verdict.
    estate.add_task("two", "t-2", &[]);
    let l2 = estate.claim("t-2", EXEC);
    estate.checkpoint("t-2", &l2, EXEC, H1);
    let a2 = estate.resolved_attention("t-2");
    estate.verdict_ok("t-2", REVIEWER, &[H1], &[&a2], PLANNER);
    std::thread::sleep(Duration::from_millis(30));
    estate.checkpoint("t-2", &l2, EXEC, H2);
    estate.refuse_exact(
        &["task", "move", "t-2", "done", "--as", EXEC, "--json"],
        &s2_stale("t-2", H1, H2),
    );

    // 3. Self-review.
    estate.add_task("three", "t-3", &[]);
    let a3 = estate.resolved_attention("t-3");
    estate.verdict_ok("t-3", EXEC, &[H1], &[&a3], PLANNER);
    let l3 = estate.claim("t-3", EXEC);
    estate.checkpoint("t-3", &l3, EXEC, H1);
    estate.refuse_exact(
        &["task", "move", "t-3", "done", "--as", EXEC, "--json"],
        &s3_self_review("t-3", EXEC),
    );

    // 4. Non-geoyws force.
    estate.refuse_exact(
        &[
            "task", "move", "t-1", "done", "--as", EXEC, "--force", "--json",
        ],
        &s4_force("t-1"),
    );

    // 5 + 6. Bad SHA shapes and unpublished SHAs (same-task evidence).
    let a1 = estate.resolved_attention("t-1");
    let bad5 = estate.verdict("t-1", REVIEWER, &[SHORT], &[&a1], PLANNER);
    assert!(!bad5.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&bad5.stdout).unwrap()["error"]
            .as_str()
            .unwrap(),
        s5_bad_sha("t-1", SHORT)
    );
    let bad6 = estate.verdict("t-1", REVIEWER, &[UNPUB], &[&a1], PLANNER);
    assert!(!bad6.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&bad6.stdout).unwrap()["error"]
            .as_str()
            .unwrap(),
        s6_unpublished("t-1", UNPUB)
    );

    // 7. Non-geoyws gate toggle.
    let bad7 = estate.run(&["task", "verdict", "gate", "off", "--as", EXEC, "--json"]);
    assert!(!bad7.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&bad7.stdout).unwrap()["error"]
            .as_str()
            .unwrap(),
        s7_toggle(&board)
    );
}
