use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

fn fake_codex() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/codex_queue_adapter_fake_codex.rs");
            let root = env::temp_dir().join(format!(
                "kanban-codex-queue-adapter-fake-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&root).unwrap();
            let binary = root.join("fake-codex");
            let status = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .args(["--edition=2024"])
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "compile fake codex from {}",
                source.display()
            );
            binary
        })
        .as_path()
}

struct Fixture {
    root: PathBuf,
    codex_home: PathBuf,
    capture: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-codex-queue-adapter-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        let codex_home = root.join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let mut permissions = fs::metadata(&codex_home).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&codex_home, permissions).unwrap();
        let capture = fake_codex().parent().unwrap().join("capture.ndjson");
        let _ = fs::remove_file(&capture);
        Self {
            root,
            codex_home,
            capture,
        }
    }

    fn adapter(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban-codex-queue-adapter"));
        command
            .current_dir(&self.root)
            .arg("--codex")
            .arg(fake_codex())
            .arg("--codex-home")
            .arg(&self.codex_home)
            .arg("--thread")
            .arg("queue-thread")
            .arg("--required-version")
            .arg("1.2.3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn request(attempt: i64) -> Value {
    json!({
        "protocolVersion": 1,
        "delivery": {
            "subscriptionID": "sub-test",
            "eventID": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "attempt": attempt,
            "createdAt": 1720000000_i64,
        },
        "target": {
            "consumerID": "codex.queue",
            "actionID": "enqueue-turn"
        },
        "event": {
            "eventID": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "eventHash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "timestamp": 1720000000_i64
        }
    })
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn codex_queue_adapter_compiled_process_happy_path() {
    let fixture = Fixture::new("happy");

    let output = {
        let mut command = fixture.adapter();
        let request = serde_json::to_vec(&request(2)).unwrap();
        command.stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(&request).unwrap();
        child.wait_with_output().unwrap()
    };
    assert_success(&output, "adapter run");

    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response,
        json!({
            "protocolVersion": 1,
            "subscriptionID": "sub-test",
            "eventID": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "createdAt": 1720000000_i64,
            "replay": true
        })
    );
    assert!(output.stderr.is_empty());

    let capture = fs::read_to_string(&fixture.capture).unwrap();
    let records = capture
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3, "{capture}");

    let expected_env = json!([[
        "CODEX_HOME",
        fs::canonicalize(&fixture.codex_home)
            .unwrap()
            .to_string_lossy()
    ]]);

    let version = &records[0];
    assert_eq!(version["mode"], "version", "{capture}");
    assert_eq!(version["argv"], json!(["--version"]), "{capture}");
    assert_eq!(version["env"], expected_env, "{capture}");
    assert_eq!(version["stdin"], "", "{capture}");
    assert_eq!(version["stdout"], "codex-cli 1.2.3\n", "{capture}");

    let queue_help = &records[1];
    assert_eq!(queue_help["mode"], "queue-help", "{capture}");
    assert_eq!(queue_help["argv"], json!(["queue", "--help"]), "{capture}");
    assert_eq!(queue_help["env"], expected_env, "{capture}");
    assert_eq!(queue_help["stdin"], "", "{capture}");
    assert_eq!(
        queue_help["stdout"],
        "Queue a message for an existing session\n\nUsage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>\n\nOptions:\n      --config <PATH>    Use a named config file\n      --thread <THREAD>\n      --message <TEXT>\n",
        "{capture}"
    );

    let queue = &records[2];
    assert_eq!(queue["mode"], "queue", "{capture}");
    let queue_argv = queue["argv"].as_array().unwrap();
    assert_eq!(queue_argv.len(), 5, "{capture}");
    assert_eq!(queue_argv[0], "queue", "{capture}");
    assert_eq!(queue_argv[1], "--thread", "{capture}");
    assert_eq!(queue_argv[2], "queue-thread", "{capture}");
    assert_eq!(queue_argv[3], "--message", "{capture}");
    assert_eq!(queue["env"], expected_env, "{capture}");
    assert_eq!(queue["stdin"], "", "{capture}");
    assert_eq!(queue["stdout"], "Queued message\n", "{capture}");
    assert!(
        serde_json::from_str::<Value>(queue["stdout"].as_str().unwrap()).is_err(),
        "{capture}"
    );

    let message: Value = serde_json::from_str(queue_argv[4].as_str().unwrap()).unwrap();
    assert_eq!(
        message,
        json!({
            "instruction": "At-least-once delivery; deduplicate by idempotency key.",
            "idempotencyKey": "sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "subscriptionID": "sub-test",
            "eventID": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "attempt": 2,
            "event": {
                "eventHash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "eventID": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "timestamp": 1720000000_i64
            }
        }),
        "{capture}"
    );
}
