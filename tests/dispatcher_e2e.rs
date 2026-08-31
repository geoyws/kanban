use rusqlite::Connection;
use serde_json::{Value, json};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fake_adapter() -> &'static Path {
    static ADAPTER: OnceLock<PathBuf> = OnceLock::new();
    ADAPTER
        .get_or_init(|| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/dispatcher_fake_adapter.rs");
            let root = env::temp_dir().join(format!(
                "kanban-dispatcher-adapter-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&root).unwrap();
            let binary = root.join("fake-adapter");
            let status = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .args(["--edition=2024"])
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "compile fake adapter from {}",
                source.display()
            );
            binary
        })
        .as_path()
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    project: PathBuf,
    board: PathBuf,
    capture: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-dispatcher-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let output = Self::kanban_command(&project, &data)
            .args(["init", "--name", "DISPATCH-E2E", "--json"])
            .output()
            .unwrap();
        assert_success(&output, "kanban init");
        let initialized: Value = serde_json::from_slice(&output.stdout).unwrap();
        let board = PathBuf::from(initialized["boardPath"].as_str().unwrap());
        let capture = root.join("adapter-capture.ndjson");
        Self {
            root,
            data,
            project,
            board,
            capture,
        }
    }

    fn kanban_command(cwd: &Path, data: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(cwd)
            .env("KANBAN_DATA_DIR", data)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn kanban(&self, args: &[&str]) -> Output {
        Self::kanban_command(&self.project, &self.data)
            .args(args)
            .output()
            .unwrap()
    }

    fn dispatcher(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban-dispatcher"));
        command
            .current_dir(&self.project)
            .env("KANBAN_DATA_DIR", &self.data)
            .env("SOURCE_SECRET", "top-secret-value")
            .env("LEAK_ME", "must-not-reach-adapter")
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn add_subscription(&self, id: &str, consumer: &str, timeout_ms: &str, max_retries: &str) {
        let output = self.kanban(&[
            "subscription",
            "add",
            "--id",
            id,
            "--consumer",
            consumer,
            "--action",
            "send",
            "--timeout-ms",
            timeout_ms,
            "--max-retries",
            max_retries,
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--kind",
            "tag_added",
            "--secret-ref",
            "token",
            "--as",
            "test@dispatcher",
            "--json",
        ]);
        assert_success(&output, "subscription add");
    }

    fn append_event(&self, tag: &str) {
        let output = self.kanban(&["tag", "add", tag, "--as", "test@dispatcher", "--json"]);
        assert_success(&output, "tag add");
    }

    fn write_config(&self, primary_mode: &str, other_mode: Option<&str>) {
        let action = |mode: &str| {
            json!({
                "capability": "deliver",
                "executable": fake_adapter(),
                "args": [mode, self.capture],
            })
        };
        let mut consumers = serde_json::Map::new();
        consumers.insert(
            "consumer.test".into(),
            json!({
                "capabilities": ["deliver"],
                "actions": {"send": action(primary_mode)},
                "secrets": {"token": {"sourceEnv": "SOURCE_SECRET", "targetEnv": "DISPATCH_TOKEN"}},
            }),
        );
        if let Some(mode) = other_mode {
            consumers.insert(
                "consumer.other".into(),
                json!({
                    "capabilities": ["deliver"],
                    "actions": {"send": action(mode)},
                    "secrets": {"token": {"sourceEnv": "SOURCE_SECRET", "targetEnv": "DISPATCH_TOKEN"}},
                }),
            );
        }
        let path = self.data.join("dispatchers.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        serde_json::to_writer_pretty(&mut file, &json!({"version": 1, "consumers": consumers}))
            .unwrap();
        file.write_all(b"\n").unwrap();
    }

    fn seed(&self, mode: &str, timeout_ms: &str, max_retries: &str) {
        self.write_config(mode, None);
        self.add_subscription("sub-e2e", "consumer.test", timeout_ms, max_retries);
        self.append_event(&format!("dispatch-{}", self.label_suffix()));
    }

    fn label_suffix(&self) -> String {
        self.root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .chars()
            .rev()
            .take(12)
            .collect()
    }

    fn db(&self) -> Connection {
        Connection::open(&self.board).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

#[test]
fn dispatcher_help_version_and_conflicts_cross_the_compiled_process_boundary() {
    let fixture = Fixture::new("surface");
    fs::remove_file(fixture.data.join("registry.db")).unwrap();
    let help = fixture.dispatcher().arg("--help").output().unwrap();
    assert_success(&help, "dispatcher help");
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("--db PATH | --project NAME | --workspace PATH"));
    assert!(help.stderr.is_empty());
    assert!(!fixture.data.join("registry.db").exists());

    let version = fixture.dispatcher().arg("--version").output().unwrap();
    assert_success(&version, "dispatcher version");
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!("kanban-dispatcher {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(!fixture.data.join("registry.db").exists());

    let conflict = fixture
        .dispatcher()
        .env("KANBAN_DB", "/tmp/wrong.db")
        .args(["--db", fixture.board.to_str().unwrap(), "--once"])
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert!(conflict.stdout.is_empty());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("conflicts with KANBAN_DB"));
}

#[test]
fn dispatcher_success_targets_one_consumer_and_resolves_every_explicit_selector() {
    let fixture = Fixture::new("success");
    fixture.write_config("success", Some("exit"));
    fixture.add_subscription("sub-primary", "consumer.test", "1000", "1");
    fixture.add_subscription("sub-other", "consumer.other", "1000", "1");
    fixture.append_event("dispatch-success");

    let output = fixture
        .dispatcher()
        .args([
            "--db",
            fixture.board.to_str().unwrap(),
            "--consumer",
            "consumer.test",
            "--once",
            "--json",
        ])
        .output()
        .unwrap();
    assert_success(&output, "targeted dispatcher success");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["attempted"], 1);
    assert_eq!(report["succeeded"], 1);
    assert!(output.stderr.is_empty());

    let db = fixture.db();
    let primary: (String, Option<String>) = db
        .query_row(
            "SELECT status,last_error_code FROM subscription_deliveries WHERE subscription_id='sub-primary'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let other: String = db
        .query_row(
            "SELECT status FROM subscription_deliveries WHERE subscription_id='sub-other'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let outcome: String = db
        .query_row(
            "SELECT outcome FROM subscription_delivery_attempts WHERE subscription_id='sub-primary'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(primary, ("acked".into(), None));
    assert_eq!(other, "pending");
    assert_eq!(outcome, "success");
    let capture = fs::read_to_string(&fixture.capture).unwrap();
    assert!(capture.contains("secret=true inherited=false"), "{capture}");
    assert!(capture.contains("\"protocolVersion\":1"), "{capture}");
    assert!(capture.contains("\"schemaVersion\":1"), "{capture}");
    assert!(!capture.contains("top-secret-value"), "{capture}");
    assert!(!capture.contains("_semanticV1"), "{capture}");

    for selector in [
        vec!["--project", "DISPATCH-E2E"],
        vec!["--workspace", fixture.project.to_str().unwrap()],
    ] {
        let idle = fixture
            .dispatcher()
            .args(selector)
            .args(["--consumer", "consumer.test", "--once", "--json"])
            .output()
            .unwrap();
        assert_success(&idle, "registry selector");
        assert_eq!(
            serde_json::from_slice::<Value>(&idle.stdout).unwrap()["idle"],
            true
        );
    }
}

#[test]
fn dispatcher_failure_modes_are_safe_and_durable() {
    for (mode, expected, timeout) in [
        ("exit", "adapter_exit", "5000"),
        ("malformed", "adapter_response_invalid", "5000"),
        ("mismatch", "adapter_response_invalid", "5000"),
        ("oversized", "adapter_stdout_overflow", "5000"),
        ("sleep", "adapter_timeout", "500"),
    ] {
        let fixture = Fixture::new(mode);
        fixture.seed(mode, timeout, "0");
        let output = fixture
            .dispatcher()
            .args(["--db", fixture.board.to_str().unwrap(), "--once", "--json"])
            .output()
            .unwrap();
        assert_success(&output, mode);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["failed"], 1, "mode={mode}");
        assert!(output.stderr.is_empty(), "mode={mode}");
        let state: (String, String) = fixture
            .db()
            .query_row(
                "SELECT status,last_error_code FROM subscription_deliveries WHERE subscription_id='sub-e2e'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            state,
            ("dead_letter".into(), expected.into()),
            "mode={mode}"
        );
        let capture = fs::read_to_string(&fixture.capture).unwrap();
        assert!(
            !capture.contains("top-secret-value"),
            "mode={mode} {capture}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("top-secret-value"));
    }
}

#[test]
fn dispatcher_missing_config_fails_before_materialize_or_claim() {
    let fixture = Fixture::new("config-before-claim");
    fixture.add_subscription("sub-e2e", "consumer.test", "1000", "1");
    fixture.append_event("dispatch-config-before-claim");
    let output = fixture
        .dispatcher()
        .args(["--db", fixture.board.to_str().unwrap(), "--once", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("dispatcher config"));
    let count: i64 = fixture
        .db()
        .query_row("SELECT COUNT(*) FROM subscription_deliveries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn dispatcher_pause_and_resume_are_rechecked_at_the_process_boundary() {
    let fixture = Fixture::new("pause-resume");
    fixture.write_config("success", None);
    fixture.add_subscription("sub-e2e", "consumer.test", "1000", "1");
    assert_success(
        &fixture.kanban(&[
            "subscription",
            "pause",
            "sub-e2e",
            "--as",
            "test@dispatcher",
            "--json",
        ]),
        "pause subscription",
    );
    fixture.append_event("dispatch-while-paused");
    let paused = fixture
        .dispatcher()
        .args(["--db", fixture.board.to_str().unwrap(), "--once", "--json"])
        .output()
        .unwrap();
    assert_success(&paused, "paused dispatcher");
    assert_eq!(
        serde_json::from_slice::<Value>(&paused.stdout).unwrap()["idle"],
        true
    );
    assert_eq!(
        fixture
            .db()
            .query_row(
                "SELECT status FROM subscription_deliveries WHERE subscription_id='sub-e2e'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "pending"
    );

    assert_success(
        &fixture.kanban(&[
            "subscription",
            "resume",
            "sub-e2e",
            "--as",
            "test@dispatcher",
            "--json",
        ]),
        "resume subscription",
    );
    let resumed = fixture
        .dispatcher()
        .args(["--db", fixture.board.to_str().unwrap(), "--once", "--json"])
        .output()
        .unwrap();
    assert_success(&resumed, "resumed dispatcher");
    assert_eq!(
        serde_json::from_slice::<Value>(&resumed.stdout).unwrap()["succeeded"],
        1
    );
}

#[test]
fn concurrent_dispatcher_processes_deliver_one_event_only_once() {
    let fixture = Fixture::new("concurrent");
    fixture.seed("success", "1000", "1");
    let args = ["--db", fixture.board.to_str().unwrap(), "--once", "--json"];
    let first = fixture.dispatcher().args(args).spawn().unwrap();
    let second = fixture.dispatcher().args(args).spawn().unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert_success(&first, "first concurrent dispatcher");
    assert_success(&second, "second concurrent dispatcher");
    let delivered = [&first, &second]
        .into_iter()
        .map(|output| {
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["succeeded"]
                .as_u64()
                .unwrap()
        })
        .sum::<u64>();
    assert_eq!(delivered, 1);
    assert_eq!(
        fs::read_to_string(&fixture.capture)
            .unwrap()
            .lines()
            .count(),
        1
    );
    let db = fixture.db();
    assert_eq!(
        db.query_row(
            "SELECT status FROM subscription_deliveries WHERE subscription_id='sub-e2e'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        "acked"
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM subscription_delivery_attempts WHERE subscription_id='sub-e2e'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
}

#[test]
fn dispatcher_crash_after_adapter_success_recovers_at_least_once() {
    let fixture = Fixture::new("crash-recovery");
    fixture.seed("success", "5000", "2");
    let event_id: String = fixture
        .db()
        .query_row(
            "SELECT event_hash FROM events WHERE kind='tag_added' ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let crashed = fixture
        .dispatcher()
        .env("KANBAN_DISPATCHER_TEST_CRASH_AFTER_EVENT_ID", &event_id)
        .args(["--db", fixture.board.to_str().unwrap(), "--once", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "dispatcher did not hit the post-success crash seam; stdout={} stderr={}",
        String::from_utf8_lossy(&crashed.stdout),
        String::from_utf8_lossy(&crashed.stderr)
    );
    assert!(crashed.stdout.is_empty());
    let leased: String = fixture
        .db()
        .query_row(
            "SELECT status FROM subscription_deliveries WHERE subscription_id='sub-e2e'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leased, "leased");
    thread::sleep(Duration::from_millis(36_200));

    let recovered = fixture
        .dispatcher()
        .args(["--db", fixture.board.to_str().unwrap(), "--once", "--json"])
        .output()
        .unwrap();
    assert_success(&recovered, "crash recovery");
    assert_eq!(
        serde_json::from_slice::<Value>(&recovered.stdout).unwrap()["succeeded"],
        1
    );
    let db = fixture.db();
    let status: String = db
        .query_row(
            "SELECT status FROM subscription_deliveries WHERE subscription_id='sub-e2e'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "acked");
    let attempts = db
        .prepare(
            "SELECT attempt,outcome FROM subscription_delivery_attempts WHERE subscription_id='sub-e2e' ORDER BY attempt",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        attempts,
        vec![(1, "lease_expired".into()), (2, "success".into())]
    );
    assert_eq!(
        fs::read_to_string(&fixture.capture)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn dispatcher_sigterm_stops_long_poll_and_running_adapter_cleanly() {
    let idle = Fixture::new("signal-idle");
    idle.write_config("success", None);
    idle.add_subscription("sub-e2e", "consumer.test", "1000", "1");
    let child = idle
        .dispatcher()
        .args(["--db", idle.board.to_str().unwrap(), "--json"])
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_secs(1));
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_success(&output, "idle SIGTERM");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["cancelled"],
        true
    );

    let running = Fixture::new("signal-adapter");
    running.seed("sleep", "5000", "1");
    let child = running
        .dispatcher()
        .args(["--db", running.board.to_str().unwrap(), "--once", "--json"])
        .spawn()
        .unwrap();
    wait_for_file(&running.capture, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_success(&output, "adapter SIGTERM");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failed"], 1);
    let state: (String, String) = running
        .db()
        .query_row(
            "SELECT status,last_error_code FROM subscription_deliveries WHERE subscription_id='sub-e2e'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, ("retry_wait".into(), "adapter_cancelled".into()));
}
