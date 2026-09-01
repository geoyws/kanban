use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const THREAD_ID: &str = "01890f47-2f88-7b8f-9b2c-1c2d3e4f5a6b";
const TURN_ID: &str = "01890f47-2f88-7b8f-9b2c-1c2d3e4f5a6c";
const SUBSCRIPTION_ID: &str = "sub-test";
const EVENT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EVENT_HASH: &str = EVENT_ID;
const TEST_CODEX_VERSION: &str = env!("CARGO_PKG_VERSION");
const BASE_INSTRUCTIONS: &str = "Return only the JSON acknowledgement.";
const DEVELOPER_INSTRUCTIONS: &str =
    "Do not use tools, files, network, or commands. Return only the JSON acknowledgement.";
const AT_LEAST_ONCE_INSTRUCTION: &str = "At-least-once delivery; deduplicate by idempotency key.";
const TIMED_FAILURE_TIMEOUT_MS: u64 = 2_500;
const TIMED_FAILURE_SLEEP_MS: u64 = 4_000;

#[derive(Clone)]
enum Emit {
    None,
    Text(String),
    Repeat { byte: u8, count: usize },
    Sleep(u64),
}

impl Emit {
    fn text(value: Value) -> Self {
        Self::Text(value.to_string())
    }

    fn plain(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    fn spec(&self) -> String {
        match self {
            Self::None => "none".to_owned(),
            Self::Text(text) => format!("hex:{}", hex_encode(text.as_bytes())),
            Self::Repeat { byte, count } => format!("repeat:{count}:{}", char::from(*byte)),
            Self::Sleep(ms) => format!("sleep:{ms}"),
        }
    }
}

#[derive(Clone)]
struct CommandScenario {
    stdout: Emit,
    stderr: Emit,
    exit_code: i32,
    delay_ms: u64,
}

#[derive(Clone)]
struct ListenScenario {
    response1: Emit,
    response2: Emit,
    thread_started: Emit,
    response3: Emit,
    posts: Vec<Emit>,
    stderr: Emit,
    exit_code: i32,
    exit_after_stage: u8,
    wait_for_eof: bool,
    stubborn_after_stage: u8,
    pid_file: Option<PathBuf>,
    grandchild_pid_file: Option<PathBuf>,
}

#[derive(Clone)]
struct Scenario {
    version: CommandScenario,
    help: CommandScenario,
    schema_stdout: Emit,
    schema_stderr: Emit,
    schema_exit_code: i32,
    schema_client_request: Vec<u8>,
    schema_protocol: Vec<u8>,
    listen: ListenScenario,
}

type ScenarioMutator = fn(&mut Scenario);
type ProbeCase = (&'static str, ScenarioMutator);
type TimedCase = (&'static str, ScenarioMutator, u64, bool);

struct FixturePaths {
    cwd: PathBuf,
    codex_home: PathBuf,
}

#[derive(Clone)]
struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    codex_home: PathBuf,
    capture: PathBuf,
    codex: PathBuf,
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(rendered, "{byte:02x}").unwrap();
    }
    rendered
}

fn scenario_text(key_values: &[(String, String)]) -> String {
    let mut rendered = String::new();
    for (key, value) in key_values {
        rendered.push_str(key);
        rendered.push('=');
        rendered.push_str(value);
        rendered.push('\n');
    }
    rendered
}

fn load_capture(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap();
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect()
}

fn parse_line(line: &str) -> Value {
    serde_json::from_str(line.trim_end_matches('\n')).unwrap()
}

fn compile_fake_codex() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex_app_server_adapter_fake_codex.rs");
        let root = env::temp_dir().join(format!(
            "kanban-codex-app-server-adapter-fake-{}-{}",
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

fn process_test_mutex() -> &'static Mutex<()> {
    static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    MUTEX.get_or_init(|| Mutex::new(()))
}

fn thread_object(cwd: &str, thread_id: &str) -> Value {
    json!({
        "cliVersion": TEST_CODEX_VERSION,
        "id": thread_id,
        "cwd": cwd,
        "ephemeral": true,
        "modelProvider": "openai",
        "preview": "",
        "sessionId": "session-1",
        "createdAt": 1,
        "updatedAt": 2,
        "projectId": null,
        "source": "cli",
        "status": {"type": "idle"},
        "turns": []
    })
}

fn agent_ack_text(event_idempotency_key: &str) -> String {
    json!({
        "accepted": true,
        "idempotencyKey": event_idempotency_key,
    })
    .to_string()
}

fn scenario_for_cwd(cwd: &str, codex_home: &str, ack_key: &str) -> Scenario {
    let thread = thread_object(cwd, THREAD_ID);
    let response2 = json!({
        "id": 2,
        "result": {
            "approvalPolicy": "never",
            "approvalsReviewer": "user",
            "cwd": cwd,
            "model": "codex-1",
            "modelProvider": "openai",
            "sandbox": {"type": "readOnly", "networkAccess": false},
            "thread": thread.clone(),
        }
    })
    .to_string();
    let turn_items = vec![
        json!({"id":"u-1","type":"userMessage","content":[{"type":"text","text":"hi"}]}),
        json!({"id":"r-1","type":"reasoning","content":["reason"],"summary":["summary"]}),
        json!({"id":"a-1","type":"agentMessage","text":agent_ack_text(ack_key)}),
    ];
    Scenario {
        version: CommandScenario {
            stdout: Emit::plain(format!("codex-cli {TEST_CODEX_VERSION}\n")),
            stderr: Emit::None,
            exit_code: 0,
            delay_ms: 0,
        },
        help: CommandScenario {
            stdout: Emit::plain("Usage: codex app-server\n--listen <URL>\ngenerate-json-schema\n"),
            stderr: Emit::None,
            exit_code: 0,
            delay_ms: 0,
        },
        schema_stdout: Emit::None,
        schema_stderr: Emit::None,
        schema_exit_code: 0,
        schema_client_request: b"{\"kind\":\"client-request\"}".to_vec(),
        schema_protocol: b"{\"kind\":\"protocol-schema\"}".to_vec(),
        listen: ListenScenario {
            response1: Emit::text(json!({
                "id": 1,
                "result": {
                    "codexHome": codex_home,
                    "platformFamily": "unix",
                    "platformOs": "linux",
                    "userAgent": format!("codex-cli/{TEST_CODEX_VERSION}"),
                }
            })),
            response2: Emit::plain(response2),
            thread_started: Emit::text(json!({
                "method": "thread/started",
                "params": {
                    "thread": thread.clone(),
                }
            })),
            response3: Emit::text(json!({
                "id": 3,
                "result": {
                    "turn": {
                        "id": TURN_ID,
                        "status": "inProgress",
                        "items": []
                    }
                }
            })),
            posts: vec![
                Emit::text(json!({
                    "method": "turn/started",
                    "params": {
                        "threadId": THREAD_ID,
                        "turn": {"id": TURN_ID, "status": "inProgress", "items": []}
                    }
                })),
                Emit::text(json!({
                    "method": "item/started",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "startedAtMs": 1,
                        "item": {"id": "u-1", "type": "userMessage", "content": [{"type":"text","text":"hi"}]}
                    }
                })),
                Emit::text(json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "completedAtMs": 2,
                        "item": {"id": "u-1", "type": "userMessage", "content": [{"type":"text","text":"hi"}]}
                    }
                })),
                Emit::text(json!({
                    "method": "item/started",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "startedAtMs": 1,
                        "item": {"id": "r-1", "type": "reasoning", "content": ["reason"], "summary": ["summary"]}
                    }
                })),
                Emit::text(json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "completedAtMs": 2,
                        "item": {"id": "r-1", "type": "reasoning", "content": ["reason"], "summary": ["summary"]}
                    }
                })),
                Emit::text(json!({
                    "method": "item/started",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "startedAtMs": 1,
                        "item": {"id": "a-1", "type": "agentMessage", "text": agent_ack_text(ack_key)}
                    }
                })),
                Emit::text(json!({
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "itemId": "a-1",
                        "delta": "hello"
                    }
                })),
                Emit::text(json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "completedAtMs": 2,
                        "item": {"id": "a-1", "type": "agentMessage", "text": agent_ack_text(ack_key)}
                    }
                })),
                Emit::text(json!({
                    "method": "thread/tokenUsage/updated",
                    "params": {
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "tokenUsage": {
                            "last": {"input": 1, "output": 2},
                            "total": {"input": 3, "output": 4}
                        }
                    }
                })),
                Emit::text(json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": THREAD_ID,
                        "turn": {
                            "id": TURN_ID,
                            "status": "completed",
                            "error": null,
                            "items": turn_items,
                        }
                    }
                })),
            ],
            stderr: Emit::None,
            exit_code: 0,
            exit_after_stage: 0,
            wait_for_eof: true,
            stubborn_after_stage: 0,
            pid_file: None,
            grandchild_pid_file: None,
        },
    }
}

impl Scenario {
    fn serialize(&self) -> String {
        let mut lines = Vec::new();
        lines.push(("version.stdout".to_owned(), self.version.stdout.spec()));
        lines.push(("version.stderr".to_owned(), self.version.stderr.spec()));
        lines.push((
            "version.exit".to_owned(),
            self.version.exit_code.to_string(),
        ));
        lines.push((
            "version.delay_ms".to_owned(),
            self.version.delay_ms.to_string(),
        ));
        lines.push(("help.stdout".to_owned(), self.help.stdout.spec()));
        lines.push(("help.stderr".to_owned(), self.help.stderr.spec()));
        lines.push(("help.exit".to_owned(), self.help.exit_code.to_string()));
        lines.push(("help.delay_ms".to_owned(), self.help.delay_ms.to_string()));
        lines.push((
            "schema.client_request".to_owned(),
            format!("hex:{}", hex_encode(&self.schema_client_request)),
        ));
        lines.push((
            "schema.protocol".to_owned(),
            format!("hex:{}", hex_encode(&self.schema_protocol)),
        ));
        lines.push(("schema.stdout".to_owned(), self.schema_stdout.spec()));
        lines.push(("schema.stderr".to_owned(), self.schema_stderr.spec()));
        lines.push(("schema.exit".to_owned(), self.schema_exit_code.to_string()));
        lines.push(("listen.response1".to_owned(), self.listen.response1.spec()));
        lines.push(("listen.response2".to_owned(), self.listen.response2.spec()));
        lines.push((
            "listen.thread_started".to_owned(),
            self.listen.thread_started.spec(),
        ));
        lines.push(("listen.response3".to_owned(), self.listen.response3.spec()));
        lines.push((
            "listen.post_count".to_owned(),
            self.listen.posts.len().to_string(),
        ));
        for (index, post) in self.listen.posts.iter().enumerate() {
            lines.push((format!("listen.post{index}"), post.spec()));
        }
        lines.push(("listen.stderr".to_owned(), self.listen.stderr.spec()));
        lines.push(("listen.exit".to_owned(), self.listen.exit_code.to_string()));
        lines.push((
            "listen.exit_after_stage".to_owned(),
            self.listen.exit_after_stage.to_string(),
        ));
        lines.push((
            "listen.wait_for_eof".to_owned(),
            if self.listen.wait_for_eof {
                "true".to_owned()
            } else {
                "false".to_owned()
            },
        ));
        lines.push((
            "listen.stubborn_after_stage".to_owned(),
            self.listen.stubborn_after_stage.to_string(),
        ));
        if let Some(path) = &self.listen.pid_file {
            lines.push((
                "listen.pid_file".to_owned(),
                path.to_string_lossy().into_owned(),
            ));
        }
        if let Some(path) = &self.listen.grandchild_pid_file {
            lines.push((
                "listen.grandchild_pid_file".to_owned(),
                path.to_string_lossy().into_owned(),
            ));
        }
        scenario_text(&lines)
    }
}

impl Fixture {
    fn new<F>(label: &str, build: F) -> Self
    where
        F: FnOnce(&FixturePaths) -> Scenario,
    {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-codex-app-server-adapter-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let mut permissions = fs::metadata(&root).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&root, permissions).unwrap();

        let cwd = root.join("cwd");
        let codex_home = root.join("codex-home");
        let capture = root.join("capture.ndjson");
        let scenario = root.join("scenario.txt");
        let codex = root.join("fake-codex");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        for path in [&cwd, &codex_home] {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).unwrap();
        }
        let codex_home = codex_home.canonicalize().unwrap();

        fs::copy(compile_fake_codex(), &codex).unwrap();
        let mut permissions = fs::metadata(&codex).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&codex, permissions).unwrap();

        let paths = FixturePaths {
            cwd: cwd.clone(),
            codex_home: codex_home.clone(),
        };
        let scenario_value = build(&paths);
        fs::write(&scenario, scenario_value.serialize()).unwrap();

        Self {
            root,
            cwd,
            codex_home,
            capture,
            codex,
        }
    }

    fn adapter_command(
        &self,
        client_request_hash: &str,
        protocol_schema_hash: &str,
        timeout_ms: u64,
    ) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban-codex-app-server-adapter"));
        command
            .current_dir(&self.root)
            .arg("--codex")
            .arg(&self.codex)
            .arg("--codex-home")
            .arg(&self.codex_home)
            .arg("--cwd")
            .arg(&self.cwd)
            .arg("--required-version")
            .arg(TEST_CODEX_VERSION)
            .arg("--client-request-sha256")
            .arg(client_request_hash)
            .arg("--protocol-schema-sha256")
            .arg(protocol_schema_hash)
            .arg("--protocol-timeout-ms")
            .arg(timeout_ms.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(
        &self,
        request: &Value,
        client_request_hash: &str,
        protocol_schema_hash: &str,
        timeout_ms: u64,
    ) -> Output {
        let before = self.cwd_entries();
        let mut child = self
            .adapter_command(client_request_hash, protocol_schema_hash, timeout_ms)
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(serde_json::to_string(request).unwrap().as_bytes())
            .unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert_eq!(self.cwd_entries(), before);
        output
    }

    fn capture_records(&self) -> Vec<Value> {
        load_capture(&self.capture)
    }

    fn cwd_entries(&self) -> Vec<String> {
        let mut entries = fs::read_dir(&self.cwd)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn request() -> Value {
    json!({
        "protocolVersion": 1,
        "delivery": {
            "subscriptionID": SUBSCRIPTION_ID,
            "eventID": EVENT_ID,
            "attempt": 2,
            "createdAt": 1720000000_i64,
        },
        "target": {
            "consumerID": "codex.app-server",
            "actionID": "start-readonly-turn"
        },
        "event": {
            "eventHash": EVENT_HASH,
            "eventID": EVENT_ID,
            "timestamp": 1720000000_i64
        }
    })
}

fn schema_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn parse_invocation<'a>(records: &'a [Value], mode: &str) -> &'a Value {
    records
        .iter()
        .find(|record| record["mode"] == mode)
        .unwrap_or_else(|| panic!("missing invocation mode {mode}"))
}

fn assert_failure(output: &Output) {
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "unexpected adapter stdout: {:?}",
        output.stdout
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("Error:"));
    assert!(stderr.len() < 8192, "stderr too long: {}", stderr.len());
}

fn pid_exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks process existence/permission.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn assert_pid_disappears(pid: i32, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_exists(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!pid_exists(pid), "{label} process {pid} survived cleanup");
}

fn happy_fixture(label: &str) -> Fixture {
    Fixture::new(label, |paths| {
        let cwd = paths.cwd.canonicalize().unwrap();
        let codex_home = paths.codex_home.canonicalize().unwrap();
        scenario_for_cwd(
            &cwd.to_string_lossy(),
            &codex_home.to_string_lossy(),
            &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
        )
    })
}

fn fixture_with_mutation<F>(label: &str, mutate: F) -> Fixture
where
    F: FnOnce(&mut Scenario),
{
    Fixture::new(label, |paths| {
        let cwd = paths.cwd.canonicalize().unwrap();
        let codex_home = paths.codex_home.canonicalize().unwrap();
        let mut scenario = scenario_for_cwd(
            &cwd.to_string_lossy(),
            &codex_home.to_string_lossy(),
            &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
        );
        mutate(&mut scenario);
        scenario
    })
}

fn fixture_with_path_mutation<F>(label: &str, mutate: F) -> Fixture
where
    F: FnOnce(&FixturePaths, &mut Scenario),
{
    Fixture::new(label, |paths| {
        let cwd = paths.cwd.canonicalize().unwrap();
        let codex_home = paths.codex_home.canonicalize().unwrap();
        let mut scenario = scenario_for_cwd(
            &cwd.to_string_lossy(),
            &codex_home.to_string_lossy(),
            &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
        );
        mutate(paths, &mut scenario);
        scenario
    })
}

#[test]
fn compiled_process_happy_path_reaches_the_exact_transcript() {
    let _test_guard = process_test_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = happy_fixture("happy");
    let fixture_cwd = fixture.cwd.canonicalize().unwrap();
    let fixture_codex_home = fixture.codex_home.canonicalize().unwrap();
    let scenario = scenario_for_cwd(
        &fixture_cwd.to_string_lossy(),
        &fixture_codex_home.to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let client_request_hash = schema_hash(&scenario.schema_client_request);
    let protocol_schema_hash = schema_hash(&scenario.schema_protocol);
    let output = fixture.run(
        &request(),
        &client_request_hash,
        &protocol_schema_hash,
        4000,
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_str::<Value>(String::from_utf8(output.stdout.clone()).unwrap().trim())
            .unwrap(),
        json!({
            "protocolVersion": 1,
            "subscriptionID": SUBSCRIPTION_ID,
            "eventID": EVENT_ID,
            "createdAt": 1720000000_i64,
            "replay": true,
        })
    );
    assert!(output.stderr.is_empty());

    let records = fixture.capture_records();
    assert_eq!(records.len(), 18, "{records:?}");
    assert_eq!(
        records
            .iter()
            .map(|value| value["mode"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "version-stage",
            "version-stage",
            "version",
            "help-stage",
            "help-stage",
            "help",
            "schema-stage",
            "schema-stage",
            "schema",
            "listen-stage",
            "listen-stage",
            "listen-stage",
            "listen-stage",
            "listen-stage",
            "listen-stage",
            "listen-stage",
            "listen-stage",
            "listen"
        ]
    );

    let version = parse_invocation(&records, "version");
    let codex_home = fixture
        .codex_home
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let cwd = fixture
        .cwd
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(version["argv"], json!(["--version"]));
    assert_eq!(version["env"], json!([["CODEX_HOME", codex_home]]));
    assert_eq!(version["cwd"], json!(cwd));
    assert_eq!(
        version["stdout"].as_str().unwrap().trim_end_matches('\n'),
        format!("codex-cli {TEST_CODEX_VERSION}")
    );

    let help = parse_invocation(&records, "help");
    assert_eq!(help["argv"], json!(["app-server", "--help"]));
    assert_eq!(
        help["stdout"].as_str().unwrap().trim_end_matches('\n'),
        "Usage: codex app-server\n--listen <URL>\ngenerate-json-schema"
    );

    let schema = parse_invocation(&records, "schema");
    let out_dir = PathBuf::from(schema["out_dir"].as_str().unwrap());
    assert!(!out_dir.starts_with(&fixture.cwd));
    assert!(!out_dir.exists());

    let listen_stages = records
        .iter()
        .filter(|record| record["mode"] == "listen-stage")
        .collect::<Vec<_>>();
    assert_eq!(listen_stages.len(), 8, "{listen_stages:?}");
    assert_eq!(listen_stages[0]["stage"], "listen");
    assert_eq!(listen_stages[0]["phase"], "entered");
    assert_eq!(listen_stages[1]["stage"], "initialize");
    assert_eq!(listen_stages[1]["phase"], "received");
    assert_eq!(listen_stages[2]["stage"], "initialize");
    assert_eq!(listen_stages[2]["phase"], "emitted");
    assert_eq!(listen_stages[3]["stage"], "initialized");
    assert_eq!(listen_stages[3]["phase"], "received");
    assert_eq!(listen_stages[4]["stage"], "thread/start");
    assert_eq!(listen_stages[4]["phase"], "received");
    assert_eq!(listen_stages[5]["stage"], "thread/start");
    assert_eq!(listen_stages[5]["phase"], "emitted");
    assert_eq!(listen_stages[6]["stage"], "turn/start");
    assert_eq!(listen_stages[6]["phase"], "received");
    assert_eq!(listen_stages[7]["stage"], "turn/start");
    assert_eq!(listen_stages[7]["phase"], "emitted");

    let listen = parse_invocation(&records, "listen");
    let stdin_lines = listen["stdin"]
        .as_str()
        .unwrap()
        .lines()
        .collect::<Vec<_>>();
    assert_eq!(stdin_lines.len(), 4);

    let initialize = parse_line(stdin_lines[0]);
    assert_eq!(initialize["id"], 1);
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(
        initialize["params"]["clientInfo"],
        json!({
            "name": "kanban-codex-app-server-adapter",
            "version": TEST_CODEX_VERSION,
        })
    );
    assert_eq!(
        initialize["params"]["capabilities"]["experimentalApi"],
        false
    );
    assert_eq!(
        initialize["params"]["capabilities"]["optOutNotificationMethods"],
        json!([
            "remoteControl/status/changed",
            "mcpServer/startupStatus/updated",
            "thread/status/changed",
            "account/rateLimits/updated",
            "item/reasoning/summaryTextDelta",
            "item/reasoning/summaryPartAdded",
            "item/reasoning/textDelta",
        ])
    );

    let initialized = parse_line(stdin_lines[1]);
    assert_eq!(initialized, json!({"method":"initialized","params":{}}));

    let thread_start = parse_line(stdin_lines[2]);
    assert_eq!(thread_start["id"], 2);
    assert_eq!(thread_start["method"], "thread/start");
    assert_eq!(
        thread_start["params"]["cwd"],
        fixture_cwd.to_string_lossy().into_owned()
    );
    assert_eq!(thread_start["params"]["approvalPolicy"], "never");
    assert_eq!(thread_start["params"]["sandbox"], "read-only");
    assert_eq!(thread_start["params"]["ephemeral"], true);
    assert_eq!(
        thread_start["params"]["baseInstructions"],
        BASE_INSTRUCTIONS
    );
    assert_eq!(
        thread_start["params"]["developerInstructions"],
        DEVELOPER_INSTRUCTIONS
    );

    let turn_start = parse_line(stdin_lines[3]);
    assert_eq!(turn_start["id"], 3);
    assert_eq!(turn_start["method"], "turn/start");
    assert_eq!(turn_start["params"]["threadId"], THREAD_ID);
    assert_eq!(turn_start["params"]["approvalPolicy"], "never");
    assert_eq!(
        turn_start["params"]["sandboxPolicy"],
        json!({"type":"readOnly","networkAccess":false})
    );
    assert_eq!(
        turn_start["params"]["outputSchema"]["properties"]["accepted"],
        json!({"type":"boolean","const":true})
    );
    assert_eq!(
        turn_start["params"]["outputSchema"]["properties"]["idempotencyKey"],
        json!({"type":"string","const": format!("{SUBSCRIPTION_ID}:{EVENT_ID}")})
    );
    assert_eq!(
        turn_start["params"]["outputSchema"]["required"],
        json!(["accepted", "idempotencyKey"])
    );
    let prompt =
        serde_json::from_str::<Value>(turn_start["params"]["input"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(prompt["instruction"], AT_LEAST_ONCE_INSTRUCTION);
    assert_eq!(
        prompt["idempotencyKey"],
        format!("{SUBSCRIPTION_ID}:{EVENT_ID}")
    );
    assert_eq!(
        prompt["event"],
        json!({"eventHash":EVENT_HASH,"eventID":EVENT_ID,"timestamp":1720000000_i64})
    );

    let listen_stdout_lines = listen["stdout"]
        .as_str()
        .unwrap()
        .lines()
        .collect::<Vec<_>>();
    assert_eq!(listen_stdout_lines.len(), 14);
    let initialize_response = parse_line(listen_stdout_lines[0]);
    assert_eq!(
        initialize_response["result"]["codexHome"],
        json!(fixture_codex_home.to_string_lossy().into_owned())
    );
    assert_eq!(initialize_response["result"]["platformFamily"], "unix");
    assert_eq!(initialize_response["result"]["platformOs"], "linux");
    assert_eq!(
        initialize_response["result"]["userAgent"],
        format!("codex-cli/{TEST_CODEX_VERSION}")
    );
    assert_eq!(
        parse_line(&format!("{}\n", listen_stdout_lines[1]))["id"],
        2
    );
    assert_eq!(
        parse_line(&format!("{}\n", listen_stdout_lines[2]))["method"],
        "thread/started"
    );
    assert_eq!(
        parse_line(&format!("{}\n", listen_stdout_lines[3]))["id"],
        3
    );
    assert_eq!(
        parse_line(&format!("{}\n", listen_stdout_lines[13]))["method"],
        "turn/completed"
    );
    assert!(listen["stderr"].as_str().unwrap().is_empty());
}

#[test]
fn compiled_process_rechecks_that_cwd_stays_empty_before_each_spawn() {
    let _test_guard = process_test_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = fixture_with_mutation("cwd-dirty-after-validate", |scenario| {
        scenario.version.delay_ms = 1_000;
    });
    let dirty_path = fixture.cwd.join("marker");
    let capture_path = fixture.capture.clone();
    let writer = std::thread::spawn({
        let dirty_path = dirty_path.clone();
        move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if fs::read_to_string(&capture_path)
                    .map(|text| {
                        text.contains("\"mode\":\"version-stage\"")
                            && text.contains("\"phase\":\"entered\"")
                    })
                    .unwrap_or(false)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "version probe did not reach the entered boundary"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            fs::write(dirty_path, b"seed").unwrap();
        }
    });
    let scenario = scenario_for_cwd(
        &fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &fixture.codex_home.canonicalize().unwrap().to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let mut command = fixture.adapter_command(
        &schema_hash(&scenario.schema_client_request),
        &schema_hash(&scenario.schema_protocol),
        4_000,
    );
    let request = serde_json::to_vec(&request()).unwrap();
    let mut child = command.spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(&request).unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert_failure(&output);
    let records = fixture.capture_records();
    let modes = records
        .iter()
        .map(|record| record["mode"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(modes, vec!["version-stage", "version-stage", "version"]);
    assert!(!records.iter().any(|record| record["mode"] == "help-stage"));
    assert!(
        !records
            .iter()
            .any(|record| record["mode"] == "listen-stage")
    );
    assert!(!records.iter().any(|record| record["mode"] == "listen"));
}

#[test]
fn compiled_process_initialize_response_user_agent_drift_is_fail_closed() {
    let fixture = fixture_with_path_mutation("init-user-agent-wrong", |paths, scenario| {
        let codex_home = paths.codex_home.canonicalize().unwrap();
        scenario.listen.response1 = Emit::text(json!({
            "id": 1,
            "result": {
                "codexHome": codex_home.to_string_lossy().into_owned(),
                "platformFamily": "unix",
                "platformOs": "linux",
                "userAgent": format!("codex-cli/{TEST_CODEX_VERSION}.1"),
            }
        }));
    });
    let scenario = scenario_for_cwd(
        &fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &fixture.codex_home.canonicalize().unwrap().to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let output = fixture.run(
        &request(),
        &schema_hash(&scenario.schema_client_request),
        &schema_hash(&scenario.schema_protocol),
        4_000,
    );
    assert_failure(&output);
    let records = fixture.capture_records();
    assert!(!records.iter().any(|record| record["mode"] == "listen"));
}

#[test]
fn compiled_process_thread_cli_version_drift_is_fail_closed() {
    let fixture = fixture_with_path_mutation("thread-cli-version-wrong", |paths, scenario| {
        let cwd = paths.cwd.canonicalize().unwrap();
        scenario.listen.response2 = Emit::text(json!({
            "id": 2,
            "result": {
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "cwd": cwd.to_string_lossy().into_owned(),
                "model": "codex-1",
                "modelProvider": "openai",
                "sandbox": {"type": "readOnly", "networkAccess": false},
                "thread": {
                    "cliVersion": format!("{TEST_CODEX_VERSION}.1"),
                    "id": THREAD_ID,
                    "cwd": cwd.to_string_lossy().into_owned(),
                    "ephemeral": true,
                    "modelProvider": "openai",
                    "preview": "",
                    "sessionId": "session-1",
                    "createdAt": 1,
                    "updatedAt": 2,
                    "projectId": null,
                    "source": "cli",
                    "status": {"type": "idle"},
                    "turns": []
                }
            }
        }));
    });
    let scenario = scenario_for_cwd(
        &fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &fixture.codex_home.canonicalize().unwrap().to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let output = fixture.run(
        &request(),
        &schema_hash(&scenario.schema_client_request),
        &schema_hash(&scenario.schema_protocol),
        4_000,
    );
    assert_failure(&output);
    let records = fixture.capture_records();
    assert!(!records.iter().any(|record| record["mode"] == "listen"));
}

#[test]
fn compiled_process_cwd_and_thread_identity_drift_are_independent() {
    let cwd_fixture = fixture_with_path_mutation("cwd-mismatch", |paths, scenario| {
        let cwd = paths.cwd.canonicalize().unwrap();
        scenario.listen.response2 = Emit::text(json!({
            "id": 2,
            "result": {
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "cwd": "/wrong",
                "model": "codex-1",
                "modelProvider": "openai",
                "sandbox": {"type": "readOnly", "networkAccess": false},
                "thread": {
                    "cliVersion": TEST_CODEX_VERSION,
                    "id": THREAD_ID,
                    "cwd": cwd.to_string_lossy().into_owned(),
                    "ephemeral": true,
                    "modelProvider": "openai",
                    "preview": "",
                    "sessionId": "session-1",
                    "createdAt": 1,
                    "updatedAt": 2,
                    "projectId": null,
                    "source": "cli",
                    "status": {"type": "idle"},
                    "turns": []
                }
            }
        }));
    });
    let cwd_scenario = scenario_for_cwd(
        &cwd_fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &cwd_fixture
            .codex_home
            .canonicalize()
            .unwrap()
            .to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let cwd_output = cwd_fixture.run(
        &request(),
        &schema_hash(&cwd_scenario.schema_client_request),
        &schema_hash(&cwd_scenario.schema_protocol),
        4_000,
    );
    assert_failure(&cwd_output);

    let thread_fixture = fixture_with_path_mutation("thread-id-mismatch", |paths, scenario| {
        let cwd = paths.cwd.canonicalize().unwrap();
        scenario.listen.response2 = Emit::text(json!({
            "id": 2,
            "result": {
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "cwd": cwd.to_string_lossy().into_owned(),
                "model": "codex-1",
                "modelProvider": "openai",
                "sandbox": {"type": "readOnly", "networkAccess": false},
                "thread": {
                    "cliVersion": TEST_CODEX_VERSION,
                    "id": "wrong-thread",
                    "cwd": cwd.to_string_lossy().into_owned(),
                    "ephemeral": true,
                    "modelProvider": "openai",
                    "preview": "",
                    "sessionId": "session-1",
                    "createdAt": 1,
                    "updatedAt": 2,
                    "projectId": null,
                    "source": "cli",
                    "status": {"type": "idle"},
                    "turns": []
                }
            }
        }));
    });
    let thread_scenario = scenario_for_cwd(
        &thread_fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &thread_fixture
            .codex_home
            .canonicalize()
            .unwrap()
            .to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let thread_output = thread_fixture.run(
        &request(),
        &schema_hash(&thread_scenario.schema_client_request),
        &schema_hash(&thread_scenario.schema_protocol),
        4_000,
    );
    assert_failure(&thread_output);
}

#[test]
fn compiled_process_probe_failures_stay_out_of_interactive_mode() {
    let _test_guard = process_test_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let happy = scenario_for_cwd(
        "/tmp/cwd",
        "/private/tmp/kanban-codex-home",
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let good_client_request_hash = schema_hash(&happy.schema_client_request);
    let good_protocol_schema_hash = schema_hash(&happy.schema_protocol);

    let cases: Vec<ProbeCase> = vec![
        ("wrong-version", |scenario| {
            scenario.version.stdout = Emit::plain("codex-cli 9.9.9\n");
        }),
        ("help-drift", |scenario| {
            scenario.help.stdout = Emit::plain("Usage: codex something-else\n");
        }),
        ("client-request-drift", |scenario| {
            scenario.schema_client_request = b"{\"kind\":\"client-request-drift\"}".to_vec();
        }),
        ("protocol-schema-drift", |scenario| {
            scenario.schema_protocol = b"{\"kind\":\"protocol-schema-drift\"}".to_vec();
        }),
        ("probe-stderr", |scenario| {
            scenario.version.stderr = Emit::plain("probe stderr\n");
        }),
    ];

    for (label, mutate) in cases {
        let fixture = fixture_with_mutation(label, mutate);
        let output = fixture.run(
            &request(),
            &good_client_request_hash,
            &good_protocol_schema_hash,
            4000,
        );
        assert_failure(&output);
        let records = fixture.capture_records();
        assert!(
            !records
                .iter()
                .any(|record| record["mode"] == "listen" || record["mode"] == "listen-stage"),
            "{records:?}"
        );
        assert!(
            records
                .iter()
                .map(|record| record["mode"].as_str().unwrap())
                .take(3)
                .collect::<Vec<_>>()
                .starts_with(&["version-stage", "version-stage", "version"]),
            "{records:?}"
        );
    }
}

#[test]
fn compiled_process_protocol_and_policy_failures_are_sanitized() {
    let _test_guard = process_test_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let happy = scenario_for_cwd(
        "/tmp/cwd",
        "/private/tmp/kanban-codex-home",
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let good_client_request_hash = schema_hash(&happy.schema_client_request);
    let good_protocol_schema_hash = schema_hash(&happy.schema_protocol);

    let cases: Vec<ProbeCase> = vec![
        ("wrong-response-ids", |scenario| {
            scenario.listen.response1 = Emit::plain("{\"id\":99,\"result\":{}}");
        }),
        ("wrong-approval", |scenario| {
            scenario.listen.response2 =
                Emit::plain("{\"id\":2,\"result\":{\"approvalPolicy\":\"on-request\"}}");
        }),
        ("turn-status", |scenario| {
            scenario.listen.response3 = Emit::plain(
                "{\"id\":3,\"result\":{\"turn\":{\"id\":\"wrong-turn\",\"status\":\"failed\",\"items\":[]}}}",
            );
        }),
        ("false-ack", |scenario| {
            scenario.listen.posts = vec![Emit::plain(
                "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-1\",\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\",\"error\":null,\"items\":[{\"id\":\"a-1\",\"type\":\"agentMessage\",\"text\":\"{\\\"accepted\\\":false,\\\"idempotencyKey\\\":\\\"sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\\"}\"}]}}}",
            )];
        }),
        ("wrong-ack", |scenario| {
            scenario.listen.posts = vec![Emit::plain(
                "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-1\",\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\",\"error\":null,\"items\":[{\"id\":\"a-1\",\"type\":\"agentMessage\",\"text\":\"{\\\"accepted\\\":true,\\\"idempotencyKey\\\":\\\"wrong\\\"}\"}]}}}",
            )];
        }),
        ("extra-ack", |scenario| {
            scenario.listen.posts = vec![Emit::plain(
                "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-1\",\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\",\"error\":null,\"items\":[{\"id\":\"a-1\",\"type\":\"agentMessage\",\"text\":\"{\\\"accepted\\\":true,\\\"idempotencyKey\\\":\\\"sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\\",\\\"extra\\\":1}\"}]}}}",
            )];
        }),
        ("duplicate-ack", |scenario| {
            scenario.listen.posts = vec![Emit::plain(
                "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-1\",\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\",\"error\":null,\"items\":[{\"id\":\"a-1\",\"type\":\"agentMessage\",\"text\":\"{\\\"accepted\\\":true,\\\"idempotencyKey\\\":\\\"sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\\"}\"},{\"id\":\"a-1\",\"type\":\"agentMessage\",\"text\":\"{\\\"accepted\\\":true,\\\"idempotencyKey\\\":\\\"sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\\"}\"}]}}}",
            )];
        }),
        ("tool-item", |scenario| {
            scenario.listen.posts = vec![Emit::plain(
                "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-1\",\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\",\"error\":null,\"items\":[{\"id\":\"x-1\",\"type\":\"commandExecution\"}]}}}",
            )];
        }),
        ("unknown-notification", |scenario| {
            scenario.listen.posts = vec![Emit::plain(
                "{\"method\":\"mystery/changed\",\"params\":{}}",
            )];
        }),
        ("malformed-json", |scenario| {
            scenario.listen.response2 = Emit::plain("{not-json");
        }),
    ];

    for (label, mutate) in cases {
        let fixture = fixture_with_mutation(label, mutate);
        let output = fixture.run(
            &request(),
            &good_client_request_hash,
            &good_protocol_schema_hash,
            4000,
        );
        assert_failure(&output);
        let records = fixture.capture_records();
        assert_eq!(
            records
                .iter()
                .map(|record| record["mode"].as_str().unwrap())
                .take(10)
                .collect::<Vec<_>>(),
            vec![
                "version-stage",
                "version-stage",
                "version",
                "help-stage",
                "help-stage",
                "help",
                "schema-stage",
                "schema-stage",
                "schema",
                "listen-stage",
            ],
            "{records:?}"
        );
        assert!(
            records
                .iter()
                .any(|record| record["mode"] == "listen-stage")
        );
        let received_stage = |stage: &str| {
            records.iter().any(|record| {
                record["mode"] == "listen-stage"
                    && record["phase"] == "received"
                    && record["stage"] == stage
            })
        };
        let (expected_thread_start, expected_turn_start) = match label {
            "wrong-response-ids" => (false, false),
            "wrong-approval" | "malformed-json" => (true, false),
            _ => (true, true),
        };
        assert_eq!(
            received_stage("thread/start"),
            expected_thread_start,
            "{label}: {records:?}"
        );
        assert_eq!(
            received_stage("turn/start"),
            expected_turn_start,
            "{label}: {records:?}"
        );
        assert!(output.stdout.is_empty());
        assert!(output.status.code().is_some());
    }
}

#[test]
fn compiled_process_turn_start_items_fail_closed_exactly_at_turn_start() {
    let fixture = fixture_with_mutation("turn-start-items", |scenario| {
        scenario.listen.response3 = Emit::plain(
            "{\"id\":3,\"result\":{\"turn\":{\"id\":\"01890f47-2f88-7b8f-9b2c-1c2d3e4f5a6c\",\"status\":\"inProgress\",\"items\":[{\"id\":\"u-1\",\"type\":\"userMessage\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}]}}}",
        );
    });
    let scenario = scenario_for_cwd(
        &fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &fixture.codex_home.canonicalize().unwrap().to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let output = fixture.run(
        &request(),
        &schema_hash(&scenario.schema_client_request),
        &schema_hash(&scenario.schema_protocol),
        4000,
    );
    assert_failure(&output);
    let records = fixture.capture_records();
    assert!(
        records
            .iter()
            .any(|record| record["mode"] == "listen-stage" && record["stage"] == "turn/start"),
        "{records:?}"
    );
}

#[test]
fn compiled_process_turn_started_notification_items_fail_closed_exactly_at_notification() {
    let fixture = fixture_with_mutation("turn-start-notification-items", |scenario| {
        scenario.listen.posts[0] = Emit::plain(
            "{\"method\":\"turn/started\",\"params\":{\"threadId\":\"01890f47-2f88-7b8f-9b2c-1c2d3e4f5a6b\",\"turn\":{\"id\":\"01890f47-2f88-7b8f-9b2c-1c2d3e4f5a6c\",\"status\":\"inProgress\",\"items\":[{\"id\":\"u-1\",\"type\":\"userMessage\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}]}}}",
        );
    });
    let scenario = scenario_for_cwd(
        &fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &fixture.codex_home.canonicalize().unwrap().to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let output = fixture.run(
        &request(),
        &schema_hash(&scenario.schema_client_request),
        &schema_hash(&scenario.schema_protocol),
        4000,
    );
    assert_failure(&output);
    let records = fixture.capture_records();
    assert!(
        records
            .iter()
            .any(|record| record["mode"] == "listen-stage" && record["stage"] == "turn/start"),
        "{records:?}"
    );
}

#[test]
fn compiled_process_initialize_response_failures_are_fail_closed() {
    let _test_guard = process_test_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let happy = scenario_for_cwd(
        "/tmp/cwd",
        "/private/tmp/kanban-codex-home",
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let good_client_request_hash = schema_hash(&happy.schema_client_request);
    let good_protocol_schema_hash = schema_hash(&happy.schema_protocol);

    let cases: Vec<ProbeCase> = vec![
        ("init-codex-home-mismatch", |scenario| {
            scenario.listen.response1 = Emit::plain(
                "{\"id\":1,\"result\":{\"codexHome\":\"/wrong\",\"platformFamily\":\"unix\",\"platformOs\":\"linux\",\"userAgent\":\"codex-cli/0.150.1.1\"}}",
            );
        }),
        ("init-user-agent-missing", |scenario| {
            scenario.listen.response1 = Emit::plain(
                "{\"id\":1,\"result\":{\"codexHome\":\"/private/tmp/kanban-codex-home\",\"platformFamily\":\"unix\",\"platformOs\":\"linux\"}}",
            );
        }),
    ];

    for (label, mutate) in cases {
        let fixture = fixture_with_mutation(label, mutate);
        let output = fixture.run(
            &request(),
            &good_client_request_hash,
            &good_protocol_schema_hash,
            4000,
        );
        assert_failure(&output);
        let records = fixture.capture_records();
        let listen_stage = records
            .iter()
            .filter(|record| record["mode"] == "listen-stage")
            .collect::<Vec<_>>();
        match label {
            "init-codex-home-mismatch" => {
                let modes = records
                    .iter()
                    .map(|record| record["mode"].as_str().unwrap())
                    .collect::<Vec<_>>();
                assert!(
                    matches!(
                        modes.as_slice(),
                        [
                            "version-stage",
                            "version-stage",
                            "version",
                            "help-stage",
                            "help-stage",
                            "help",
                            "schema-stage",
                            "schema-stage",
                            "schema",
                            "listen-stage",
                            "listen-stage",
                        ] | [
                            "version-stage",
                            "version-stage",
                            "version",
                            "help-stage",
                            "help-stage",
                            "help",
                            "schema-stage",
                            "schema-stage",
                            "schema",
                            "listen-stage",
                            "listen-stage",
                            "listen-stage",
                        ]
                    ),
                    "{records:?}"
                );
                assert_eq!(
                    listen_stage.len(),
                    2 + usize::from(modes.len() == 12),
                    "{listen_stage:?}"
                );
                assert_eq!(listen_stage[0]["stage"], "listen");
                assert_eq!(listen_stage[0]["phase"], "entered");
                assert_eq!(listen_stage[1]["stage"], "initialize");
                assert_eq!(listen_stage[1]["phase"], "received");
                if listen_stage.len() == 3 {
                    assert_eq!(listen_stage[2]["stage"], "initialize");
                    assert_eq!(listen_stage[2]["phase"], "emitted");
                }
            }
            "init-user-agent-missing" => {
                let modes = records
                    .iter()
                    .map(|record| record["mode"].as_str().unwrap())
                    .collect::<Vec<_>>();
                assert!(
                    matches!(
                        modes.as_slice(),
                        [
                            "version-stage",
                            "version-stage",
                            "version",
                            "help-stage",
                            "help-stage",
                            "help",
                            "schema-stage",
                            "schema-stage",
                            "schema",
                            "listen-stage",
                            "listen-stage",
                        ] | [
                            "version-stage",
                            "version-stage",
                            "version",
                            "help-stage",
                            "help-stage",
                            "help",
                            "schema-stage",
                            "schema-stage",
                            "schema",
                            "listen-stage",
                            "listen-stage",
                            "listen-stage",
                        ]
                    ),
                    "{records:?}"
                );
                assert_eq!(
                    listen_stage.len(),
                    2 + usize::from(modes.len() == 12),
                    "{listen_stage:?}"
                );
                assert_eq!(listen_stage[0]["stage"], "listen");
                assert_eq!(listen_stage[0]["phase"], "entered");
                assert_eq!(listen_stage[1]["stage"], "initialize");
                assert_eq!(listen_stage[1]["phase"], "received");
                if listen_stage.len() == 3 {
                    assert_eq!(listen_stage[2]["stage"], "initialize");
                    assert_eq!(listen_stage[2]["phase"], "emitted");
                }
            }
            _ => unreachable!("unexpected case label {label}"),
        }
        assert!(!records.iter().any(|record| record["mode"] == "listen"));
    }
}

#[test]
fn compiled_process_stream_and_cleanup_failures_are_bounded() {
    let _test_guard = process_test_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let happy = scenario_for_cwd(
        "/tmp/cwd",
        "/private/tmp/kanban-codex-home",
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let good_client_request_hash = schema_hash(&happy.schema_client_request);
    let good_protocol_schema_hash = schema_hash(&happy.schema_protocol);

    let cases: Vec<TimedCase> = vec![
        (
            "slow-probe",
            |scenario| {
                scenario.version.stdout = Emit::Sleep(TIMED_FAILURE_SLEEP_MS);
            },
            TIMED_FAILURE_TIMEOUT_MS,
            true,
        ),
        (
            "oversized-stdout-line",
            |scenario| {
                scenario.listen.response3 = Emit::Repeat {
                    byte: b'x',
                    count: 65_537,
                };
            },
            4000,
            false,
        ),
        (
            "oversized-stdout-total",
            |scenario| {
                scenario.listen.posts = vec![
                    Emit::Repeat {
                        byte: b'x',
                        count: 40_000,
                    },
                    Emit::Repeat {
                        byte: b'y',
                        count: 30_000,
                    },
                ];
            },
            4000,
            false,
        ),
        (
            "app-stderr",
            |scenario| {
                scenario.listen.stderr = Emit::plain("app-server stderr\n");
            },
            4000,
            false,
        ),
        (
            "early-eof-nonzero",
            |scenario| {
                scenario.listen.exit_after_stage = 2;
                scenario.listen.exit_code = 7;
                scenario.listen.wait_for_eof = false;
            },
            4000,
            false,
        ),
        (
            "slow-interactive",
            |scenario| {
                scenario.listen.response3 = Emit::Sleep(TIMED_FAILURE_SLEEP_MS);
            },
            TIMED_FAILURE_TIMEOUT_MS,
            true,
        ),
        (
            "post-completion",
            |scenario| {
                scenario.listen.posts.push(Emit::plain(
                    "{\"method\":\"post-completion/changed\",\"params\":{}}",
                ));
            },
            4000,
            false,
        ),
    ];

    for (label, mutate, timeout_ms, measure_elapsed) in cases {
        let fixture = fixture_with_mutation(label, mutate);
        let start = Instant::now();
        let output = fixture.run(
            &request(),
            &good_client_request_hash,
            &good_protocol_schema_hash,
            timeout_ms,
        );
        eprintln!(
            "timed_failure_case={label} exit={:?} stdout={:?} stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let elapsed = start.elapsed();
        assert_failure(&output);
        if measure_elapsed {
            assert!(elapsed >= Duration::from_millis(timeout_ms));
            assert!(
                elapsed < Duration::from_millis(timeout_ms + 5_000),
                "elapsed={elapsed:?}"
            );
        }
        let records = fixture.capture_records();
        let modes = records
            .iter()
            .map(|record| record["mode"].as_str().unwrap())
            .collect::<Vec<_>>();
        match label {
            "slow-probe" => {
                assert_eq!(modes, vec!["version-stage"], "{records:?}");
            }
            "slow-interactive" => {
                assert_eq!(
                    modes,
                    vec![
                        "version-stage",
                        "version-stage",
                        "version",
                        "help-stage",
                        "help-stage",
                        "help",
                        "schema-stage",
                        "schema-stage",
                        "schema",
                        "listen-stage",
                        "listen-stage",
                        "listen-stage",
                        "listen-stage",
                        "listen-stage",
                        "listen-stage",
                        "listen-stage",
                    ],
                    "{records:?}"
                );
            }
            _ => {
                assert_eq!(
                    &modes[..9],
                    &[
                        "version-stage",
                        "version-stage",
                        "version",
                        "help-stage",
                        "help-stage",
                        "help",
                        "schema-stage",
                        "schema-stage",
                        "schema",
                    ],
                    "{records:?}"
                );
                assert!(modes.contains(&"listen-stage"), "{records:?}");
            }
        }
    }
}

#[test]
fn compiled_process_blocked_protocol_write_uses_the_shared_deadline_and_reaps_the_group() {
    let _test_guard = process_test_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new("blocked-write", |paths| {
        let cwd = paths.cwd.canonicalize().unwrap();
        let codex_home = paths.codex_home.canonicalize().unwrap();
        let root = paths.cwd.parent().unwrap();
        let mut scenario = scenario_for_cwd(
            &cwd.to_string_lossy(),
            &codex_home.to_string_lossy(),
            &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
        );
        scenario.listen.stubborn_after_stage = 2;
        scenario.listen.pid_file = Some(root.join("stubborn.pid"));
        scenario.listen.grandchild_pid_file = Some(root.join("grandchild.pid"));
        scenario
    });
    let scenario = scenario_for_cwd(
        &fixture.cwd.canonicalize().unwrap().to_string_lossy(),
        &fixture.codex_home.canonicalize().unwrap().to_string_lossy(),
        &format!("{SUBSCRIPTION_ID}:{EVENT_ID}"),
    );
    let mut large_request = request();
    large_request["event"]["padding"] = Value::String("x".repeat(58_000));
    let start = Instant::now();
    let output = fixture.run(
        &large_request,
        &schema_hash(&scenario.schema_client_request),
        &schema_hash(&scenario.schema_protocol),
        1_500,
    );
    let elapsed = start.elapsed();
    eprintln!("blocked_write_elapsed_ms={}", elapsed.as_millis());
    assert_failure(&output);
    assert!(
        elapsed >= Duration::from_millis(1_300),
        "elapsed={elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(4), "elapsed={elapsed:?}");

    let child_pid: i32 = fs::read_to_string(fixture.root.join("stubborn.pid"))
        .expect("fake must reach the thread-start stage")
        .parse()
        .unwrap();
    let grandchild_pid: i32 = fs::read_to_string(fixture.root.join("grandchild.pid"))
        .expect("fake must spawn its stubborn grandchild")
        .parse()
        .unwrap();
    assert_pid_disappears(child_pid, "app-server");
    assert_pid_disappears(grandchild_pid, "app-server grandchild");
    let records = fixture.capture_records();
    assert!(records.iter().any(|record| {
        record["mode"] == "listen-stage" && record["stage"] == "2" && record["phase"] == "stubborn"
    }));
}
