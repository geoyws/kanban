use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EVENT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHILD_PATH: &str = "/usr/bin:/bin";
const NORMAL_TIMEOUT_MS: &str = "5000";
const DEADLINE_TIMEOUT_MS: &str = "1000";

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

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
    permissions.set_mode(0o755);
    fs::set_permissions(target, permissions).unwrap();
}

fn request() -> Value {
    json!({
        "protocolVersion": 1,
        "delivery": {
            "subscriptionID": "sub-test",
            "eventID": EVENT_ID,
            "attempt": 2,
            "createdAt": 1_720_000_000_i64
        },
        "target": {"consumerID": "kimi.acp", "actionID": "enqueue-turn"},
        "event": {
            "eventID": EVENT_ID,
            "eventHash": EVENT_ID,
            "timestamp": 1_720_000_000_i64,
            "body": "delivered over the ACP transport"
        }
    })
}

fn acknowledgement() -> Value {
    json!({
        "protocolVersion": 1,
        "subscriptionID": "sub-test",
        "eventID": EVENT_ID,
        "createdAt": 1_720_000_000_i64,
        "replay": true
    })
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    peer: PathBuf,
    capture: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Parallel tests share the pid and can observe the same coarse clock
        // reading, so the counter -- not the timestamp -- is what keeps two
        // fixtures from colliding on one root.
        let root = env::temp_dir().join(format!(
            "kanban-kimi-acp-adapter-e2e-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ROOT.fetch_add(1, Ordering::SeqCst)
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir(&cwd).unwrap();
        for path in [&root, &home, &cwd] {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).unwrap();
        }
        let peer = root.join("fake-peer");
        compile_fake("tests/fixtures/kimi_acp_adapter_fake_peer.rs", &peer);
        let capture = root.join("capture.ndjson");
        Self {
            root,
            home,
            cwd,
            peer,
            capture,
        }
    }

    fn deliver(&self, scenario: &str, timeout_ms: &str) -> Output {
        self.deliver_request(scenario, timeout_ms, &request())
    }

    fn deliver_request(&self, scenario: &str, timeout_ms: &str, request: &Value) -> Output {
        fs::write(self.home.join("scenario.txt"), scenario).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kanban-kimi-acp-adapter"))
            .arg("--kimi")
            .arg(&self.peer)
            .arg("--home")
            .arg(&self.home)
            .arg("--cwd")
            .arg(&self.cwd)
            .args(["--request-timeout-ms", timeout_ms])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Refusals that fire before the delivery is read -- an untrusted peer
        // path -- close this pipe first, so the write itself is not the
        // assertion; the adapter's exit status and stderr are.
        let _ = child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&serde_json::to_vec(request).unwrap());
        child.wait_with_output().unwrap()
    }

    /// Every invocation the fake peer observed, one entry per spawn.
    fn captured(&self) -> Vec<Value> {
        match fs::read_to_string(&self.capture) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(_) => Vec::new(),
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
    let binary = env!("CARGO_BIN_EXE_kanban-kimi-acp-adapter");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert_eq!(
        String::from_utf8(help.stdout).unwrap(),
        "kanban-kimi-acp-adapter --kimi ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --request-timeout-ms N\n"
    );
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("kanban-kimi-acp-adapter {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn an_accepted_delivery_is_one_framed_exchange_and_returns_the_peer_acknowledgement() {
    let fixture = Fixture::new();
    let output = fixture.deliver("accept", NORMAL_TIMEOUT_MS);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        acknowledgement()
    );

    let captured = fixture.captured();
    assert_eq!(captured.len(), 1, "the peer was not spawned exactly once");
    let observed = &captured[0];
    assert_eq!(observed["argv"], json!([]), "the peer was given argv");
    // The adapter pins the canonical path and passes that to the peer.
    assert_eq!(
        observed["env"],
        json!([
            [
                "HOME",
                fs::canonicalize(&fixture.home).unwrap().to_str().unwrap()
            ],
            ["PATH", CHILD_PATH]
        ]),
        "the peer saw an environment beyond its pinned HOME and PATH"
    );
    assert_eq!(
        observed["cwd"],
        json!(fs::canonicalize(&fixture.cwd).unwrap().to_str().unwrap())
    );
    assert_eq!(
        observed["frame"],
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "_kanban/deliverEvent",
            "params": request()
        })
    );
}

#[test]
fn a_peer_that_exits_without_answering_reports_the_retryable_unanswered_code() {
    let fixture = Fixture::new();
    let output = fixture.deliver("silent", NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 10, "{stderr}");
    assert!(
        stderr.contains("kimi_peer_unanswered (retryable)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("without answering the delivery"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
    assert_eq!(fixture.captured().len(), 1, "the peer never read the frame");
}

#[test]
fn a_frame_without_its_terminator_is_never_read_as_an_answer() {
    let fixture = Fixture::new();
    let output = fixture.deliver("truncate", NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 10, "{stderr}");
    assert!(
        stderr.contains("kimi_peer_unanswered (retryable)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("bytes of an unterminated frame"),
        "a partial frame was not reported as an unterminated frame: {stderr}"
    );
    assert!(
        !stderr.contains("kimi_frame_malformed"),
        "a partial frame was parsed instead of refused: {stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
}

#[test]
fn a_malformed_frame_reports_the_terminal_malformed_code() {
    let fixture = Fixture::new();
    let output = fixture.deliver("malformed", NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 11, "{stderr}");
    assert!(
        stderr.contains("kimi_frame_malformed (terminal)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("not one JSON-RPC response object"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
}

#[test]
fn an_oversized_frame_is_refused_at_the_cap_rather_than_read_to_the_end() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let output = fixture.deliver("oversized", NORMAL_TIMEOUT_MS);
    let elapsed = started.elapsed();
    let stderr = stderr(&output);

    assert_eq!(code(&output), 12, "{stderr}");
    assert!(
        stderr.contains("kimi_frame_oversized (terminal)"),
        "{stderr}"
    );
    assert!(stderr.contains("refused unread"), "{stderr}");
    assert!(output.stdout.is_empty(), "{stderr}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the adapter spent {elapsed:?} on a frame it had already exceeded"
    );
}

#[test]
fn an_acknowledgement_for_another_event_is_never_accepted_as_success() {
    let fixture = Fixture::new();
    let output = fixture.deliver("mismatch", NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 13, "{stderr}");
    assert!(
        stderr.contains("kimi_identity_mismatch (terminal)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("event identity does not match"),
        "the mismatch was not reported as an identity refusal: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "a mismatched acknowledgement reached the dispatcher: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn an_answer_under_an_unasked_request_id_is_never_accepted_as_success() {
    let fixture = Fixture::new();
    let output = fixture.deliver("alien-id", NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 13, "{stderr}");
    assert!(
        stderr.contains("kimi_identity_mismatch (terminal)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("answered request id 99, not the id 1"),
        "{stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "an answer to another request reached the dispatcher: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn a_peer_that_refuses_the_method_reports_the_terminal_rejected_code() {
    let fixture = Fixture::new();
    let output = fixture.deliver("reject", NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 15, "{stderr}");
    assert!(
        stderr.contains("kimi_request_rejected (terminal)"),
        "{stderr}"
    );
    assert!(stderr.contains("-32601: Method not found"), "{stderr}");
    assert!(
        !stderr.contains("kimi_frame_malformed"),
        "an explicit refusal was reported as a framing fault: {stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
}

#[test]
fn a_breached_deadline_is_distinguishable_from_a_refusal() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let output = fixture.deliver("hang", DEADLINE_TIMEOUT_MS);
    let elapsed = started.elapsed();
    let stderr = stderr(&output);

    assert_eq!(code(&output), 14, "{stderr}");
    assert!(
        stderr.contains("kimi_deadline_exceeded (retryable)"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("kimi_request_rejected"),
        "a breached deadline reported the refusal code: {stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
    assert!(
        elapsed >= Duration::from_millis(1_000),
        "the adapter gave up after {elapsed:?}, before its own deadline"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the adapter waited {elapsed:?} on a peer that never answered"
    );
    assert_eq!(fixture.captured().len(), 1, "the peer never read the frame");
}

#[test]
fn a_peer_that_keeps_running_after_answering_still_completes_the_delivery() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let output = fixture.deliver("linger", NORMAL_TIMEOUT_MS);
    let elapsed = started.elapsed();

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        acknowledgement()
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the adapter waited {elapsed:?} on a peer that had already answered"
    );
}

#[test]
fn a_delivery_for_another_consumer_never_reaches_the_peer() {
    let fixture = Fixture::new();
    let mut wrong = request();
    wrong["target"]["consumerID"] = json!("codex.queue");
    let output = fixture.deliver_request("accept", NORMAL_TIMEOUT_MS, &wrong);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 1, "{stderr}");
    assert!(stderr.contains("consumer ID must be kimi.acp"), "{stderr}");
    assert!(output.stdout.is_empty(), "{stderr}");
    assert!(
        fixture.captured().is_empty(),
        "the adapter spawned a peer for a delivery aimed elsewhere"
    );
}

#[test]
fn an_untrusted_peer_path_is_refused_before_anything_is_spawned() {
    let fixture = Fixture::new();
    let mut permissions = fs::metadata(&fixture.peer).unwrap().permissions();
    permissions.set_mode(0o777);
    fs::set_permissions(&fixture.peer, permissions).unwrap();

    let output = fixture.deliver("accept", NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 1, "{stderr}");
    assert!(
        stderr.contains("--kimi permissions are not trusted"),
        "{stderr}"
    );
    assert!(
        fixture.captured().is_empty(),
        "a world-writable peer was spawned"
    );
}
