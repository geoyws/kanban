use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EVENT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
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
        "target": {"consumerID": "opencode.server", "actionID": "enqueue-turn"},
        "event": {
            "eventID": EVENT_ID,
            "eventHash": EVENT_ID,
            "timestamp": 1_720_000_000_i64,
            "body": "delivered over loopback HTTP"
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

/// One running fake endpoint on an ephemeral loopback port.
struct Endpoint {
    child: Child,
    port: u16,
    capture: PathBuf,
}

impl Endpoint {
    fn capture(&self) -> Vec<u8> {
        fs::read(&self.capture).unwrap()
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Fixture {
    root: PathBuf,
    fake: PathBuf,
    body: PathBuf,
    next: usize,
}

impl Fixture {
    fn new() -> Self {
        // Parallel tests share the pid and can observe the same coarse clock
        // reading, so the counter -- not the timestamp -- is what keeps two
        // fixtures from colliding on one root and killing each other's fake.
        let root = env::temp_dir().join(format!(
            "kanban-opencode-adapter-e2e-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ROOT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let fake = root.join("fake-server");
        compile_fake("tests/fixtures/opencode_adapter_fake_server.rs", &fake);
        let body = root.join("acknowledgement.json");
        fs::write(&body, serde_json::to_vec(&acknowledgement()).unwrap()).unwrap();
        Self {
            root,
            fake,
            body,
            next: 0,
        }
    }

    /// Start the fake in `scenario` and wait until it publishes its port.
    fn start(&mut self, scenario: &str) -> Endpoint {
        self.next += 1;
        let port_file = self.root.join(format!("port-{}", self.next));
        let capture = self.root.join(format!("capture-{}", self.next));
        let child = Command::new(&self.fake)
            .arg(scenario)
            .arg(&port_file)
            .arg(&capture)
            .arg(&self.body)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = fs::read_to_string(&port_file)
                && let Ok(port) = text.trim().parse::<u16>()
            {
                return Endpoint {
                    child,
                    port,
                    capture,
                };
            }
            assert!(
                Instant::now() < deadline,
                "the {scenario} fake never published a port"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn post(&self, port: u16, timeout_ms: &str) -> Output {
        self.post_request(port, timeout_ms, &request())
    }

    fn post_request(&self, port: u16, timeout_ms: &str, request: &Value) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kanban-opencode-adapter"))
            .args(["--endpoint", &format!("http://127.0.0.1:{port}/delivery")])
            .args(["--request-timeout-ms", timeout_ms])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&serde_json::to_vec(request).unwrap())
            .unwrap();
        child.wait_with_output().unwrap()
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
    let binary = env!("CARGO_BIN_EXE_kanban-opencode-adapter");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert_eq!(
        String::from_utf8(help.stdout).unwrap(),
        "kanban-opencode-adapter --endpoint http://LOOPBACK_IP:PORT/ABSOLUTE_PATH --request-timeout-ms N\n"
    );
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("kanban-opencode-adapter {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn an_accepted_post_carries_the_delivery_and_returns_the_endpoint_acknowledgement() {
    let mut fixture = Fixture::new();
    let endpoint = fixture.start("accept");
    let output = fixture.post(endpoint.port, NORMAL_TIMEOUT_MS);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        acknowledgement()
    );

    let captured = endpoint.capture();
    let text = String::from_utf8(captured.clone()).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(
        head.starts_with("POST /delivery HTTP/1.1\r\n"),
        "unexpected request line in {head}"
    );
    assert!(head.contains(&format!("\r\nHost: 127.0.0.1:{}\r\n", endpoint.port)));
    assert!(head.contains("\r\nContent-Type: application/json\r\n"));
    assert!(head.contains(&format!("\r\nContent-Length: {}\r\n", body.len())));
    assert!(
        head.ends_with("\r\nConnection: close"),
        "the delivery did not ask the endpoint to close the connection: {head}"
    );
    assert_eq!(serde_json::from_str::<Value>(body).unwrap(), request());
}

#[test]
fn an_unreachable_endpoint_reports_the_retryable_unreachable_code() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let output = fixture.post(port, NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);
    assert_eq!(code(&output), 10, "{stderr}");
    assert!(
        stderr.contains("opencode_endpoint_unreachable (retryable)"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
}

#[test]
fn a_rejected_delivery_reports_the_terminal_rejected_code() {
    let mut fixture = Fixture::new();
    let endpoint = fixture.start("reject");
    let output = fixture.post(endpoint.port, NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 11, "{stderr}");
    assert!(
        stderr.contains("opencode_request_rejected (terminal)"),
        "{stderr}"
    );
    assert!(stderr.contains("HTTP 400"), "{stderr}");
    assert!(
        !stderr.contains("opencode_endpoint_unreachable"),
        "a rejection reported the unreachable code: {stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
}

#[test]
fn a_failing_endpoint_reports_the_retryable_endpoint_failure_code() {
    let mut fixture = Fixture::new();
    let endpoint = fixture.start("fail");
    let output = fixture.post(endpoint.port, NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 12, "{stderr}");
    assert!(
        stderr.contains("opencode_endpoint_failed (retryable)"),
        "{stderr}"
    );
    assert!(stderr.contains("HTTP 503"), "{stderr}");
    assert!(output.stdout.is_empty(), "{stderr}");
}

#[test]
fn an_acknowledgement_that_never_completes_is_never_reported_as_delivered() {
    let mut fixture = Fixture::new();
    let endpoint = fixture.start("truncate");
    let output = fixture.post(endpoint.port, NORMAL_TIMEOUT_MS);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 12, "{stderr}");
    assert!(
        stderr.contains("opencode_endpoint_failed (retryable)"),
        "{stderr}"
    );
    assert!(stderr.contains("closed the connection after"), "{stderr}");
    assert!(output.stdout.is_empty(), "{stderr}");
}

#[test]
fn a_breached_deadline_is_distinguishable_from_a_rejection() {
    let mut fixture = Fixture::new();
    let endpoint = fixture.start("hang");
    let started = Instant::now();
    let output = fixture.post(endpoint.port, DEADLINE_TIMEOUT_MS);
    let elapsed = started.elapsed();
    let stderr = stderr(&output);

    assert_eq!(code(&output), 13, "{stderr}");
    assert!(
        stderr.contains("opencode_deadline_exceeded (retryable)"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("opencode_request_rejected"),
        "a breached deadline reported the rejection code: {stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
    assert!(
        elapsed >= Duration::from_millis(1_000),
        "the adapter gave up after {elapsed:?}, before its own deadline"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the adapter waited {elapsed:?} on a hung endpoint"
    );
    assert!(
        !endpoint.capture().is_empty(),
        "the delivery was not posted"
    );
}

#[test]
fn a_delivery_for_another_consumer_never_reaches_the_endpoint() {
    let mut fixture = Fixture::new();
    let endpoint = fixture.start("accept");
    let mut wrong = request();
    wrong["target"]["consumerID"] = json!("codex.queue");
    let output = fixture.post_request(endpoint.port, NORMAL_TIMEOUT_MS, &wrong);
    let stderr = stderr(&output);

    assert_eq!(code(&output), 1, "{stderr}");
    assert!(
        stderr.contains("consumer ID must be opencode.server"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty(), "{stderr}");
    assert!(
        !endpoint.capture.exists(),
        "the adapter posted a delivery aimed at another consumer"
    );
}
