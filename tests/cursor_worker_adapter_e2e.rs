//! Compiled-process contract for `kanban-cursor-worker-adapter`.
//!
//! Every test drives a dependency-free fake worker compiled from
//! `tests/fixtures/cursor_worker_adapter_fake_worker.rs`, so nothing here
//! needs Cursor installed, logged in, or reachable over a network.
//!
//! The test this binary exists for is
//! [`two_overlapping_deliveries_run_one_turn_at_a_time`]: the fake records one
//! `start` and one `end` line per turn into an append-only log, and the order
//! of those lines is the order the turns really ran. One delivery succeeding
//! would prove nothing about serialization.

use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CREATED_AT: i64 = 1_720_000_000;
const NORMAL_TURN_TIMEOUT_MS: &str = "10000";
const HELD_TURN_TIMEOUT_MS: &str = "600000";
const DEADLINE_TURN_TIMEOUT_MS: &str = "1000";
const NORMAL_QUEUE_WAIT_MS: &str = "10000";
const NO_QUEUE_WAIT_MS: &str = "0";
/// How long the `hold` scenario occupies the worker; must match the fixture.
const HOLD: Duration = Duration::from_millis(400);

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

fn event_id(marker: char) -> String {
    marker.to_string().repeat(64)
}

fn request(marker: char, attempt: i64) -> Value {
    let id = event_id(marker);
    json!({
        "protocolVersion": 1,
        "delivery": {
            "subscriptionID": "sub-test",
            "eventID": id,
            "attempt": attempt,
            "createdAt": CREATED_AT
        },
        "target": {"consumerID": "cursor.worker", "actionID": "start-turn"},
        "event": {
            "eventID": id,
            "eventHash": id,
            "timestamp": CREATED_AT,
            "body": "must not reach the worker's argv"
        }
    })
}

fn acknowledgement(marker: char, replay: bool) -> Value {
    json!({
        "protocolVersion": 1,
        "subscriptionID": "sub-test",
        "eventID": event_id(marker),
        "createdAt": CREATED_AT,
        "replay": replay
    })
}

/// Compile a fixture fake program into `target` and make it executable.
fn compile_fake(source: &str, target: &Path) {
    let source_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(source);
    let status = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .args(["--edition=2024"])
        .arg(&source_path)
        .arg("-o")
        .arg(target)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "compile fake from {}",
        source_path.display()
    );
    let mut permissions = fs::metadata(target).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(target, permissions).unwrap();
}

struct Fixture {
    root: PathBuf,
    worker: PathBuf,
    state_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Parallel tests share the pid and can read the same coarse clock, so
        // the counter is what keeps two fixtures off one another's state dir.
        let root = env::temp_dir().join(format!(
            "kanban-cursor-adapter-e2e-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ROOT.fetch_add(1, Ordering::SeqCst)
        ));
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        for directory in [&root, &state_dir] {
            let mut permissions = fs::metadata(directory).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(directory, permissions).unwrap();
        }
        let worker = root.join("cursor-worker");
        compile_fake(
            "tests/fixtures/cursor_worker_adapter_fake_worker.rs",
            &worker,
        );
        Self {
            root,
            worker,
            state_dir,
        }
    }

    fn state(&self) -> PathBuf {
        fs::canonicalize(&self.state_dir).unwrap()
    }

    /// Pin the scenario for one delivery, keyed on its event ID so two turns
    /// sharing a state directory can behave differently.
    fn scenario(&self, marker: char, scenario: &str) {
        fs::write(
            self.state_dir
                .join(format!("scenario-{}.txt", event_id(marker))),
            scenario,
        )
        .unwrap();
    }

    /// Start one delivery and hand back the running adapter, with the delivery
    /// already written and its stdin closed.
    fn start(&self, marker: char, attempt: i64, turn_timeout: &str, queue_wait: &str) -> Child {
        self.start_with(&request(marker, attempt), turn_timeout, queue_wait)
    }

    fn start_with(&self, request: &Value, turn_timeout: &str, queue_wait: &str) -> Child {
        self.start_at(&self.worker, request, turn_timeout, queue_wait)
    }

    fn start_at(
        &self,
        worker: &Path,
        request: &Value,
        turn_timeout: &str,
        queue_wait: &str,
    ) -> Child {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kanban-cursor-worker-adapter"))
            .arg("--worker")
            .arg(worker)
            .arg("--state-dir")
            .arg(&self.state_dir)
            .args(["--turn-timeout-ms", turn_timeout])
            .args(["--queue-wait-ms", queue_wait])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(&serde_json::to_vec(request).unwrap())
            .unwrap();
        drop(stdin);
        child
    }

    fn deliver(&self, marker: char, attempt: i64, turn_timeout: &str, queue_wait: &str) -> Output {
        self.start(marker, attempt, turn_timeout, queue_wait)
            .wait_with_output()
            .unwrap()
    }

    /// Every record the fake has logged, in the order the turns produced them.
    fn turns(&self) -> Vec<Value> {
        let path = self.state_dir.join("turns.ndjson");
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    fn phases(&self) -> Vec<(String, String)> {
        self.turns()
            .iter()
            .map(|record| {
                (
                    record["phase"].as_str().unwrap().to_owned(),
                    record["eventID"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// Wait until the fake logs `phase` for `marker`, so a test can act while
    /// a turn is genuinely in flight instead of guessing with a sleep.
    fn await_phase(&self, phase: &str, marker: char) {
        let id = event_id(marker);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self
            .phases()
            .iter()
            .any(|(logged, event)| logged == phase && *event == id)
        {
            assert!(
                Instant::now() < deadline,
                "the fake never logged {phase} for {id}: {:?}",
                self.phases()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

#[test]
fn binary_help_and_version_are_exact() {
    let binary = env!("CARGO_BIN_EXE_kanban-cursor-worker-adapter");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert_eq!(
        String::from_utf8(help.stdout).unwrap(),
        "kanban-cursor-worker-adapter --worker ABSOLUTE_PATH --state-dir ABSOLUTE_PATH --turn-timeout-ms N --queue-wait-ms N\n"
    );
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!(
            "kanban-cursor-worker-adapter {}\n",
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn a_successful_turn_carries_the_delivery_on_stdin_and_returns_its_acknowledgement() {
    let fixture = Fixture::new();
    fixture.scenario('a', "ok");
    let output = fixture.deliver('a', 2, NORMAL_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        acknowledgement('a', true)
    );

    let turns = fixture.turns();
    assert_eq!(turns.len(), 2, "{turns:?}");
    let start = &turns[0];
    assert_eq!(
        start["argv"],
        json!(["--headless", "--protocol-version", "1"])
    );
    assert!(
        !start["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|token| token.as_str().unwrap().contains("must not reach")),
        "the event body reached the worker's argv: {start}"
    );
    assert_eq!(
        start["env"],
        json!([
            ["HOME", fixture.state().to_string_lossy()],
            ["PATH", "/usr/bin:/bin"]
        ])
    );
    assert_eq!(start["cwd"], fixture.state().to_string_lossy().as_ref());
    assert_eq!(
        serde_json::from_str::<Value>(start["stdin"].as_str().unwrap()).unwrap(),
        request('a', 2)
    );
}

/// The row this binary was written for: two deliveries that overlap in time
/// must not overlap in the worker.
#[test]
fn two_overlapping_deliveries_run_one_turn_at_a_time() {
    let fixture = Fixture::new();
    fixture.scenario('a', "hold");
    fixture.scenario('b', "hold");

    let started = Instant::now();
    let first = fixture.start('a', 1, NORMAL_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    let second = fixture.start('b', 1, NORMAL_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    let elapsed = started.elapsed();

    assert_eq!(code(&first), 0, "{}", stderr(&first));
    assert_eq!(code(&second), 0, "{}", stderr(&second));
    // Each delivery gets its own acknowledgement: a worker answering for the
    // other delivery is the cross-talk this serialization prevents.
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap(),
        acknowledgement('a', false)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&second.stdout).unwrap(),
        acknowledgement('b', false)
    );

    let phases = fixture.phases();
    assert_eq!(
        phases.len(),
        4,
        "expected exactly two complete turns: {phases:?}"
    );
    let (first_phase, first_event) = &phases[0];
    let (second_phase, second_event) = &phases[1];
    let (third_phase, third_event) = &phases[2];
    let (fourth_phase, fourth_event) = &phases[3];
    assert_eq!(
        [
            first_phase.as_str(),
            second_phase.as_str(),
            third_phase.as_str(),
            fourth_phase.as_str()
        ],
        ["start", "end", "start", "end"],
        "the two turns interleaved instead of running one at a time: {phases:?}"
    );
    assert_eq!(
        first_event, second_event,
        "a turn ended for a delivery that had not started: {phases:?}"
    );
    assert_eq!(
        third_event, fourth_event,
        "a turn ended for a delivery that had not started: {phases:?}"
    );
    assert_ne!(
        first_event, third_event,
        "one delivery ran twice: {phases:?}"
    );
    // Two serialized 400ms turns cannot finish in the time one takes; this
    // fails loudly if the fake ever stops actually occupying the worker.
    assert!(
        elapsed >= HOLD * 2,
        "two 400ms turns finished in {elapsed:?}, so they cannot have been serialized"
    );
}

#[test]
fn a_second_delivery_is_refused_once_the_queue_wait_is_spent() {
    let fixture = Fixture::new();
    fixture.scenario('a', "hang");
    fixture.scenario('b', "ok");
    let mut holder = fixture.start('a', 1, HELD_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    fixture.await_phase("start", 'a');

    let refused = fixture.deliver('b', 1, NORMAL_TURN_TIMEOUT_MS, NO_QUEUE_WAIT_MS);
    let reported = stderr(&refused);
    assert_eq!(code(&refused), 20, "{reported}");
    assert!(
        reported.contains("cursor_worker_busy (retryable)"),
        "{reported}"
    );
    assert!(refused.stdout.is_empty(), "{reported}");
    assert_eq!(
        fixture.phases(),
        vec![("start".to_owned(), event_id('a'))],
        "the refused delivery still reached the worker"
    );

    let _ = holder.kill();
    let _ = holder.wait();
}

/// The documented claim that a holder which died without releasing needs no
/// reaping: the kernel drops the `flock` when the descriptor closes.
#[test]
fn a_slot_left_by_a_killed_holder_is_taken_by_the_next_delivery() {
    let fixture = Fixture::new();
    fixture.scenario('a', "hang");
    fixture.scenario('b', "ok");
    let mut holder = fixture.start('a', 1, HELD_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    fixture.await_phase("start", 'a');
    holder.kill().unwrap();
    holder.wait().unwrap();

    let next = fixture.deliver('b', 1, NORMAL_TURN_TIMEOUT_MS, NO_QUEUE_WAIT_MS);
    assert_eq!(code(&next), 0, "{}", stderr(&next));
    assert_eq!(
        serde_json::from_slice::<Value>(&next.stdout).unwrap(),
        acknowledgement('b', false)
    );
}

#[test]
fn a_worker_that_exits_non_zero_reports_the_retryable_turn_failure_code() {
    let fixture = Fixture::new();
    fixture.scenario('a', "nonzero");
    let output = fixture.deliver('a', 1, NORMAL_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    let reported = stderr(&output);

    assert_eq!(code(&output), 22, "{reported}");
    assert!(
        reported.contains("cursor_worker_turn_failed (retryable)"),
        "{reported}"
    );
    assert!(reported.contains("exited with status 23"), "{reported}");
    assert!(
        reported.contains("the fake worker refused this turn"),
        "the worker's stderr was not reported: {reported}"
    );
    assert!(output.stdout.is_empty(), "{reported}");
}

#[test]
fn a_malformed_acknowledgement_reports_the_terminal_invalid_code() {
    let fixture = Fixture::new();
    fixture.scenario('a', "malformed");
    let output = fixture.deliver('a', 1, NORMAL_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    let reported = stderr(&output);

    assert_eq!(code(&output), 24, "{reported}");
    assert!(
        reported.contains("cursor_worker_response_invalid (terminal)"),
        "{reported}"
    );
    assert!(output.stdout.is_empty(), "{reported}");
}

#[test]
fn a_turn_that_outruns_the_deadline_is_killed_and_reported_as_a_deadline() {
    let fixture = Fixture::new();
    fixture.scenario('a', "hang");
    let started = Instant::now();
    let output = fixture.deliver('a', 1, DEADLINE_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    let elapsed = started.elapsed();
    let reported = stderr(&output);

    assert_eq!(code(&output), 23, "{reported}");
    assert!(
        reported.contains("cursor_worker_deadline_exceeded (retryable)"),
        "{reported}"
    );
    assert!(output.stdout.is_empty(), "{reported}");
    assert!(
        elapsed < Duration::from_secs(20),
        "the adapter waited {elapsed:?} for a turn it had a 1s deadline for"
    );
    let phases = fixture.phases();
    assert_eq!(
        phases,
        vec![("start".to_owned(), event_id('a'))],
        "the wedged turn was not killed: {phases:?}"
    );
}

#[test]
fn an_acknowledgement_for_another_delivery_is_never_reported_as_delivered() {
    let fixture = Fixture::new();
    fixture.scenario('a', "wrong-delivery");
    let output = fixture.deliver('a', 1, NORMAL_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS);
    let reported = stderr(&output);

    assert_eq!(code(&output), 25, "{reported}");
    assert!(
        reported.contains("cursor_worker_response_mismatched (terminal)"),
        "{reported}"
    );
    assert!(
        !reported.contains("cursor_worker_response_invalid"),
        "a mismatched acknowledgement was reported as a malformed one: {reported}"
    );
    assert!(output.stdout.is_empty(), "{reported}");
}

#[test]
fn a_worker_that_is_not_there_reports_the_retryable_unavailable_code() {
    let fixture = Fixture::new();
    let missing = fixture.root.join("no-such-worker");
    let output = fixture
        .start_at(
            &missing,
            &request('a', 1),
            NORMAL_TURN_TIMEOUT_MS,
            NORMAL_QUEUE_WAIT_MS,
        )
        .wait_with_output()
        .unwrap();
    let reported = stderr(&output);

    assert_eq!(code(&output), 21, "{reported}");
    assert!(
        reported.contains("cursor_worker_unavailable (retryable)"),
        "{reported}"
    );
    assert!(output.stdout.is_empty(), "{reported}");
}

#[test]
fn a_delivery_for_another_bridge_is_refused_before_any_turn_runs() {
    let fixture = Fixture::new();
    let mut foreign = request('a', 1);
    foreign["target"]["consumerID"] = json!("claude.print");
    foreign["target"]["actionID"] = json!("start-readonly-turn");
    let output = fixture
        .start_with(&foreign, NORMAL_TURN_TIMEOUT_MS, NORMAL_QUEUE_WAIT_MS)
        .wait_with_output()
        .unwrap();
    let reported = stderr(&output);

    // An unclassified local refusal, not one of the delivery classes.
    assert_eq!(code(&output), 1, "{reported}");
    assert!(
        reported.contains("consumer ID must be cursor.worker"),
        "{reported}"
    );
    assert!(output.stdout.is_empty(), "{reported}");
    assert!(
        fixture.turns().is_empty(),
        "a foreign delivery reached the worker: {:?}",
        fixture.turns()
    );
}
