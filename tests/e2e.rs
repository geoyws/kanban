use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use headless_chrome::{Browser, LaunchOptionsBuilder};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    main: PathBuf,
    worktree: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kanban-rust-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let main = root.join("main");
        let worktree = root.join("worktree");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        Self {
            root,
            data,
            main,
            worktree,
        }
    }

    fn command(&self, cwd: &Path) -> Command {
        self.command_with_data_dir(cwd, &self.data)
    }

    fn command_with_data_dir(&self, cwd: &Path, data_dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(cwd)
            .env("KANBAN_DATA_DIR", data_dir)
            .env_remove("KANBAN_DB")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd).args(args).output().unwrap()
    }

    fn ok_json(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "command failed: {:?}\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn ndjson_values(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn decode_watch_cursor(cursor: &str) -> Value {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn board_path_for_project(fixture: &Fixture, cwd: &Path, project_name: &str) -> PathBuf {
    fixture
        .ok_json(cwd, &["workspace", "list", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"].as_str() == Some(project_name))
        .unwrap_or_else(|| panic!("missing project named {project_name}"))["boardPath"]
        .as_str()
        .unwrap()
        .into()
}

fn insert_raw_board_event(
    board_path: &Path,
    task_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: Value,
) -> i64 {
    let connection = Connection::open(board_path).unwrap();
    let seq = connection
        .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    connection
        .execute(
            "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) \
             VALUES(?,?,?,?,?,?,0,?,?)",
            params![
                seq,
                task_id,
                kind,
                actor,
                payload.to_string(),
                seq,
                format!("prev-{seq}"),
                format!("hash-{seq}")
            ],
        )
        .unwrap();
    seq
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn seed_legacy_rootless_duplicate(fixture: &Fixture, slug: &str, name: &str) {
    let board_path = fixture.data.join("boards").join(format!("{slug}.db"));
    fs::create_dir_all(board_path.parent().unwrap()).unwrap();
    let _board = Connection::open(&board_path).unwrap();
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute(
            "INSERT INTO boards(board_path,name,created_at,last_used_at) VALUES(?,?,?,?)",
            params![
                board_path.to_string_lossy().into_owned(),
                name,
                2_i64,
                2_i64
            ],
        )
        .unwrap();
}

struct ServerGuard {
    child: Option<std::process::Child>,
    port: u16,
}

impl ServerGuard {
    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct WatchSession {
    child: Option<std::process::Child>,
    stdout_rx: mpsc::Receiver<String>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl WatchSession {
    fn start(fixture: &Fixture, cwd: &Path, data_dir: &Path, args: &[&str]) -> Self {
        let mut child = fixture
            .command_with_data_dir(cwd, data_dir)
            .args(["watch"])
            .args(args)
            .stdin(Stdio::null())
            .env_remove("KANBAN_PROJECT")
            .spawn()
            .unwrap_or_else(|error| panic!("spawn kanban watch: {error}"));
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (stdout_tx, stdout_rx) = mpsc::channel();
        let stdout_thread = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if stdout_tx.send(line).is_err() {
                    return;
                }
            }
        });
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_sink = Arc::clone(&stderr_lines);
        let stderr_thread = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                stderr_sink.lock().unwrap().push(line);
            }
        });
        Self {
            child: Some(child),
            stdout_rx,
            stdout_thread: Some(stdout_thread),
            stderr_lines,
            stderr_thread: Some(stderr_thread),
        }
    }

    fn next_stdout_line(&self, timeout: Duration) -> String {
        self.stdout_rx
            .recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("watch stdout stalled: {error}"))
    }

    fn next_stdout_json(&self, timeout: Duration) -> Value {
        serde_json::from_str(&self.next_stdout_line(timeout)).unwrap()
    }

    #[allow(dead_code)]
    fn try_next_stdout_json(&self, timeout: Duration) -> Option<Value> {
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(line) => Some(serde_json::from_str(&line).unwrap()),
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                None
            }
        }
    }

    fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr_lines.lock().unwrap().clone()
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.shutdown();
        self.stderr_snapshot()
    }
}

impl Drop for WatchSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn chrome_binary() -> PathBuf {
    let explicit = env::var_os("KANBAN_CHROME").map(PathBuf::from);
    let candidates = chrome_binary_candidates(
        explicit.as_deref().and_then(Path::to_str),
        env::var_os("PATH")
            .as_deref()
            .and_then(std::ffi::OsStr::to_str),
        cfg!(target_os = "macos"),
    );
    if let Some(path) = chrome_binary_from_candidates(candidates.clone(), Path::exists) {
        return path;
    }
    panic!(
        "could not find Chrome; tried: {}",
        candidates
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn chrome_binary_candidates(
    explicit: Option<&str>,
    path_env: Option<&str>,
    is_macos: bool,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(PathBuf::from(path));
    }
    if is_macos {
        candidates.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
    }
    if let Some(path_env) = path_env {
        for dir in std::env::split_paths(path_env) {
            for name in [
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
            ] {
                candidates.push(dir.join(name));
            }
        }
    }
    for path in [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/local/bin/google-chrome",
        "/opt/google/chrome/google-chrome",
        "/snap/bin/chromium",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ] {
        candidates.push(PathBuf::from(path));
    }
    candidates
}

fn chrome_binary_from_candidates<F>(candidates: Vec<PathBuf>, exists: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    candidates.into_iter().find(|path| exists(path))
}

#[test]
fn chrome_discovery_prefers_explicit_then_platform_then_path_then_defaults() {
    let candidates = chrome_binary_candidates(Some("/explicit/chrome"), Some("/a:/b"), true);
    assert_eq!(candidates[0], PathBuf::from("/explicit/chrome"));
    assert_eq!(
        candidates[1],
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    );
    assert_eq!(candidates[2], PathBuf::from("/a/google-chrome"));
    assert_eq!(candidates[3], PathBuf::from("/a/google-chrome-stable"));
    assert_eq!(candidates[4], PathBuf::from("/a/chromium"));
    assert_eq!(candidates[5], PathBuf::from("/a/chromium-browser"));
    assert_eq!(candidates[6], PathBuf::from("/b/google-chrome"));
    assert_eq!(candidates[10], PathBuf::from("/usr/bin/google-chrome"));
    let picked = chrome_binary_from_candidates(
        vec![
            PathBuf::from("/missing"),
            PathBuf::from("/still-missing"),
            PathBuf::from("/found"),
        ],
        |path| path == Path::new("/found"),
    )
    .unwrap();
    assert_eq!(picked, PathBuf::from("/found"));
}

fn launch_browser(chrome_path: PathBuf) -> Browser {
    let options = LaunchOptionsBuilder::default()
        .path(Some(chrome_path))
        .headless(true)
        .build()
        .expect("build Chrome launch options");
    Browser::new(options).expect("launch Chrome")
}

fn browser_loopback_reservation_supported() -> Result<(), String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .map(drop)
        .map_err(|error| format!("reserve loopback port for browser tests: {error}"))
}

fn spawn_server(fixture: &Fixture) -> ServerGuard {
    let mut failures = Vec::new();
    for attempt in 0..16_u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap_or_else(|error| {
            panic!("reserve loopback port for browser attempt {attempt}: {error}")
        });
        let port = listener
            .local_addr()
            .unwrap_or_else(|error| {
                panic!("read reserved loopback port for browser attempt {attempt}: {error}")
            })
            .port();
        drop(listener);
        let mut child = fixture
            .command(&fixture.main)
            .args(["serve", "--port", &port.to_string()])
            .spawn()
            .unwrap_or_else(|error| panic!("spawn kanban serve on {port}: {error}"));
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stderr = String::new();
        loop {
            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|error| panic!("wait on kanban serve candidate {port}: {error}"))
            {
                if let Some(mut reader) = child.stderr.take() {
                    let _ = reader.read_to_string(&mut stderr);
                }
                failures.push(format!(
                    "port {port} exited before readiness on attempt {attempt}: {status}: {stderr}"
                ));
                break;
            }
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                if let Some(status) = child.try_wait().unwrap_or_else(|error| {
                    panic!("post-connect wait on kanban serve candidate {port}: {error}")
                }) {
                    if let Some(mut reader) = child.stderr.take() {
                        let _ = reader.read_to_string(&mut stderr);
                    }
                    failures.push(format!(
                        "port {port} connected but exited before usable readiness on attempt {attempt}: {status}: {stderr}"
                    ));
                    break;
                }
                return ServerGuard {
                    child: Some(child),
                    port,
                };
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(mut reader) = child.stderr.take() {
                    let _ = reader.read_to_string(&mut stderr);
                }
                failures.push(format!(
                    "port {port} timed out on attempt {attempt}: {stderr}"
                ));
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    panic!(
        "kanban serve never bound after {} attempts:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

/// Return a current fixture to the exact pre-search schema shape before a
/// historical migration test removes or renames tables referenced by V13.
fn remove_v13_search_schema(connection: &Connection) {
    let trigger_names = {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type='trigger' AND name LIKE 'search_%'")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    for trigger in trigger_names {
        connection
            .execute_batch(&format!("DROP TRIGGER \"{trigger}\""))
            .unwrap();
    }
    connection
        .execute_batch(
            "DROP VIEW search_source_rows;\
             DROP TABLE search_fts;\
             DROP TABLE search_documents;",
        )
        .unwrap();
}

fn remove_v18_board_audit_schema(connection: &Connection) {
    connection
        .execute_batch(
            "DELETE FROM board_meta WHERE key LIKE 'audit_chain_%';\
             ALTER TABLE events DROP COLUMN event_hash;\
             ALTER TABLE events DROP COLUMN prev_hash;",
        )
        .unwrap();
}

#[test]
fn compiled_binary_persists_across_processes_and_rotates_handoff_lease() {
    let fixture = Fixture::new("handoff");
    fixture.ok_json(&fixture.main, &["init", "--name", "E2E", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "First handoff rule.",
            "--as",
            "geo",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Second handoff rule.",
            "--as",
            "geo",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.worktree,
        &[
            "workspace",
            "attach",
            "--to",
            fixture.main.to_str().unwrap(),
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Cross-process handoff",
            "--id",
            "t-e2e",
            "--driver-only",
            "--json",
        ],
    );
    let outgoing = fixture.ok_json(
        &fixture.worktree,
        &[
            "claim",
            "t-e2e",
            "--as",
            "outgoing",
            "--session",
            "old-session",
            "--caller-scope",
            "driver",
            "--json",
        ],
    );
    let outgoing_token = outgoing["leaseToken"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-e2e",
            "Process one wrote this",
            "--as",
            "outgoing",
            "--kind",
            "progress",
            "--json",
        ],
    );
    let handoff = fixture.ok_json(
        &fixture.worktree,
        &[
            "handoff",
            "create",
            "t-e2e",
            "--lease",
            &outgoing_token,
            "--as",
            "outgoing",
            "--summary",
            "Rust E2E persisted the work",
            "--intent",
            "Continue from another process",
            "--next-action",
            "Accept and checkpoint",
            "--reason",
            "token_pressure",
            "--json",
        ],
    );
    let handoff_id = handoff["id"].as_str().unwrap();

    let stale = fixture.run(
        &fixture.main,
        &["heartbeat", "t-e2e", "--lease", &outgoing_token, "--json"],
    );
    assert!(!stale.status.success(), "stale outgoing lease was accepted");

    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            handoff_id,
            "--as",
            "incoming",
            "--session",
            "new-session",
            "--caller-scope",
            "driver",
            "--json",
        ],
    );
    let incoming_token = accepted["claim"]["leaseToken"].as_str().unwrap();
    assert_ne!(incoming_token, outgoing_token);
    assert_eq!(accepted["rules"][0]["tags"], json!(["ALL"]));
    assert_eq!(accepted["rules"][1]["tags"], json!(["ALL"]));

    let shown = fixture.run(&fixture.worktree, &["task", "show", "t-e2e", "--json"]);
    assert!(shown.status.success());
    let shown_text = String::from_utf8(shown.stdout).unwrap();
    assert!(shown_text.contains("Process one wrote this"));
    assert!(!shown_text.contains(incoming_token));
    assert!(!shown_text.contains("leaseToken"));

    let context = fixture.run(&fixture.main, &["context", "t-e2e"]);
    let context_text = String::from_utf8(context.stdout).unwrap();
    assert!(context_text.contains("Rust E2E persisted the work"));
    assert!(context_text.contains("Next action: Accept and checkpoint"));
    assert!(!context_text.contains(incoming_token));

    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-e2e",
            "--lease",
            incoming_token,
            "--as",
            "incoming",
            "--summary",
            "Fresh process resumed",
            "--intent",
            "Finish safely",
            "--next-action",
            "Close the task",
            "--state",
            "done",
            "--validation",
            "compiled Rust process boundary",
            "--json",
        ],
    );
    let dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert!(
        !dashboard[0]
            .as_object()
            .unwrap()
            .contains_key("canonicalRoot")
    );
    assert!(!dashboard[0].as_object().unwrap().contains_key("canonical"));
    assert_eq!(dashboard[0]["workspaceRoots"].as_array().unwrap().len(), 2);
    assert_eq!(dashboard[0]["taskCounts"]["done"], 1);
    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(doctor["healthy"], true);
    assert_eq!(doctor["registrySchemaVersion"], 11);
    assert_eq!(doctor["supportedRegistrySchemaVersion"], 11);
    assert_eq!(doctor["supportedBoardSchemaVersion"], 20);
    assert_eq!(doctor["projects"][0]["schemaVersion"], 20);
    assert_eq!(doctor["projects"][0]["supportedSchemaVersion"], 20);
    assert_eq!(
        doctor["projects"][0]["workspaceRoots"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        !doctor["projects"][0]
            .as_object()
            .unwrap()
            .contains_key("canonicalRoot")
    );
    assert!(
        !doctor["projects"][0]
            .as_object()
            .unwrap()
            .contains_key("canonical")
    );
    assert_eq!(doctor["projects"][0]["rootless"], false);
}

#[test]
fn init_requires_an_explicit_board_name() {
    let fixture = Fixture::new("init-name-required");
    let failed = fixture.run(&fixture.main, &["init", "--json"]);
    assert!(
        !failed.status.success(),
        "init without --name unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("--name"),
        "stderr did not name the missing explicit board-name flag: {stderr}"
    );
    assert!(
        fixture
            .ok_json(&fixture.main, &["workspace", "list", "--json"])
            .as_array()
            .unwrap()
            .is_empty(),
        "a failed init without --name must not register a board"
    );
}

#[test]
fn blocked_and_terminal_task_handoffs_can_be_acknowledged_without_a_claim() {
    let fixture = Fixture::new("settled-handoff");
    fixture.ok_json(&fixture.main, &["init", "--name", "E2E", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "add",
            "handoff",
            "--description",
            "handoff lifecycle",
            "--as",
            "geo",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Handoff-tagged task rule.",
            "--as",
            "geo",
            "--tag",
            "handoff",
            "--json",
        ],
    );

    for status in ["blocked", "done", "cancelled"] {
        let task_id = format!("t-{status}");
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                &format!("{status} handoff"),
                "--id",
                &task_id,
                "--tag",
                "handoff",
                "--json",
            ],
        );
        let claim = fixture.ok_json(
            &fixture.main,
            &["claim", &task_id, "--as", "outgoing", "--json"],
        );
        let handoff = fixture.ok_json(
            &fixture.main,
            &[
                "handoff",
                "create",
                &task_id,
                "--lease",
                claim["leaseToken"].as_str().unwrap(),
                "--as",
                "outgoing",
                "--summary",
                "The old brief was preserved",
                "--intent",
                "Let a successor absorb it",
                "--next-action",
                "Acknowledge without reopening work",
                "--json",
            ],
        );
        fixture.ok_json(
            &fixture.main,
            &[
                "task", "move", &task_id, status, "--as", "operator", "--json",
            ],
        );

        let accepted = fixture.ok_json(
            &fixture.main,
            &[
                "handoff",
                "accept",
                handoff["id"].as_str().unwrap(),
                "--as",
                "incoming",
                "--json",
            ],
        );
        assert_eq!(accepted["handoff"]["status"], "accepted");
        assert!(accepted["claim"].is_null(), "{status} minted a lease");
        assert!(
            accepted["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tag| tag == "handoff")),
            "task-scoped rules were lost when no claim was minted"
        );
        assert_eq!(
            fixture.ok_json(&fixture.main, &["task", "show", &task_id, "--json"])["status"],
            status,
            "acknowledgement reopened {status} work"
        );
    }

    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["handoff", "list", "--status", "pending", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "acknowledged handoffs stayed in the pending resume queue"
    );
}

#[test]
fn compiled_binary_searches_hybrid_knowledge_across_cli_and_boards() {
    let fixture = Fixture::new("rag-search");
    fixture.ok_json(&fixture.main, &["init", "--name", "SEARCH-A", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "release", "--as", "tester", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "ops", "--as", "tester", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Production rollout checklist",
            "--id",
            "t-release",
            "--body",
            "Install the optimized binary and restart the live service safely.",
            "--tag",
            "release",
            "--tag",
            "ops",
            "--as",
            "tester",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-release",
            "Keep a rollback receipt and verify the public route.",
            "--as",
            "tester",
            "--kind",
            "evidence",
            "--json",
        ],
    );

    let exact = fixture.ok_json(
        &fixture.main,
        &["search", "t-release", "--limit", "3", "--json"],
    );
    assert_eq!(exact["results"][0]["sourceId"], "t-release");
    assert_eq!(
        exact["results"][0]["citation"],
        "kanban://SEARCH-A/task/t-release"
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-release",
            "The Production rollout phrase is repeated in this supporting receipt.",
            "--as",
            "tester",
            "--kind",
            "evidence",
            "--json",
        ],
    );
    let title_phrase = fixture.ok_json(
        &fixture.main,
        &["search", "Production rollout", "--limit", "3", "--json"],
    );
    assert_eq!(title_phrase["results"][0]["sourceKind"], "task");
    assert_eq!(title_phrase["results"][0]["sourceId"], "t-release");

    let paraphrase = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "deploy the live build",
            "--tag",
            "release",
            "--tag",
            "ops",
            "--max-chars",
            "1000",
            "--json",
        ],
    );
    assert_eq!(paraphrase["results"][0]["sourceId"], "t-release");
    assert!(paraphrase["resultChars"].as_u64().unwrap() <= 1000);
    assert_eq!(paraphrase["embeddingModel"], "kanban-semantic-lite-v1");

    let rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Canary feedback tier rule.",
            "--tag",
            "release",
            "--as",
            "tester",
            "--json",
        ],
    );
    let rule_search = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "Canary feedback tier",
            "--source",
            "rule",
            "--tag",
            "release",
            "--json",
        ],
    );
    assert_eq!(rule_search["results"][0]["sourceKind"], "rule");
    assert_eq!(rule_search["results"][0]["sourceId"], rule["id"]);
    assert_eq!(
        rule_search["results"][0]["citation"],
        format!("kanban://rules/rule/{}", rule["id"].as_str().unwrap())
    );
    assert_eq!(rule_search["results"][0]["tags"], json!(["ALL", "release"]));

    let rebuilt = fixture.ok_json(
        &fixture.main,
        &["search-rebuild", "--as", "tester", "--json"],
    );
    assert_eq!(rebuilt["documents"], rebuilt["embedded"]);
    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(doctor["projects"][0]["searchIndex"]["healthy"], true);
    assert_eq!(doctor["projects"][0]["searchIndex"]["missingEmbeddings"], 0);

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-release",
            "--body",
            "Deploy the release with a reversible restart and a public route receipt.",
            "--as",
            "tester",
            "--json",
        ],
    );
    let after_mutation = fixture.ok_json(
        &fixture.main,
        &["search", "reversible deployment", "--json"],
    );
    assert_eq!(after_mutation["results"][0]["sourceId"], "t-release");
    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(doctor["projects"][0]["searchIndex"]["healthy"], true);
    assert!(
        doctor["projects"][0]["searchIndex"]["missingEmbeddings"]
            .as_i64()
            .unwrap()
            > 0,
        "a source mutation did not invalidate its cached vector"
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Retired alias decision",
            "--id",
            "t-cold-search",
            "--body",
            "Cold history contains the retired alias decision.",
            "--as",
            "tester",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-cold-search",
            "done",
            "--as",
            "tester",
            "--json",
        ],
    );
    let board_path = fixture
        .ok_json(&fixture.main, &["workspace", "list", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["name"] == json!("WATCH-REPLAY"))
        .and_then(|record| record["boardPath"].as_str())
        .unwrap()
        .to_owned();
    Connection::open(board_path)
        .unwrap()
        .execute(
            "UPDATE tasks SET completed_at=1,updated_at=1 WHERE id='t-cold-search'",
            [],
        )
        .unwrap();
    fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "tester",
            "--json",
        ],
    );
    let active = fixture.ok_json(
        &fixture.main,
        &["search", "retired alias decision", "--json"],
    );
    assert!(
        active["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|result| result["sourceId"] != "t-cold-search")
    );
    let cold = fixture.ok_json(
        &fixture.main,
        &["search", "retired alias decision", "--all", "--json"],
    );
    assert!(
        cold["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["sourceId"] == "t-cold-search" && result["archived"] == true)
    );

    let second = fixture.root.join("second-search-board");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&second, &["init", "--name", "SEARCH-B", "--json"]);
    fixture.ok_json(
        &second,
        &[
            "task",
            "add",
            "Authentication recovery",
            "--id",
            "t-auth",
            "--body",
            "Restore the login session without storing credentials.",
            "--as",
            "tester",
            "--json",
        ],
    );
    let isolated = fixture.ok_json(
        &fixture.main,
        &["search", "credential session login", "--json"],
    );
    assert!(
        isolated["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|result| result["sourceId"] != "t-auth")
    );
    let across = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "credential session login",
            "--all-boards",
            "--json",
        ],
    );
    assert!(
        across["boards"]
            .as_array()
            .unwrap()
            .iter()
            .any(|board| board == "SEARCH-B")
    );
    assert!(
        across["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["sourceId"] == "t-auth")
    );
}

#[test]
fn compiled_binary_keeps_linked_deployment_search_documents_after_task_mutations() {
    let fixture = Fixture::new("deployment-search-refresh");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SEARCH-DEPLOY", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "release", "--as", "tester", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Deploy indexed release",
            "--id",
            "t-deploy-search",
            "--as",
            "tester",
            "--json",
        ],
    );
    let deployment = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/kanban",
            "--commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--tier",
            "@_bs",
            "--environment",
            "driver-feedback",
            "--host",
            "hax",
            "--url",
            "https://kb.geoy.ws",
            "--task",
            "t-deploy-search",
            "--as",
            "tester",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            deployment["id"].as_str().unwrap(),
            "--token",
            deployment["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--receipt",
            "served linked deployment",
            "--served-commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--as",
            "tester",
            "--json",
        ],
    );

    let assert_healthy = || {
        let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
        let search = &doctor["projects"][0]["searchIndex"];
        assert_eq!(search["healthy"], true, "{doctor}");
        assert_eq!(search["sourceRows"], search["documents"], "{doctor}");
        assert_eq!(search["documents"], search["ftsRows"], "{doctor}");
    };
    assert_healthy();

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-deploy-search",
            "--body",
            "The linked deployment must remain searchable after this refresh.",
            "--as",
            "tester",
            "--json",
        ],
    );
    assert_healthy();

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-deploy-search",
            "--tag",
            "release",
            "--as",
            "tester",
            "--json",
        ],
    );
    assert_healthy();

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-deploy-search",
            "done",
            "--as",
            "tester",
            "--json",
        ],
    );
    assert_healthy();
}

#[test]
fn the_v13_search_migration_preserves_v12_knowledge() {
    let fixture = Fixture::new("search-migration");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SEARCH-MIGRATION", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Knowledge present before V13",
            "--id",
            "t-before-search",
            "--body",
            "A durable handoff survives the search schema migration.",
            "--as",
            "tester",
            "--json",
        ],
    );
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let connection = Connection::open(&board).unwrap();
    remove_v18_board_audit_schema(&connection);
    remove_v13_search_schema(&connection);
    connection
        .execute_batch(
            r#"
            DROP INDEX idx_attention_status_priority;
            DROP INDEX idx_handoffs_status_priority;
            ALTER TABLE attention DROP COLUMN priority;
            ALTER TABLE handoffs DROP COLUMN priority;
            ALTER TABLE rules DROP COLUMN task_tags;
            DROP TABLE attention_tags;
            PRAGMA user_version=12;
            "#,
        )
        .unwrap();
    drop(connection);

    let found = fixture.ok_json(&fixture.main, &["search", "durable handoff", "--json"]);
    assert!(
        found["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["sourceId"] == "t-before-search")
    );
    let reopened = Connection::open(board).unwrap();
    assert_eq!(
        reopened
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        20
    );
    assert_eq!(
        reopened
            .query_row(
                "SELECT count(*) FROM tasks WHERE id='t-before-search'",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        1
    );
}

#[test]
fn semantic_lite_retrieves_the_checked_in_paraphrase_corpus() {
    let fixture = Fixture::new("search-evaluation");
    fixture.ok_json(&fixture.main, &["init", "--name", "SEARCH-EVAL", "--json"]);
    for (id, title, body) in [
        (
            "t-search-handoff",
            "Token-pressure continuation",
            "A successor resumes from the durable handoff after agent context exhaustion.",
        ),
        (
            "t-search-context",
            "Context budget",
            "The bounded context packet preserves the newest evidence.",
        ),
        (
            "t-search-release",
            "Release installation",
            "Deploy and publish the optimized binary, then restart the live website.",
        ),
        (
            "t-search-archive",
            "Settled-history retention",
            "Archive and prune old completed tasks from the hot working set into cold history.",
        ),
        (
            "t-search-auth",
            "Public-board edge authentication",
            "SSO login policy determines who may sign in to the public board.",
        ),
        (
            "t-search-stale",
            "Stale lease detection",
            "Find overdue tasks when an owner stops heartbeat check-ins.",
        ),
    ] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task", "add", title, "--id", id, "--body", body, "--as", "tester", "--json",
            ],
        );
    }
    let exact = fixture.ok_json(
        &fixture.main,
        &["search", "bounded context packet", "--limit", "5", "--json"],
    );
    assert_eq!(exact["results"][0]["sourceId"], "t-search-context");
    for (query, expected) in [
        (
            "continue work after an agent runs out of context",
            "t-search-handoff",
        ),
        (
            "publish the new binary and restart the website",
            "t-search-release",
        ),
        (
            "keep old completed items out of the hot working set",
            "t-search-archive",
        ),
        (
            "who is allowed to sign in to the public board",
            "t-search-auth",
        ),
        (
            "find overdue work whose owner stopped checking in",
            "t-search-stale",
        ),
    ] {
        let found = fixture.ok_json(&fixture.main, &["search", query, "--limit", "5", "--json"]);
        let rank = found["results"]
            .as_array()
            .unwrap()
            .iter()
            .position(|result| result["sourceId"] == expected);
        assert!(
            rank.is_some(),
            "{expected} was not top-five for {query:?}: {}",
            found["results"]
        );
    }
}

#[test]
fn compiled_binary_allows_exactly_one_concurrent_claimer() {
    let fixture = Fixture::new("atomic-claim");
    fixture.ok_json(&fixture.main, &["init", "--name", "Atomic", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Claim once", "--id", "t-race", "--json"],
    );

    let mut first = fixture.command(&fixture.main);
    first.args(["claim", "t-race", "--as", "agent-a", "--json"]);
    let mut second = fixture.command(&fixture.main);
    second.args(["claim", "t-race", "--as", "agent-b", "--json"]);
    let first = first.spawn().unwrap();
    let second = second.spawn().unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert_eq!(
        usize::from(first.status.success()) + usize::from(second.status.success()),
        1,
        "exactly one separate process must win the SQLite immediate transaction"
    );
}

#[test]
fn compiled_binary_rejects_two_simultaneous_init_attempts_with_the_same_name() {
    let fixture = Fixture::new("atomic-init");
    let first_cwd = fixture.root.join("same-first");
    let second_cwd = fixture.root.join("same-second");
    fs::create_dir_all(&first_cwd).unwrap();
    fs::create_dir_all(&second_cwd).unwrap();

    let outputs = std::thread::scope(|scope| {
        let start = Arc::new(Barrier::new(3));
        let first_start = Arc::clone(&start);
        let fixture = &fixture;
        let first_cwd = &first_cwd;
        let second_cwd = &second_cwd;
        let first = scope.spawn(move || {
            first_start.wait();
            fixture.run(first_cwd, &["init", "--name", "SAME", "--json"])
        });
        let second_start = Arc::clone(&start);
        let second = scope.spawn(move || {
            second_start.wait();
            fixture.run(second_cwd, &["init", "--name", "SAME", "--json"])
        });
        start.wait();
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1,
        "exactly one separate process must win the SQLite immediate transaction"
    );
    let refused = outputs
        .iter()
        .find(|output| !output.status.success())
        .expect("one init attempt must refuse");
    let refused_message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        refused_message.contains("a Kanban board is already named SAME"),
        "{refused_message}"
    );

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    let active_count = registry
        .query_row("SELECT count(*) FROM boards WHERE name='SAME'", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(
        active_count, 1,
        "simultaneous init attempts must leave exactly one active board named SAME"
    );
}

#[test]
fn compiled_binary_enforces_pull_routing_task_graph_and_story_gates() {
    let fixture = Fixture::new("workflow");
    let registered = fixture.ok_json(&fixture.main, &["init", "--name", "Workflow", "--json"]);
    let board = Path::new(registered["boardPath"].as_str().unwrap());
    assert_eq!(
        fs::metadata(&fixture.data).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(board).unwrap().permissions().mode() & 0o777,
        0o600
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Epic", "--id", "e-one", "--type", "epic", "--status", "todo", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "metadata",
            "e-one",
            "--as",
            "operator",
            "--patch-json",
            r#"{"workflowStatus":"ready","dropMe":true}"#,
            "--json",
        ],
    );
    let epic = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "metadata",
            "e-one",
            "--as",
            "operator",
            "--patch-json",
            r#"{"dropMe":null}"#,
            "--json",
        ],
    );
    assert!(epic["metadata"].get("dropMe").is_none());
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Story", "--id", "s-one", "--type", "story", "--parent", "e-one",
            "--status", "backlog", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "metadata",
            "s-one",
            "--as",
            "operator",
            "--patch-json",
            r#"{"workflowStatus":"planning","mergeMode":"feature-branch"}"#,
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Develop", "--id", "t-dev", "--parent", "s-one", "--lane", "be",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Test", "--id", "t-test", "--parent", "s-one", "--lane", "test",
            "--json",
        ],
    );

    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["story", "advance", "s-one", "--as", "driver", "--json"]
        )["to"],
        "ready"
    );
    let started = fixture.ok_json(
        &fixture.main,
        &["story", "advance", "s-one", "--as", "driver", "--json"],
    );
    assert_eq!(started["to"], "in-progress");
    assert_eq!(started["parentEpicFlipped"], true);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "e-one", "--json"])["metadata"]["workflowStatus"],
        "in-progress"
    );
    let blocked_testing = fixture.run(
        &fixture.main,
        &["story", "advance", "s-one", "--as", "driver", "--json"],
    );
    assert!(!blocked_testing.status.success());
    assert!(String::from_utf8_lossy(&blocked_testing.stderr).contains("t-dev"));
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-dev", "done", "--as", "worker", "--json"],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["story", "advance", "s-one", "--as", "driver", "--json"]
        )["to"],
        "testing"
    );
    let blocked_review = fixture.run(
        &fixture.main,
        &[
            "story",
            "advance",
            "s-one",
            "--as",
            "driver",
            "--reviewer",
            "reviewer",
            "--json",
        ],
    );
    assert!(!blocked_review.status.success());
    assert!(String::from_utf8_lossy(&blocked_review.stderr).contains("t-test"));
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-test", "done", "--as", "tester", "--json"],
    );
    let review = fixture.ok_json(
        &fixture.main,
        &[
            "story",
            "advance",
            "s-one",
            "--as",
            "driver",
            "--reviewer",
            "reviewer",
            "--json",
        ],
    );
    let review_task = review["dispatchedTaskID"].as_str().unwrap();
    let review_task_json = fixture.ok_json(&fixture.main, &["task", "show", review_task, "--json"]);
    assert_eq!(review_task_json["assignee"], "reviewer");
    assert_eq!(review_task_json["lane"], "review");

    let signed = fixture.ok_json(
        &fixture.main,
        &[
            "story",
            "signoff",
            "s-one",
            "--as",
            "reviewer",
            "--note",
            "looks good",
            "--json",
        ],
    );
    assert_eq!(signed["storyID"], "s-one");
    fixture.ok_json(
        &fixture.main,
        &[
            "story",
            "unsignoff",
            "s-one",
            "--as",
            "reviewer",
            "--note",
            "recheck",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["story", "signoff", "s-one", "--as", "reviewer", "--json"],
    );
    let merging = fixture.ok_json(
        &fixture.main,
        &[
            "story",
            "advance",
            "s-one",
            "--as",
            "driver",
            "--committer",
            "committer",
            "--json",
        ],
    );
    let merge_task = merging["dispatchedTaskID"].as_str().unwrap();
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", merge_task, "--json"])["assignee"],
        "committer"
    );
    let consumed = fixture.run(
        &fixture.main,
        &["story", "unsignoff", "s-one", "--as", "reviewer", "--json"],
    );
    assert!(!consumed.status.success());
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            merge_task,
            "done",
            "--as",
            "committer",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["story", "advance", "s-one", "--as", "driver", "--json"]
        )["to"],
        "done"
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Foundation",
            "--id",
            "t-base",
            "--priority",
            "2",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Blocked",
            "--id",
            "t-blocked",
            "--priority",
            "1",
            "--depends-on",
            "t-base",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Ready",
            "--id",
            "t-ready",
            "--priority",
            "3",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "worker-a", "--json"]
        )["taskID"],
        "t-base"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "worker-b", "--json"]
        )["taskID"],
        "t-ready"
    );
    let unmet = fixture.run(
        &fixture.main,
        &["claim", "t-blocked", "--as", "worker-c", "--json"],
    );
    assert!(!unmet.status.success());
    let cycle = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-base",
            "--as",
            "operator",
            "--depends-on",
            "t-blocked",
            "--json",
        ],
    );
    assert!(!cycle.status.success());
    assert!(String::from_utf8_lossy(&cycle.stderr).contains("cycle"));

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Frontend",
            "--id",
            "t-fe",
            "--lane",
            "fe",
            "--priority",
            "2",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Backend",
            "--id",
            "t-be",
            "--lane",
            "be",
            "--priority",
            "1",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "General",
            "--id",
            "t-free",
            "--priority",
            "3",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Driver",
            "--id",
            "t-driver",
            "--lane",
            "ops",
            "--priority",
            "0",
            "--driver-only",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Owned",
            "--id",
            "t-other",
            "--assignee",
            "worker-b",
            "--priority",
            "0",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "claim", "--next", "--as", "worker-a", "--lane", "fe", "--json"
            ]
        )["taskID"],
        "t-fe"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "claim", "--next", "--as", "worker-a", "--role", "be", "--json"
            ]
        )["taskID"],
        "t-be"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "claim",
                "--next",
                "--as",
                "driver",
                "--role",
                "ops",
                "--caller-scope",
                "driver",
                "--json",
            ]
        )["taskID"],
        "t-driver"
    );
    assert!(
        !fixture
            .run(
                &fixture.main,
                &["claim", "t-other", "--as", "worker-a", "--json"]
            )
            .status
            .success()
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Disposable", "--id", "t-remove", "--json"],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["task", "remove", "t-remove", "--as", "operator", "--json"]
        )["removed"],
        "t-remove"
    );
    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-remove", "--json"])
            .status
            .success()
    );
}

#[test]
fn claim_candidates_are_read_only_and_match_the_atomic_scheduler() {
    let fixture = Fixture::new("claim-candidates");
    fixture.ok_json(&fixture.main, &["init", "--name", "CANDIDATES", "--json"]);
    for tag in ["claims", "other"] {
        fixture.ok_json(&fixture.main, &["tag", "add", tag, "--as", "geo", "--json"]);
    }
    let add = |id: &str, extra: &[&str]| {
        let mut args = vec!["task", "add", id, "--id", id, "--tag", "claims"];
        args.extend_from_slice(extra);
        args.push("--json");
        fixture.ok_json(&fixture.main, &args)
    };

    add("t-base", &["--priority", "9"]);
    add(
        "t-dependency-blocked",
        &["--priority", "0", "--depends-on", "t-base"],
    );
    add("e-container", &["--type", "epic", "--priority", "0"]);
    add(
        "e-draft",
        &["--type", "epic", "--status", "draft", "--priority", "0"],
    );
    add("t-under-draft", &["--parent", "e-draft", "--priority", "0"]);
    add("t-leased", &["--priority", "0"]);
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-leased", "--as", "other-agent", "--json"],
    );
    add(
        "t-assigned-away",
        &["--assignee", "other-agent", "--priority", "0"],
    );
    add("t-driver-only", &["--driver-only", "--priority", "0"]);
    add("t-ready-first", &["--priority", "1"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "t-ready-other-tag",
            "--id",
            "t-ready-other-tag",
            "--tag",
            "other",
            "--priority",
            "2",
            "--json",
        ],
    );

    let board_path =
        fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
            .as_str()
            .unwrap()
            .to_owned();
    let before_bytes = fs::read(&board_path).unwrap();
    let before_metadata = fs::metadata(&board_path).unwrap().modified().unwrap();
    let registry_path = fixture.data.join("registry.db");
    let before_registry_bytes = fs::read(&registry_path).unwrap();
    let before_registry_metadata = fs::metadata(&registry_path).unwrap().modified().unwrap();
    let before_counts = Connection::open(&board_path)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM events),(SELECT count(*) FROM task_claims)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();

    let candidates = fixture.ok_json(
        &fixture.worktree,
        &[
            "claim",
            "--candidates",
            "--project",
            "CANDIDATES",
            "--as",
            "worker",
            "--tag",
            "claims",
            "--limit",
            "10",
            "--json",
        ],
    );
    assert_eq!(candidates.as_array().unwrap().len(), 2);
    assert_eq!(candidates[0]["id"], "t-ready-first");
    assert_eq!(candidates[1]["id"], "t-base");
    assert_eq!(candidates[0]["tags"], json!(["claims"]));
    assert_eq!(candidates[0]["priority"], 1);
    assert_eq!(candidates[0]["driverOnly"], false);
    assert!(candidates[0].get("leaseToken").is_none());

    let after_counts = Connection::open(&board_path)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM events),(SELECT count(*) FROM task_claims)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(before_counts, after_counts, "inspection wrote ledger state");
    assert_eq!(before_bytes, fs::read(&board_path).unwrap());
    assert_eq!(
        before_metadata,
        fs::metadata(&board_path).unwrap().modified().unwrap()
    );
    assert_eq!(before_registry_bytes, fs::read(&registry_path).unwrap());
    assert_eq!(
        before_registry_metadata,
        fs::metadata(&registry_path).unwrap().modified().unwrap()
    );

    // Each returned row is accepted by the real atomic claim path immediately
    // afterwards. This would fail if inspection's predicate were only a loose
    // approximation of scheduler eligibility.
    for id in ["t-ready-first", "t-base"] {
        let claimed = fixture.ok_json(&fixture.main, &["claim", id, "--as", "worker", "--json"]);
        assert_eq!(claimed["taskID"], id);
    }
}

#[test]
fn compiled_binary_bounds_context_and_generates_non_authoritative_todo() {
    let fixture = Fixture::new("projections");
    fixture.ok_json(&fixture.main, &["init", "--name", "Projection", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Resume safely",
            "--id",
            "t-context",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-context", "--as", "worker", "--json"],
    );
    let token = claim["leaseToken"].as_str().unwrap();
    for index in 0..20 {
        let note = format!("historical note {index} {}", "x".repeat(100));
        fixture.ok_json(
            &fixture.main,
            &[
                "note",
                "t-context",
                &note,
                "--as",
                "worker",
                "--kind",
                "progress",
                "--json",
            ],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-context",
            "--lease",
            token,
            "--as",
            "worker",
            "--summary",
            "Important latest summary",
            "--intent",
            "Preserve the continuity contract",
            "--next-action",
            "Run the exact verification command",
            "--json",
        ],
    );
    let context = fixture.run(
        &fixture.main,
        &["context", "t-context", "--max-chars", "1200"],
    );
    assert!(context.status.success());
    let context = String::from_utf8(context.stdout).unwrap();
    assert!(context.chars().count() <= 1201);
    assert!(context.contains("Run the exact verification command"));
    assert!(context.contains("[older history omitted]"));

    let output = fixture.root.join("TODO.md");
    let receipt = fixture.ok_json(
        &fixture.main,
        &["todo", "--output", output.to_str().unwrap(), "--json"],
    );
    assert_eq!(receipt["output"], output.to_str().unwrap());
    let todo = fs::read_to_string(output).unwrap();
    assert!(todo.contains("Projection only. SQLite is authoritative"));
    assert!(todo.contains("Run the exact verification command"));
}

#[test]
fn compiled_binary_surfaces_open_attention_on_task_story_and_epic_contexts() {
    let fixture = Fixture::new("attention-context");
    fixture.ok_json(&fixture.main, &["init", "--name", "ATTNCTX", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Review the linked attention",
            "--type",
            "epic",
            "--status",
            "todo",
            "--id",
            "e-attn",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Linked story",
            "--type",
            "story",
            "--status",
            "todo",
            "--id",
            "s-attn",
            "--parent",
            "e-attn",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Linked task",
            "--id",
            "t-attn",
            "--body",
            &"t".repeat(1_600),
            "--json",
        ],
    );
    let epic_attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Epic review is waiting on George.",
            "--as",
            "codex@driver",
            "--kind",
            "blocking",
            "--task",
            "e-attn",
            "--json",
        ],
    );
    let story_attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Story waiting on the operator.",
            "--as",
            "codex@driver",
            "--kind",
            "decision",
            "--task",
            "s-attn",
            "--json",
        ],
    );
    let task_attention_one = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Task needs a blocking review.",
            "--as",
            "codex@driver",
            "--kind",
            "blocking",
            "--task",
            "t-attn",
            "--json",
        ],
    );
    let task_attention_two = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Task also needs approval.",
            "--as",
            "codex@driver",
            "--kind",
            "approval",
            "--task",
            "t-attn",
            "--json",
        ],
    );

    let epic_ctx = fixture.ok_json(&fixture.main, &["context", "e-attn", "--json"]);
    assert_eq!(epic_ctx["openAttention"].as_array().unwrap().len(), 1);
    assert_eq!(epic_ctx["openAttention"][0]["id"], epic_attention["id"]);
    assert_eq!(epic_ctx["openAttention"][0]["taskID"], "e-attn");

    let story_ctx = fixture.ok_json(&fixture.main, &["context", "s-attn", "--json"]);
    assert_eq!(story_ctx["openAttention"].as_array().unwrap().len(), 1);
    assert_eq!(story_ctx["openAttention"][0]["id"], story_attention["id"]);
    assert_eq!(story_ctx["openAttention"][0]["taskID"], "s-attn");

    let task_ctx = fixture.ok_json(&fixture.main, &["context", "t-attn", "--json"]);
    assert_eq!(task_ctx["openAttention"].as_array().unwrap().len(), 2);
    assert_eq!(task_ctx["openAttention"][0]["id"], task_attention_one["id"]);
    assert_eq!(task_ctx["openAttention"][1]["id"], task_attention_two["id"]);
    assert_eq!(task_ctx["openAttention"][0]["taskID"], "t-attn");
    assert_eq!(task_ctx["openAttention"][1]["taskID"], "t-attn");

    let rendered = fixture.run(&fixture.main, &["context", "t-attn"]);
    assert!(rendered.status.success());
    let rendered = String::from_utf8(rendered.stdout).unwrap();
    assert!(rendered.contains("## Open attention"), "{rendered}");
    assert!(rendered.contains("2 open items"), "{rendered}");
    assert!(
        rendered.contains("Task needs a blocking review."),
        "{rendered}"
    );
    assert!(rendered.contains("Task also needs approval."), "{rendered}");

    let compact = fixture.run(&fixture.main, &["context", "t-attn", "--max-chars", "1200"]);
    assert!(compact.status.success());
    let compact = String::from_utf8(compact.stdout).unwrap();
    assert!(
        compact.contains("# Kanban cold-start context (compact)"),
        "{compact}"
    );
    assert!(
        compact.contains("Open attention: 2 open items"),
        "{compact}"
    );
    assert!(
        compact.contains("Task needs a blocking review."),
        "{compact}"
    );
    assert!(compact.contains("Task also needs approval."), "{compact}");
}

#[test]
fn compiled_binary_imports_both_atmux_formats_backs_up_and_opens_v3_databases() {
    let fixture = Fixture::new("migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "Import", "--json"]);
    let json_path = fixture.root.join("kanban.json");
    fs::write(
        &json_path,
        serde_json::to_vec(&json!({
            "epics": [{"id":"e-json","title":"JSON epic","status":"in-progress","isReady":true}],
            "stories": [{"id":"s-json","epic":"e-json","title":"JSON story","status":"testing"}],
            "tasks": [{"id":"t-json","story":"s-json","epic":"e-json","subject":"JSON task","status":"todo"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let receipt = fixture.ok_json(
        &fixture.main,
        &[
            "import",
            "atmux-json",
            json_path.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(receipt["imported"], 3);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-json", "--json"])["parentID"],
        "s-json"
    );

    let second = fixture.root.join("second");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&second, &["init", "--name", "SQLite import", "--json"]);
    let source = fixture.root.join("atmux-state.db");
    let legacy = Connection::open(&source).unwrap();
    legacy
        .execute_batch(
            r#"
            CREATE TABLE epics(id TEXT,title TEXT,status TEXT,created_at INTEGER,completed_at INTEGER,depends_on TEXT,stories TEXT,body TEXT,driver_ref TEXT,is_ready INTEGER,spawned_at INTEGER,extra TEXT);
            CREATE TABLE stories(id TEXT,epic TEXT,title TEXT,status TEXT,created_at INTEGER,completed_at INTEGER,advanced_at INTEGER,body TEXT,acceptance_criteria TEXT,review_signoff INTEGER,merge_task_id TEXT,merge_mode TEXT,extra TEXT);
            CREATE TABLE tasks(id TEXT,subject TEXT,status TEXT,created_at INTEGER,claimed_at INTEGER,completed_at INTEGER,epic TEXT,story TEXT,owner TEXT,deps TEXT,priority INTEGER,body TEXT,lane TEXT,deliverable TEXT,stale_min INTEGER,driver_only INTEGER,claimed_from TEXT,created_from TEXT,note TEXT,extra TEXT);
            INSERT INTO epics VALUES('e-sql','SQL epic','ready',1700000000,NULL,'[]','[]',NULL,NULL,1,NULL,'{}');
            INSERT INTO tasks VALUES('t-sql','SQL task','todo',1700000001,NULL,NULL,'e-sql',NULL,NULL,'[]',3,NULL,NULL,NULL,NULL,0,NULL,NULL,'legacy note','{}');
            "#,
        )
        .unwrap();
    drop(legacy);
    let sql_receipt = fixture.ok_json(
        &second,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(sql_receipt["imported"], 2);
    assert_eq!(sql_receipt["created"], 2);
    assert_eq!(sql_receipt["updated"], 0);

    let legacy = Connection::open(&source).unwrap();
    legacy
        .execute(
            "UPDATE tasks SET subject='SQL task refreshed', status='blocked' WHERE id='t-sql'",
            [],
        )
        .unwrap();
    drop(legacy);
    let duplicate = fixture.run(
        &second,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("--reconcile"));
    let reconciled = fixture.ok_json(
        &second,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--reconcile",
            "--json",
        ],
    );
    assert_eq!(reconciled["created"], 0);
    assert_eq!(reconciled["updated"], 2);
    assert_eq!(
        fixture.ok_json(&second, &["task", "show", "t-sql", "--json"])["title"],
        "SQL task refreshed"
    );

    let backup = fixture.root.join("backup");
    let backup_receipt = fixture.ok_json(
        &fixture.main,
        &["backup", "--output", backup.to_str().unwrap(), "--json"],
    );
    let reopened = backup_receipt["boards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|board| {
            fixture.run(
                &fixture.main,
                &["task", "list", "--db", board.as_str().unwrap(), "--json"],
            )
        })
        .find(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("t-json")
        })
        .expect("one backed-up board must contain the imported JSON hierarchy");
    assert!(
        String::from_utf8(reopened.stdout)
            .unwrap()
            .contains("t-json")
    );

    let v3 = fixture.root.join("typescript-v3.db");
    let database = Connection::open(&v3).unwrap();
    database
        .execute_batch(
            r#"
            PRAGMA user_version=3;
            CREATE TABLE tasks(id TEXT PRIMARY KEY,type TEXT,parent_id TEXT,title TEXT,body TEXT,status TEXT,priority INTEGER,created_at INTEGER,updated_at INTEGER,completed_at INTEGER,metadata TEXT,assignee TEXT,lane TEXT,deliverable TEXT,stale_minutes INTEGER,driver_only INTEGER);
            INSERT INTO tasks VALUES('t-v3','task',NULL,'Existing TypeScript board',NULL,'todo',3,1,1,NULL,'{}',NULL,NULL,NULL,NULL,0);
            "#,
        )
        .unwrap();
    drop(database);
    let compatible = fixture.run(
        &fixture.main,
        &["task", "list", "--db", v3.to_str().unwrap(), "--json"],
    );
    assert!(
        compatible.status.success(),
        "opening a V3 board failed: {}",
        String::from_utf8_lossy(&compatible.stderr)
    );
    assert!(
        String::from_utf8(compatible.stdout)
            .unwrap()
            .contains("Existing TypeScript board")
    );
}

/// Global addressing: a board must be reachable from a directory that belongs
/// to no registered project at all, and reachable BY NAME rather than by
/// knowing where its board file lives.
///
/// Honest-test note: every leg asserts the board it actually landed on, not
/// merely that the command exited 0. A `--project` that silently fell back to
/// the cwd-resolved board would still exit 0 — so the reads assert which task
/// came back, the write asserts the task appears on the target board AND is
/// absent from the other, and the ambiguous-name leg asserts a refusal rather
/// than a lucky pick.
#[test]
fn compiled_binary_addresses_projects_globally_without_cwd() {
    let fixture = Fixture::new("global");
    let beta = fixture.root.join("beta");
    let alpha_twin = fixture.root.join("alpha-twin");
    fs::create_dir_all(&beta).unwrap();
    fs::create_dir_all(&alpha_twin).unwrap();

    let alpha = fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let alpha_board = alpha["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(&beta, &["init", "--name", "Beta", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "alpha work", "--id", "t-alpha", "--json"],
    );
    fixture.ok_json(
        &beta,
        &["task", "add", "beta work", "--id", "t-beta", "--json"],
    );
    let ids = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_str().unwrap().to_owned())
            .collect()
    };

    // `fixture.root` is inside no registered project: it is the parent of the
    // registered roots, and resolution walks UP, never down.
    let outside = fixture.root.clone();

    // (1) With nothing to go on, the CLI must refuse — and the refusal must
    // teach the global route, or the operator's only recourse is to cd.
    let bare = fixture.run(&outside, &["task", "list", "--json"]);
    assert!(
        !bare.status.success(),
        "bare command outside a project must fail"
    );
    let message = String::from_utf8_lossy(&bare.stderr).into_owned();
    assert!(
        message.contains("--project"),
        "refusal must name --project: {message}"
    );
    assert!(
        message.contains("KANBAN_PROJECT"),
        "refusal must name the env var: {message}"
    );
    assert!(
        message.contains("Alpha") && message.contains("Beta"),
        "refusal must list known projects: {message}"
    );

    // (2) --project reaches a board from a directory owning no project.
    assert_eq!(
        ids(&fixture.ok_json(&outside, &["task", "list", "--project", "Alpha", "--json"])),
        vec!["t-alpha".to_owned()]
    );

    // (3) KANBAN_PROJECT does the same, so a cage can export it once.
    let env_output = fixture
        .command(&outside)
        .env("KANBAN_PROJECT", "Beta")
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        env_output.status.success(),
        "{}",
        String::from_utf8_lossy(&env_output.stderr)
    );
    assert_eq!(
        ids(&serde_json::from_slice::<Value>(&env_output.stdout).unwrap()),
        vec!["t-beta".to_owned()]
    );

    // (4) --workspace resolves the project containing a path other than cwd.
    assert_eq!(
        ids(&fixture.ok_json(
            &outside,
            &[
                "task",
                "list",
                "--workspace",
                fixture.main.to_str().unwrap(),
                "--json"
            ]
        )),
        vec!["t-alpha".to_owned()]
    );

    // (5) An explicit --project beats the cwd it is standing in. The working
    // directory is a fallback, not a request, so there is nothing to disagree
    // with.
    assert_eq!(
        ids(&fixture.ok_json(
            &fixture.main,
            &["task", "list", "--project", "Beta", "--json"]
        )),
        vec!["t-beta".to_owned()]
    );

    // (5b) Two selectors a caller typed is ambiguity, not precedence. --db used
    // to win silently, answering from a board the caller had also named
    // otherwise — and creating it, empty, when the path did not exist.
    let two_flags = fixture.run(
        &fixture.main,
        &[
            "task",
            "list",
            "--project",
            "Beta",
            "--db",
            &alpha_board,
            "--json",
        ],
    );
    assert!(
        !two_flags.status.success(),
        "--db silently beat --project instead of refusing"
    );
    let conflict = String::from_utf8_lossy(&two_flags.stderr).to_string();
    assert!(conflict.contains("--project Beta"), "{conflict}");
    assert!(conflict.contains("--db"), "{conflict}");
    assert!(conflict.contains("each name a board"), "{conflict}");

    // The refusal must not have conjured or touched a board on the way.
    assert_eq!(
        ids(&fixture.ok_json(
            &fixture.main,
            &["task", "list", "--project", "Alpha", "--json"]
        )),
        vec!["t-alpha".to_owned()]
    );

    // A --db path that does not exist is the sharper case: precedence used to
    // answer from a file it created on the spot, so the caller who named a
    // project got an empty board and no error.
    let ghost = fixture.root.join("conjured.db");
    let conjuring = fixture.run(
        &fixture.main,
        &[
            "task",
            "list",
            "--project",
            "Beta",
            "--db",
            ghost.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        !conjuring.status.success(),
        "a ghost --db won by precedence"
    );
    assert!(!ghost.exists(), "the refused command still created a board");

    // Each selector alone still works, and the environment stays a default a
    // flag is free to override.
    assert_eq!(
        ids(&fixture.ok_json(
            &fixture.main,
            &["task", "list", "--db", &alpha_board, "--json"]
        )),
        vec!["t-alpha".to_owned()]
    );
    let env_override = fixture
        .command(&fixture.main)
        .args(["task", "list", "--project", "Alpha", "--json"])
        .env("KANBAN_PROJECT", "Beta")
        .output()
        .unwrap();
    assert!(
        env_override.status.success(),
        "a flag overriding its own env default is not a conflict: {}",
        String::from_utf8_lossy(&env_override.stderr)
    );
    assert_eq!(
        ids(&serde_json::from_slice::<Value>(&env_override.stdout).unwrap()),
        vec!["t-alpha".to_owned()]
    );

    // (6) Writes land on the named board, and nowhere else.
    fixture.ok_json(
        &outside,
        &[
            "task",
            "add",
            "written from outside",
            "--id",
            "t-remote",
            "--project",
            "Beta",
            "--json",
        ],
    );
    let beta_ids =
        ids(&fixture.ok_json(&outside, &["task", "list", "--project", "Beta", "--json"]));
    assert!(
        beta_ids.contains(&"t-remote".to_owned()),
        "write did not land on Beta: {beta_ids:?}"
    );
    let alpha_ids =
        ids(&fixture.ok_json(&outside, &["task", "list", "--project", "Alpha", "--json"]));
    assert!(
        !alpha_ids.contains(&"t-remote".to_owned()),
        "write leaked onto Alpha: {alpha_ids:?}"
    );

    // (7) An unknown name fails with the roster rather than an empty board.
    let unknown = fixture.run(&outside, &["task", "list", "--project", "Gamma", "--json"]);
    assert!(!unknown.status.success());
    let unknown_message = String::from_utf8_lossy(&unknown.stderr).into_owned();
    assert!(
        unknown_message.contains("no Kanban project named Gamma"),
        "{unknown_message}"
    );
    assert!(unknown_message.contains("Alpha"), "{unknown_message}");

    // (8) Registry names are not unique. A duplicate must refuse and name the
    // candidate roots — picking one would corrupt the loser's work state.
    seed_legacy_rootless_duplicate(&fixture, "alpha-twin", "Alpha");
    let listed = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "Alpha" && row["rootless"] == true),
        "workspace list omitted the rootless Alpha board: {listed}"
    );
    let ambiguous = fixture.run(&outside, &["task", "list", "--project", "Alpha", "--json"]);
    assert!(
        !ambiguous.status.success(),
        "duplicate project name must not resolve silently"
    );
    let ambiguous_message = String::from_utf8_lossy(&ambiguous.stderr).into_owned();
    assert!(
        ambiguous_message.contains("2 Kanban projects are named Alpha"),
        "{ambiguous_message}"
    );
    assert!(
        ambiguous_message.contains(fixture.main.canonicalize().unwrap().to_str().unwrap())
            && ambiguous_message.contains("Alpha (rootless)"),
        "{ambiguous_message}"
    );

    let second_rootless = fixture.root.join("alpha-rootless-two");
    fs::create_dir_all(&second_rootless).unwrap();
    let refused = fixture.run(
        &second_rootless,
        &["init", "--name", "Alpha", "--rootless", "--json"],
    );
    assert!(
        !refused.status.success(),
        "a second active rootless Alpha board was accepted"
    );
    let refused_message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        refused_message.contains("a Kanban board is already named Alpha"),
        "{refused_message}"
    );

    let attach = fixture.root.join("alpha-attach");
    fs::create_dir_all(&attach).unwrap();
    let attached = fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "attach",
            "--workspace",
            "../alpha-attach",
            "--to",
            "Alpha",
            "--json",
        ],
    );
    let attached_root = attach
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let rootless_board_path = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "Alpha" && row["rootless"] == true)
        .expect("rootless Alpha row")["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        attached["boardPath"], rootless_board_path,
        "name-based attach should choose the unique rootless board"
    );
    let attached_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    let attached_row = attached_list
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rootPath"] == attached_root)
        .expect("attached root missing from workspace list");
    assert_eq!(attached_row["boardPath"], rootless_board_path);
    assert_eq!(attached_row["rootless"], false);

    // (9) Path-like attach targets stay path-like, even when they look short.
    let dot_attach = fixture.root.join("alpha-dot-attach");
    fs::create_dir_all(&dot_attach).unwrap();
    let rooted_root = fixture
        .main
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let rooted_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    let rooted_board_path = rooted_list
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rootPath"] == rooted_root)
        .expect("rooted board missing from workspace list")["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let dot = fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "attach",
            "--workspace",
            "../alpha-dot-attach",
            "--to",
            ".",
            "--json",
        ],
    );
    assert_eq!(dot["boardPath"], rooted_board_path);
    let dot_root = dot_attach
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let dot_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    let dot_row = dot_list
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rootPath"] == dot_root)
        .expect("dot-attached root missing from workspace list");
    assert_eq!(dot_row["boardPath"], rooted_board_path);

    // (9) --workspace still disambiguates what the name cannot.
    assert_eq!(
        ids(&fixture.ok_json(
            &outside,
            &[
                "task",
                "list",
                "--workspace",
                fixture.main.to_str().unwrap(),
                "--json"
            ]
        )),
        vec!["t-alpha".to_owned()]
    );
}

#[test]
fn compiled_binary_keeps_rootless_boards_out_of_unreachable_roots() {
    let fixture = Fixture::new("rootless-doctor-repoint");
    let rootless = fixture.root.join("rootless");
    fs::create_dir_all(&rootless).unwrap();

    fixture.ok_json(
        &rootless,
        &["init", "--name", "ROOTLESS", "--rootless", "--json"],
    );
    fixture.ok_json(
        &fixture.root,
        &[
            "task",
            "add",
            "Rootless work",
            "--id",
            "t-rootless",
            "--project",
            "ROOTLESS",
            "--json",
        ],
    );

    let listed = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "ROOTLESS" && row["rootless"] == true),
        "workspace list omitted the healthy rootless board: {listed}"
    );

    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert!(
        doctor["unreachableRoots"].as_array().unwrap().is_empty(),
        "doctor should not report a healthy rootless board as an unreachable root: {doctor}"
    );
    let project = doctor["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "ROOTLESS")
        .expect("rootless project missing from doctor");
    assert_eq!(project["rootless"], true);
    assert!(
        project["workspaceRoots"].as_array().unwrap().is_empty(),
        "the rootless board should remain rootless in doctor output"
    );

    let tasks = fixture.ok_json(
        &fixture.root,
        &["task", "list", "--project", "ROOTLESS", "--json"],
    );
    assert_eq!(
        tasks.as_array().unwrap().len(),
        1,
        "the healthy rootless board should remain addressable by name"
    );
    assert_eq!(tasks[0]["id"], "t-rootless");

    let repoint = fixture.run(
        &fixture.main,
        &["workspace", "repoint", "--root", "", "--json"],
    );
    assert!(
        !repoint.status.success(),
        "an empty root was accepted as a repoint candidate"
    );
    let repoint_message = String::from_utf8_lossy(&repoint.stderr);
    assert!(
        repoint_message.contains("not a registered root that needs repointing"),
        "{repoint_message}"
    );
}

/// Every fix below has a probe on the pre-fix binary behind it. These assert the
/// dangerous behaviour is gone, not merely that the happy path still works.
#[test]
fn compiled_binary_refuses_unknown_flags_instead_of_writing_to_the_wrong_board() {
    let fixture = Fixture::new("flags");
    let beta = fixture.root.join("beta");
    fs::create_dir_all(&beta).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    fixture.ok_json(&beta, &["init", "--name", "Beta", "--json"]);

    // A typo in --project must not fall through to directory resolution. Before
    // this guard the task landed on Beta's board and the command reported
    // success, which is the wrong-board damage ADR-007 exists to prevent.
    let typo = fixture.run(
        &beta,
        &[
            "task",
            "add",
            "meant for alpha",
            "--projct",
            "Alpha",
            "--id",
            "t-oops",
            "--json",
        ],
    );
    assert!(
        !typo.status.success(),
        "a mistyped --project must not be ignored"
    );
    let message = String::from_utf8_lossy(&typo.stderr).into_owned();
    assert!(message.contains("unknown flag --projct"), "{message}");
    assert!(message.contains("did you mean --project?"), "{message}");
    for cwd in [&fixture.main, &beta] {
        let listed = fixture.ok_json(cwd, &["task", "list", "--json"]);
        assert!(
            listed.as_array().unwrap().is_empty(),
            "a rejected command still wrote: {listed}"
        );
    }

    // A flag that is real elsewhere is still wrong here.
    let misplaced = fixture.run(&fixture.main, &["task", "list", "--lease", "x", "--json"]);
    assert!(!misplaced.status.success());
    assert!(String::from_utf8_lossy(&misplaced.stderr).contains("unknown flag --lease"));

    // A silently-ignored --status typo used to return the whole board.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "real", "--id", "t-real", "--json"],
    );
    let status_typo = fixture.run(
        &fixture.main,
        &["task", "list", "--statis", "done", "--json"],
    );
    assert!(
        !status_typo.status.success(),
        "a mistyped --status must not list everything"
    );

    // Sibling commands do not lend each other flags: --reason belongs to
    // `handoff create`, not `checkpoint`, and must not be quietly swallowed.
    let borrowed = fixture.run(
        &fixture.main,
        &[
            "checkpoint",
            "t-real",
            "--lease",
            "x",
            "--as",
            "a",
            "--reason",
            "manual",
            "--json",
        ],
    );
    assert!(!borrowed.status.success());
    assert!(String::from_utf8_lossy(&borrowed.stderr).contains("unknown flag --reason"));

    // Valid flags, including the globals, keep working.
    fixture.ok_json(
        &fixture.main,
        &["task", "list", "--status", "todo", "--json"],
    );
    let version = fixture.run(&fixture.main, &["version"]);
    let version = String::from_utf8_lossy(&version.stdout);
    assert!(version.contains("kanban"));
    assert!(
        version.contains("board schema 20"),
        "version output: {version}"
    );
    assert!(
        version.contains("registry schema 11"),
        "version output: {version}"
    );
}

#[test]
fn compiled_binary_never_repermissions_directories_it_does_not_own() {
    let fixture = Fixture::new("perms");
    let shared = fixture.root.join("shared");
    fs::create_dir_all(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    let board = shared.join("board.db");

    fixture.ok_json(
        &fixture.main,
        &[
            "--db",
            board.to_str().unwrap(),
            "task",
            "add",
            "external board",
            "--json",
        ],
    );

    // `--db /tmp/x.db` used to chmod the containing directory to 0700. As root
    // that locks a shared directory away from every other process on the host.
    assert_eq!(
        fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
        0o755,
        "kanban re-permissioned an operator directory it does not own",
    );
    // The board itself is still private, and was never briefly world-readable.
    assert_eq!(
        fs::metadata(&board).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Directories kanban does create are private from creation.
    let nested = shared.join("deep/nest/board.db");
    fixture.ok_json(
        &fixture.main,
        &["--db", nested.to_str().unwrap(), "task", "list", "--json"],
    );
    assert_eq!(
        fs::metadata(shared.join("deep"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700,
    );
}

#[test]
fn compiled_binary_protects_live_leases_from_operator_overrides() {
    let fixture = Fixture::new("leases");
    fixture.ok_json(&fixture.main, &["init", "--name", "Leases", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "leased", "--id", "t-lease", "--json"],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-lease", "--as", "worker", "--json"],
    );
    let token = claim["leaseToken"].as_str().unwrap().to_owned();

    // Moving a leased task used to delete the claim row silently, so the holder
    // discovered it only when its checkpoint failed after the work was done.
    let stolen = fixture.run(
        &fixture.main,
        &["task", "move", "t-lease", "todo", "--as", "other", "--json"],
    );
    assert!(
        !stolen.status.success(),
        "move must not void another agent's lease"
    );
    let message = String::from_utf8_lossy(&stolen.stderr).into_owned();
    assert!(message.contains("leased by worker"), "{message}");
    assert!(message.contains("--force"), "{message}");
    let removed = fixture.run(
        &fixture.main,
        &["task", "remove", "t-lease", "--as", "other", "--json"],
    );
    assert!(
        !removed.status.success(),
        "remove must not void another agent's lease"
    );

    // The holder can still finish, which is the property the guard protects.
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-lease",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "did the work",
            "--intent",
            "keep going",
            "--next-action",
            "ship it",
            "--json",
        ],
    );

    // A `continue` checkpoint retains the lease, so worker still holds it here.
    // --force is the deliberate override, and it is recorded as a seizure.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-lease", "todo", "--as", "operator", "--force", "--json",
        ],
    );
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let events = Connection::open(&board).unwrap();
    let seized: i64 = events
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind='lease_seized'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seized, 1, "a forced seizure must be recorded in the ledger");

    // Removing a parent names its children instead of raising a raw FK error.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "parent", "--id", "s-p", "--type", "story", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "child", "--id", "t-c", "--parent", "s-p", "--json",
        ],
    );
    let parent = fixture.run(
        &fixture.main,
        &["task", "remove", "s-p", "--as", "operator", "--json"],
    );
    assert!(!parent.status.success());
    let parent_message = String::from_utf8_lossy(&parent.stderr).into_owned();
    assert!(
        parent_message.contains("child task(s): t-c"),
        "{parent_message}"
    );

    // A lease length that would overflow the millisecond conversion is refused,
    // not panicked on.
    let overflow = fixture.run(
        &fixture.main,
        &[
            "claim",
            "t-c",
            "--as",
            "worker",
            "--lease-minutes",
            "999999999999999",
            "--json",
        ],
    );
    assert!(!overflow.status.success());
    let overflow_message = String::from_utf8_lossy(&overflow.stderr).into_owned();
    assert!(
        overflow_message.contains("lease minutes must be between"),
        "{overflow_message}"
    );
    assert!(!overflow_message.contains("panicked"), "{overflow_message}");
}

#[test]
fn compiled_binary_reports_context_truncation_truthfully() {
    let fixture = Fixture::new("truncation");
    fixture.ok_json(&fixture.main, &["init", "--name", "Truncation", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "long history", "--id", "t-long", "--json"],
    );

    // Under the note cap the packet is complete and must not claim otherwise.
    for index in 0..5 {
        fixture.ok_json(
            &fixture.main,
            &[
                "note",
                "t-long",
                &format!("early note {index}"),
                "--as",
                "worker",
                "--json",
            ],
        );
    }
    let short = fixture.ok_json(&fixture.main, &["context", "t-long", "--json"]);
    assert_eq!(short["truncated"], false);
    assert_eq!(short["notes"].as_array().unwrap().len(), 5);

    // Past it, `truncated` was hardcoded false: a resuming agent was told it
    // held the whole record while the oldest notes were being dropped.
    for index in 5..110 {
        fixture.ok_json(
            &fixture.main,
            &[
                "note",
                "t-long",
                &format!("later note {index}"),
                "--as",
                "worker",
                "--json",
            ],
        );
    }
    let long = fixture.ok_json(&fixture.main, &["context", "t-long", "--json"]);
    let notes = long["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 100, "the cap itself still holds");
    assert_eq!(long["truncated"], true, "dropped history must be declared");
    // The retained window is the newest, and the rendered packet says so.
    assert_eq!(notes.last().unwrap()["body"], "later note 109");
    assert!(notes.iter().all(|note| note["body"] != "early note 0"));
    let rendered = fixture.run(&fixture.main, &["context", "t-long"]);
    assert!(rendered.status.success());
    assert!(String::from_utf8_lossy(&rendered.stdout).contains("[older history omitted]"));
}

#[test]
fn compiled_binary_refuses_to_shadow_an_enclosing_project() {
    let fixture = Fixture::new("nesting");
    fixture.ok_json(&fixture.main, &["init", "--name", "Outer", "--json"]);
    let inner = fixture.main.join("packages/inner");
    fs::create_dir_all(&inner).unwrap();

    // `kanban init` in a subdirectory used to create a second board. Tasks added
    // there resolved to the nearer board and were invisible from the root.
    let nested = fixture.run(&inner, &["init", "--name", "Inner", "--json"]);
    assert!(
        !nested.status.success(),
        "init must not silently shadow an enclosing board"
    );
    let message = String::from_utf8_lossy(&nested.stderr).into_owned();
    assert!(
        message.contains("already inside Kanban project Outer"),
        "{message}"
    );
    assert!(message.contains("workspace attach --to"), "{message}");
    assert!(message.contains("--force"), "{message}");

    // Attaching is the documented route, and shares one board across worktrees.
    fixture.ok_json(
        &inner,
        &[
            "workspace",
            "attach",
            "--to",
            fixture.main.to_str().unwrap(),
            "--json",
        ],
    );
    fixture.ok_json(
        &inner,
        &[
            "task",
            "add",
            "from the subtree",
            "--id",
            "t-inner",
            "--json",
        ],
    );
    let from_root = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    assert_eq!(
        from_root.as_array().unwrap().len(),
        1,
        "attached worktree wrote to a different board"
    );
    assert_eq!(from_root[0]["id"], "t-inner");

    // A deliberate nested board is still reachable, but only when asked for.
    let sibling = fixture.main.join("packages/separate");
    fs::create_dir_all(&sibling).unwrap();
    fixture.ok_json(
        &sibling,
        &["init", "--name", "Separate", "--force", "--json"],
    );
    assert!(
        fixture
            .ok_json(&sibling, &["task", "list", "--json"])
            .as_array()
            .unwrap()
            .is_empty(),
        "a forced nested board must be its own board",
    );
}

#[test]
fn compiled_binary_installs_as_kb_and_resolves_command_aliases() {
    let fixture = Fixture::new("aliases");
    // `kb` is a second binary, not a shell alias: agents call it from
    // non-interactive cages that never source a shell profile.
    let kb = |cwd: &Path, args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_kb"))
            .current_dir(cwd)
            .env("KANBAN_DATA_DIR", &fixture.data)
            .env_remove("KANBAN_DB")
            .args(args)
            .output()
            .unwrap()
    };
    let kb_json = |cwd: &Path, args: &[&str]| -> Value {
        let output = kb(cwd, args);
        assert!(
            output.status.success(),
            "kb failed: {args:?}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    assert!(String::from_utf8_lossy(&kb(&fixture.main, &["version"]).stdout).contains("kanban"));
    kb_json(&fixture.main, &["init", "--name", "Aliased", "--json"]);

    // Every alias reaches the same command as its long form.
    kb_json(
        &fixture.main,
        &["t", "new", "aliased", "--id", "t-1", "--json"],
    );
    assert_eq!(
        kb_json(&fixture.main, &["t", "ls", "--json"])[0]["id"],
        "t-1"
    );
    kb_json(
        &fixture.main,
        &["t", "mv", "t-1", "review", "--as", "geo", "--json"],
    );
    assert_eq!(
        kb_json(&fixture.main, &["t", "cat", "t-1", "--json"])["status"],
        "review"
    );
    kb_json(
        &fixture.main,
        &["t", "up", "t-1", "--as", "geo", "--priority", "1", "--json"],
    );
    kb_json(
        &fixture.main,
        &["n", "t-1", "a note", "--as", "geo", "--json"],
    );
    assert!(kb(&fixture.main, &["ctx", "t-1"]).status.success());
    assert!(kb(&fixture.main, &["dash"]).status.success());
    kb_json(&fixture.main, &["w", "ls", "--json"]);

    // Both binaries are one program over one board.
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["priority"],
        1
    );
    kb_json(&fixture.main, &["t", "rm", "t-1", "--as", "geo", "--json"]);
    assert!(
        fixture
            .ok_json(&fixture.main, &["task", "list", "--json"])
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Sub-aliases apply only where the second positional is a subcommand. A
    // task genuinely called `rm` must not be rewritten into a removal.
    kb_json(
        &fixture.main,
        &["t", "new", "edge case", "--id", "rm", "--json"],
    );
    kb_json(
        &fixture.main,
        &["n", "rm", "note on task rm", "--as", "geo", "--json"],
    );
    assert_eq!(
        kb_json(&fixture.main, &["t", "cat", "rm", "--json"])["id"],
        "rm"
    );

    // Aliases are an exact-match table, so an unlisted one stays unknown
    // rather than being inferred (ADR-008).
    let invented = kb(&fixture.main, &["t", "zz", "--json"]);
    assert!(!invented.status.success());
    assert!(String::from_utf8_lossy(&invented.stderr).contains("unknown command"));
    let stem = kb(&fixture.main, &["task", "li", "--json"]);
    assert!(
        !stem.status.success(),
        "an unlisted stem must not resolve to list"
    );
}

#[test]
fn compiled_binary_suggests_the_flag_an_abbreviation_was_reaching_for() {
    let fixture = Fixture::new("hints");
    fixture.ok_json(&fixture.main, &["init", "--name", "Hints", "--json"]);
    let stderr = |args: &[&str]| -> String {
        let output = fixture.run(&fixture.main, args);
        assert!(!output.status.success(), "{args:?} should have failed");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    // Abbreviating is at least as common as mistyping, and edit distance alone
    // misses it: `proj` is three edits from `project`.
    assert!(stderr(&["task", "list", "--proj", "Hints"]).contains("did you mean --project?"));
    assert!(stderr(&["task", "list", "--pro", "Hints"]).contains("did you mean --project?"));
    assert!(stderr(&["task", "list", "--projct", "Hints"]).contains("did you mean --project?"));
    assert!(stderr(&["heartbeat", "t-1", "--lese", "x"]).contains("did you mean --lease?"));

    // An ambiguous stem is not guessed at. Under `task add`, --p could be
    // parent, priority or project, so the accepted list is the answer.
    let ambiguous = stderr(&["task", "add", "T", "--p", "x"]);
    assert!(!ambiguous.contains("did you mean"), "{ambiguous}");
    assert!(
        ambiguous.contains("--parent") && ambiguous.contains("--priority"),
        "{ambiguous}"
    );

    // A stem is a suggestion, never an alias: it must still fail.
    assert!(stderr(&["task", "list", "--proj", "Hints"]).contains("unknown flag --proj"));
}

#[test]
fn compiled_binary_retires_dead_leases_before_any_read_and_records_them() {
    let fixture = Fixture::new("sweep");
    fixture.ok_json(&fixture.main, &["init", "--name", "Sweep", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "abandoned", "--id", "t-1", "--json"],
    );
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "ghost", "--json"]);

    // Simulate the agent vanishing: the lease runs out with nobody to release
    // it. Expiry used to happen only inside claim/accept_handoff, so every read
    // path kept reporting the task as owned while `claim --next` gave it away.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute("UPDATE task_claims SET expires_at=1", [])
        .unwrap();

    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    assert_eq!(
        listed[0]["status"], "todo",
        "a dead lease must not read as in_progress"
    );
    assert!(
        listed[0]["assignee"].is_null(),
        "a dead lease must not keep its assignee"
    );
    assert!(fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["claim"].is_null());

    // The TODO projection used to contradict itself: the task appeared under
    // "Restart here" as in-progress with "Owner: unclaimed" beneath it.
    let todo = String::from_utf8_lossy(&fixture.run(&fixture.main, &["todo"]).stdout).into_owned();
    assert!(todo.contains("No task is currently in progress."), "{todo}");
    assert!(!todo.contains("Owner: unclaimed"), "{todo}");

    // The sweep is itself durable history, not a silent correction.
    let expired = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "claim_expired", "--json"],
    );
    assert_eq!(expired.as_array().unwrap().len(), 1);
    assert_eq!(expired[0]["actor"], "ghost");
}

#[test]
fn compiled_binary_exposes_the_audit_trail_it_writes() {
    let fixture = Fixture::new("events");
    fixture.ok_json(&fixture.main, &["init", "--name", "Events", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "audited", "--id", "t-1", "--json"],
    );
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);

    // A forced override is only a safety feature if someone can review it.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-1", "todo", "--as", "operator", "--force", "--json",
        ],
    );
    let seized = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "lease_seized", "--json"],
    );
    assert_eq!(seized.as_array().unwrap().len(), 1);
    assert_eq!(seized[0]["actor"], "operator");
    assert_eq!(seized[0]["payload"]["heldBy"], "worker");
    assert_eq!(seized[0]["payload"]["action"], "move");

    // A destructive removal records what it destroyed, before it is gone.
    fixture.ok_json(
        &fixture.main,
        &["note", "t-1", "evidence", "--as", "worker", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "t-1", "--as", "operator", "--json"],
    );
    let removed = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "task_removed", "--json"],
    );
    assert_eq!(removed[0]["payload"]["discardedNotes"], 1);

    // Newest first, filterable by task, and bounded.
    let all = fixture.ok_json(&fixture.main, &["events", "--json"]);
    let seqs: Vec<i64> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] > pair[1]),
        "not newest-first: {seqs:?}"
    );
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["events", "--limit", "2", "--json"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Filtering by a task that does not exist is an error, not an empty list.
    assert!(
        !fixture
            .run(&fixture.main, &["events", "--task", "t-nope", "--json"])
            .status
            .success()
    );
}

#[test]
fn compiled_binary_detects_a_structurally_valid_board_event_edit() {
    let fixture = Fixture::new("audit-board-tamper");
    fixture.ok_json(&fixture.main, &["init", "--name", "Audit", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "audited", "--id", "t-audit", "--json"],
    );
    let clean = fixture.ok_json(&fixture.main, &["audit", "verify", "--json"]);
    assert_eq!(clean["healthy"], true);
    assert_eq!(clean["boards"][0]["audit"]["healthy"], true);

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(board)
        .unwrap()
        .execute("UPDATE events SET payload='{}' WHERE kind='task_added'", [])
        .unwrap();

    let audit = fixture.run(&fixture.main, &["audit", "verify", "--json"]);
    assert!(
        !audit.status.success(),
        "edited history passed audit verification"
    );
    let receipt: Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(receipt["healthy"], false);
    assert!(
        receipt["boards"][0]["audit"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("event hash")),
        "receipt did not identify the broken digest: {receipt}"
    );
    assert!(
        !fixture
            .run(&fixture.main, &["doctor", "--json"])
            .status
            .success(),
        "doctor ignored a broken audit chain"
    );
}

#[test]
fn compiled_binary_detects_registry_rule_history_edit() {
    let fixture = Fixture::new("audit-registry-tamper");
    fixture.ok_json(&fixture.main, &["init", "--name", "Audit", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["rule", "add", "Keep evidence.", "--as", "geo", "--json"],
    );
    fixture.ok_json(&fixture.main, &["audit", "verify", "--json"]);

    Connection::open(fixture.data.join("registry.db"))
        .unwrap()
        .execute("UPDATE rule_events SET actor='intruder'", [])
        .unwrap();
    let audit = fixture.run(&fixture.main, &["audit", "verify", "--json"]);
    assert!(
        !audit.status.success(),
        "edited registry history passed verification"
    );
    let receipt: Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(receipt["registry"]["healthy"], false);
    assert!(
        receipt["registry"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("event hash")),
        "receipt did not identify the registry digest mismatch: {receipt}"
    );
}

#[test]
fn compiled_binary_detects_deleted_and_reordered_board_history() {
    for mutation in ["delete", "reorder"] {
        let fixture = Fixture::new(&format!("audit-{mutation}"));
        fixture.ok_json(&fixture.main, &["init", "--name", "Audit", "--json"]);
        fixture.ok_json(
            &fixture.main,
            &["task", "add", "audited", "--id", "t-audit", "--json"],
        );
        fixture.ok_json(
            &fixture.main,
            &["claim", "t-audit", "--as", "worker", "--json"],
        );
        fixture.ok_json(
            &fixture.main,
            &["note", "t-audit", "evidence", "--as", "worker", "--json"],
        );
        let board =
            fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
                .as_str()
                .unwrap()
                .to_owned();
        let connection = Connection::open(board).unwrap();
        let sequences = {
            let mut statement = connection
                .prepare("SELECT seq FROM events ORDER BY seq LIMIT 3")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(sequences.len(), 3);
        if mutation == "delete" {
            connection
                .execute("DELETE FROM events WHERE seq=?", [sequences[1]])
                .unwrap();
        } else {
            connection
                .execute_batch(&format!(
                    "UPDATE events SET seq=-1 WHERE seq={};\
                     UPDATE events SET seq={} WHERE seq={};\
                     UPDATE events SET seq={} WHERE seq=-1;",
                    sequences[0], sequences[0], sequences[1], sequences[1]
                ))
                .unwrap();
        }
        drop(connection);

        let audit = fixture.run(&fixture.main, &["audit", "verify", "--json"]);
        assert!(
            !audit.status.success(),
            "{mutation} passed audit verification"
        );
        let receipt: Value = serde_json::from_slice(&audit.stdout).unwrap();
        assert!(
            !receipt["boards"][0]["audit"]["errors"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{mutation} produced no forensic diagnostic: {receipt}"
        );
    }
}

#[test]
fn compiled_binary_reports_tasks_that_overran_their_stale_budget() {
    let fixture = Fixture::new("stale");
    fixture.ok_json(&fixture.main, &["init", "--name", "Stale", "--json"]);
    // `stale_minutes` was accepted, stored and imported from atmux, and then
    // read by nothing: a task could be configured stale-aware and never
    // reported. Only tasks that carry a budget are in scope.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "budgeted",
            "--id",
            "t-slow",
            "--stale-minutes",
            "1",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "no budget", "--id", "t-free", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-slow", "--as", "worker", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-free", "--as", "worker", "--json"],
    );

    // A live heartbeat is not stale, whatever the budget says.
    assert!(
        fixture
            .ok_json(&fixture.main, &["stale", "--json"])
            .as_array()
            .unwrap()
            .is_empty(),
        "a task heartbeating now is not stale"
    );

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE task_claims SET heartbeat_at=heartbeat_at-600000",
            [],
        )
        .unwrap();

    let stale = fixture.ok_json(&fixture.main, &["stale", "--json"]);
    let rows = stale.as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the budgeted task is stale: {stale}");
    assert_eq!(rows[0]["id"], "t-slow");
    assert_eq!(rows[0]["idleMinutes"], 10);
    assert_eq!(rows[0]["overdueMinutes"], 9);
    assert_eq!(rows[0]["lastSignal"], "heartbeat");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["dashboard", "--json"])[0]["staleTasks"],
        1
    );
}

#[test]
fn compiled_binary_restores_a_snapshot_over_destroyed_work_state() {
    let fixture = Fixture::new("restore");
    fixture.ok_json(&fixture.main, &["init", "--name", "Recover", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "real work", "--id", "t-keep", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["note", "t-keep", "evidence", "--as", "worker", "--json"],
    );

    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // Destroy the work the snapshot holds.
    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "t-keep", "--as", "oops", "--json"],
    );
    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-keep", "--json"])
            .status
            .success()
    );

    // Restore overwrites live state, so it refuses until asked twice.
    let unforced = fixture.run(&fixture.main, &["restore", "--from", &snapshot, "--json"]);
    assert!(
        !unforced.status.success(),
        "restore must not overwrite live state by default"
    );
    assert!(String::from_utf8_lossy(&unforced.stderr).contains("--force"));

    let restored = fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    // A mistaken restore has to be recoverable in turn.
    let rescue = restored["rescueSnapshot"].as_str().unwrap();
    assert!(
        Path::new(rescue).join("registry.db").is_file(),
        "no rescue snapshot at {rescue}"
    );

    let recovered = fixture.ok_json(&fixture.main, &["task", "show", "t-keep", "--json"]);
    assert_eq!(recovered["title"], "real work");
    assert_eq!(
        recovered["notes"][0]["body"], "evidence",
        "durable history came back too"
    );

    // A directory that is not a snapshot is rejected before anything is touched.
    let bogus = fixture.root.join("not-a-snapshot");
    fs::create_dir_all(&bogus).unwrap();
    let refused = fixture.run(
        &fixture.main,
        &[
            "restore",
            "--from",
            bogus.to_str().unwrap(),
            "--force",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no registry.db"));
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-keep", "--json"])["title"],
        "real work",
        "a refused restore must leave live state untouched"
    );
}

#[test]
fn compiled_binary_restores_a_rootless_board_snapshot_and_keeps_name_addressing() {
    let fixture = Fixture::new("restore-rootless");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "ROOTLESS", "--rootless", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "rootless work",
            "--id",
            "t-rootless",
            "--project",
            "ROOTLESS",
            "--json",
        ],
    );

    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    let attach = fixture.root.join("attach-rootless");
    fs::create_dir_all(&attach).unwrap();
    fixture.ok_json(
        &attach,
        &["workspace", "attach", "--to", "ROOTLESS", "--json"],
    );

    let attached = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    let attached_board = attached
        .as_array()
        .unwrap()
        .iter()
        .find(|board| board["name"] == "ROOTLESS")
        .expect("the attached board must be listed");
    assert_eq!(
        attached_board["workspaceRoots"].as_array().unwrap().len(),
        1
    );

    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );

    let restored = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    let restored_board = restored
        .as_array()
        .unwrap()
        .iter()
        .find(|board| board["name"] == "ROOTLESS")
        .expect("the restored board must be listed");
    assert!(
        restored_board["workspaceRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "show",
                "t-rootless",
                "--project",
                "ROOTLESS",
                "--json"
            ]
        )["id"],
        "t-rootless"
    );
}

#[test]
fn compiled_binary_refuses_a_snapshot_changed_after_its_manifest() {
    let fixture = Fixture::new("manifest-tamper");
    fixture.ok_json(&fixture.main, &["init", "--name", "Manifest", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "live work", "--id", "t-live", "--json"],
    );
    let backup = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    let manifest = Path::new(backup["manifest"].as_str().unwrap());
    assert!(manifest.is_file());
    assert_eq!(backup["manifestSha256"].as_str().unwrap().len(), 64);
    let manifested: Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
    assert!(
        manifested["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["audit"]["head"].as_str().unwrap().len() == 64)
    );

    let copied_board = backup["boards"][0].as_str().unwrap();
    Connection::open(copied_board)
        .unwrap()
        .execute("UPDATE tasks SET title='substituted' WHERE id='t-live'", [])
        .unwrap();
    let restore = fixture.run(
        &fixture.main,
        &[
            "restore",
            "--from",
            backup["directory"].as_str().unwrap(),
            "--force",
            "--json",
        ],
    );
    assert!(
        !restore.status.success(),
        "a substituted snapshot was restored"
    );
    assert!(
        String::from_utf8_lossy(&restore.stderr).contains("SHA-256 differs"),
        "restore did not name the failed manifest check: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-live", "--json"])["title"],
        "live work",
        "a refused restore changed live state"
    );
}

#[test]
fn compiled_binary_detects_rollback_past_a_retained_manifest_anchor() {
    let fixture = Fixture::new("manifest-rollback");
    fixture.ok_json(&fixture.main, &["init", "--name", "Rollback", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "first", "--id", "t-first", "--json"],
    );
    let old = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "anchored", "--id", "t-anchor", "--json"],
    );
    let anchor = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    let live_board =
        fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
            .as_str()
            .unwrap()
            .to_owned();

    fs::copy(old["boards"][0].as_str().unwrap(), &live_board).unwrap();
    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        true,
        "an intact older chain should be internally valid"
    );
    let anchored = fixture.run(
        &fixture.main,
        &[
            "audit",
            "verify",
            "--against",
            anchor["manifest"].as_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        !anchored.status.success(),
        "rollback passed the retained anchor"
    );
    let receipt: Value = serde_json::from_slice(&anchored.stdout).unwrap();
    assert!(
        receipt["boards"][0]["audit"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("before anchored sequence")),
        "rollback receipt did not name the missing anchored history: {receipt}"
    );
}

#[test]
fn compiled_binary_prunes_only_the_backups_directory_it_manages() {
    let fixture = Fixture::new("prune");
    fixture.ok_json(&fixture.main, &["init", "--name", "Prune", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "work", "--id", "t-1", "--json"],
    );
    for _ in 0..3 {
        fixture.ok_json(&fixture.main, &["backup", "--json"]);
    }
    let kept = fixture.ok_json(&fixture.main, &["backup", "--keep", "2", "--json"]);
    assert_eq!(
        kept["pruned"].as_array().unwrap().len(),
        2,
        "4 snapshots, keep 2"
    );
    let remaining = fs::read_dir(fixture.data.join("backups")).unwrap().count();
    assert_eq!(remaining, 2);

    // Deleting from a directory the operator chose is the same overreach as
    // re-permissioning one, so --keep refuses outside the managed root.
    let mine = fixture.root.join("mine/snap");
    let refused = fixture.run(
        &fixture.main,
        &[
            "backup",
            "--output",
            mine.to_str().unwrap(),
            "--keep",
            "1",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("only prunes the managed"));
    assert!(
        !fixture
            .run(&fixture.main, &["backup", "--keep", "0", "--json"])
            .status
            .success()
    );
}

#[test]
fn compiled_binary_locks_the_data_root_against_a_concurrent_restore() {
    let fixture = Fixture::new("lock");
    fixture.ok_json(&fixture.main, &["init", "--name", "Locked", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "work", "--id", "t-1", "--json"],
    );
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();
    let lock_path = fixture.data.join(".lock");
    // Created here rather than assumed: the test must fail on the behaviour it
    // asserts, not on the absence of a file that is an implementation detail.
    let hold = || {
        fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap()
    };

    // A live board command holds the data root shared. `restore --force` used
    // to document "stop every kanban process first" and enforce nothing, so it
    // would rename database files out from under an open SQLite connection.
    {
        let held = hold();
        held.lock_shared().unwrap();
        let refused = fixture.run(
            &fixture.main,
            &["restore", "--from", &snapshot, "--force", "--json"],
        );
        assert!(
            !refused.status.success(),
            "restore replaced the data root while another process held it open"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("another kanban process"),
            "stderr: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    // Released, the identical restore succeeds — so the refusal above was the
    // lock, not something else about the snapshot.
    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );

    // Shared holders never exclude each other: the lock must not serialize the
    // agents it exists to protect.
    {
        let held = hold();
        held.lock_shared().unwrap();
        let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
        assert_eq!(listed[0]["id"], "t-1");
    }

    // While a restore holds it exclusively, a board command waits out its
    // window and then says so, rather than reading a half-replaced root.
    {
        let held = hold();
        held.lock().unwrap();
        let refused = fixture.run(&fixture.main, &["task", "list", "--json"]);
        assert!(
            !refused.status.success(),
            "a board command read the data root mid-restore"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("restore is replacing"),
            "stderr: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
}

#[test]
fn compiled_binary_locks_only_the_data_root_it_was_asked_to_touch() {
    let fixture = Fixture::new("lock-scope");
    let outside = fixture.root.join("outside.db");

    // A board named straight by path, living elsewhere, is not data-root
    // state. Locking it anyway would create a private data root as a side
    // effect of a command that never wanted one — the same overreach as
    // re-permissioning a directory we do not own.
    fixture.ok_json(
        &fixture.main,
        &[
            "--db",
            outside.to_str().unwrap(),
            "task",
            "add",
            "standalone",
            "--json",
        ],
    );
    assert!(
        !fixture.data.exists(),
        "an external --db board created a data root it never needed"
    );

    // A board that does live under the data root is covered, even when the
    // path spells the root through a traversal.
    fixture.ok_json(&fixture.main, &["init", "--name", "Scoped", "--json"]);
    let inside = fixture.data.join("boards/../boards/inside.db");
    let held = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fixture.data.join(".lock"))
        .unwrap();
    held.lock().unwrap();
    let refused = fixture.run(
        &fixture.main,
        &["--db", inside.to_str().unwrap(), "task", "list", "--json"],
    );
    assert!(
        !refused.status.success(),
        "a board inside the data root escaped the lock through .."
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("restore is replacing"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn compiled_binary_bounds_priority_without_rewriting_history() {
    let fixture = Fixture::new("priority");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Priority", "--json"]);
    let board = project["boardPath"].as_str().unwrap().to_owned();

    // The band is the one the ledger already uses: 0 is the routing tier
    // driver-only work sorts on, 9 the least urgent.
    for good in ["0", "3", "9"] {
        let task = fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "in band",
                "--id",
                &format!("t-{good}"),
                "--priority",
                good,
                "--json",
            ],
        );
        let expected = match good {
            "0" => "P0",
            "3" => "P1",
            _ => "P2",
        };
        assert_eq!(task["priorityLevel"], expected);
    }

    let routine = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "routine default",
            "--id",
            "t-default",
            "--json",
        ],
    );
    assert_eq!(routine["priority"], 6);
    assert_eq!(routine["priorityLevel"], "P2");

    for (symbol, anchor) in [("P0", 0), ("p1", 3), ("P2", 6)] {
        let task = fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "symbolic",
                "--id",
                &format!("t-{symbol}"),
                "--priority",
                symbol,
                "--json",
            ],
        );
        assert_eq!(task["priority"], anchor);
    }

    let attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "interrupt",
            "--as",
            "agent",
            "--priority",
            "P0",
            "--json",
        ],
    );
    assert_eq!(attention["priority"], 0);
    assert_eq!(attention["priorityLevel"], "P0");

    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "agent",
            "--summary",
            "resume this",
            "--intent",
            "finish it",
            "--next-action",
            "claim work",
            "--priority",
            "P1",
            "--json",
        ],
    );
    assert_eq!(handoff["priority"], 3);
    assert_eq!(handoff["priorityLevel"], "P1");

    // `claim --next` hands work out in ascending priority, so an unbounded
    // field let a negative value hold the head of every queue permanently:
    // nothing can outrank the bottom of an i64.
    for bad in ["-1", "10", "-9223372036854775808", "9223372036854775807"] {
        let refused = fixture.run(
            &fixture.main,
            &["task", "add", "out of band", "--priority", bad, "--json"],
        );
        assert!(
            !refused.status.success(),
            "task add --priority {bad} was accepted"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("most urgent"),
            "stderr: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    // A value that is not a number at all names the flag it came from:
    // "invalid digit found in string" does not say which of --priority,
    // --stale-minutes or --lease-minutes was wrong, and an agent reading
    // stderr has nothing to act on.
    for (flag, value) in [("--priority", "abc"), ("--stale-minutes", "soon")] {
        let refused = fixture.run(
            &fixture.main,
            &[
                "task", "update", "t-3", "--as", "geo", flag, value, "--json",
            ],
        );
        assert!(!refused.status.success(), "{flag} {value} was accepted");
        let message = String::from_utf8_lossy(&refused.stderr);
        assert!(message.contains(flag), "stderr must name {flag}: {message}");
        assert!(
            message.contains(value),
            "stderr must quote the value: {message}"
        );
    }

    // The same band applies on update, and a refused update changes nothing.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-3",
            "--as",
            "geo",
            "--priority",
            "-1",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-3", "--json"])["priority"],
        3,
        "a refused update must leave the recorded priority alone"
    );

    // A row that already holds an out-of-band priority — an atmux import, or a
    // board written before this rule — keeps it. Validating what a caller
    // types is not a licence to rewrite recorded history to match.
    let database = Connection::open(&board).unwrap();
    database
        .execute("UPDATE tasks SET priority=99 WHERE id='t-3'", [])
        .unwrap();
    drop(database);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-3", "--json"])["priority"],
        99,
        "an existing out-of-band priority must still be readable"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-3", "--json"])["priorityLevel"],
        Value::Null
    );
    let updated = fixture.ok_json(
        &fixture.main,
        &[
            "task", "update", "t-3", "--as", "geo", "--title", "renamed", "--json",
        ],
    );
    assert_eq!(updated["title"], "renamed");
    assert_eq!(
        updated["priority"], 99,
        "an update that never mentioned priority silently normalized it"
    );
}

#[test]
fn compiled_binary_waits_out_a_long_write_lock_instead_of_dropping_the_write() {
    let fixture = Fixture::new("busy");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Busy", "--json"]);
    let board = project["boardPath"].as_str().unwrap().to_owned();

    // Hold the write lock past the ceiling the binary used to give up at. A
    // swarm write that loses the race has to queue, not fail: an agent reads
    // an exit status and moves on, so a dropped write is lost work that
    // nothing downstream will notice is missing.
    let holder = std::thread::spawn(move || {
        let connection = Connection::open(&board).unwrap();
        connection
            .busy_handler(Some(|_| {
                std::thread::sleep(Duration::from_millis(50));
                true
            }))
            .unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        std::thread::sleep(Duration::from_millis(7_500));
        connection.execute_batch("COMMIT").unwrap();
    });
    std::thread::sleep(Duration::from_millis(250));

    let started = Instant::now();
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "queued behind a long writer",
            "--id",
            "t-queued",
            "--json",
        ],
    );
    let waited = started.elapsed();
    holder.join().unwrap();

    assert!(
        waited >= Duration::from_secs(5),
        "the write never queued behind the lock, so this proves nothing ({waited:?})"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-queued", "--json"])["title"],
        "queued behind a long writer"
    );
}

#[test]
fn compiled_binary_previews_an_import_and_will_not_void_a_live_lease_quietly() {
    let fixture = Fixture::new("import-safety");
    fixture.ok_json(&fixture.main, &["init", "--name", "Reconcile", "--json"]);
    let export = fixture.root.join("export.json");
    let write_export = |id: &str, title: &str| {
        fs::write(
            &export,
            serde_json::to_vec(&json!({
                "epics": [],
                "stories": [],
                "tasks": [{"id":id,"subject":title,"status":"todo"}]
            }))
            .unwrap(),
        )
        .unwrap();
    };
    let import = |extra: &[&str]| {
        let mut args = vec![
            "import",
            "atmux-json",
            export.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ];
        args.extend_from_slice(extra);
        fixture.run(&fixture.main, &args)
    };
    let title = || {
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let seizures = || {
        fixture
            .ok_json(
                &fixture.main,
                &["events", "--kind", "lease_seized", "--json"],
            )
            .as_array()
            .unwrap()
            .len()
    };

    write_export("t-1", "original");
    assert!(import(&[]).status.success());
    assert_eq!(title(), "original");

    // A dry run reports what it would create and leaves the board alone.
    write_export("t-2", "previewed creation");
    let preview: Value =
        serde_json::from_slice(&import(&["--dry-run"]).stdout).expect("dry run must still report");
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["created"], 1);
    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-2", "--json"])
            .status
            .success(),
        "a dry run wrote to the board"
    );

    write_export("t-1", "previewed");

    // Claimed by a live agent, `--reconcile` used to delete the claim row on
    // its way past — the same silent lease void that task move/remove refuse.
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let refused = import(&["--reconcile"]);
    assert!(
        !refused.status.success(),
        "reconcile voided a live lease without being asked twice"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("live lease"), "stderr: {message}");
    assert!(
        message.contains("held by worker"),
        "the refusal must name the holder: {message}"
    );
    assert_eq!(title(), "original", "a refused import wrote anyway");

    // Forced, but previewed: it says which leases it would seize and still
    // takes none of them.
    let forecast: Value =
        serde_json::from_slice(&import(&["--reconcile", "--force", "--dry-run"]).stdout)
            .expect("forced dry run must report");
    assert_eq!(forecast["seizedLeases"], json!(["t-1"]));
    assert_eq!(title(), "original");
    assert_eq!(seizures(), 0, "a dry run recorded a seizure it never made");

    // Forced for real: the overwrite lands and the seizure is on the record.
    let applied: Value =
        serde_json::from_slice(&import(&["--reconcile", "--force"]).stdout).unwrap();
    assert_eq!(applied["dryRun"], false);
    assert_eq!(applied["seizedLeases"], json!(["t-1"]));
    assert_eq!(title(), "previewed");
    assert_eq!(seizures(), 1, "a forced seizure left no audit trail");
}

#[test]
fn compiled_binary_reports_a_missing_board_instead_of_replacing_it() {
    let fixture = Fixture::new("missing-board");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Gone", "--json"]);
    let board = PathBuf::from(project["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "real work", "--id", "t-1", "--json"],
    );
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // A partial restore, a stray rm, a half-copied data root.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", board.display()));
    }

    // Opening a board creates it, so `doctor` used to recreate the very file
    // it was asked to inspect and then certify the empty result healthy — the
    // health check destroying the evidence that anything was wrong.
    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !checked.status.success(),
        "doctor called a board with no file healthy"
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["projects"][0]["present"], false);
    assert!(!board.is_file(), "doctor recreated the board it inspected");

    // A command that does work on that board refuses, and names both ways out.
    let refused = fixture.run(&fixture.main, &["task", "list", "--json"]);
    assert!(
        !refused.status.success(),
        "a work command silently stood an empty board up in place of the lost one"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("registered but missing"),
        "stderr: {message}"
    );
    assert!(message.contains("kanban restore"), "stderr: {message}");
    assert!(
        !board.is_file(),
        "a refused command still created the board"
    );

    // A survey command snapshots what remains and says what it could not take.
    let partial = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    assert_eq!(partial["boards"].as_array().unwrap().len(), 0);
    assert_eq!(
        partial["missingBoards"][0],
        board.to_string_lossy().as_ref()
    );

    // And the documented recovery actually recovers.
    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"],
        "real work"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["healthy"],
        true
    );
}

#[test]
fn compiled_binary_doctor_looks_past_the_btree() {
    let fixture = Fixture::new("doctor-depth");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Deep", "--json"]);
    let board = project["boardPath"].as_str().unwrap().to_owned();
    for id in ["t-ok", "t-future"] {
        fixture.ok_json(&fixture.main, &["task", "add", id, "--id", id, "--json"]);
    }
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["healthy"],
        true
    );

    // `integrity_check` validates the b-tree and says nothing about what the
    // rows mean, so both of these leave a structurally perfect board.
    let database = Connection::open(&board).unwrap();
    database
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             INSERT INTO task_notes(task_id,author,kind,body,created_at)
               VALUES('t-vanished','ghost','progress','orphan',1);",
        )
        .unwrap();
    let horizon = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 86_400_000;
    database
        .execute(
            "UPDATE tasks SET created_at=? WHERE id='t-future'",
            [horizon],
        )
        .unwrap();
    drop(database);

    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(!checked.status.success());
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    let board_report = &report["projects"][0];
    assert_eq!(
        board_report["integrity"],
        json!(["ok"]),
        "the b-tree really is intact; that is the point"
    );
    assert_eq!(report["healthy"], false);
    assert!(
        board_report["orphanedRows"][0]
            .as_str()
            .unwrap()
            .contains("task_notes"),
        "a note on a task that does not exist went unreported: {board_report}"
    );
    // A task stamped in the future sorts ahead of real work, and on a claim it
    // holds a lease no sweep will ever retire.
    assert_eq!(board_report["futureDatedTasks"], json!(["t-future"]));
}

#[test]
fn compiled_binary_refuses_arguments_it_would_have_dropped() {
    let fixture = Fixture::new("positionals");
    fixture.ok_json(&fixture.main, &["init", "--name", "Args", "--json"]);

    // Forgetting to quote is the likeliest slip at a shell, and it used to
    // produce a durable record that was wrong with nothing to notice it by:
    // this recorded the title `Fix` and reported success.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task", "add", "Fix", "the", "broken", "parser", "--id", "t-1", "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "an unquoted title was accepted and silently cut to its first word"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("unexpected arguments"),
        "stderr: {message}"
    );
    assert!(
        message.contains("after `task add Fix`"),
        "the error must show what it thought the command was: {message}"
    );
    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-1", "--json"])
            .status
            .success(),
        "a refused add wrote a task anyway"
    );

    // Quoted, the whole title lands.
    let added = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Fix the broken parser",
            "--id",
            "t-1",
            "--json",
        ],
    );
    assert_eq!(added["title"], "Fix the broken parser");

    // The same slip on a note body recorded `the`.
    let refused = fixture.run(
        &fixture.main,
        &[
            "note", "t-1", "the", "build", "is", "red", "--as", "ci", "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "an unquoted note body was accepted"
    );
    fixture.ok_json(
        &fixture.main,
        &["note", "t-1", "the build is red", "--as", "ci", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["notes"][0]["body"],
        "the build is red"
    );

    // An extra id is refused too — it was never going to be read.
    let refused = fixture.run(&fixture.main, &["task", "show", "t-1", "t-2", "--json"]);
    assert!(!refused.status.success(), "a second task id was ignored");

    // And every arity the surface actually uses still parses: no positional,
    // one, and the two `task move` takes.
    fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-1", "todo", "--as", "geo", "--json"],
    );
}

#[test]
fn compiled_binary_refuses_two_requests_dressed_as_one() {
    let fixture = Fixture::new("ambiguous");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "add",
            "scheduler",
            "--description",
            "queue selection",
            "--as",
            "test",
            "--json",
        ],
    );
    let other = fixture.root.join("other");
    fs::create_dir_all(&other).unwrap();
    fixture.ok_json(&other, &["init", "--name", "Beta", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "head of the queue",
            "--id",
            "t-first",
            "--priority",
            "1",
            "--tag",
            "scheduler",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "the one asked for",
            "--id",
            "t-named",
            "--priority",
            "9",
            "--json",
        ],
    );

    // `claim t-named --next` used to drop the id and hand back t-first, so an
    // agent that asked for a named task held a lease on a different one.
    let refused = fixture.run(
        &fixture.main,
        &["claim", "t-named", "--next", "--as", "worker", "--json"],
    );
    assert!(
        !refused.status.success(),
        "claim ignored the task id it was given and picked a different task"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("not both"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    // Either request alone still means what it says.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "t-named", "--as", "worker", "--json"]
        )["taskID"],
        "t-named"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "other", "--json"]
        )["taskID"],
        "t-first"
    );

    // A repeated single-valued flag kept the last occurrence, so a wrapper
    // appending a default --project silently retargeted the board.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "whose board?",
            "--id",
            "t-stray",
            "--project",
            "Alpha",
            "--project",
            "Beta",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "a repeated --project picked one board without saying which"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--project (Alpha, Beta)"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    for project in ["Alpha", "Beta"] {
        assert!(
            !fixture
                .run(
                    &fixture.main,
                    &["task", "show", "t-stray", "--project", project, "--json"]
                )
                .status
                .success(),
            "the refused task landed on {project} anyway"
        );
    }

    // List-valued flags are exactly what repeating is for, and still repeat.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "with deps",
            "--id",
            "t-deps",
            "--depends-on",
            "t-first",
            "--depends-on",
            "t-named",
            "--json",
        ],
    );
    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--with-relations", "--json"],
    );
    let deps = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "t-deps")
        .unwrap();
    assert_eq!(deps["dependencies"], json!(["t-first", "t-named"]));
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-deps", "--json"]);
    let dependency = shown["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "t-first")
        .unwrap();
    assert_eq!(dependency["tags"], json!(["scheduler"]));
}

#[test]
fn compiled_binary_never_shortens_context_without_saying_so() {
    let fixture = Fixture::new("context-budget");
    fixture.ok_json(&fixture.main, &["init", "--name", "Budget", "--json"]);
    let long = "x".repeat(600);
    let title = "T".repeat(300);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", &title, "--id", "t-1", "--json"],
    );
    let lease =
        fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"])["leaseToken"]
            .as_str()
            .unwrap()
            .to_owned();
    for command in ["checkpoint", "handoff"] {
        let mut args = vec![command];
        if command == "handoff" {
            args.push("create");
        }
        args.extend_from_slice(&[
            "t-1",
            "--lease",
            &lease,
            "--as",
            "worker",
            "--summary",
            &long,
            "--intent",
            &long,
            "--next-action",
            &long,
            "--json",
        ]);
        fixture.ok_json(&fixture.main, &args);
    }
    for index in 0..5 {
        fixture.ok_json(
            &fixture.main,
            &[
                "note",
                "t-1",
                &format!("note {index} {long}"),
                "--as",
                "worker",
                "--json",
            ],
        );
    }

    // Every render is stamped, so two runs differ on that line alone.
    let render = |budget: &str| -> String {
        let output = fixture.run(&fixture.main, &["context", "t-1", "--max-chars", budget]);
        assert!(
            output.status.success(),
            "context --max-chars {budget} failed"
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                if line.starts_with("Generated: ") {
                    "Generated: N"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let complete = render("999999");

    // The compact rendering used to append its marker and hope: past the
    // smallest budgets the body already overran, `take_chars` cut from the
    // end, and the marker was the first thing lost — precisely when the
    // reader most needed telling that the ancestry, the dependencies, every
    // earlier checkpoint and every note had gone.
    for budget in [
        "1000", "1001", "1100", "1200", "1500", "3000", "5000", "8000", "9000", "20000",
    ] {
        let text = render(budget);
        let length = text.chars().count();
        assert!(
            length <= budget.parse::<usize>().unwrap(),
            "--max-chars {budget} produced {length} characters"
        );
        if text != complete {
            assert!(
                text.contains("[context compacted") || text.contains("[older history omitted]"),
                "--max-chars {budget} dropped history and said nothing (ends: {:?})",
                &text.chars().rev().take(60).collect::<String>()
            );
        }
    }

    // --max-chars bounds the rendered text and never did anything here, so
    // accepting it handed an unbounded packet to a caller asking for a bound.
    let refused = fixture.run(
        &fixture.main,
        &["context", "t-1", "--json", "--max-chars", "1000"],
    );
    assert!(
        !refused.status.success(),
        "--json accepted --max-chars and ignored it"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("returns the whole packet"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    // Each on its own still works.
    fixture.ok_json(&fixture.main, &["context", "t-1", "--json"]);
    assert!(!render("2000").is_empty());
}

#[test]
fn a_lease_is_only_ever_granted_on_a_task() {
    let fixture = Fixture::new("claimable-type");
    fixture.ok_json(&fixture.main, &["init", "--name", "TYPES", "--json"]);

    // Both containers sort ahead of the real work on every tiebreak --next
    // uses: lower priority number first, then created_at.
    for (id, kind) in [("e-top", "epic"), ("s-top", "story")] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "Container",
                "--id",
                id,
                "--type",
                kind,
                "--priority",
                "0",
                "--json",
            ],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "The real work",
            "--id",
            "t-work",
            "--priority",
            "9",
            "--json",
        ],
    );

    // --next skips a container instead of failing on it: a row that was never
    // claimable must not stall the queue behind it.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "worker", "--json"]
        )["taskID"],
        "t-work"
    );

    // Naming one explicitly is refused, and the refusal says what to do instead.
    let epic = fixture.run(
        &fixture.main,
        &["claim", "e-top", "--as", "worker", "--json"],
    );
    assert!(!epic.status.success(), "an epic was handed out as work");
    let epic_error = String::from_utf8_lossy(&epic.stderr).to_string();
    assert!(
        epic_error.contains("only a task is claimable"),
        "{epic_error}"
    );
    assert!(epic_error.contains("children"), "{epic_error}");

    let story = fixture.run(
        &fixture.main,
        &["claim", "s-top", "--as", "worker", "--json"],
    );
    assert!(!story.status.success(), "a story was handed out as work");
    let story_error = String::from_utf8_lossy(&story.stderr).to_string();
    assert!(
        story_error.contains("story advance"),
        "a story refusal must point at its gate: {story_error}"
    );

    // The refusal left both rows exactly as they were — no assignee written,
    // no status flipped, which is what made the ledger contradict itself.
    for id in ["e-top", "s-top"] {
        let shown = fixture.ok_json(&fixture.main, &["task", "show", id, "--json"]);
        assert_eq!(shown["status"], "todo", "{id} was moved by a refused claim");
        assert!(shown["assignee"].is_null(), "{id} was assigned anyway");
        assert!(shown["claim"].is_null(), "{id} holds a lease");
    }
}

#[test]
fn a_handoff_addressed_to_a_container_cannot_be_accepted() {
    // A board written before this rule — or imported from atmux — can still
    // carry a pending handoff on a row that is not a task. Accepting it would
    // mint exactly the lease `claim` now refuses, so the guard sits on both
    // lease-minting paths rather than on the verb the operator happened to use.
    let fixture = Fixture::new("handoff-container");
    fixture.ok_json(&fixture.main, &["init", "--name", "LEGACY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Legacy row", "--id", "t-legacy", "--json"],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-legacy", "--as", "outgoing", "--json"],
    );
    let token = claim["leaseToken"].as_str().unwrap().to_owned();
    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-legacy",
            "--lease",
            &token,
            "--as",
            "outgoing",
            "--summary",
            "Ran out of context",
            "--intent",
            "Continue the work",
            "--next-action",
            "Pick up where I stopped",
            "--json",
        ],
    );
    let handoff_id = handoff["id"].as_str().unwrap().to_owned();

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute("UPDATE tasks SET type='story' WHERE id='t-legacy'", [])
        .unwrap();

    let accepted = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &handoff_id,
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert!(
        !accepted.status.success(),
        "a handoff on a container minted a lease"
    );
    assert!(
        String::from_utf8_lossy(&accepted.stderr).contains("only a task is claimable"),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[test]
fn a_story_status_is_not_writable_around_its_gate() {
    let fixture = Fixture::new("story-projection");
    fixture.ok_json(&fixture.main, &["init", "--name", "GATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Story", "--id", "s-1", "--type", "story", "--json",
        ],
    );

    // Marking it done by hand would stamp completed_at while the gate never
    // took a signoff, dispatched a merge task, or flipped a parent epic.
    let direct = fixture.run(
        &fixture.main,
        &["task", "move", "s-1", "done", "--as", "geo", "--json"],
    );
    assert!(
        !direct.status.success(),
        "a story was completed around its gate"
    );
    let error = String::from_utf8_lossy(&direct.stderr).to_string();
    assert!(error.contains("story advance"), "{error}");
    assert!(
        error.contains("planning"),
        "the refusal must say where the gate actually is: {error}"
    );

    let untouched = fixture.ok_json(&fixture.main, &["task", "show", "s-1", "--json"]);
    // `task add` defaults a story to todo regardless of type, so this is the
    // status the row already held — the point is that the refused move did not
    // change it, and did not stamp completedAt.
    assert_eq!(untouched["status"], "todo", "the refused move still landed");
    assert!(
        untouched["completedAt"].is_null(),
        "completedAt was stamped"
    );

    // The gate itself keeps writing the same column, and the projection holds.
    fixture.ok_json(
        &fixture.main,
        &["story", "advance", "s-1", "--as", "geo", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "s-1", "--json"])["status"],
        "todo",
        "ready must project to todo"
    );

    // blocked is outside the gate's vocabulary, so it stays directly writable —
    // refusing it would remove the only way to say it.
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "s-1", "blocked", "--as", "geo", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "s-1", "--json"])["status"],
        "blocked"
    );

    // --force overwrites the projection and says so in the ledger.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "s-1", "done", "--as", "geo", "--force", "--json",
        ],
    );
    let events = fixture.ok_json(&fixture.main, &["events", "--task", "s-1", "--json"]);
    let bypassed = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["payload"]["gateBypassed"] == json!(true))
        .count();
    assert_eq!(bypassed, 1, "the forced override was not recorded once");

    // A plain task is untouched by any of this.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-1", "done", "--as", "geo", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["status"],
        "done"
    );
}

#[test]
fn a_task_cannot_be_made_to_contain_work() {
    let fixture = Fixture::new("nesting");
    fixture.ok_json(&fixture.main, &["init", "--name", "TREE", "--json"]);
    for (id, kind) in [("e-1", "epic"), ("s-1", "story"), ("t-1", "task")] {
        fixture.ok_json(
            &fixture.main,
            &["task", "add", "Row", "--id", id, "--type", kind, "--json"],
        );
    }

    // The three shapes the ledger is actually used in.
    for (id, kind, parent) in [
        ("s-ok", "story", "e-1"),
        ("t-ok-epic", "task", "e-1"),
        ("t-ok-story", "task", "s-1"),
    ] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task", "add", "Row", "--id", id, "--type", kind, "--parent", parent, "--json",
            ],
        );
    }

    // A story under a task is the costly one: `story advance` flips a parent
    // only when it is an epic, so the mis-nested story would silently never
    // flip anything and nothing would ever say so.
    let inverted = fixture.run(
        &fixture.main,
        &[
            "task", "add", "Row", "--id", "s-bad", "--type", "story", "--parent", "t-1", "--json",
        ],
    );
    assert!(!inverted.status.success(), "a story nested under a task");
    let error = String::from_utf8_lossy(&inverted.stderr).to_string();
    assert!(error.contains("story") && error.contains("task"), "{error}");
    assert!(error.contains("contains nothing"), "{error}");

    // An epic nests under an epic: a plan is an epic, so a programme plan holds
    // its sub-plans. This was refused until plans had somewhere to live.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Sub-plan", "--id", "e-sub", "--type", "epic", "--parent", "e-1",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "e-sub", "--json"])["parentID"],
        "e-1"
    );
    // A story in a story still has no meaning, and neither does a container
    // inside something narrower than itself.
    for (id, kind, parent) in [("s-bad", "story", "s-1"), ("e-bad2", "epic", "s-1")] {
        let refused = fixture.run(
            &fixture.main,
            &[
                "task", "add", "Row", "--id", id, "--type", kind, "--parent", parent, "--json",
            ],
        );
        assert!(!refused.status.success(), "{kind} nested under a story");
    }

    let epic_under_task = fixture.run(
        &fixture.main,
        &[
            "task", "add", "Row", "--id", "e-bad", "--type", "epic", "--parent", "t-1", "--json",
        ],
    );
    assert!(
        !epic_under_task.status.success(),
        "an epic nested under a task"
    );

    // Re-parenting is the same rule: it is the other way to write the field.
    let reparent = fixture.run(
        &fixture.main,
        &[
            "task", "update", "s-ok", "--as", "geo", "--parent", "t-1", "--json",
        ],
    );
    assert!(
        !reparent.status.success(),
        "a story was re-parented under a task"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "s-ok", "--json"])["parentID"],
        "e-1",
        "the refused re-parent still landed"
    );

    // Nothing the refusals touched was created.
    for ghost in ["s-bad", "e-bad"] {
        assert!(
            !fixture
                .run(&fixture.main, &["task", "show", ghost, "--json"])
                .status
                .success(),
            "{ghost} was written despite the refusal"
        );
    }
}

/// The bounded fresh-turn protocol of `docs/integrating-orch.md`, driven end to
/// end against the compiled binary, including the restart it exists to survive.
#[test]
fn the_long_horizon_turn_protocol_survives_a_runner_restart() {
    let fixture = Fixture::new("orch-protocol");
    fixture.ok_json(&fixture.main, &["init", "--name", "ORCH", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Long horizon work", "--id", "t-lh", "--json"],
    );

    // (1)-(2) The runner claims the task under its run identity.
    let first = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-lh",
            "--as",
            "orch/run-1",
            "--session",
            "session-1",
            "--json",
        ],
    );
    let stale_token = first["leaseToken"].as_str().unwrap().to_owned();

    // (3) Context is fetched before each model invocation, in both renderings.
    let packet = fixture.ok_json(&fixture.main, &["context", "t-lh", "--json"]);
    assert_eq!(packet["task"]["id"], "t-lh");
    let rendered = fixture.run(&fixture.main, &["context", "t-lh"]);
    assert!(rendered.status.success());

    // The lease token authorizes writes and must never reach a prompt. No read
    // surface may carry it, whichever rendering the runner feeds the model.
    for surface in [
        vec!["context", "t-lh"],
        vec!["context", "t-lh", "--json"],
        vec!["task", "show", "t-lh", "--json"],
        vec!["events", "--task", "t-lh", "--json"],
        vec!["dashboard", "--json"],
    ] {
        let out = fixture.run(&fixture.main, &surface);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !text.contains(&stale_token),
            "{surface:?} leaked the lease token"
        );
    }

    // (5)-(6) The runner writes the envelope itself. `continue` keeps the lease.
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &stale_token,
            "--as",
            "orch/run-1",
            "--state",
            "continue",
            "--summary",
            "turn one changed the parser",
            "--intent",
            "the next turn is the suite",
            "--next-action",
            "run cargo test",
            "--validation",
            "cargo build passed",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-lh", "--json"])["status"],
        "in_progress",
        "a continue checkpoint must keep the task running"
    );

    // The runner now dies. Its lease is still live, so nobody else may take the
    // task -- a crash must not hand work to a second runner mid-turn.
    let contested = fixture.run(
        &fixture.main,
        &["claim", "t-lh", "--as", "orch/run-2", "--json"],
    );
    assert!(!contested.status.success(), "a live lease was taken over");

    // Time passes and the lease lapses. Expiry is what makes the work
    // reclaimable, so drive it the way the sweep does.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE task_claims SET expires_at=1 WHERE task_id='t-lh'",
            [],
        )
        .unwrap();

    // A restarted runner reacquires, and resumes from the newest durable
    // checkpoint rather than from any memory of the old session.
    let second = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-lh",
            "--as",
            "orch/run-2",
            "--session",
            "session-2",
            "--json",
        ],
    );
    let live_token = second["leaseToken"].as_str().unwrap().to_owned();
    assert_ne!(live_token, stale_token, "a restart reused the dead lease");

    let resumed = fixture.ok_json(&fixture.main, &["context", "t-lh", "--json"]);
    let newest = resumed["checkpoints"].as_array().unwrap().last().unwrap();
    assert_eq!(newest["nextAction"], "run cargo test");
    assert_eq!(newest["state"], "continue");

    // The hazard the protocol names: the crashed runner wakes up holding a
    // token from before the handover. It must be refused, and told the truth --
    // "no active lease" would send it to claim a task somebody else is running.
    let zombie = fixture.run(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &stale_token,
            "--as",
            "orch/run-1",
            "--state",
            "done",
            "--summary",
            "zombie write",
            "--intent",
            "stale",
            "--next-action",
            "stale",
            "--json",
        ],
    );
    assert!(!zombie.status.success(), "a superseded lease still wrote");
    let refusal = String::from_utf8_lossy(&zombie.stderr).to_string();
    assert!(
        refusal.contains("orch/run-2"),
        "the refusal must name the live holder: {refusal}"
    );
    assert!(
        refusal.contains("superseded"),
        "the refusal must say the lease was replaced, not that none exists: {refusal}"
    );
    assert!(
        !refusal.contains(&live_token),
        "the refusal handed out the live lease token"
    );

    // The live runner is untouched by the zombie, and closes the task. `done`
    // atomically releases the lease in the same transaction as the checkpoint.
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &live_token,
            "--as",
            "orch/run-2",
            "--state",
            "done",
            "--summary",
            "the suite is green",
            "--intent",
            "nothing further is needed",
            "--next-action",
            "none",
            "--json",
        ],
    );
    let closed = fixture.ok_json(&fixture.main, &["task", "show", "t-lh", "--json"]);
    assert_eq!(closed["status"], "done");
    assert!(
        !closed["completedAt"].is_null(),
        "done left no completion stamp"
    );
    assert!(
        closed["claim"].is_null(),
        "done must release the lease in the same transaction"
    );

    // And a lease released that way cannot be used again by anyone.
    let after_release = fixture.run(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &live_token,
            "--as",
            "orch/run-2",
            "--summary",
            "after the close",
            "--intent",
            "stale",
            "--next-action",
            "stale",
            "--json",
        ],
    );
    assert!(
        !after_release.status.success(),
        "a released lease still wrote"
    );
    assert!(
        String::from_utf8_lossy(&after_release.stderr).contains("no active lease"),
        "a genuinely unheld task must say so"
    );
}

/// Step 6 of `docs/integrating-atmux.md` requires a parity receipt against real
/// private state before a cutover. This is that receipt, green and red.
#[test]
fn a_parity_receipt_proves_the_board_holds_what_the_source_held() {
    let fixture = Fixture::new("parity");
    let source = fixture.root.join("atmux-state.db");
    fs::create_dir_all(&fixture.root).unwrap();
    let legacy = Connection::open(&source).unwrap();
    legacy
        .execute_batch(
            r#"
            CREATE TABLE epics(id TEXT,title TEXT,status TEXT,created_at INTEGER,completed_at INTEGER,depends_on TEXT,stories TEXT,body TEXT,driver_ref TEXT,is_ready INTEGER,spawned_at INTEGER,extra TEXT);
            CREATE TABLE stories(id TEXT,epic TEXT,title TEXT,status TEXT,created_at INTEGER,completed_at INTEGER,advanced_at INTEGER,body TEXT,acceptance_criteria TEXT,review_signoff INTEGER,merge_task_id TEXT,merge_mode TEXT,extra TEXT);
            CREATE TABLE tasks(id TEXT,subject TEXT,status TEXT,created_at INTEGER,claimed_at INTEGER,completed_at INTEGER,epic TEXT,story TEXT,owner TEXT,deps TEXT,priority INTEGER,body TEXT,lane TEXT,deliverable TEXT,stale_min INTEGER,driver_only INTEGER,claimed_from TEXT,created_from TEXT,note TEXT,extra TEXT);
            INSERT INTO epics VALUES('e-a','Epic A','ready',1700000000,NULL,'[]','["s-a"]','epic body','driver-1',1,1700000500,'{"customEpicField":"keep me"}');
            INSERT INTO stories VALUES('s-a','e-a','Story A','review',1700000100,NULL,1700000600,'story body','AC text',1,'t-merge','feature-branch','{"customStoryField":"keep me too"}');
            INSERT INTO tasks VALUES('t-a','Task A','in_progress',1700000200,1700000300,NULL,'e-a','s-a','agent-7','["t-b"]',2,'task body','fe','the deliverable',45,1,'driver-2','planner','a legacy note','{"customTaskField":"and me"}');
            INSERT INTO tasks VALUES('t-b','Task B','done',1700000210,NULL,1700000400,'e-a',NULL,'agent-8','[]',1,NULL,'be',NULL,NULL,0,NULL,NULL,NULL,'{}');
            "#,
        )
        .unwrap();
    drop(legacy);

    fixture.ok_json(&fixture.main, &["init", "--name", "PARITY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ],
    );

    // A faithful import verifies, and says how much it looked at. A receipt
    // that reports "verified" without a scope is not evidence of anything.
    let green = fixture.ok_json(
        &fixture.main,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--verify",
            "--json",
        ],
    );
    assert_eq!(green["verified"], true, "a faithful import failed parity");
    assert_eq!(green["compared"], 4);
    assert_eq!(green["matched"], 4);
    assert!(green["missing"].as_array().unwrap().is_empty());
    assert!(green["differing"].as_array().unwrap().is_empty());
    let fields = green["fields"].as_array().unwrap();
    for named in [
        "createdAt",
        "dependencies",
        "atmuxExtra",
        "note",
        "priority",
    ] {
        assert!(
            fields.iter().any(|f| f == named),
            "{named} is not in the stated scope"
        );
    }

    // Now the board drifts from the source, the way a partial or interfered-with
    // migration would leave it.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let tampered = Connection::open(&board).unwrap();
    tampered
        .execute(
            "UPDATE tasks SET title='TAMPERED', priority=9 WHERE id='t-a'",
            [],
        )
        .unwrap();
    tampered
        .execute("DELETE FROM tasks WHERE id='t-b'", [])
        .unwrap();
    drop(tampered);

    let red = fixture.ok_json(
        &fixture.main,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--verify",
            "--json",
        ],
    );
    assert_eq!(red["verified"], false, "a drifted board still verified");
    assert_eq!(red["missing"], json!(["t-b"]));
    let differing = red["differing"].as_array().unwrap();
    let named = |field: &str| {
        differing
            .iter()
            .any(|d| d["id"] == "t-a" && d["field"] == field)
    };
    assert!(named("title"), "the retitle was not reported");
    assert!(named("priority"), "the repricing was not reported");
    assert!(
        named("dependencies"),
        "the dependency lost with t-b was not reported"
    );
    for entry in differing {
        assert_ne!(
            entry["source"], entry["board"],
            "a matching field was reported as differing"
        );
    }

    // A diagnostic never modifies what it diagnoses: the two verifications
    // above must not have repaired, re-imported, or otherwise touched the board.
    let after = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let ids = after
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        !ids.contains(&"t-b"),
        "--verify re-imported the missing row"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-a", "--json"])["title"],
        "TAMPERED",
        "--verify repaired the row it was asked to report on"
    );

    // --verify reads; --reconcile, --force and --dry-run describe a write.
    // Asking for both is two requests, not a precedence puzzle.
    for conflicting in ["--reconcile", "--force", "--dry-run"] {
        let both = fixture.run(
            &fixture.main,
            &[
                "import",
                "atmux-sqlite",
                source.to_str().unwrap(),
                "--as",
                "operator",
                "--verify",
                conflicting,
            ],
        );
        assert!(
            !both.status.success(),
            "--verify {conflicting} was accepted"
        );
        assert!(
            String::from_utf8_lossy(&both.stderr).contains("writes nothing"),
            "{conflicting}"
        );
    }
}

/// ADR-001 §6: consumers receive narrow operations, and MCP/plugin adapters
/// expose the same ones the CLI does. The manifest is how an adapter gets them
/// without restating them -- and `readOnly` is only worth anything if it is
/// true, so this proves each labelled operation writes nothing.
#[test]
fn the_schema_describes_the_real_surface_and_read_only_really_is() {
    let fixture = Fixture::new("schema");
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    assert_eq!(schema["version"], env!("CARGO_PKG_VERSION"));

    let operations = schema["operations"].as_array().unwrap();
    assert!(operations.len() > 25, "the manifest lost operations");

    // Every operation the parser accepts appears, and nothing else does.
    for name in [
        "task add",
        "task list",
        "claim",
        "checkpoint",
        "handoff accept",
        "doctor",
        "schema",
    ] {
        assert!(
            operations.iter().any(|o| o["name"] == name),
            "{name} is missing from the manifest"
        );
    }
    for operation in operations {
        // Positionals are named and ordered, so an adapter can build an
        // argument list instead of guessing what the slots mean.
        for positional in operation["positionals"].as_array().unwrap() {
            let name = positional.as_str().unwrap();
            assert!(
                !name.is_empty(),
                "an unnamed positional reached the manifest"
            );
        }
        for flag in operation["flags"].as_array().unwrap() {
            let kind = flag["kind"].as_str().unwrap();
            assert!(
                ["value", "boolean", "list"].contains(&kind),
                "unknown flag kind {kind}"
            );
        }
    }
    // A list-valued flag is described as one, or an adapter generates a tool
    // that can only ever pass a single dependency.
    let add = operations.iter().find(|o| o["name"] == "task add").unwrap();
    assert_eq!(add["positionals"], json!(["title"]));
    let move_op = operations
        .iter()
        .find(|o| o["name"] == "task move")
        .unwrap();
    assert_eq!(move_op["positionals"], json!(["id", "status"]));
    // `claim` takes an id or `--next`, so its positional is marked optional
    // rather than silently required.
    let claim = operations.iter().find(|o| o["name"] == "claim").unwrap();
    assert_eq!(claim["positionals"], json!(["?id"]));
    let depends = add["flags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "depends-on")
        .unwrap();
    assert_eq!(depends["kind"], "list");

    // Now the claim that matters. Set up a board with something to damage,
    // record its bytes, run every read-only operation, and require the file to
    // be untouched. A label an adapter trusts to withhold mutation has to be
    // measured, not asserted.
    fixture.ok_json(&fixture.main, &["init", "--name", "SCHEMA", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Some work", "--id", "t-1", "--json"],
    );
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Read-only schema probe.",
            "--as",
            "geo",
            "--json",
        ],
    );
    let rule_id = rule["id"].as_str().unwrap().to_owned();
    let deployment = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/kanban",
            "--commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://kb.geoy.ws",
            "--as",
            "schema@e2e",
            "--json",
        ],
    );
    let deployment_id = deployment["id"].as_str().unwrap().to_owned();
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();

    let arguments = |name: &str| -> Option<Vec<String>> {
        let base: Vec<&str> = match name {
            "workspace list" => vec!["workspace", "list"],
            "dashboard" => vec!["dashboard"],
            "doctor" => vec!["doctor"],
            "audit verify" => vec!["audit", "verify"],
            "search" => vec!["search", "Some work"],
            "task list" => vec!["task", "list"],
            "task show" => vec!["task", "show", "t-1"],
            "handoff list" => vec!["handoff", "list"],
            "attention list" => vec!["attention", "list"],
            "tag list" => vec!["tag", "list"],
            "rule list" => vec!["rule", "list"],
            "rule show" => vec!["rule", "show", &rule_id],
            "sitrep list" => vec!["sitrep", "list"],
            "deploy show" => vec!["deploy", "show", &deployment_id],
            "deploy list" => vec!["deploy", "list"],
            "deploy current" => vec!["deploy", "current"],
            "schema" => vec!["schema"],
            "events" => vec!["events"],
            "stale" => vec!["stale"],
            "context" => vec!["context", "t-1"],
            _ => return None,
        };
        Some(base.into_iter().map(str::to_owned).collect())
    };

    let before = fs::read(&board).unwrap();
    let mut covered = 0;
    // A server is not an operation: `mcp` and `serve` block until killed, so
    // they cannot be run to completion and compared. They are excluded by the
    // property the manifest publishes, not by name, so a third one added later
    // is excluded by being declared rather than by editing this test.
    let servers = operations
        .iter()
        .filter(|o| o["longRunning"] == true)
        .map(|o| o["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        servers.contains(&"mcp") && servers.contains(&"serve"),
        "the long-running commands must declare themselves: {servers:?}"
    );
    for operation in operations
        .iter()
        .filter(|o| o["readOnly"] == true && o["longRunning"] != true)
    {
        let name = operation["name"].as_str().unwrap();
        let args = arguments(name)
            .unwrap_or_else(|| panic!("{name} is labelled readOnly but this test cannot run it"));
        let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = fixture.run(&fixture.main, &borrowed);
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(&board).unwrap(),
            before,
            "{name} is labelled readOnly and modified the board"
        );
        covered += 1;
    }
    assert_eq!(
        covered,
        operations
            .iter()
            .filter(|o| o["readOnly"] == true && o["longRunning"] != true)
            .count(),
        "a readOnly operation went unexercised"
    );
    assert!(covered >= 9, "too few read-only operations were proven");

    // And the converse is not claimed by accident: an operation that plainly
    // writes must not be labelled read-only.
    for name in [
        "task add",
        "task move",
        "claim",
        "checkpoint",
        "attention raise",
        "attention resolve",
        "tag add",
        "tag remove",
        "rule add",
        "rule update",
        "rule retire",
        "handoff create",
        "restore",
        "backup",
    ] {
        let operation = operations.iter().find(|o| o["name"] == name).unwrap();
        assert_eq!(operation["readOnly"], false, "{name} is labelled readOnly");
    }
}

#[test]
fn the_watch_surface_matches_help_and_the_mcp_manifest_excludes_it() {
    let fixture = Fixture::new("watch-surface");
    fixture.ok_json(&fixture.main, &["init", "--name", "WATCH", "--json"]);

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operations = schema["operations"].as_array().unwrap();
    let watch = operations
        .iter()
        .find(|operation| operation["name"] == "watch")
        .expect("watch is missing from the manifest");
    assert_eq!(watch["readOnly"], true);
    assert_eq!(watch["longRunning"], true);
    assert_eq!(watch["positionals"], json!([]));
    let flags = watch["flags"].as_array().unwrap();
    let flag_kind = |name: &str| -> &str {
        flags.iter().find(|flag| flag["name"] == name).unwrap()["kind"]
            .as_str()
            .unwrap()
    };
    assert_eq!(flag_kind("cursor"), "value");
    assert_eq!(flag_kind("follow"), "boolean");
    assert_eq!(flag_kind("all"), "boolean");
    assert_eq!(flag_kind("task"), "value");
    assert_eq!(flag_kind("rule"), "value");
    assert_eq!(flag_kind("registry"), "boolean");

    let help = fixture.run(&fixture.main, &["watch", "--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("kanban watch"));
    assert!(help_text.contains("--cursor"));
    assert!(help_text.contains("--follow"));
    assert!(help_text.contains("--registry"));

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(
        !tools.iter().any(|tool| tool["name"] == "watch"),
        "the long-running watch command leaked into the MCP tool list"
    );
}

#[test]
fn watch_replays_resumes_and_respects_selector_boundaries() {
    let fixture = Fixture::new("watch-replay");
    fixture.ok_json(&fixture.main, &["init", "--name", "WATCH-REPLAY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Main board task", "--id", "t-main", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-main",
            "Main board task note",
            "--as",
            "geo",
            "--kind",
            "progress",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Settled board task",
            "--id",
            "t-settled",
            "--json",
        ],
    );
    let settled_claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-settled", "--as", "worker", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-settled",
            "--lease",
            settled_claim["leaseToken"].as_str().unwrap(),
            "--as",
            "worker",
            "--state",
            "done",
            "--summary",
            "settled",
            "--intent",
            "close it",
            "--next-action",
            "none",
            "--json",
        ],
    );
    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-REPLAY");
    Connection::open(&board_path)
        .unwrap()
        .execute(
            "UPDATE tasks SET completed_at=1,updated_at=1 WHERE id='t-settled'",
            [],
        )
        .unwrap();
    fixture.ok_json(
        &fixture.worktree,
        &["init", "--name", "WATCH-SECONDARY", "--json"],
    );
    fixture.ok_json(
        &fixture.worktree,
        &[
            "task",
            "add",
            "Second board task",
            "--id",
            "t-second",
            "--json",
        ],
    );
    let rule_alpha = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Alpha registry rule.",
            "--as",
            "geo",
            "--json",
        ],
    );
    let rule_beta = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Beta registry rule.",
            "--as",
            "geo",
            "--json",
        ],
    );
    let rule_alpha_id = rule_alpha["id"].as_str().unwrap().to_owned();
    let rule_beta_id = rule_beta["id"].as_str().unwrap().to_owned();
    let default_output = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "32", "--json"],
    );
    assert!(default_output.status.success());
    let default_rows = ndjson_values(&default_output);
    assert!(!default_rows.is_empty());
    assert!(
        default_rows
            .iter()
            .all(|row| row["payload"]["taskID"] != json!("t-second")),
        "default board watch leaked the second board: {}",
        String::from_utf8_lossy(&default_output.stdout)
    );
    let saved_cursor = default_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let saved_cursor_seq = decode_watch_cursor(&saved_cursor)["seq"].as_i64().unwrap();

    for args in [
        vec![
            "watch",
            "--task",
            "t-main",
            "--cursor",
            &saved_cursor,
            "--json",
        ],
        vec![
            "watch",
            "--kind",
            "task_added",
            "--cursor",
            &saved_cursor,
            "--json",
        ],
        vec!["watch", "--all", "--cursor", &saved_cursor, "--json"],
        vec!["watch", "--registry", "--cursor", &saved_cursor, "--json"],
        vec![
            "watch",
            "--rule",
            &rule_alpha_id,
            "--cursor",
            &saved_cursor,
            "--json",
        ],
    ] {
        let mismatch = fixture.run(&fixture.main, &args);
        assert!(
            !mismatch.status.success(),
            "selector mismatch unexpectedly reused the stream: {:?}",
            args
        );
        assert!(
            mismatch.stdout.is_empty(),
            "selector mismatch wrote to stdout: {}",
            String::from_utf8_lossy(&mismatch.stdout)
        );
        assert!(
            String::from_utf8_lossy(&mismatch.stderr).contains("different watch stream"),
            "selector mismatch did not name the stream boundary: {}",
            String::from_utf8_lossy(&mismatch.stderr)
        );
    }

    let later = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Later board task",
            "--id",
            "t-later",
            "--json",
        ],
    );
    assert_eq!(later["id"], json!("t-later"));
    let resumed = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--cursor",
            &saved_cursor,
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(resumed.status.success());
    let resumed_rows = ndjson_values(&resumed);
    assert!(!resumed_rows.is_empty());
    assert!(
        resumed_rows.iter().all(|row| {
            row["payload"]["seq"].as_i64().unwrap() > saved_cursor_seq
                && row["payload"]["taskID"] == json!("t-later")
                && row["payload"]["kind"] == json!("task_added")
        }),
        "resumed watch did not stay on the later task: {}",
        String::from_utf8_lossy(&resumed.stdout)
    );

    let main_task_output = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-main",
            "--kind",
            "task_added",
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(main_task_output.status.success());
    let main_task_rows = ndjson_values(&main_task_output);
    assert!(!main_task_rows.is_empty());
    assert!(
        main_task_rows
            .iter()
            .all(|row| row["payload"]["taskID"] == json!("t-main")
                && row["payload"]["kind"] == json!("task_added")),
        "task/kind watch leaked other rows: {}",
        String::from_utf8_lossy(&main_task_output.stdout)
    );

    let second_board_output = fixture.run(
        &fixture.worktree,
        &["watch", "--cursor", "0", "--limit", "32", "--json"],
    );
    assert!(second_board_output.status.success());
    let second_board_rows = ndjson_values(&second_board_output);
    assert!(
        second_board_rows
            .iter()
            .any(|row| row["payload"]["taskID"] == json!("t-second")),
        "second board watch never saw its task: {}",
        String::from_utf8_lossy(&second_board_output.stdout)
    );

    let registry_output = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--registry",
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(registry_output.status.success());
    let registry_rows = ndjson_values(&registry_output);
    assert!(!registry_rows.is_empty());
    let registry_rule_ids = registry_rows
        .iter()
        .filter_map(|row| {
            row["payload"]["payload"]["ruleID"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        registry_rule_ids.contains(&rule_alpha_id) && registry_rule_ids.contains(&rule_beta_id),
        "registry watch did not include both rule IDs: {registry_rule_ids:?}"
    );
    assert!(
        registry_rows
            .iter()
            .all(|row| row["payload"]["taskID"].is_null()),
        "registry watch leaked board events: {}",
        String::from_utf8_lossy(&registry_output.stdout)
    );

    let rule_output = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--rule",
            &rule_alpha_id,
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(rule_output.status.success());
    let rule_rows = ndjson_values(&rule_output);
    assert!(!rule_rows.is_empty());
    assert!(
        rule_rows
            .iter()
            .all(|row| row["payload"]["payload"]["ruleID"] == json!(rule_alpha_id)),
        "rule watch crossed out of its selector: {}",
        String::from_utf8_lossy(&rule_output.stdout)
    );

    let archive = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(archive["tasks"], 1);
    let archived_default = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "64", "--json"],
    );
    assert!(archived_default.status.success());
    let archived_default_rows = ndjson_values(&archived_default);
    assert!(!archived_default_rows.is_empty());
    assert!(
        archived_default_rows
            .iter()
            .all(|row| row["payload"]["archived"] == json!(false)),
        "default watch emitted archived history: {}",
        String::from_utf8_lossy(&archived_default.stdout)
    );
    let archived_all = fixture.run(
        &fixture.main,
        &["watch", "--all", "--cursor", "0", "--limit", "64", "--json"],
    );
    assert!(archived_all.status.success());
    let archived_all_rows = ndjson_values(&archived_all);
    assert!(
        archived_all_rows
            .iter()
            .any(|row| row["payload"]["archived"] == json!(true)),
        "archive history stayed hidden from --all: {}",
        String::from_utf8_lossy(&archived_all.stdout)
    );

    let exact_board = board_path_for_project(&fixture, &fixture.main, "WATCH-REPLAY");
    let db_root = fixture.root.join("watch-db-only");
    fs::create_dir_all(&db_root).unwrap();
    let db_output = fixture
        .command_with_data_dir(&db_root, &fixture.root.join("watch-db-data"))
        .args([
            "watch",
            "--db",
            exact_board.to_str().unwrap(),
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        db_output.status.success(),
        "--db exact board path failed: stdout={} stderr={}",
        String::from_utf8_lossy(&db_output.stdout),
        String::from_utf8_lossy(&db_output.stderr)
    );
    let db_rows = ndjson_values(&db_output);
    assert!(!db_rows.is_empty());
    let canonical_board = exact_board.canonicalize().unwrap_or(exact_board);
    assert!(
        db_rows.iter().all(|row| {
            row["scope"]["sourceKind"] == json!("board")
                && row["scope"]["selectorKind"] == json!("board")
                && row["scope"]["source"] == json!(canonical_board.to_string_lossy().into_owned())
        }),
        "--db did not stay on the exact board path: {}",
        String::from_utf8_lossy(&db_output.stdout)
    );
}

#[test]
fn watch_follow_streams_new_events_and_keeps_outputs_separated() {
    let fixture = Fixture::new("watch-follow");
    fixture.ok_json(&fixture.main, &["init", "--name", "WATCH-FOLLOW", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Follow me", "--id", "t-follow", "--json"],
    );
    let preflight = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "32", "--json"],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    assert!(!preflight_rows.is_empty());
    let start_cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let start_cursor_json = decode_watch_cursor(&start_cursor);
    assert!(start_cursor_json.get("seq").is_some());
    assert!(start_cursor_json.get("lastSeq").is_none());
    assert_eq!(
        start_cursor_json["seq"].as_i64().unwrap(),
        preflight_rows.last().unwrap()["payload"]["seq"]
            .as_i64()
            .unwrap()
    );

    let watch = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &[
            "--cursor",
            &start_cursor,
            "--follow",
            "--limit",
            "1",
            "--json",
        ],
    );
    let heartbeat = watch.next_stdout_json(Duration::from_secs(5));
    assert_eq!(heartbeat["type"], "heartbeat");
    assert_eq!(heartbeat["version"], 1);
    assert!(
        heartbeat["scope"]["sourceKind"] == json!("board")
            && heartbeat["scope"]["selectorKind"] == json!("board")
            && heartbeat["scope"]["selectorValue"].is_null()
    );
    let heartbeat_cursor = decode_watch_cursor(heartbeat["cursor"].as_str().unwrap());
    assert_eq!(heartbeat_cursor["seq"], start_cursor_json["seq"]);

    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-FOLLOW");
    let raw_secret_seq = insert_raw_board_event(
        &board_path,
        Some("t-follow"),
        "watch_secret_probe",
        "geo",
        json!({
            "token": "outer-secret",
            "tokenCount": 7,
            "snake_token": "snake-secret",
            "camelToken": "camel-secret",
            "nested": {
                "tokenCount": 9,
                "snake_token": "nested-snake-secret",
                "camelToken": "nested-camel-secret",
                "items": [
                    {
                        "tokenCount": 3,
                        "materialValue": "deep-secret",
                        "secretValue": "deeper-secret",
                        "keep": "ok"
                    }
                ]
            }
        }),
    );
    let next_event = watch.next_stdout_json(Duration::from_secs(10));
    assert_eq!(next_event["type"], "event");
    let next_cursor = decode_watch_cursor(next_event["cursor"].as_str().unwrap());
    assert_eq!(next_cursor["seq"].as_i64().unwrap(), raw_secret_seq);
    let payload = &next_event["payload"]["payload"];
    assert!(payload.get("token").is_none());
    assert!(payload.get("snake_token").is_none());
    assert!(payload.get("camelToken").is_none());
    assert_eq!(payload["tokenCount"], 7);
    assert!(payload["nested"].get("snake_token").is_none());
    assert!(payload["nested"].get("camelToken").is_none());
    assert_eq!(payload["nested"]["tokenCount"], 9);
    assert!(payload["nested"]["items"][0].get("materialValue").is_none());
    assert!(payload["nested"]["items"][0].get("secretValue").is_none());
    assert_eq!(payload["nested"]["items"][0]["tokenCount"], 3);
    assert_eq!(payload["nested"]["items"][0]["keep"], "ok");
    assert!(
        watch.stderr_snapshot().is_empty(),
        "watch wrote diagnostics to stderr"
    );
    let stderr = watch.finish();
    assert!(
        stderr.is_empty(),
        "watch did not keep stderr separate from NDJSON"
    );
}

#[test]
fn watch_drains_backlogs_in_bounded_batches_and_rejects_invalid_limits() {
    let fixture = Fixture::new("watch-bounded");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-BOUNDED", "--json"],
    );
    for (id, title) in [("t-one", "One"), ("t-two", "Two"), ("t-three", "Three")] {
        fixture.ok_json(&fixture.main, &["task", "add", title, "--id", id, "--json"]);
    }
    fixture.ok_json(
        &fixture.main,
        &["note", "t-one", "Backlog note", "--as", "geo", "--json"],
    );
    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-BOUNDED");
    let expected_seqs = {
        let connection = Connection::open(&board_path).unwrap();
        connection
            .prepare("SELECT seq FROM events ORDER BY seq ASC")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };

    let bounded = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &["--cursor", "0", "--follow", "--limit", "2", "--json"],
    );
    let mut envelopes = Vec::new();
    let heartbeat = loop {
        let envelope = bounded.next_stdout_json(Duration::from_secs(5));
        match envelope["type"].as_str().unwrap() {
            "event" => envelopes.push(envelope),
            "heartbeat" => break envelope,
            other => panic!("unexpected watch envelope type {other}"),
        }
    };
    let seqs = envelopes
        .iter()
        .map(|envelope| envelope["payload"]["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert!(
        !seqs.is_empty(),
        "bounded follow never replayed the backlog"
    );
    assert_eq!(
        seqs, expected_seqs,
        "bounded follow replayed the wrong seqs"
    );
    let heartbeat_cursor = decode_watch_cursor(heartbeat["cursor"].as_str().unwrap());
    assert_eq!(
        heartbeat_cursor["seq"].as_i64().unwrap(),
        *expected_seqs.last().unwrap()
    );
    assert_eq!(heartbeat["scope"]["sourceKind"], json!("board"));
    assert_eq!(heartbeat["scope"]["selectorKind"], json!("board"));
    assert!(heartbeat["scope"]["selectorValue"].is_null());
    assert!(bounded.finish().is_empty());

    let huge_limit = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "1001", "--json"],
    );
    assert!(
        !huge_limit.status.success(),
        "an oversized watch limit was accepted"
    );
    let huge_limit_stderr = String::from_utf8_lossy(&huge_limit.stderr);
    assert!(
        huge_limit_stderr.contains("1000"),
        "stderr did not name the exact watch limit cap: {huge_limit_stderr}"
    );

    let zero_follow = fixture.run(
        &fixture.main,
        &[
            "watch", "--cursor", "0", "--follow", "--limit", "0", "--json",
        ],
    );
    assert!(
        !zero_follow.status.success(),
        "--follow with --limit 0 stayed live instead of being rejected"
    );
}

#[test]
fn watch_rejects_malformed_unsupported_and_future_cursors() {
    let fixture = Fixture::new("watch-cursors");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-CURSORS", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Cursor probe", "--id", "t-cursor", "--json"],
    );

    let preflight = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "1", "--json"],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    assert!(!preflight_rows.is_empty());
    let cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let cursor_json = decode_watch_cursor(&cursor);
    assert!(cursor_json.get("seq").is_some());
    assert!(cursor_json.get("lastSeq").is_none());
    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-CURSORS");
    let head = Connection::open(&board_path)
        .unwrap()
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();

    let malformed = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "not-a-token", "--json"],
    );
    assert!(
        !malformed.status.success(),
        "malformed cursor unexpectedly worked"
    );
    assert!(
        String::from_utf8_lossy(&malformed.stderr).contains("valid watch token"),
        "malformed cursor did not report a token error: {}",
        String::from_utf8_lossy(&malformed.stderr)
    );

    let mut unsupported_json = cursor_json.clone();
    unsupported_json["version"] = json!(2);
    let unsupported = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&unsupported_json).unwrap());
    let unsupported_run = fixture.run(
        &fixture.main,
        &["watch", "--cursor", &unsupported, "--json"],
    );
    assert!(
        !unsupported_run.status.success(),
        "unsupported cursor protocol version unexpectedly worked"
    );
    assert!(
        String::from_utf8_lossy(&unsupported_run.stderr).contains("unsupported protocol version"),
        "unsupported cursor version did not fail clearly: {}",
        String::from_utf8_lossy(&unsupported_run.stderr)
    );

    let mut future_json = cursor_json.clone();
    future_json["seq"] = json!(head + 1);
    let future = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&future_json).unwrap());
    let future_run = fixture.run(&fixture.main, &["watch", "--cursor", &future, "--json"]);
    assert!(
        !future_run.status.success(),
        "future cursor unexpectedly worked"
    );
    assert!(
        String::from_utf8_lossy(&future_run.stderr).contains("ahead of the current ledger head"),
        "future cursor did not report the current head check: {}",
        String::from_utf8_lossy(&future_run.stderr)
    );
}

/// A live MCP session, spoken over real pipes to the real binary.
struct Session {
    child: std::process::Child,
    outgoing: std::process::ChildStdin,
    incoming: std::sync::mpsc::Receiver<String>,
}

impl Session {
    fn start(binary: &Path, cwd: &Path, data: &Path) -> Self {
        let mut child = Command::new(binary)
            .arg("mcp")
            .current_dir(cwd)
            .env("KANBAN_DATA_DIR", data)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let outgoing = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, incoming) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            outgoing,
            incoming,
        }
    }

    /// Write several requests in one syscall, so the server can pull more than
    /// one into a single read and genuinely hold an unparsed one in memory.
    /// Two separate writes usually arrive as two reads and never produce the
    /// state this is here to create.
    fn send_batch(&mut self, requests: &[Value]) {
        let mut frame = String::new();
        for request in requests {
            frame.push_str(&request.to_string());
            frame.push('\n');
        }
        self.outgoing.write_all(frame.as_bytes()).unwrap();
        self.outgoing.flush().unwrap();
    }

    /// The next reply, whichever request it belongs to.
    fn recv(&mut self) -> Value {
        let line = self
            .incoming
            .recv_timeout(Duration::from_secs(20))
            .expect("the server owed a reply and did not send one");
        serde_json::from_str(&line).unwrap()
    }

    /// Send one request and wait for its reply. Never blocks forever: a hung
    /// server is a failure, not a test that runs until someone kills it.
    fn ask(&mut self, request: Value) -> Value {
        writeln!(self.outgoing, "{request}").unwrap();
        self.outgoing.flush().unwrap();
        let line = self
            .incoming
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no reply to {request}"));
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_executable(path: &Path, body: &str) {
    // Written beside the target and renamed over it, because that is how a
    // binary is actually replaced -- and the rename is what leaves the running
    // process holding an unlinked inode.
    let staging = path.with_extension("staging");
    fs::write(&staging, body).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&staging, path).unwrap();
}

#[test]
fn the_mcp_server_answers_over_stdio_and_runs_the_real_cli() {
    let fixture = Fixture::new("mcp");
    fixture.ok_json(&fixture.main, &["init", "--name", "MCP", "--json"]);
    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );

    let initialized = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    assert_eq!(initialized["result"]["serverInfo"]["name"], "kanban");
    assert_eq!(
        initialized["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    // The tool list is the manifest, so it cannot describe a surface the CLI
    // does not have.
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "task_add"));
    assert!(tools.iter().any(|t| t["name"] == "import_atmux_sqlite"));
    let search = tools.iter().find(|t| t["name"] == "search").unwrap();
    assert_eq!(search["annotations"]["readOnlyHint"], true);
    let rebuild = tools
        .iter()
        .find(|t| t["name"] == "search_rebuild")
        .unwrap();
    assert_eq!(rebuild["annotations"]["readOnlyHint"], false);
    let read_only = tools.iter().find(|t| t["name"] == "doctor").unwrap();
    assert_eq!(read_only["annotations"]["readOnlyHint"], true);
    let writes = tools.iter().find(|t| t["name"] == "claim").unwrap();
    assert_eq!(writes["annotations"]["readOnlyHint"], false);
    let rule_add = tools.iter().find(|t| t["name"] == "rule_add").unwrap();
    assert!(
        rule_add["inputSchema"]["properties"]
            .get("global")
            .is_none(),
        "MCP still advertised the retired rule scope flag"
    );
    assert_eq!(
        rule_add["inputSchema"]["properties"]["board"]["type"],
        "array"
    );
    // A list-valued flag must be typed as an array, or an agent can only ever
    // pass one dependency and the rest are dropped without a word.
    let add = tools.iter().find(|t| t["name"] == "task_add").unwrap();
    assert_eq!(
        add["inputSchema"]["properties"]["depends-on"]["type"],
        "array"
    );
    assert_eq!(add["inputSchema"]["required"], json!(["title"]));

    // A call writes through the real CLI, and the board shows it.
    let created = session.ask(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "task_add", "arguments": { "title": "Over the wire", "id": "t-wire" } }
    }));
    assert_eq!(created["result"]["isError"], false);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-wire", "--json"])["title"],
        "Over the wire"
    );
    let rule = session.ask(json!({
        "jsonrpc": "2.0", "id": 30, "method": "tools/call",
        "params": { "name": "rule_add", "arguments": {
            "body": "MCP-created rule.", "board": ["MCP"], "as": "geo"
        } }
    }));
    assert_eq!(rule["result"]["isError"], false, "{rule}");
    let rule_text = rule["result"]["content"][0]["text"].as_str().unwrap();
    assert!(rule_text.contains("ONLY:MCP"), "{rule_text}");
    let context = fixture.ok_json(&fixture.main, &["context", "t-wire", "--json"]);
    assert!(context["rules"].as_array().unwrap().iter().any(|item| {
        item["headline"] == "MCP-created rule." && item["tags"] == json!(["ONLY:MCP"])
    }));
    let found = session.ask(json!({
        "jsonrpc": "2.0", "id": 31, "method": "tools/call",
        "params": { "name": "search", "arguments": { "query": "Over the wire" } }
    }));
    assert_eq!(found["result"]["isError"], false);
    assert!(
        found["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("kanban://MCP/task/t-wire")
    );

    // A refusal is a tool result carrying the CLI's own message, not a
    // transport error: the refusal names the fix, and an agent needs to read it.
    let refused = session.ask(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "task_move", "arguments": { "id": "t-wire", "status": "nonsense", "as": "geo" } }
    }));
    assert_eq!(refused["result"]["isError"], true);
    assert!(
        refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid task status"),
        "the CLI's refusal did not reach the caller"
    );

    // An argument the operation does not define is refused rather than dropped.
    let unknown = session.ask(json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "task_add", "arguments": { "title": "x", "frobnicate": "y" } }
    }));
    assert_eq!(unknown["result"]["isError"], true);
    assert!(
        unknown["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("frobnicate")
    );

    // A notification has no id and must not be answered at all. Asking a real
    // question afterwards proves the stream is still aligned: a stray reply
    // would arrive here, one response out of step, and fail the assertion.
    writeln!(
        session.outgoing,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    session.outgoing.flush().unwrap();
    let ping = session.ask(json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}));
    assert_eq!(ping["id"], 6, "a notification was answered");

    // `--help` would answer the call with the usage page. Accepting it meant an
    // agent that asked for a task list got the manual, reported as success,
    // with the operation never run and nothing saying so.
    let helped = session.ask(json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "task_list", "arguments": { "help": true } }
    }));
    assert_eq!(
        helped["result"]["isError"], true,
        "--help answered a tool call"
    );
    let helped_text = helped["result"]["content"][0]["text"].as_str().unwrap();
    assert!(helped_text.contains("help"), "{helped_text}");
    assert!(
        !helped_text.contains("durable work ledger"),
        "the usage page was returned instead of a refusal"
    );

    // `--json` is supplied by this layer, so accepting it again produced
    // "given more than once" -- a refusal naming a flag the caller passed once.
    let doubled = session.ask(json!({
        "jsonrpc": "2.0", "id": 8, "method": "tools/call",
        "params": { "name": "task_list", "arguments": { "json": true } }
    }));
    assert_eq!(doubled["result"]["isError"], true);
    let doubled_text = doubled["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !doubled_text.contains("more than once"),
        "the caller was blamed for a flag this layer added: {doubled_text}"
    );
    assert!(doubled_text.contains("json"), "{doubled_text}");

    // A list where one value belongs is refused, not flattened into a title
    // that reads `["a","b"]` and is reported as success.
    let flattened = session.ask(json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "task_add", "arguments": { "title": ["a", "b"], "id": "t-flat" } }
    }));
    assert_eq!(
        flattened["result"]["isError"], true,
        "a list became a title"
    );
    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-flat", "--json"])
            .status
            .success(),
        "the refused call still wrote a row"
    );

    // Arguments that are not an object were read as "no arguments", so a call
    // meant to be constrained ran unconstrained and reported success.
    let malformed = session.ask(json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": { "name": "task_list", "arguments": "not-an-object" }
    }));
    assert_eq!(malformed["result"]["isError"], true);
    assert!(
        malformed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("must be an object")
    );

    // A scalar still reaches a value flag, and a number is not a type error.
    let numeric = session.ask(json!({
        "jsonrpc": "2.0", "id": 11, "method": "tools/call",
        "params": { "name": "task_add", "arguments": { "title": "numeric", "id": "t-num", "priority": 2 } }
    }));
    assert_eq!(numeric["result"]["isError"], false);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-num", "--json"])["priority"],
        2
    );

    let unknown_method = session.ask(json!({"jsonrpc": "2.0", "id": 12, "method": "no/such"}));
    assert_eq!(unknown_method["error"]["code"], -32601);
}

#[test]
fn the_mcp_server_replaces_itself_without_dropping_the_session() {
    let fixture = Fixture::new("mcp-reload");
    fixture.ok_json(&fixture.main, &["init", "--name", "RELOAD", "--json"]);

    // Serve from a copy, so the test can replace the binary underneath it the
    // way `install` does.
    let binary = fixture.root.join("kanban");
    fs::copy(env!("CARGO_BIN_EXE_kanban"), &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

    let mut session = Session::start(&binary, &fixture.main, &fixture.data);
    let pid = session.child.id();
    let before =
        session.ask(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}));
    assert_eq!(
        before["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    // A replacement is noticed between requests, so a swap lands on a request
    // boundary and never mid-reply. The check runs before the server blocks
    // reading, so a binary that appears while it is blocked is acted on at the
    // end of the next turn -- one request later. Every phase below therefore
    // sends two requests: the first finishes the turn in flight, the second is
    // the first one the check has had its chance at.
    //
    // A replacement that cannot run must not be adopted: exec'ing a program
    // that exits immediately closes the pipe, which is indistinguishable from
    // a crashed server. Both requests must come back on the previous build,
    // and the second is the one that proves the health probe refused it.
    write_executable(&binary, "#!/bin/sh\nexit 1\n");
    for id in [2, 3] {
        let survived =
            session.ask(json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {}}));
        assert_eq!(
            survived["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION"),
            "a broken build was adopted at request {id}"
        );
        assert_eq!(session.child.id(), pid, "the process was replaced anyway");
    }

    // Now a replacement that works. It answers `version` like the real binary,
    // so the health probe passes, and reports a version of its own so the swap
    // is observable from the client side.
    write_executable(
        &binary,
        r#"#!/bin/sh
if [ "$1" = "version" ]; then echo "kanban 9.9.9"; exit 0; fi
while IFS= read -r line; do
  id=`printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'`
  printf '{"jsonrpc":"2.0","id":%s,"result":{"serverInfo":{"name":"kanban","version":"9.9.9"}}}\n' "${id:-0}"
done
"#,
    );

    // Same two-request shape: the first finishes the turn in flight, the
    // second is served by the build that replaced it.
    let last_of_the_old =
        session.ask(json!({"jsonrpc": "2.0", "id": 4, "method": "initialize", "params": {}}));
    assert_eq!(last_of_the_old["id"], 4);
    let after =
        session.ask(json!({"jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {}}));
    assert_eq!(
        after["result"]["serverInfo"]["version"], "9.9.9",
        "the new build never took over"
    );
    assert_eq!(after["id"], 5, "the reply belongs to a different request");

    // The whole point: same process, same pipes, no reconnection. A client
    // that had to restart the server would not be undisturbed.
    assert_eq!(
        session.child.id(),
        pid,
        "the process id changed, so the client's pipe did too"
    );
}

#[test]
fn a_reload_never_swallows_a_request_already_on_the_wire() {
    // A client may pipeline: two requests can be sitting in one read before
    // either is parsed. `execve` keeps the file descriptors and discards
    // memory, so a swap performed while anything is buffered would take the
    // unparsed request with it -- and the client would wait forever for a
    // reply to a request the server did receive. The reload is therefore
    // skipped whenever the buffer is not empty.
    let fixture = Fixture::new("mcp-pipeline");
    fixture.ok_json(&fixture.main, &["init", "--name", "PIPELINE", "--json"]);

    let binary = fixture.root.join("kanban");
    fs::copy(env!("CARGO_BIN_EXE_kanban"), &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

    let mut session = Session::start(&binary, &fixture.main, &fixture.data);
    assert_eq!(
        session.ask(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))["id"],
        1
    );

    // Make a replacement available, then put two requests on the wire before
    // the server has parsed either.
    write_executable(
        &binary,
        r#"#!/bin/sh
if [ "$1" = "version" ]; then echo "kanban 9.9.9"; exit 0; fi
while IFS= read -r line; do
  id=`printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'`
  printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "${id:-0}"
done
"#,
    );
    session.send_batch(&[
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
    ]);

    // Both are answered, whichever build ends up answering them. Losing one is
    // the failure this guards against, and it presents as a client hanging.
    let mut answered = vec![
        session.recv()["id"].as_i64().unwrap(),
        session.recv()["id"].as_i64().unwrap(),
    ];
    answered.sort_unstable();
    assert_eq!(
        answered,
        vec![2, 3],
        "a request already on the wire was lost across the reload"
    );
}

#[test]
fn a_handoff_can_be_about_the_session_rather_than_one_task() {
    let fixture = Fixture::new("session-handoff");
    fixture.ok_json(&fixture.main, &["init", "--name", "SESSION", "--json"]);

    // No task id and no lease: a handoff about the work as a whole. This is
    // the shape a lane hands to its successor, and it had nowhere to live --
    // task_id and checkpoint_seq were both NOT NULL.
    let session = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "claude@driver-2",
            "--to",
            "driver-2",
            "--reason",
            "session_end",
            "--summary",
            "Phase 3 landed",
            "--intent",
            "the successor continues at the merge gate",
            "--next-action",
            "run the tenancy leg then push",
            "--branch",
            "px-crm-geoyws-driver-2",
            "--json",
        ],
    );
    assert!(session["taskID"].is_null(), "a session handoff took a task");
    assert!(session["checkpointSeq"].is_null());
    assert_eq!(session["status"], "pending");

    // Found by who it is for, not by where it was written. A successor knows
    // its own lane; it does not necessarily know which directory the previous
    // session was standing in.
    let addressed = fixture.ok_json(
        &fixture.main,
        &[
            "handoff", "list", "--status", "pending", "--to", "driver-2", "--json",
        ],
    );
    assert_eq!(addressed.as_array().unwrap().len(), 1);
    assert_eq!(addressed[0]["id"], session["id"]);
    assert_eq!(addressed[0]["branch"], "px-crm-geoyws-driver-2");
    // Someone else's lane must not see it.
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["handoff", "list", "--to", "driver-1", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Accepting one is an acknowledgement: there is no task, so no lease.
    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            session["id"].as_str().unwrap(),
            "--as",
            "driver-2",
            "--json",
        ],
    );
    assert_eq!(accepted["handoff"]["status"], "accepted");
    assert!(
        accepted["claim"].is_null(),
        "a session handoff minted a lease over nothing"
    );

    // A task id and a lease travel together; neither half means anything alone.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    let no_lease = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-1",
            "--as",
            "a",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
        ],
    );
    assert!(!no_lease.status.success(), "a task was handed over unheld");
    assert!(String::from_utf8_lossy(&no_lease.stderr).contains("--lease"));

    let no_task = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--lease",
            "made-up",
            "--as",
            "a",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
        ],
    );
    assert!(
        !no_task.status.success(),
        "a lease was accepted with no task"
    );
    assert!(String::from_utf8_lossy(&no_task.stderr).contains("task id"));
}

#[test]
fn handoff_history_survives_the_task_it_was_about() {
    // A handoff is an account of a handover that happened. Removing the task
    // does not un-happen it -- but task_id was ON DELETE CASCADE, so every
    // handoff ever taken over a task vanished with it.
    let fixture = Fixture::new("handoff-trail");
    fixture.ok_json(&fixture.main, &["init", "--name", "TRAIL", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-1", "--as", "agent-a", "--json"],
    );
    let created = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-1",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "agent-a",
            "--summary",
            "ran out of context mid-parser",
            "--intent",
            "continue from the checkpoint",
            "--next-action",
            "finish the parser",
            "--json",
        ],
    );
    assert_eq!(created["taskID"], "t-1");

    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "t-1", "--as", "geo", "--force", "--json"],
    );

    let after = fixture.ok_json(&fixture.main, &["handoff", "list", "--json"]);
    let kept = after
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == created["id"])
        .expect("the handoff was deleted with its task");
    assert_eq!(
        kept["summary"], "ran out of context mid-parser",
        "the account did not survive"
    );
    assert_eq!(kept["fromAgent"], "agent-a");
    assert!(
        kept["taskID"].is_null(),
        "the link to a removed task should be dropped, not dangle"
    );

    // And the board is still consistent: a dangling reference would show here.
    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(doctor["healthy"], true, "{doctor}");
}

#[test]
fn attention_is_recorded_for_the_operator_and_kept_after_it_is_settled() {
    let fixture = Fixture::new("attention");
    fixture.ok_json(&fixture.main, &["init", "--name", "ATTN", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    for tag in ["infra", "ui"] {
        fixture.ok_json(&fixture.main, &["tag", "add", tag, "--as", "geo", "--json"]);
    }

    let blocking = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "field set contradicts the manual; confirm which wins",
            "--as",
            "claude/driver-2",
            "--kind",
            "blocking",
            "--task",
            "t-1",
            "--tag",
            "infra",
            "--json",
        ],
    );
    assert_eq!(blocking["status"], "open");
    assert_eq!(blocking["taskID"], "t-1");
    assert_eq!(blocking["tags"], json!(["infra"]));
    let approval = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "2 commits ready for a staging push, which is George-manual",
            "--as",
            "claude/driver-1",
            "--kind",
            "approval",
            "--json",
        ],
    );
    assert!(approval["taskID"].is_null(), "an item may be about no task");
    assert_eq!(approval["tags"], json!([]));

    let retagged = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            blocking["id"].as_str().unwrap(),
            "--as",
            "claude/driver-2",
            "--tag",
            "ui",
            "--tag",
            "infra",
            "--json",
        ],
    );
    assert_eq!(retagged["tags"], json!(["infra", "ui"]));
    let corrected = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            blocking["id"].as_str().unwrap(),
            "--as",
            "claude/driver-2",
            "--body",
            "The manual is authoritative; confirm the migration timing.",
            "--json",
        ],
    );
    assert_eq!(
        corrected["body"],
        "The manual is authoritative; confirm the migration timing."
    );
    assert_eq!(corrected["tags"], json!(["infra", "ui"]));
    let updates = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_updated", "--json"],
    );
    assert_eq!(updates[0]["payload"]["changed"], json!(["body"]));
    assert_eq!(
        updates[0]["payload"]["previousBody"],
        "field set contradicts the manual; confirm which wins"
    );
    assert_eq!(
        updates[0]["payload"]["previousTags"],
        json!(["infra", "ui"])
    );
    assert_eq!(updates[1]["payload"]["changed"], json!(["tags"]));
    assert_eq!(updates[1]["payload"]["previousTags"], json!(["infra"]));
    let infra = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--tag", "infra", "--json"],
    );
    assert_eq!(infra.as_array().unwrap().len(), 1);
    assert_eq!(infra[0]["id"], blocking["id"]);

    let unknown_tag = fixture.run(
        &fixture.main,
        &["attention", "list", "--tag", "missing", "--json"],
    );
    assert!(!unknown_tag.status.success());
    assert!(String::from_utf8_lossy(&unknown_tag.stderr).contains("master file"));

    let self_settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            blocking["id"].as_str().unwrap(),
            "--as",
            "claude/driver-2",
            "--note",
            "The raiser withdrew its own item after verification.",
            "--json",
        ],
    );
    assert_eq!(self_settled["status"], "resolved");
    let self_reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            blocking["id"].as_str().unwrap(),
            "--as",
            "claude/driver-2",
            "--note",
            "The underlying request still needs George.",
            "--json",
        ],
    );
    assert_eq!(self_reopened["status"], "open");
    assert_eq!(self_reopened["resolvedBy"], "claude/driver-2");
    assert_eq!(
        self_reopened["resolution"],
        "The raiser withdrew its own item after verification."
    );
    assert_eq!(self_reopened["reopenedBy"], "claude/driver-2");
    assert_eq!(
        self_reopened["reopenNote"],
        "The underlying request still needs George."
    );

    // A kind outside the closed set is refused: "what sort of thing is this"
    // is the part a reader needs first, and free text would not answer it.
    let invented = fixture.run(
        &fixture.main,
        &[
            "attention",
            "raise",
            "x",
            "--as",
            "a",
            "--kind",
            "vibes",
            "--json",
        ],
    );
    assert!(!invented.status.success(), "an invented kind was accepted");
    assert!(String::from_utf8_lossy(&invented.stderr).contains("attention kind"));

    // Open first, and oldest first within that -- an unanswered question does
    // not get less urgent by being ignored.
    let listed = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    let ids = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            blocking["id"].as_str().unwrap(),
            approval["id"].as_str().unwrap()
        ]
    );

    let unauthorized = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "claude/driver-3",
            "--note",
            "Probe",
            "--json",
        ],
    );
    assert!(!unauthorized.status.success());
    assert!(String::from_utf8_lossy(&unauthorized.stderr).contains("only geo"));
    let missing_note = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--json",
        ],
    );
    assert!(!missing_note.status.success());
    assert!(String::from_utf8_lossy(&missing_note.stderr).contains("--note is required"));

    // Settling one keeps it: the trail is the feature.
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--note",
            "approved and pushed",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["resolvedBy"], "geo");
    assert_eq!(settled["resolution"], "approved and pushed");
    assert!(!settled["resolvedAt"].is_null());

    let wrong_reopener = fixture.run(
        &fixture.main,
        &[
            "attention",
            "reopen",
            approval["id"].as_str().unwrap(),
            "--as",
            "claude/driver-3",
            "--note",
            "Trying to alter George's decision.",
            "--json",
        ],
    );
    assert!(!wrong_reopener.status.success());
    assert!(String::from_utf8_lossy(&wrong_reopener.stderr).contains("only geo"));
    let reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            approval["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--note",
            "The wrong item was resolved.",
            "--json",
        ],
    );
    assert_eq!(reopened["status"], "open");
    assert_eq!(reopened["resolvedBy"], "geo");
    assert_eq!(reopened["resolution"], "approved and pushed");
    assert!(!reopened["reopenedAt"].is_null());
    assert_eq!(reopened["reopenedBy"], "geo");
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--note",
            "Approved after reopening the mistaken transition.",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert!(settled["reopenedAt"].is_null());

    let rewrite_history = fixture.run(
        &fixture.main,
        &[
            "attention",
            "update",
            approval["id"].as_str().unwrap(),
            "--as",
            "someone-else",
            "--clear-tags",
            "--json",
        ],
    );
    assert!(!rewrite_history.status.success());
    assert!(String::from_utf8_lossy(&rewrite_history.stderr).contains("resolved history"));

    let still_there = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(
        still_there.as_array().unwrap().len(),
        2,
        "a resolved item was dropped from the record"
    );
    // Resolved sinks below open, so the queue reads as a queue.
    assert_eq!(still_there[1]["id"], approval["id"]);
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["attention", "list", "--status", "open", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Resolving twice would overwrite who settled it and when, which is the
    // part worth keeping.
    let again = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "someone-else",
            "--json",
        ],
    );
    assert!(!again.status.success(), "a settled item was re-settled");
    assert!(String::from_utf8_lossy(&again.stderr).contains("already resolved by geo"));

    // The operator sees the count without having to ask for it.
    let dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert_eq!(dashboard[0]["openAttention"], 1);

    // Raised and settled are both on the durable audit trail.
    let events = fixture.ok_json(&fixture.main, &["events", "--json"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"attention_raised"), "{kinds:?}");
    assert!(kinds.contains(&"attention_updated"), "{kinds:?}");
    assert!(kinds.contains(&"attention_resolved"), "{kinds:?}");
    assert!(kinds.contains(&"attention_reopened"), "{kinds:?}");

    // An item about a removed task keeps its text, like a handoff does.
    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "t-1", "--as", "geo", "--force", "--json"],
    );
    let orphaned = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    let survivor = orphaned
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == blocking["id"])
        .expect("the item was deleted with its task");
    assert!(survivor["taskID"].is_null());
    assert_eq!(
        survivor["status"], "open",
        "removing a task answered nothing"
    );

    // A v16 board already carrying tagged attention rows keeps both sides of
    // that relationship through the v17 table rebuild.
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            blocking["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--note",
            "Settled before simulating the v16 migration boundary.",
            "--json",
        ],
    );
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let connection = Connection::open(&board).unwrap();
    remove_v18_board_audit_schema(&connection);
    connection.execute_batch("PRAGMA user_version=16;").unwrap();
    let migrated = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    let survivor = migrated
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == blocking["id"])
        .unwrap();
    assert_eq!(survivor["tags"], json!(["infra", "ui"]));
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["projects"][0]["schemaVersion"],
        20
    );
}

#[test]
fn a_negative_limit_is_refused_rather_than_read_as_no_limit() {
    // SQLite reads LIMIT -1 as *no limit*, so a caller who explicitly bounded
    // a listing got every row of it back and reported success -- the same
    // shape as a --max-chars that is accepted and ignored.
    let fixture = Fixture::new("limit");
    fixture.ok_json(&fixture.main, &["init", "--name", "LIMIT", "--json"]);
    for index in 0..4 {
        fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "raise",
                &format!("item {index}"),
                "--as",
                "agent",
                "--kind",
                "risk",
                "--json",
            ],
        );
    }

    // A bound is honoured, and zero means zero.
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["attention", "list", "--limit", "2", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["attention", "list", "--limit", "0", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a limit of zero asks for nothing"
    );

    // A negative is refused on every command that takes the flag.
    for command in [
        vec!["attention", "list", "--limit", "-1", "--json"],
        vec!["events", "--limit", "-1", "--json"],
    ] {
        let refused = fixture.run(&fixture.main, &command);
        assert!(
            !refused.status.success(),
            "{command:?} accepted a negative limit"
        );
        let error = String::from_utf8_lossy(&refused.stderr).to_string();
        assert!(error.contains("--limit"), "{error}");
        assert!(error.contains("-1"), "{error}");
    }
}

#[test]
fn a_draft_task_is_not_offered_as_work_until_it_is_promoted() {
    // `backlog` already meant real work that is simply unscheduled. There was
    // nothing for the state before that -- a row still being written -- so an
    // unfinished one read as a specification and got claimed and worked.
    let fixture = Fixture::new("draft");
    fixture.ok_json(&fixture.main, &["init", "--name", "DRAFT", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Still being written",
            "--id",
            "t-draft",
            "--status",
            "draft",
            "--priority",
            "0",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Ready to work",
            "--id",
            "t-ready",
            "--priority",
            "9",
            "--json",
        ],
    );

    // The draft sorts ahead on every tiebreak --next uses and is still skipped:
    // being unfinished outranks being urgent.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "worker", "--json"]
        )["taskID"],
        "t-ready"
    );

    // Naming it explicitly is refused, and the refusal says which state stopped it.
    let explicit = fixture.run(
        &fixture.main,
        &["claim", "t-draft", "--as", "worker", "--json"],
    );
    assert!(!explicit.status.success(), "a draft was handed out as work");
    let error = String::from_utf8_lossy(&explicit.stderr).to_string();
    assert!(error.contains("draft"), "{error}");
    assert!(error.contains("not claimable"), "{error}");

    // A draft is not touched by the refusal, and is not a stale-work candidate.
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-draft", "--json"]);
    assert_eq!(shown["status"], "draft");
    assert!(shown["assignee"].is_null());
    assert!(
        fixture
            .ok_json(&fixture.main, &["stale", "--json"])
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Promoting it is an ordinary move, and then it is work like any other.
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-draft", "todo", "--as", "geo", "--json"],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "t-draft", "--as", "worker", "--json"]
        )["taskID"],
        "t-draft"
    );

    // It is a first-class status: filterable, and counted where the others are.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Another draft",
            "--id",
            "t-d2",
            "--status",
            "draft",
            "--json",
        ],
    );
    let drafts = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--status", "draft", "--json"],
    );
    assert_eq!(drafts.as_array().unwrap().len(), 1);
    assert_eq!(drafts[0]["id"], "t-d2");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["dashboard", "--json"])[0]["taskCounts"]["draft"],
        1
    );

    // An invented status is still refused -- the set stays closed.
    let invented = fixture.run(
        &fixture.main,
        &["task", "add", "x", "--status", "nearly", "--json"],
    );
    assert!(!invented.status.success());
    assert!(String::from_utf8_lossy(&invented.stderr).contains("invalid task status"));
}

#[test]
fn the_draft_migration_preserves_the_table_it_rebuilds() {
    // Widening a CHECK means rebuilding the table, and a rebuild is the easiest
    // place in a schema to change something nobody asked to change. The one
    // that matters here: parent_id has no ON DELETE clause, so removing a
    // parent is meant to fail and name its children rather than orphan them.
    let fixture = Fixture::new("draft-migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "MIGRATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Epic", "--id", "e-1", "--type", "epic", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Child", "--id", "t-1", "--parent", "e-1", "--json",
        ],
    );

    let refused = fixture.run(
        &fixture.main,
        &["task", "remove", "e-1", "--as", "geo", "--force", "--json"],
    );
    assert!(
        !refused.status.success(),
        "a parent with children was removed, orphaning them"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("t-1"),
        "the refusal must name the child"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["parentID"],
        "e-1",
        "the child lost its parent"
    );

    // Public writers now create routine work at P2 even on a board whose
    // preserved historical table default remains the old numeric 3.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Plain", "--id", "t-2", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-2", "--json"])["priority"],
        6,
        "the priority default was lost in the rebuild"
    );

    // Read the schema itself. The behaviour above is defended twice -- the
    // remove path names children in code before the foreign key is consulted --
    // so it cannot see a changed ON DELETE clause. The DDL can, and that clause
    // is the difference between a removal that refuses and one that silently
    // orphans, so it is asserted where it is actually written.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let connection = Connection::open(&board).unwrap();
    let schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='tasks'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        schema.contains("parent_id TEXT REFERENCES tasks(id),"),
        "parent_id gained an ON DELETE clause in the rebuild: {schema}"
    );
    assert!(
        !schema.contains("ON DELETE"),
        "the rebuilt tasks table carries a delete rule it never had: {schema}"
    );
    assert!(
        schema.contains("priority INTEGER NOT NULL DEFAULT 3"),
        "{schema}"
    );
    assert!(schema.contains("'draft'"), "the widened CHECK is missing");

    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='tasks'")
        .unwrap();
    let indexes = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for expected in [
        "idx_tasks_status_priority",
        "idx_tasks_parent",
        "idx_tasks_assignee_status",
        "idx_tasks_lane_status",
    ] {
        assert!(
            indexes.iter().any(|name| name == expected),
            "{expected} did not survive the rebuild: {indexes:?}"
        );
    }
    drop(statement);

    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(doctor["healthy"], true, "{doctor}");
}

#[test]
fn the_v10_sitrep_rename_preserves_v9_rows_and_their_trail() {
    let fixture = Fixture::new("sitrep-migration");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SITREP-MIGRATION", "--json"],
    );
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();

    // Recreate the exact V9 surface on an otherwise current fixture. Opening
    // it through the compiled binary below must run V10, not merely exercise
    // fresh-board behaviour.
    let connection = Connection::open(&board).unwrap();
    remove_v18_board_audit_schema(&connection);
    remove_v13_search_schema(&connection);
    connection
        .execute_batch(
            r#"
            DROP INDEX idx_sitreps_lane_created;
            ALTER TABLE sitreps RENAME TO status_updates;
            CREATE INDEX idx_status_lane_created ON status_updates(lane,archived,created_at DESC);
            DROP INDEX idx_rules_active;
            DROP TABLE rules;
            DROP INDEX idx_tasks_status_priority;
            DROP INDEX idx_tasks_parent;
            DROP INDEX idx_tasks_assignee_status;
            DROP INDEX idx_tasks_lane_status;
            DROP INDEX idx_task_notes_task_seq;
            DROP INDEX idx_checkpoints_task_seq;
            DROP INDEX idx_events_task_seq;
            DROP INDEX idx_handoffs_task_created;
            DROP INDEX idx_handoffs_status_created;
            DROP INDEX idx_handoffs_status_priority;
            DROP INDEX idx_attention_status_created;
            DROP INDEX idx_attention_task;
            DROP INDEX idx_attention_status_priority;
            DROP INDEX idx_task_tags_tag;
            DROP TABLE attention_tags;
            ALTER TABLE tasks DROP COLUMN archived_at;
            ALTER TABLE tasks DROP COLUMN archived;
            ALTER TABLE task_notes DROP COLUMN archived;
            ALTER TABLE checkpoints DROP COLUMN archived;
            ALTER TABLE events DROP COLUMN archived;
            ALTER TABLE handoffs DROP COLUMN priority;
            ALTER TABLE handoffs DROP COLUMN archived;
            ALTER TABLE attention DROP COLUMN priority;
            ALTER TABLE attention DROP COLUMN archived;
            ALTER TABLE task_tags DROP COLUMN archived;
            CREATE INDEX idx_tasks_status_priority ON tasks(status,priority,created_at);
            CREATE INDEX idx_tasks_parent ON tasks(parent_id);
            CREATE INDEX idx_tasks_assignee_status ON tasks(assignee,status);
            CREATE INDEX idx_tasks_lane_status ON tasks(lane,status);
            CREATE INDEX idx_task_notes_task_seq ON task_notes(task_id,seq);
            CREATE INDEX idx_checkpoints_task_seq ON checkpoints(task_id,seq);
            CREATE INDEX idx_events_task_seq ON events(task_id,seq);
            CREATE INDEX idx_handoffs_task_created ON handoffs(task_id,created_at);
            CREATE INDEX idx_handoffs_status_created ON handoffs(status,created_at);
            CREATE INDEX idx_attention_status_created ON attention(status,created_at);
            CREATE INDEX idx_attention_task ON attention(task_id);
            CREATE INDEX idx_task_tags_tag ON task_tags(tag);
            INSERT INTO status_updates VALUES(
              'u-11111111','driver-2',NULL,'claude@driver-2','first body',
              '/repo','main','abc123',NULL,'clean',1,1000
            );
            INSERT INTO status_updates VALUES(
              'u-22222222','driver-2',NULL,'codex@driver-2','second body',
              '/repo','main','def456',NULL,'1 file changed',0,2000
            );
            INSERT INTO events(task_id,kind,actor,payload,created_at) VALUES(
              NULL,'status_posted','claude@driver-2',
              '{"statusID":"u-11111111","lane":"driver-2","archived":0}',1000
            );
            INSERT INTO events(task_id,kind,actor,payload,created_at) VALUES(
              NULL,'status_posted','codex@driver-2',
              '{"statusID":"u-22222222","lane":"driver-2","archived":1}',2000
            );
            PRAGMA user_version=9;
            "#,
        )
        .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM status_updates", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2,
        "the V9 setup wrote no rows, so the migration test would prove nothing"
    );
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        9,
        "the fixture was not actually held at V9"
    );
    drop(connection);

    let rows = fixture.ok_json(
        &fixture.main,
        &["sitrep", "list", "--db", &board, "--all", "--json"],
    );
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["id"], "sr-22222222");
    assert_eq!(rows[0]["body"], "second body");
    assert_eq!(rows[0]["archived"], false);
    assert_eq!(rows[1]["id"], "sr-11111111");
    assert_eq!(rows[1]["body"], "first body");
    assert_eq!(rows[1]["archived"], true);

    let connection = Connection::open(&board).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        20
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM rules", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0,
        "V11 did not recreate the rules table after the V10 fixture migrated"
    );
    let mut statement = connection
        .prepare("SELECT kind,payload FROM events WHERE kind='sitrep_posted' ORDER BY seq")
        .unwrap();
    let trail = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(trail.len(), 2, "V10 lost trail entries: {trail:?}");
    assert!(trail.iter().all(|(kind, _)| kind == "sitrep_posted"));
    assert!(trail[0].1.contains("\"sitrepID\":\"sr-11111111\""));
    assert!(trail[1].1.contains("\"sitrepID\":\"sr-22222222\""));
    assert!(
        trail
            .iter()
            .all(|(_, payload)| !payload.contains("statusID"))
    );
}

#[test]
fn a_plan_is_an_epic_whose_body_survives_being_revised() {
    // A plan is an epic: its body is the plan, its children are the work it
    // became, and `draft` is a plan saved up but not ready to act on. Two
    // things had to be true for that to hold -- a body can come from a file,
    // and revising one does not destroy what it replaced.
    let fixture = Fixture::new("plan");
    fixture.ok_json(&fixture.main, &["init", "--name", "PLAN", "--json"]);

    let first = "# Q4 migration\n\n## Phase 1\nEnumerate every consumer.\n";
    let plan_file = fixture.root.join("plan.md");
    fs::write(&plan_file, first).unwrap();

    let plan = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Q4 migration",
            "--id",
            "e-plan",
            "--type",
            "epic",
            "--status",
            "draft",
            "--body-file",
            plan_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(plan["status"], "draft", "a plan saved up is not yet work");
    assert_eq!(plan["body"], first, "the body did not come from the file");

    // The work it becomes hangs beneath it, so "what did this plan produce" is
    // answered by the tree rather than by a link nobody maintains.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Phase 1", "--id", "e-phase1", "--type", "epic", "--parent", "e-plan",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Enumerate consumers",
            "--id",
            "t-enum",
            "--parent",
            "e-phase1",
            "--json",
        ],
    );

    // Revise it. The previous plan has to survive, or a revision is a deletion
    // with extra steps.
    let second = "# Q4 migration\n\n## Phase 1\nEnumerate every consumer, with receipts.\n";
    fs::write(&plan_file, second).unwrap();
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "e-plan",
            "--as",
            "geo",
            "--body-file",
            plan_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "e-plan", "--json"])["body"],
        second
    );

    let events = fixture.ok_json(&fixture.main, &["events", "--task", "e-plan", "--json"]);
    let updated = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "task_updated")
        .expect("the revision was not recorded");
    assert_eq!(
        updated["payload"]["changed"],
        json!(["body"]),
        "the trail must name what moved, not just that something did"
    );
    assert_eq!(
        updated["payload"]["previousBody"], first,
        "the plan it replaced was destroyed"
    );

    // A change that is not the body records what moved and carries no body.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "e-plan",
            "--as",
            "geo",
            "--title",
            "Q4 migration programme",
            "--json",
        ],
    );
    let latest = fixture.ok_json(&fixture.main, &["events", "--task", "e-plan", "--json"]);
    let title_only = latest
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "task_updated" && e["payload"]["changed"] == json!(["title"]))
        .expect("a title-only change was not recorded");
    assert!(
        title_only["payload"]["previousBody"].is_null(),
        "a body was recorded for a change that did not touch it"
    );

    // Two answers to one question is refused rather than ranked.
    let both = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "Ambiguous",
            "--body",
            "inline",
            "--body-file",
            plan_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        !both.status.success(),
        "--body and --body-file were both taken"
    );
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("pass one"),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );

    // A body file that is not there is an error, not an empty plan.
    let missing = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "Ghost",
            "--body-file",
            "/nonexistent/plan.md",
            "--json",
        ],
    );
    assert!(!missing.status.success(), "a missing body file was ignored");
}

/// Make `dir` a git repository with one commit, so provenance has something to
/// resolve. Returns false when git is unavailable, which is not a test failure.
fn make_repo(dir: &Path) -> bool {
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !run(&["init", "-q", "-b", "work"]) {
        return false;
    }
    fs::write(dir.join("seed.txt"), "seed").unwrap();
    run(&["add", "-A"]) && run(&["commit", "-qm", "seed"])
}

fn commit_all(dir: &Path, message: &str) -> bool {
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    };
    run(&["add", "-A"]) && run(&["commit", "-qm", message])
}

#[test]
fn where_work_happened_is_captured_rather_than_asked_for() {
    // The columns for this existed and were empty: measured across the live
    // boards, 0 of 20 checkpoints carried a HEAD sha, because filling them
    // meant passing --repo --branch --head by hand and nobody did.
    let fixture = Fixture::new("provenance");
    fs::create_dir_all(&fixture.main).unwrap();
    if !make_repo(&fixture.main) {
        eprintln!("git unavailable; skipping provenance assertions");
        return;
    }
    fixture.ok_json(&fixture.main, &["init", "--name", "PROV", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Work",
            "--id",
            "t-1",
            "--as",
            "claude@solo",
            "--json",
        ],
    );

    // A claim now says where it was taken, which on a box running several lanes
    // of one repository is the first question anyone asks.
    let claim = fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let worktree = claim["worktree"].as_str().expect("no worktree recorded");
    assert!(
        worktree.ends_with("main"),
        "the recorded worktree is not where the command ran: {worktree}"
    );
    assert_eq!(claim["branch"], "work");
    assert_eq!(claim["worktreeKind"], "main");
    let claimed_head = claim["headSha"].as_str().unwrap().to_owned();
    assert_eq!(
        claimed_head.len(),
        40,
        "a HEAD sha should be a full object id"
    );

    // A heartbeat is a fresh receipt, not merely a longer expiry stamped onto
    // the checkout where the claim was first taken.
    fs::write(fixture.main.join("after-claim.txt"), "new head").unwrap();
    assert!(commit_all(&fixture.main, "advance after claim"));
    let token = claim["leaseToken"].as_str().unwrap().to_owned();
    let heartbeat = fixture.ok_json(
        &fixture.main,
        &[
            "heartbeat",
            "t-1",
            "--lease",
            &token,
            "--lease-minutes",
            "30",
            "--json",
        ],
    );
    assert_ne!(heartbeat["headSha"], claimed_head);
    let current_head = Command::new("git")
        .arg("-C")
        .arg(&fixture.main)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        heartbeat["headSha"],
        String::from_utf8(current_head.stdout).unwrap().trim()
    );

    // Creating a task is attributable. Every other event kind recorded who did
    // it; this one could not, because there was no --as to record.
    let events = fixture.ok_json(&fixture.main, &["events", "--task", "t-1", "--json"]);
    let added = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "task_added")
        .expect("no task_added event");
    assert_eq!(
        added["actor"], "claude@solo",
        "the creator was not recorded"
    );
    assert_eq!(added["payload"]["type"], "task");

    // A checkpoint fills the columns that used to be null.
    let checkpoint = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    assert_eq!(checkpoint["branch"], "work");
    assert!(!checkpoint["headSha"].is_null(), "head was not captured");
    assert!(
        !checkpoint["repoPath"].is_null(),
        "repo path was not captured"
    );
    assert_eq!(
        checkpoint["dirtySummary"], "clean",
        "a clean tree should say so"
    );

    // An explicit flag still wins: capture is a default, not an override.
    let explicit = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--branch",
            "stated-by-hand",
            "--json",
        ],
    );
    assert_eq!(explicit["branch"], "stated-by-hand");
}

#[test]
fn a_command_outside_a_repository_records_no_provenance() {
    // Capture is opportunistic. Running outside a repository is not an error --
    // it simply has no git context, and recording none is the truthful outcome.
    let fixture = Fixture::new("provenance-none");
    fs::create_dir_all(&fixture.main).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "NONE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );

    let claim = fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    assert!(claim["worktree"].is_null(), "provenance was invented");
    assert!(claim["branch"].is_null());
    assert!(claim["headSha"].is_null());

    // And the command itself is unaffected.
    assert_eq!(claim["taskID"], "t-1");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["status"],
        "in_progress"
    );
}

#[test]
fn work_under_an_unopened_plan_is_not_handed_to_a_driver() {
    // A plan is an epic, so drafting a plan and hanging work under it produced
    // tasks that were immediately claimable: whether the plan was ready was
    // recorded on the plan and consulted by nobody. A draft protected the row
    // it sat on and nothing beneath it.
    let fixture = Fixture::new("draft-gate");
    fixture.ok_json(&fixture.main, &["init", "--name", "GATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Q4 plan", "--id", "e-plan", "--type", "epic", "--status", "draft",
            "--json",
        ],
    );
    // Two levels down, because a plan holds sub-plans and the gate has to reach
    // through them rather than only checking the immediate parent.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Phase 1", "--id", "e-p1", "--type", "epic", "--parent", "e-plan",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "The work", "--id", "t-work", "--parent", "e-p1", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Unrelated", "--id", "t-free", "--json"],
    );

    // --next steps over the whole drafted tree and finds the open work instead.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "driver-1", "--json"]
        )["taskID"],
        "t-free",
        "a driver was handed work from a plan nobody had opened"
    );

    // Naming it explicitly is refused, and the refusal names the plan and the
    // command that opens it -- an agent told only "no" has no next move.
    let refused = fixture.run(
        &fixture.main,
        &["claim", "t-work", "--as", "driver-2", "--json"],
    );
    assert!(!refused.status.success(), "work under a draft was claimed");
    let error = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(
        error.contains("e-plan"),
        "the draft ancestor is not named: {error}"
    );
    assert!(error.contains("draft"), "{error}");
    assert!(error.contains("task move e-plan todo"), "{error}");

    // Opening the plan makes its work available to any driver.
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "e-plan", "todo", "--as", "geo", "--json"],
    );
    let claimed = fixture.ok_json(
        &fixture.main,
        &["claim", "t-work", "--as", "driver-2", "--json"],
    );
    assert_eq!(claimed["taskID"], "t-work");
    assert_eq!(claimed["agentID"], "driver-2");

    // And a second driver gets the ordinary already-claimed refusal, not the
    // draft one -- drivers are just identities competing for the same work.
    let contested = fixture.run(
        &fixture.main,
        &["claim", "t-work", "--as", "driver-3", "--json"],
    );
    assert!(!contested.status.success());
    assert!(
        String::from_utf8_lossy(&contested.stderr).contains("already claimed"),
        "{}",
        String::from_utf8_lossy(&contested.stderr)
    );

    // Every driver sees the same board, drafts included. Visibility was never
    // per-driver and is not being made so: a draft is hidden from the queue,
    // not from the reader.
    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 4);
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["task", "list", "--status", "draft", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        0,
        "e-plan was opened above, so nothing should still read as draft"
    );
}

#[test]
fn a_tag_is_a_master_file_entry_before_it_is_a_label() {
    // Free-text labels are how one subsystem ends up spelled four ways --
    // `infra`, `Infra`, `infrastructure`, `infra-` -- and a board that answers
    // "show me infra" with three of the four is worse than one with no tags at
    // all, because the answer looks complete. So a tag exists in a per-board
    // master file first, and only a registered tag can be attached.
    let fixture = Fixture::new("tags");
    fixture.ok_json(&fixture.main, &["init", "--name", "TAGS", "--json"]);

    let registered = fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "add",
            "infra",
            "--description",
            "hosts, containers, deploys",
            "--as",
            "geo",
            "--json",
        ],
    );
    assert_eq!(registered["name"], "infra");
    assert_eq!(registered["description"], "hosts, containers, deploys");
    assert_eq!(registered["createdBy"], "geo");
    assert_eq!(registered["uses"], 0);

    // Registering the same concept twice is the collision the file exists to
    // prevent, so it is refused rather than treated as an upsert -- a silent
    // second add would quietly discard the first one's description.
    let again = fixture.run(&fixture.main, &["tag", "add", "infra", "--json"]);
    assert!(!again.status.success(), "a tag was registered twice");
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already in the master file"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    // The shape is fixed at the door: `Infra` is refused, not folded to `infra`,
    // because folding decides on the caller's behalf which spelling was meant.
    let shouted = fixture.run(&fixture.main, &["tag", "add", "Infra", "--json"]);
    assert!(!shouted.status.success(), "an uppercase tag was registered");
    assert!(
        String::from_utf8_lossy(&shouted.stderr).contains("one concept"),
        "{}",
        String::from_utf8_lossy(&shouted.stderr)
    );

    fixture.ok_json(&fixture.main, &["tag", "add", "queuer", "--json"]);
    fixture.ok_json(&fixture.main, &["tag", "add", "askie", "--json"]);

    // Every row type carries tags, because the axis is "which subsystem" and a
    // plan belongs to one as much as the task it produces does.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Queue rework",
            "--id",
            "e-plan",
            "--type",
            "epic",
            "--status",
            "draft",
            "--tag",
            "queuer",
            "--tag",
            "infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Retry backoff",
            "--id",
            "t-retry",
            "--parent",
            "e-plan",
            "--tag",
            "queuer",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Chat replies",
            "--id",
            "t-chat",
            "--tag",
            "askie",
            "--json",
        ],
    );

    let plan = fixture.ok_json(&fixture.main, &["task", "show", "e-plan", "--json"]);
    assert_eq!(
        plan["tags"],
        json!(["infra", "queuer"]),
        "a draft epic must carry its tags, and read back sorted"
    );

    // An unregistered tag is refused at the point of use, naming the nearest
    // registered one and the command that would make this one real -- the same
    // shape as a mistyped flag, because it is the same mistake.
    let typo = fixture.run(
        &fixture.main,
        &[
            "task", "update", "t-chat", "--tag", "askiee", "--as", "geo", "--json",
        ],
    );
    assert!(!typo.status.success(), "an unregistered tag was attached");
    let error = String::from_utf8_lossy(&typo.stderr).to_string();
    assert!(error.contains("master file"), "{error}");
    assert!(error.contains("did you mean askie?"), "{error}");
    assert!(error.contains("tag add askiee"), "{error}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-chat", "--json"])["tags"],
        json!(["askie"]),
        "a refused update must leave the existing tags alone"
    );

    // Listing narrows by tag, and the count is the tag's own answer to
    // "is anyone using this".
    let queuer = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--tag", "queuer", "--json"],
    );
    let ids = queuer
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(
        ids.contains(&"e-plan") && ids.contains(&"t-retry"),
        "{ids:?}"
    );

    let listed = fixture.ok_json(&fixture.main, &["tag", "list", "--json"]);
    let uses = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["name"].as_str().unwrap(), row["uses"].as_i64().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        uses,
        vec![("askie", 1), ("infra", 1), ("queuer", 2)],
        "tag list must report real use counts, sorted by name"
    );

    // Filtering by a tag nobody registered is refused rather than answered with
    // an empty list: an empty list reads as "nothing is tagged that", which is
    // exactly how a typo becomes a wrong answer somebody acts on.
    let ghost = fixture.run(&fixture.main, &["task", "list", "--tag", "infr", "--json"]);
    assert!(!ghost.status.success(), "an unregistered filter answered");
    let ghost_error = String::from_utf8_lossy(&ghost.stderr).to_string();
    assert!(ghost_error.contains("did you mean infra?"), "{ghost_error}");
    assert!(ghost_error.contains("read like an answer"), "{ghost_error}");

    // --tag replaces wholesale rather than appending, and --clear-tags is the
    // way to say "none" -- passing both is two answers to one question.
    let both = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "e-plan",
            "--tag",
            "infra",
            "--clear-tags",
            "--as",
            "geo",
            "--json",
        ],
    );
    assert!(
        !both.status.success(),
        "--tag and --clear-tags were both taken"
    );
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("mutually exclusive"),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );

    let replaced = fixture.ok_json(
        &fixture.main,
        &[
            "task", "update", "e-plan", "--tag", "infra", "--as", "geo", "--json",
        ],
    );
    assert_eq!(
        replaced["tags"],
        json!(["infra"]),
        "--tag must replace, not append"
    );

    // Retiring a tag that rows still carry would strip them silently, so it is
    // refused and says how many -- the operator gets the number they need to
    // decide, not just a no.
    let in_use = fixture.run(&fixture.main, &["tag", "remove", "queuer", "--json"]);
    assert!(!in_use.status.success(), "an in-use tag was retired");
    let in_use_error = String::from_utf8_lossy(&in_use.stderr).to_string();
    assert!(in_use_error.contains("carried by 1 row"), "{in_use_error}");
    assert!(in_use_error.contains("--force"), "{in_use_error}");

    fixture.ok_json(
        &fixture.main,
        &[
            "tag", "remove", "queuer", "--force", "--as", "geo", "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-retry", "--json"])["tags"],
        json!([]),
        "forcing a removal must strip the tag from the rows that carried it"
    );

    // Both halves of the master file land in the audit trail, because a tag
    // vanishing from every row it labelled is exactly the change someone will
    // later need explained.
    let events = fixture.ok_json(&fixture.main, &["events", "--limit", "50", "--json"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"tag_added"), "{kinds:?}");
    assert!(kinds.contains(&"tag_removed"), "{kinds:?}");
    let removal = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "tag_removed")
        .expect("the removal must be recorded");
    assert_eq!(removal["payload"]["tag"], "queuer");
    assert_eq!(
        removal["payload"]["strippedFrom"], 1,
        "the trail must say how many rows lost the tag"
    );

    // And the master file is per board: a second project starts empty rather
    // than inheriting a vocabulary that was never about it.
    fixture.ok_json(&fixture.worktree, &["init", "--name", "OTHER", "--json"]);
    assert_eq!(
        fixture
            .ok_json(&fixture.worktree, &["tag", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        0,
        "tags must not leak between boards"
    );
}

#[test]
fn a_project_whose_tree_moved_is_reported_rather_than_silently_unreachable() {
    // Registration canonicalises, so a stored root is right when written and
    // can only go wrong afterwards. This repository is the worked example: it
    // was moved into the dotfiles and a symlink left at the old path, and from
    // that moment no directory inside it resolved to its own board. The
    // database was perfect throughout and `doctor` said healthy.
    let fixture = Fixture::new("moved-tree");
    let original = fixture.root.join("project");
    let lane = original.join("lane");
    let moved = fixture.root.join("elsewhere");
    fs::create_dir_all(&lane).unwrap();

    fixture.ok_json(&original, &["init", "--name", "MOVED", "--json"]);
    let board_path =
        fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
            .as_str()
            .unwrap()
            .to_owned();
    fixture.ok_json(
        &lane,
        &[
            "workspace",
            "attach",
            "--to",
            original.to_str().unwrap(),
            "--json",
        ],
    );
    fixture.ok_json(
        &original,
        &["task", "add", "Real work", "--id", "t-1", "--json"],
    );

    // The move: the tree goes elsewhere and a symlink stands where it was, so
    // every path anyone has written down still works at the shell.
    fs::rename(&original, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &original).unwrap();
    assert!(original.join("lane").is_dir(), "the symlink must be usable");

    // And yet the board is gone from the inside: the caller's cwd resolves to
    // the new physical path, and the registry only knows the old spelling.
    let lost = fixture.run(&original, &["task", "list", "--json"]);
    assert!(
        !lost.status.success(),
        "the board resolved by cwd, so this test is no longer testing the defect"
    );
    assert!(
        String::from_utf8_lossy(&lost.stderr).contains("no Kanban project contains"),
        "{}",
        String::from_utf8_lossy(&lost.stderr)
    );

    // Doctor reports the stale discovery hints, but a root is not board
    // identity: the healthy board remains reachable by its global name.
    let sick = fixture.run(&original, &["doctor", "--json"]);
    assert!(
        sick.status.success(),
        "an unreachable discovery hint failed board integrity: {}",
        String::from_utf8_lossy(&sick.stderr)
    );
    let report: Value = serde_json::from_slice(&sick.stdout).unwrap();
    assert_eq!(report["healthy"], true);
    let roots = report["unreachableRoots"].as_array().unwrap();
    assert_eq!(
        roots.len(),
        2,
        "the project root and the lane beneath it both broke: {roots:?}"
    );
    let project_root = roots
        .iter()
        .find(|item| item["boardPath"] == board_path)
        .expect("the project root must be named");
    assert_eq!(project_root["name"], "MOVED");
    assert_eq!(
        project_root["resolvesTo"],
        moved.canonicalize().unwrap().to_string_lossy().into_owned(),
        "the report must say where the path leads now, not merely that it is wrong"
    );

    // Repointing takes every broken row by default, because one tree moving
    // breaks its root and each lane beneath it at once.
    let fixed = fixture.ok_json(&original, &["workspace", "repoint", "--json"]);
    assert_eq!(fixed.as_array().unwrap().len(), 2);

    // The board is reachable from the inside again, and it is the same board --
    // repointing changes one path's spelling and nothing about identity.
    let rows = fixture.ok_json(&original, &["task", "list", "--json"]);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], "t-1");
    assert_eq!(
        fixture.ok_json(&lane, &["task", "list", "--json"])[0]["id"],
        "t-1",
        "the lane alias must resolve to the same board it always did"
    );
    assert!(
        fixture.ok_json(&original, &["doctor", "--json"])["unreachableRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // A second repoint has nothing to do and says so rather than reporting a
    // successful no-op, which would read as a repair that happened.
    let again = fixture.run(&original, &["workspace", "repoint", "--json"]);
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("nothing to repoint"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    // A root that is simply gone has nowhere to be repointed to, and guessing
    // would be worse than the gap.
    fs::remove_file(&original).unwrap();
    fs::rename(&moved, fixture.root.join("gone")).unwrap();
    let vanished = fixture.run(&fixture.root, &["workspace", "repoint", "--json"]);
    assert!(!vanished.status.success(), "a deleted root was repointed");
    assert!(
        String::from_utf8_lossy(&vanished.stderr).contains("nowhere to repoint"),
        "{}",
        String::from_utf8_lossy(&vanished.stderr)
    );
}

#[test]
fn an_intentionally_retired_worktree_leaves_auditable_registry_history() {
    let fixture = Fixture::new("detach-worktree");
    let project = fixture.root.join("project");
    let retired = fixture.root.join("retired-lane");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&retired).unwrap();

    let registered = fixture.ok_json(&project, &["init", "--name", "DETACH", "--json"]);
    let project_root = registered["workspaceRoots"][0]
        .as_str()
        .expect("registered project root")
        .to_owned();
    let attached = fixture.ok_json(
        &retired,
        &["workspace", "attach", "--to", &project_root, "--json"],
    );
    let retired_root = attached["rootPath"]
        .as_str()
        .expect("attached root path")
        .to_owned();
    fixture.ok_json(
        &project,
        &["task", "add", "Kept work", "--id", "t-kept", "--json"],
    );
    fs::remove_dir_all(&retired).unwrap();

    let detached = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &retired_root,
            "--as",
            "geo",
            "--json",
        ],
    );
    assert_eq!(detached["rootPath"], retired_root);
    assert_eq!(detached["archived"], true);
    assert_eq!(detached["archivedBy"], "geo");
    assert!(detached["archivedAt"].as_i64().is_some());
    let lifecycle = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_detached",
            "--json",
        ],
    );
    assert_eq!(lifecycle[0]["actor"], "geo");
    assert_eq!(lifecycle[0]["payload"]["rootPath"], retired_root);

    let active = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        active
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["rootPath"] != retired_root)
    );
    let all = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|row| { row["rootPath"] == retired_root && row["archived"] == true })
    );
    assert_eq!(
        fixture.ok_json(&project, &["task", "show", "t-kept", "--json"])["id"],
        "t-kept",
        "detaching an alias changed or orphaned its board"
    );
    assert!(
        fixture.ok_json(&project, &["doctor", "--json"])["unreachableRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let detached_root = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &project_root,
            "--as",
            "geo",
            "--json",
        ],
    );
    assert_eq!(detached_root["rootPath"], project_root);
    assert_eq!(detached_root["archived"], true);
    assert_eq!(detached_root["archivedBy"], "geo");
    let doctor = fixture.ok_json(&fixture.root, &["doctor", "--json"]);
    let detached_project = doctor["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "DETACH")
        .expect("the detached board must still be listed");
    assert_eq!(detached_project["rootless"], true);
    assert!(
        detached_project["workspaceRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.root,
            &["task", "show", "t-kept", "--project", "DETACH", "--json"]
        )["id"],
        "t-kept",
        "a board with no roots must remain reachable by name"
    );

    let twice = fixture.run(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &retired_root,
            "--as",
            "geo",
            "--json",
        ],
    );
    assert!(!twice.status.success());
    assert!(String::from_utf8_lossy(&twice.stderr).contains("already detached"));

    // Disposable lane paths are reused after rebuilds. The retired row must
    // not occupy the active table's primary key forever.
    fs::create_dir_all(&retired).unwrap();
    let reattached = fixture.ok_json(
        &retired,
        &["workspace", "attach", "--to", "DETACH", "--json"],
    );
    assert_eq!(reattached["rootPath"], retired_root);
    assert_eq!(reattached["archived"], false);
    let history = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    assert_eq!(
        history
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["rootPath"] == retired_root)
            .count(),
        2,
        "reattaching a reused path erased or replaced its retired history"
    );
}

/// One HTTP request to the running server, as a real socket conversation.
///
/// Hand-rolled rather than reached for a client crate: this must exercise the
/// wire, and a test dependency that speaks HTTP for us would be one more thing
/// between the assertion and what the server actually wrote.
fn http_get(port: u16, path: &str) -> (u16, String) {
    use std::io::{Read, Write as _};
    use std::net::TcpStream;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to kanban serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

fn http_post(port: u16, path: &str, origin: &str, body: &[u8]) -> (u16, String) {
    use std::io::{Read, Write as _};
    use std::net::TcpStream;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to kanban serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, text)
}

fn read_http_head(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 16_384, "HTTP response headers never ended");
    }
    String::from_utf8(bytes).unwrap()
}

fn read_ws_text(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(head[0] & 0x0f, 1, "expected a text frame: {head:?}");
    assert_eq!(head[1] & 0x80, 0, "server frames must not be masked");
    let mut length = u64::from(head[1] & 0x7f);
    if length == 126 {
        let mut extended = [0_u8; 2];
        stream.read_exact(&mut extended).unwrap();
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0_u8; 8];
        stream.read_exact(&mut extended).unwrap();
        length = u64::from_be_bytes(extended);
    }
    let mut payload = vec![0_u8; length as usize];
    stream.read_exact(&mut payload).unwrap();
    String::from_utf8(payload).unwrap()
}

#[test]
fn the_served_pages_read_the_real_boards_and_write_to_none_of_them() {
    browser_loopback_reservation_supported()
        .expect("reserve loopback port for browser-backed server tests");
    // Approvals are the bottleneck, and settling one meant being at a terminal
    // with the right board addressed. This is the page that answers "what is
    // waiting on me" across every board at once. Phase 1 is read-only, and the
    // load-bearing claim is that it stays that way.
    let fixture = Fixture::new("serve");
    fixture.ok_json(&fixture.main, &["init", "--name", "SERVED", "--json"]);
    let other = fixture.root.join("other");
    fs::create_dir_all(&other).unwrap();
    fixture.ok_json(&other, &["init", "--name", "OTHER", "--json"]);
    fixture.ok_json(&fixture.main, &["tag", "add", "infra", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Plan the migration",
            "--id",
            "e-plan",
            "--type",
            "epic",
            "--status",
            "draft",
            "--body",
            "## Why\nBecause the queue is wrong.",
            "--tag",
            "infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Gated work",
            "--id",
            "t-gated",
            "--parent",
            "e-plan",
            "--json",
        ],
    );
    // A title carrying markup, because every row on a board is written by an
    // agent and rendering one unescaped would run script in the operator's
    // browser -- against a page that from phase 2 can approve things.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "<script>alert('xss')</script>",
            "--id",
            "t-hostile",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Staging push needs your call <b>now</b>",
            "--as",
            "claude@driver-1",
            "--kind",
            "approval",
            "--task",
            "t-hostile",
            "--json",
        ],
    );
    let live_rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Never render <b>rules</b> without escaping.\n\nThe full body is visible.",
            "--as",
            "geo",
            "--json",
        ],
    );
    let task_only_rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "TASK TAG RULE MUST NOT RENDER WITHOUT A TASK",
            "--tag",
            "infra",
            "--as",
            "geo",
            "--json",
        ],
    );
    let global_rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Global <em>rule</em> inherited everywhere.",
            "--as",
            "geo",
            "--json",
        ],
    );
    let other_only = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "OTHER ONLY MUST NOT RENDER",
            "--board",
            "OTHER",
            "--as",
            "geo",
            "--json",
        ],
    );
    let all_except_served = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "ALL EXCEPT SERVED MUST NOT RENDER",
            "--except-board",
            "SERVED",
            "--as",
            "geo",
            "--json",
        ],
    );
    let retired_rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "RETIRED RULE MUST NOT RENDER",
            "--as",
            "geo",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "retire",
            retired_rule["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--json",
        ],
    );

    let deployment = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/kanban",
            "--commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://kb.geoy.ws",
            "--as",
            "codex@driver",
            "--json",
        ],
    );
    let deployment_id = deployment["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &deployment_id,
            "--token",
            deployment["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--receipt",
            "served bundle carried exact release",
            "--as",
            "codex@driver",
            "--json",
        ],
    );

    let server = spawn_server(&fixture);
    let port = server.port;

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = fs::read(&board).unwrap();

    // The landing page is the reason this exists.
    let (status, home) = http_get(port, "/");
    assert_eq!(status, 200, "{home}");
    assert!(home.contains("Needs you"), "{home}");
    assert!(home.contains("approval"), "{home}");
    assert!(home.contains("claude@driver-1"), "{home}");
    assert!(home.contains("SERVED"), "{home}");

    // Agent-authored text is escaped everywhere it lands, and the item's own
    // body is agent-authored too.
    assert!(
        home.contains("&lt;b&gt;now&lt;/b&gt;"),
        "attention body was not escaped: {home}"
    );
    assert!(
        !home.contains("<b>now</b>"),
        "raw markup reached the page: {home}"
    );

    let (status, boards) = http_get(port, "/boards");
    assert_eq!(status, 200);
    assert!(boards.contains("SERVED"), "{boards}");

    let (status, deployments) = http_get(port, "/deployments");
    assert_eq!(status, 200, "{deployments}");
    assert!(deployments.contains("Current releases"), "{deployments}");
    assert!(deployments.contains("geoyws/kanban"), "{deployments}");
    assert!(deployments.contains("aaaaaaaaaaaa"), "{deployments}");
    let (status, deployment_detail) =
        http_get(port, &format!("/deployment/SERVED/{deployment_id}"));
    assert_eq!(status, 200, "{deployment_detail}");
    assert!(deployment_detail.contains("served bundle carried exact release"));

    let (status, search) = http_get(port, "/search?q=migration");
    assert_eq!(status, 200, "{search}");
    assert!(search.contains("Search"), "{search}");
    assert!(search.contains("Plan the migration"), "{search}");
    assert!(
        search.contains("kanban://SERVED/task/e-plan"),
        "search omitted its source citation: {search}"
    );

    // Plans: a draft epic is the plan, and it names the work it holds back.
    // Lanes: what the agents are doing, the counterpart to what waits on the
    // operator. A sitrep needs no task and no lease, which is the whole point.
    // A second lane, posted first, so the page's ordering is observable.
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "An older lane that has since gone quiet.",
            "--as",
            "claude@driver-9",
            "--lane",
            "aaa-quiet-lane",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Queue rework <i>underway</i>; retry path is the culprit.",
            "--as",
            "claude@driver-2",
            "--lane",
            "driver-2",
            "--json",
        ],
    );
    let (status, lanes) = http_get(port, "/lanes");
    assert_eq!(status, 200);
    assert!(lanes.contains("driver-2"), "{lanes}");
    // Most recently active first. Nothing deletes a sitrep, so a lane
    // whose driver is long gone keeps its rows forever -- alphabetical order
    // would park it permanently above the lanes actually working.
    assert!(
        lanes.find("driver-2").unwrap() < lanes.find("aaa-quiet-lane").unwrap(),
        "a quiet lane sorted above an active one: {lanes}"
    );
    assert!(lanes.contains("retry path is the culprit"), "{lanes}");
    assert!(
        lanes.contains("&lt;i&gt;underway&lt;/i&gt;"),
        "a sitrep body was not escaped: {lanes}"
    );
    assert!(!lanes.contains("<i>underway</i>"), "{lanes}");

    let (status, plans) = http_get(port, "/plans");
    assert_eq!(status, 200);
    assert!(plans.contains("Plan the migration"), "{plans}");
    assert!(plans.contains("Because the queue is wrong."), "{plans}");
    assert!(
        plans.contains("t-gated"),
        "a plan must name the work it gates: {plans}"
    );
    assert!(plans.contains("infra"), "tags must render: {plans}");

    // The hostile title survives as text on every page that renders it.
    let (status, one) = http_get(port, "/board/SERVED");
    assert_eq!(status, 200);
    assert!(
        one.contains("&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"),
        "a task title was not escaped: {one}"
    );
    assert!(
        !one.contains("<script>alert"),
        "a script tag reached the page: {one}"
    );
    assert!(one.contains("<h2>Rules"), "{one}");
    assert!(!one.contains("Project rules"), "{one}");
    assert!(!one.contains("Global rules"), "{one}");
    assert!(one.contains(global_rule["id"].as_str().unwrap()), "{one}");
    assert!(
        one.contains("ALL"),
        "global rule target tag did not render: {one}"
    );
    assert!(!one.contains(other_only["id"].as_str().unwrap()), "{one}");
    assert!(
        !one.contains(all_except_served["id"].as_str().unwrap()),
        "{one}"
    );
    assert!(
        one.contains("Global &lt;em&gt;rule&lt;/em&gt; inherited everywhere."),
        "a global rule was not escaped: {one}"
    );
    assert!(one.contains(live_rule["id"].as_str().unwrap()), "{one}");
    assert!(
        !one.contains(task_only_rule["id"].as_str().unwrap()),
        "task-scoped rule leaked into a taskless board projection: {one}"
    );
    assert!(
        one.contains("Never render &lt;b&gt;rules&lt;/b&gt; without escaping."),
        "a rule headline was not escaped: {one}"
    );
    assert!(one.contains("The full body is visible."), "{one}");
    assert!(
        !one.contains("<b>rules</b>"),
        "raw rule markup reached the page: {one}"
    );
    assert!(
        !one.contains("RETIRED RULE MUST NOT RENDER"),
        "a retired rule remained in force on the board page: {one}"
    );

    let (status, detail) = http_get(port, "/task/SERVED/t-hostile");
    assert_eq!(status, 200);
    assert!(!detail.contains("<script>alert"), "{detail}");
    assert!(
        detail.contains("task_added"),
        "the trail must render: {detail}"
    );

    // A lease token is a capability. A read surface that renders one hands
    // whoever loads the page the ability to write.
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-hostile", "--as", "driver-1", "--json"],
    );
    let token = claim["leaseToken"].as_str().unwrap().to_owned();
    let (_, held) = http_get(port, "/task/SERVED/t-hostile");
    assert!(
        held.contains("driver-1"),
        "the holder must be named: {held}"
    );
    assert!(
        !held.contains(&token),
        "the lease token was rendered into a page"
    );

    // An address that names nothing is a page, not a dropped connection.
    let (status, missing) = http_get(port, "/task/SERVED/t-nope");
    assert_eq!(status, 500, "{missing}");
    assert!(missing.contains("t-nope"), "{missing}");
    let (status, nowhere) = http_get(port, "/no/such/page");
    assert_eq!(status, 200, "{nowhere}");
    assert!(nowhere.contains("Not found"), "{nowhere}");

    // The whole point of phase 1: after every page has been served, the board
    // file is byte-for-byte what it was. Compared against the state captured
    // before the reads, with the one deliberate CLI write above excluded by
    // being re-read here.
    let after = fs::read(&board).unwrap();
    assert_ne!(
        after, before,
        "the claim above must have changed the board, or this comparison proves nothing"
    );
    let settled = fs::read(&board).unwrap();
    for path in [
        "/",
        "/boards",
        "/plans",
        "/deployments",
        &format!("/deployment/SERVED/{deployment_id}"),
        "/board/SERVED",
        "/task/SERVED/t-gated",
    ] {
        let (status, _) = http_get(port, path);
        assert_eq!(status, 200, "{path}");
    }
    assert_eq!(
        fs::read(&board).unwrap(),
        settled,
        "serving a page modified the board"
    );
}

#[test]
fn priority_orders_cli_dashboard_and_web_queues_before_age() {
    browser_loopback_reservation_supported()
        .expect("reserve loopback port for browser-backed server tests");
    let fixture = Fixture::new("priority-queues");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "PRIORITY-MAIN", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Routine task",
            "--id",
            "t-routine",
            "--priority",
            "P2",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Committed task",
            "--id",
            "t-committed",
            "--priority",
            "P1",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "routine attention",
            "--as",
            "agent",
            "--priority",
            "P2",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "committed attention",
            "--as",
            "agent",
            "--priority",
            "P1",
            "--json",
        ],
    );
    for (summary, priority) in [("routine handoff", "P2"), ("committed handoff", "P1")] {
        fixture.ok_json(
            &fixture.main,
            &[
                "handoff",
                "create",
                "--as",
                "agent",
                "--summary",
                summary,
                "--intent",
                "resume",
                "--next-action",
                "continue",
                "--priority",
                priority,
                "--json",
            ],
        );
    }

    let attention = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "open", "--json"],
    );
    assert_eq!(attention[0]["body"], "committed attention");
    assert_eq!(attention[1]["body"], "routine attention");
    let handoffs = fixture.ok_json(
        &fixture.main,
        &["handoff", "list", "--status", "pending", "--json"],
    );
    assert_eq!(handoffs[0]["summary"], "committed handoff");
    assert_eq!(handoffs[1]["summary"], "routine handoff");

    let urgent = fixture.root.join("urgent");
    fs::create_dir_all(&urgent).unwrap();
    fixture.ok_json(&urgent, &["init", "--name", "PRIORITY-URGENT", "--json"]);
    fixture.ok_json(
        &urgent,
        &[
            "attention",
            "raise",
            "interrupt attention",
            "--as",
            "agent",
            "--priority",
            "P0",
            "--json",
        ],
    );
    let dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert_eq!(dashboard[0]["name"], "PRIORITY-URGENT");
    assert_eq!(dashboard[0]["highestPriorityLevel"], "P0");
    assert_eq!(dashboard[1]["name"], "PRIORITY-MAIN");
    assert_eq!(dashboard[1]["highestPriorityLevel"], "P1");

    let server = spawn_server(&fixture);
    let port = server.port;

    let (status, home) = http_get(port, "/");
    assert_eq!(status, 200, "{home}");
    assert!(home.find("interrupt attention").unwrap() < home.find("committed attention").unwrap());
    assert!(home.find("committed attention").unwrap() < home.find("routine attention").unwrap());
    assert!(home.contains("priority-p0"), "{home}");
    assert!(home.contains("priority-p1"), "{home}");
    assert!(home.contains("priority-p2"), "{home}");

    let (status, board) = http_get(port, "/board/PRIORITY-MAIN");
    assert_eq!(status, 200, "{board}");
    assert!(board.find("t-committed").unwrap() < board.find("t-routine").unwrap());

    let (status, boards) = http_get(port, "/boards");
    assert_eq!(status, 200, "{boards}");
    assert!(boards.find("PRIORITY-URGENT").unwrap() < boards.find("PRIORITY-MAIN").unwrap());
}

#[test]
fn served_board_pages_fail_closed_on_duplicate_names() {
    let fixture = Fixture::new("serve-board-duplicate");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    seed_legacy_rootless_duplicate(&fixture, "alpha-rootless", "Alpha");

    let server = spawn_server(&fixture);
    let port = server.port;
    let (status, page) = http_get(port, "/board/Alpha");
    assert_eq!(status, 500, "{page}");
    assert!(page.contains("2 Kanban projects are named Alpha"), "{page}");
    assert!(page.contains("Alpha (rootless)"), "{page}");
}

#[test]
fn needs_you_replies_and_live_revisions_cross_the_real_server_process() {
    use std::net::TcpStream;

    browser_loopback_reservation_supported()
        .expect("reserve loopback port for browser-backed server tests");

    let fixture = Fixture::new("serve-reply-live");
    fixture.ok_json(&fixture.main, &["init", "--name", "SERVEWRITE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Approve this roadmap",
            "--type",
            "epic",
            "--status",
            "draft",
            "--id",
            "e-web-open",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Ship the rollout task",
            "--type",
            "story",
            "--parent",
            "e-web-open",
            "--id",
            "s-web-open",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Comment and resolve this",
            "--type",
            "epic",
            "--status",
            "draft",
            "--id",
            "e-browser-reply",
            "--json",
        ],
    );
    let approve = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Choose the rollout window",
            "--as",
            "codex@driver",
            "--kind",
            "decision",
            "--task",
            "e-web-open",
            "--json",
        ],
    );
    let reply = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Use the free-form note",
            "--as",
            "codex@driver",
            "--kind",
            "decision",
            "--task",
            "e-browser-reply",
            "--json",
        ],
    );
    let reject = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Need a second reviewer before this ships",
            "--as",
            "codex@driver-2",
            "--kind",
            "review",
            "--task",
            "s-web-open",
            "--json",
        ],
    );
    let approve_id = approve["id"].as_str().unwrap();
    let reply_id = reply["id"].as_str().unwrap();
    let reject_id = reject["id"].as_str().unwrap();
    let server = spawn_server(&fixture);
    let port = server.port;

    let (status, home) = http_get(port, "/");
    assert_eq!(status, 200, "{home}");
    assert!(home.contains("Your reply"), "{home}");
    assert!(home.contains("Approve"), "{home}");
    assert!(home.contains("Reject"), "{home}");
    assert!(
        home.contains("data-comment-label=\"Comment and Approve\""),
        "{home}"
    );
    assert!(
        home.contains("data-comment-label=\"Comment and Reject\""),
        "{home}"
    );
    assert!(home.contains("Comment and Resolve"), "{home}");
    assert!(home.contains("new WebSocket"), "{home}");
    assert!(
        home.contains(&format!("/attention/SERVEWRITE/{approve_id}/reply")),
        "{home}"
    );
    assert!(
        home.contains("about <a href=\"/task/SERVEWRITE/e-web-open\">Approve this roadmap</a>"),
        "{home}"
    );
    assert!(
        home.contains("span class=\"type type-epic\">epic</span>"),
        "{home}"
    );
    assert!(
        home.contains("about <a href=\"/task/SERVEWRITE/s-web-open\">Ship the rollout task</a>"),
        "{home}"
    );

    let path = format!("/attention/SERVEWRITE/{approve_id}/reply");
    let (status, _) = http_post(
        port,
        &path,
        "https://hostile.example",
        b"decision=approve&reply=Approved%2E+Proceed+after+the+backup%2E",
    );
    assert_eq!(status, 403, "a cross-origin form was accepted");
    let still_open = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "open", "--json"],
    );
    assert_eq!(still_open.as_array().unwrap().len(), 3);

    let origin = format!("http://127.0.0.1:{port}");
    let (status, plans) = http_get(port, "/plans");
    assert_eq!(status, 200, "{plans}");
    assert!(
        plans.contains("/plan/SERVEWRITE/e-web-open/open"),
        "the draft epic had no approval action: {plans}"
    );
    assert!(plans.contains("1 open attention"), "{plans}");
    assert!(plans.contains("Ship the rollout task"), "{plans}");
    assert!(plans.contains("s-web-open"), "{plans}");
    let (status, _) = http_post(
        port,
        "/plan/SERVEWRITE/e-web-open/open",
        "https://hostile.example",
        b"",
    );
    assert_eq!(status, 403, "a cross-origin plan approval was accepted");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "e-web-open", "--json"])["status"],
        "draft"
    );
    let (status, _) = http_post(
        port,
        &format!("/attention/NOBOARD/{approve_id}/reply"),
        &origin,
        b"decision=approve&reply=No+board",
    );
    assert_eq!(status, 404, "an unknown board write was accepted");
    let unresolved = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(unresolved.as_array().unwrap().len(), 3);
    let (status, task_page) = http_get(port, "/task/SERVEWRITE/e-web-open");
    assert_eq!(status, 200, "{task_page}");
    assert!(task_page.contains("Open attention"), "{task_page}");
    assert!(
        task_page.contains("Choose the rollout window"),
        "{task_page}"
    );
    let (status, board) = http_get(port, "/board/SERVEWRITE");
    assert_eq!(status, 200, "{board}");
    assert!(board.contains("1 open attention"), "{board}");
    assert!(board.contains("Ship the rollout task"), "{board}");
    let (status, opened) = http_post(port, "/plan/SERVEWRITE/e-web-open/open", &origin, b"");
    assert_eq!(status, 303, "{opened}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "e-web-open", "--json"])["status"],
        "todo"
    );
    let (status, duplicate) = http_post(port, "/plan/SERVEWRITE/e-web-open/open", &origin, b"");
    assert_eq!(status, 409, "a plan was opened twice: {duplicate}");

    let (status, _) = http_post(port, &path, &origin, b"reply=bad%XX");
    assert_eq!(status, 400, "malformed form data was accepted");
    let (status, response) = http_post(
        port,
        &path,
        &origin,
        b"decision=approve&reply=Approved%2E+Proceed+after+the+backup%2E",
    );
    assert_eq!(status, 303, "{response}");
    assert!(response.contains("Location: /?replied="), "{response}");
    let resolved = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    assert_eq!(resolved[0]["resolvedBy"], "geo");
    assert_eq!(
        resolved[0]["resolution"],
        "Decision: Approved. Proceed.\nComment: Approved. Proceed after the backup."
    );
    let reject_path = format!("/attention/SERVEWRITE/{reject_id}/reply");
    let (status, response) = http_post(
        port,
        &reject_path,
        &origin,
        b"decision=reject&reply=Needs+another+reviewer",
    );
    assert_eq!(status, 303, "{response}");
    let resolved = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    assert_eq!(resolved.as_array().unwrap().len(), 2);
    assert_eq!(
        resolved[1]["resolution"],
        "Decision: Declined. Do not proceed.\nComment: Needs another reviewer"
    );
    let reply_path = format!("/attention/SERVEWRITE/{reply_id}/reply");
    let (status, response) = http_post(
        port,
        &reply_path,
        &origin,
        b"decision=reply&reply=This+is+the+durable+note",
    );
    assert_eq!(status, 303, "{response}");
    let resolved = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    assert_eq!(resolved.as_array().unwrap().len(), 3);
    assert_eq!(
        resolved
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == reply_id)
            .expect("reply resolution")["resolution"],
        "Comment: This is the durable note"
    );
    let (status, _) = http_post(
        port,
        &path,
        &origin,
        b"decision=approve&reply=Resolve+twice",
    );
    assert_eq!(status, 409, "an already-resolved item changed twice");

    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    write!(
        socket,
        "GET /live HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
    .unwrap();
    let headers = read_http_head(&mut socket);
    assert!(headers.starts_with("HTTP/1.1 101"), "{headers}");
    assert!(
        headers.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "{headers}"
    );
    let ready: serde_json::Value = serde_json::from_str(&read_ws_text(&mut socket)).unwrap();
    assert_eq!(ready["type"], "ready");

    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "A new live item",
            "--as",
            "codex@driver-2",
            "--kind",
            "review",
            "--json",
        ],
    );
    let changed: serde_json::Value = serde_json::from_str(&read_ws_text(&mut socket)).unwrap();
    assert_eq!(changed["type"], "refresh", "{changed}");
    assert_ne!(changed["revision"], ready["revision"]);
}

#[test]
fn needs_you_comment_buttons_and_resolve_flow_work_in_real_chrome() {
    browser_loopback_reservation_supported()
        .expect("reserve loopback port for browser-backed server tests");
    let fixture = Fixture::new("serve-browser");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SERVE-BROWSER", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Approve this release",
            "--type",
            "epic",
            "--status",
            "draft",
            "--id",
            "e-browser-approve",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Reject this release",
            "--type",
            "epic",
            "--status",
            "draft",
            "--id",
            "e-browser-reject",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Comment and resolve this",
            "--type",
            "epic",
            "--status",
            "draft",
            "--id",
            "e-browser-reply",
            "--json",
        ],
    );
    let approve = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Approve from the browser",
            "--as",
            "codex@driver",
            "--kind",
            "decision",
            "--task",
            "e-browser-approve",
            "--json",
        ],
    );
    let reply = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Use the free-form note",
            "--as",
            "codex@driver",
            "--kind",
            "decision",
            "--task",
            "e-browser-reply",
            "--json",
        ],
    );
    let reject = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Reject from the browser",
            "--as",
            "codex@driver-2",
            "--kind",
            "review",
            "--task",
            "e-browser-reject",
            "--json",
        ],
    );
    let approve_id = approve["id"].as_str().unwrap();
    let reply_id = reply["id"].as_str().unwrap();
    let reject_id = reject["id"].as_str().unwrap();

    let server = spawn_server(&fixture);
    let origin = server.origin();

    let chrome = launch_browser(chrome_binary());
    let tab = chrome.new_tab().expect("initial tab");
    tab.navigate_to(&origin).expect("load Needs you");
    tab.wait_until_navigated().expect("initial navigation");

    let approve_textarea = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{approve_id}/reply\"] textarea[name=reply]"
        ))
        .expect("approve textarea");
    approve_textarea.click().expect("focus approve textarea");
    approve_textarea
        .type_into(" Approved. Proceed after the review. ")
        .expect("type approve comment");
    let approve_button = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{approve_id}/reply\"] button.quick.approve"
        ))
        .expect("approve button");
    assert_eq!(
        approve_button.get_inner_text().expect("approve label"),
        "Comment and Approve"
    );
    let reject_button = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{approve_id}/reply\"] button.quick.decline"
        ))
        .expect("reject button");
    assert_eq!(
        reject_button.get_inner_text().expect("reject label"),
        "Comment and Reject"
    );
    approve_button.click().expect("submit approve");
    tab.wait_until_navigated().expect("approve navigation");
    let approve_page = tab.get_content().expect("approve page");
    assert!(approve_page.contains("Reply recorded"), "{approve_page}");
    assert!(tab.get_url().contains("/?replied="), "{}", tab.get_url());
    let resolved = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    assert_eq!(resolved.as_array().unwrap().len(), 1);
    assert_eq!(resolved[0]["resolvedBy"], "geo");
    assert_eq!(
        resolved[0]["resolution"],
        "Decision: Approved. Proceed.\nComment: Approved. Proceed after the review."
    );

    tab.navigate_to(&origin).expect("reload Needs you");
    tab.wait_until_navigated().expect("reload navigation");
    let reject_textarea = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{reject_id}/reply\"] textarea[name=reply]"
        ))
        .expect("reject textarea");
    reject_textarea.click().expect("focus reject textarea");
    reject_textarea
        .type_into(" Needs a second reviewer ")
        .expect("type reject comment");
    let reject_approve = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{reject_id}/reply\"] button.quick.approve"
        ))
        .expect("reject approve button");
    assert_eq!(
        reject_approve
            .get_inner_text()
            .expect("reject approve label"),
        "Comment and Approve"
    );
    let reject_quick = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{reject_id}/reply\"] button.quick.decline"
        ))
        .expect("reject quick button");
    assert_eq!(
        reject_quick.get_inner_text().expect("reject quick label"),
        "Comment and Reject"
    );
    reject_quick.click().expect("submit reject");
    tab.wait_until_navigated().expect("reject navigation");
    let reject_page = tab.get_content().expect("reject page");
    assert!(reject_page.contains("Reply recorded"), "{reject_page}");
    let resolved = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    assert_eq!(resolved.as_array().unwrap().len(), 2);
    assert_eq!(
        resolved[1]["resolution"],
        "Decision: Declined. Do not proceed.\nComment: Needs a second reviewer"
    );

    tab.navigate_to(&origin)
        .expect("reload Needs you for reply");
    tab.wait_until_navigated().expect("reply reload navigation");
    let reply_textarea = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{reply_id}/reply\"] textarea[name=reply]"
        ))
        .expect("reply textarea");
    reply_textarea.click().expect("focus reply textarea");
    reply_textarea
        .type_into("  This is the durable note  ")
        .expect("type reply comment");
    let reply_button = tab
        .wait_for_element(&format!(
            "form.reply[action=\"/attention/SERVE-BROWSER/{reply_id}/reply\"] button.send"
        ))
        .expect("reply button");
    assert_eq!(
        reply_button.get_inner_text().expect("reply label"),
        "Comment and Resolve"
    );
    reply_button.click().expect("submit reply");
    tab.wait_until_navigated().expect("reply navigation");
    let reply_page = tab.get_content().expect("reply page");
    assert!(reply_page.contains("Reply recorded"), "{reply_page}");
    let resolved = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    let reply_resolved = resolved
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == reply_id)
        .expect("reply resolution")
        .clone();
    assert_eq!(resolved.as_array().unwrap().len(), 3);
    assert_eq!(
        reply_resolved["resolution"],
        "Comment: This is the durable note"
    );
}

#[test]
fn the_server_refuses_a_port_it_could_not_be_found_on() {
    // Port 0 asks the kernel to choose, which for a server nginx reaches by
    // number means listening somewhere nobody can find. An out-of-range value
    // cannot be bound at all. Both are refused before the socket call, so the
    // message names the flag rather than surfacing an opaque bind error.
    let fixture = Fixture::new("serve-port");
    fixture.ok_json(&fixture.main, &["init", "--name", "PORT", "--json"]);
    for bad in ["0", "-1", "65536", "999999999"] {
        let output = fixture.run(&fixture.main, &["serve", "--port", bad]);
        assert!(!output.status.success(), "--port {bad} was accepted");
        let error = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(
            error.contains("--port") || error.contains("port"),
            "--port {bad}: {error}"
        );
    }
}

#[test]
fn a_reader_that_hangs_up_ends_the_command_quietly() {
    // `kb task list --json | head` printed a Rust panic and a backtrace note
    // over the output it had just produced, and exited non-zero. Every other
    // Unix tool ends quietly when its reader leaves.
    //
    // The reader is closed here directly rather than through a shell pipeline,
    // because a pipeline's exit status is the LAST command's -- `| head` exits
    // 0 whatever happened upstream, so a shell test would have proved nothing
    // about kanban's own status. That is not hypothetical: the first version of
    // this test passed with the fix removed.
    let fixture = Fixture::new("broken-pipe");
    fixture.ok_json(&fixture.main, &["init", "--name", "PIPE", "--json"]);
    // Comfortably past a 64 KiB pipe buffer, so the writer is certain to still
    // be writing when the reader goes away. Bulk comes from long titles rather
    // than many rows: each row costs a process spawn, and 600 of them made this
    // the slowest test in the suite for no extra coverage.
    let padding = "x".repeat(2_000);
    for index in 0..60 {
        fixture.ok_json(
            &fixture.main,
            &["task", "add", &format!("Row {index} {padding}"), "--json"],
        );
    }

    let mut child = fixture
        .command(&fixture.main)
        .args(["task", "list", "--json"])
        .spawn()
        .unwrap();
    // Close the read end while the child is mid-write. This is exactly what
    // `head` does once it has what it wants.
    drop(child.stdout.take());
    let mut stderr = String::new();
    if let Some(mut handle) = child.stderr.take() {
        use std::io::Read as _;
        let _ = handle.read_to_string(&mut stderr);
    }
    let status = child.wait().unwrap();

    assert!(
        !stderr.contains("panicked"),
        "a closed pipe panicked: {stderr}"
    );
    assert!(
        !stderr.contains("Broken pipe"),
        "a closed pipe was reported as an error: {stderr}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "a closed pipe must exit 0, got {status:?} with stderr: {stderr}"
    );

    // A real error still fails, and still says so: the quiet exit is scoped to
    // the reader leaving, not to errors in general.
    let broken = fixture.run(&fixture.main, &["task", "show", "t-nope", "--json"]);
    assert!(!broken.status.success());
    assert!(
        String::from_utf8_lossy(&broken.stderr).contains("t-nope"),
        "{}",
        String::from_utf8_lossy(&broken.stderr)
    );
}

#[test]
fn a_sitrep_costs_one_command_and_retires_what_it_supersedes() {
    // A note needs a task. A checkpoint needs a task AND a lease. So an agent
    // working across several tasks, between them, or exploring before it has
    // claimed anything had nowhere to write down where things stand -- and it
    // went into a reply that scrolls away. This is the low-ceremony sibling of
    // a handoff: lane-keyed, no lease, no task required.
    let fixture = Fixture::new("sitrep");
    fixture.ok_json(&fixture.main, &["init", "--name", "SITREP", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Some work", "--id", "t-1", "--json"],
    );

    // No lease anywhere in this call, and no task.
    let first = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Reading the queue code; nothing changed yet.",
            "--as",
            "claude@driver-2",
            "--lane",
            "driver-2",
            "--json",
        ],
    );
    assert_eq!(first["lane"], "driver-2");
    assert_eq!(first["author"], "claude@driver-2");
    assert_eq!(first["archived"], false);
    assert!(first["id"].as_str().unwrap().starts_with("sr-"));
    assert_eq!(first["taskID"], serde_json::Value::Null);

    // A task link is optional, and a task that does not exist is refused --
    // a sitrep pointing at nothing would read as context and carry none.
    let linked = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Retry path is the culprit.",
            "--as",
            "claude@driver-2",
            "--lane",
            "driver-2",
            "--task",
            "t-1",
            "--json",
        ],
    );
    assert_eq!(linked["taskID"], "t-1");
    let ghost = fixture.run(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "About nothing",
            "--as",
            "a",
            "--lane",
            "driver-2",
            "--task",
            "t-nope",
            "--json",
        ],
    );
    assert!(
        !ghost.status.success(),
        "a sitrep pointed at a missing task"
    );
    assert!(
        String::from_utf8_lossy(&ghost.stderr).contains("t-nope"),
        "{}",
        String::from_utf8_lossy(&ghost.stderr)
    );

    // Lanes do not bleed into each other: the question is "where does THIS
    // lane stand", and another driver's updates are not an answer to it.
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Different lane entirely.",
            "--as",
            "claude@driver-1",
            "--lane",
            "driver-1",
            "--json",
        ],
    );
    let mine = fixture.ok_json(
        &fixture.main,
        &["sitrep", "list", "--lane", "driver-2", "--json"],
    );
    assert_eq!(mine.as_array().unwrap().len(), 2);
    // Newest first: the question this answers is "what is true now".
    assert_eq!(mine[0]["id"], linked["id"]);
    assert_eq!(mine[1]["id"], first["id"]);

    // Provenance is captured, not asked for -- an update saying "tests green"
    // without saying which checkout is a claim nobody can check. The fixture
    // is not a repository, so it is recorded as absent rather than invented.
    assert_eq!(mine[0]["branch"], serde_json::Value::Null);

    // Auto-archiving. Ten stay current per lane; the eleventh does not delete
    // the first, it retires it.
    for index in 0..12 {
        fixture.ok_json(
            &fixture.main,
            &[
                "sitrep",
                "post",
                &format!("Update number {index}"),
                "--as",
                "claude@driver-2",
                "--lane",
                "driver-2",
                "--json",
            ],
        );
    }
    let current = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep", "list", "--lane", "driver-2", "--limit", "100", "--json",
        ],
    );
    assert_eq!(
        current.as_array().unwrap().len(),
        10,
        "the current view must stay bounded without anything running on a timer"
    );
    assert_eq!(current[0]["body"], "Update number 11");

    // Retired, not destroyed: everything is still there on request.
    let everything = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep", "list", "--lane", "driver-2", "--all", "--limit", "100", "--json",
        ],
    );
    assert_eq!(everything.as_array().unwrap().len(), 14);
    assert!(
        everything
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == first["id"] && row["archived"] == true),
        "the oldest update must be archived and still readable"
    );

    // The other lane is untouched by all of that.
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["sitrep", "list", "--lane", "driver-1", "--all", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Archiving lands in the trail, so a reader can see the current view was
    // bounded rather than wonder where the rest went.
    let events = fixture.ok_json(&fixture.main, &["events", "--limit", "200", "--json"]);
    let posted = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "sitrep_posted")
        .count();
    assert_eq!(posted, 15, "every post must be recorded");
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "sitrep_posted"
                && event["payload"]["archived"].as_i64().unwrap_or(0) > 0),
        "the trail must record that an update was retired"
    );

    // A sitrep with no lane is refused rather than filed under a default: an
    // update nobody can address is one nobody will read.
    let laneless = fixture.run(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Where does this go?",
            "--as",
            "a",
            "--json",
        ],
    );
    assert!(!laneless.status.success(), "a laneless sitrep was accepted");
    assert!(
        String::from_utf8_lossy(&laneless.stderr).contains("lane"),
        "{}",
        String::from_utf8_lossy(&laneless.stderr)
    );

    // Empty prose is refused too. A sitrep that says nothing still
    // reads on the board as though the lane reported in.
    for empty in ["", "   "] {
        let blank = fixture.run(
            &fixture.main,
            &[
                "sitrep", "post", empty, "--as", "a", "--lane", "driver-2", "--json",
            ],
        );
        assert!(!blank.status.success(), "an empty sitrep was accepted");
    }

    // The short forms, because an alias nobody wrote down is one nobody can
    // use -- they resolve by exact match with no inference.
    fixture.ok_json(
        &fixture.main,
        &[
            "sr",
            "new",
            "Via the short forms.",
            "--as",
            "a",
            "--lane",
            "driver-3",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["sr", "ls", "--lane", "driver-3", "--json"])[0]["body"],
        "Via the short forms."
    );

    // The old concept fails closed. Silently accepting it would let an agent
    // believe it posted a sitrep when it did not.
    let old_name = fixture.run(&fixture.main, &["status", "list", "--json"]);
    assert!(
        !old_name.status.success(),
        "the old status command still resolves"
    );
    assert!(
        String::from_utf8_lossy(&old_name.stderr).contains("unknown command"),
        "{}",
        String::from_utf8_lossy(&old_name.stderr)
    );
}

#[test]
fn rules_have_an_ordered_audited_retire_only_lifecycle() {
    let fixture = Fixture::new("rules");
    fixture.ok_json(&fixture.main, &["init", "--name", "RULES", "--json"]);

    let first = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Never touch the PX database layer.",
            "--as",
            "geo",
            "--json",
        ],
    );
    assert!(first["id"].as_str().unwrap().starts_with("r-"));
    assert_eq!(first["author"], "geo");
    assert_eq!(first["archived"], false);

    let body_file = fixture.root.join("rule.md");
    fs::write(
        &body_file,
        "crm-react only.\n\nPX repositories are read-only references.\n",
    )
    .unwrap();
    let second = fixture.ok_json(
        &fixture.main,
        &[
            "r",
            "new",
            "--body-file",
            body_file.to_str().unwrap(),
            "--as",
            "codex@driver",
            "--json",
        ],
    );
    assert_eq!(second["body"], fs::read_to_string(&body_file).unwrap());

    let listed = fixture.ok_json(&fixture.main, &["rule", "list", "--full", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["id"], first["id"], "rules are not oldest first");
    assert_eq!(listed[1]["id"], second["id"]);

    let first_id = first["id"].as_str().unwrap();
    let revised = fixture.ok_json(
        &fixture.main,
        &[
            "r",
            "up",
            first_id,
            "--body",
            "Never alter the PX database layer.",
            "--as",
            "geo",
            "--json",
        ],
    );
    assert_eq!(revised["body"], "Never alter the PX database layer.");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["r", "cat", first_id, "--json"])["body"],
        revised["body"]
    );
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--rule", first_id, "--limit", "100", "--json"],
    );
    let revision = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "rule_updated")
        .expect("rule revision left no trail");
    assert_eq!(
        revision["payload"]["previousBody"],
        "Never touch the PX database layer."
    );

    fixture.ok_json(
        &fixture.main,
        &["rule", "retire", first_id, "--as", "geo", "--json"],
    );
    let active = fixture.ok_json(&fixture.main, &["rule", "list", "--json"]);
    assert_eq!(active.as_array().unwrap().len(), 1);
    assert_eq!(active[0]["id"], second["id"]);
    let all = fixture.ok_json(
        &fixture.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert_eq!(all[0]["id"], first["id"]);
    assert_eq!(all[0]["archived"], true);

    let deletion_alias = fixture.run(&fixture.main, &["rule", "rm", first_id, "--json"]);
    assert!(
        !deletion_alias.status.success(),
        "an rm alias implied a destructive operation that does not exist"
    );

    for args in [
        vec!["rule", "add", "", "--as", "geo", "--json"],
        vec!["rule", "add", "valid", "--as", "", "--json"],
    ] {
        let refused = fixture.run(&fixture.main, &args);
        assert!(
            !refused.status.success(),
            "an empty rule field was accepted"
        );
    }
    let two_bodies = fixture.run(
        &fixture.main,
        &[
            "rule",
            "add",
            "positional",
            "--body",
            "flagged",
            "--as",
            "geo",
            "--json",
        ],
    );
    assert!(
        !two_bodies.status.success(),
        "two rule bodies were silently ranked"
    );
}

#[test]
fn active_rule_summaries_frame_context_and_new_claims_without_leaking_into_get_claim() {
    let fixture = Fixture::new("rule-context");
    fixture.ok_json(&fixture.main, &["init", "--name", "RULE-CONTEXT", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Rule-framed work",
            "--id",
            "t-rules",
            "--json",
        ],
    );
    let short = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Never touch the PX database layer.",
            "--as",
            "geo",
            "--json",
        ],
    );
    let long_body = format!(
        "crm-react only; PX repos are read-only references.\n\n{}",
        "supporting detail ".repeat(160)
    );
    let long = fixture.ok_json(
        &fixture.main,
        &["rule", "add", "--body", &long_body, "--as", "geo", "--json"],
    );

    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-rules", "--as", "worker", "--json"],
    );
    assert_eq!(claim["taskID"], "t-rules", "claim wire shape was nested");
    assert_eq!(claim["rules"].as_array().unwrap().len(), 2);
    assert_eq!(claim["rules"][0]["id"], short["id"]);
    assert_eq!(claim["rules"][1]["id"], long["id"]);
    assert_eq!(claim["rules"][0]["tags"], json!(["ALL"]));
    assert_eq!(claim["rules"][0]["hasMore"], false);
    assert_eq!(claim["rules"][1]["hasMore"], true);
    assert!(claim["rules"][1]["bytes"].as_u64().unwrap() > 2_000);
    assert!(
        claim.get("claim").is_none(),
        "claim receipt stopped being flat"
    );

    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-rules", "--json"]);
    assert!(
        shown["claim"].get("rules").is_none(),
        "get_claim serialized an empty rules field as if it had checked the board"
    );

    let packet = fixture.ok_json(&fixture.main, &["context", "t-rules", "--json"]);
    assert_eq!(packet["rules"], claim["rules"]);
    let rendered = fixture.run(
        &fixture.main,
        &["context", "t-rules", "--max-chars", "1000"],
    );
    assert!(rendered.status.success());
    let rendered = String::from_utf8(rendered.stdout).unwrap();
    assert!(
        rendered.contains("## Rules (2 applicable; bodies lazy)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Never touch the PX database layer."),
        "{rendered}"
    );
    assert!(
        rendered.contains("crm-react only; PX repos are read-only references."),
        "{rendered}"
    );
    assert!(rendered.contains("KB · kb r cat"), "{rendered}");
    assert!(
        rendered.contains(long["id"].as_str().unwrap()),
        "{rendered}"
    );
    assert!(
        !rendered.contains("supporting detail supporting detail"),
        "context carried a full long rule instead of its table of contents"
    );
}

#[test]
fn compiled_binary_matches_task_scoped_rules_across_boards() {
    let fixture = Fixture::new("rule-task-tags");
    let second = fixture.root.join("second");
    let third = fixture.root.join("third");
    fs::create_dir_all(&second).unwrap();
    fs::create_dir_all(&third).unwrap();
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "RULE-TAGS-ONE", "--json"],
    );
    fixture.ok_json(&second, &["init", "--name", "RULE-TAGS-TWO", "--json"]);
    fixture.ok_json(&third, &["init", "--name", "RULE-TAGS-THREE", "--json"]);
    for tag in ["infra", "queuer"] {
        fixture.ok_json(&fixture.main, &["tag", "add", tag, "--as", "geo", "--json"]);
    }
    for cwd in [&fixture.main, &second, &third] {
        fixture.ok_json(cwd, &["tag", "add", "shared", "--as", "geo", "--json"]);
    }

    let scoped = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Only tagged task context.",
            "--as",
            "geo",
            "--tag",
            "queuer",
            "--tag",
            "infra",
            "--json",
        ],
    );
    assert_eq!(scoped["tags"], json!(["ALL", "infra", "queuer"]));

    let global = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Cross-board selector.",
            "--as",
            "geo",
            "--board",
            "RULE-TAGS-ONE",
            "--board",
            "RULE-TAGS-TWO",
            "--tag",
            "shared",
            "--json",
        ],
    );
    assert_eq!(
        global["tags"],
        json!(["ONLY:RULE-TAGS-ONE", "ONLY:RULE-TAGS-TWO", "shared"])
    );

    let args = vec![
        "rule",
        "add",
        "Unknown project tag.",
        "--as",
        "geo",
        "--tag",
        "missing",
        "--json",
    ];
    let refused = fixture.run(&fixture.main, &args);
    assert!(!refused.status.success(), "accepted {args:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("missing"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Tagged work",
            "--id",
            "t-tagged",
            "--tag",
            "queuer",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-tagged", "--as", "worker", "--json"],
    );
    assert_eq!(claim["rules"].as_array().unwrap().len(), 1);
    assert_eq!(claim["rules"][0]["id"], scoped["id"]);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["context", "t-tagged", "--json"])["rules"],
        claim["rules"]
    );

    for (cwd, id, should_match_global) in [
        (&fixture.main, "t-shared-one", true),
        (&second, "t-shared-two", true),
        (&third, "t-shared-three", false),
    ] {
        fixture.ok_json(
            cwd,
            &["task", "add", id, "--id", id, "--tag", "shared", "--json"],
        );
        let tagged_claim = fixture.ok_json(cwd, &["claim", id, "--as", "worker", "--json"]);
        let has_global = tagged_claim["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == global["id"]);
        assert_eq!(has_global, should_match_global, "claim: {tagged_claim}");
        assert_eq!(
            fixture.ok_json(cwd, &["context", id, "--json"])["rules"],
            tagged_claim["rules"]
        );
    }

    fixture.ok_json(
        &second,
        &["task", "add", "t-untagged", "--id", "t-untagged", "--json"],
    );
    let untagged = fixture.ok_json(
        &second,
        &["claim", "t-untagged", "--as", "worker", "--json"],
    );
    assert!(untagged["rules"].as_array().unwrap().is_empty());

    let session = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "outgoing",
            "--to",
            "incoming",
            "--reason",
            "session_end",
            "--summary",
            "Session boundary",
            "--intent",
            "Continue safely",
            "--next-action",
            "Read the board",
            "--json",
        ],
    );
    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            session["id"].as_str().unwrap(),
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert!(accepted["rules"].as_array().unwrap().is_empty());

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Handoff-tagged work",
            "--id",
            "t-handoff-tagged",
            "--tag",
            "infra",
            "--json",
        ],
    );
    let outgoing = fixture.ok_json(
        &fixture.main,
        &["claim", "t-handoff-tagged", "--as", "outgoing", "--json"],
    );
    let task_handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-handoff-tagged",
            "--lease",
            outgoing["leaseToken"].as_str().unwrap(),
            "--as",
            "outgoing",
            "--to",
            "incoming",
            "--summary",
            "Transfer tagged work",
            "--intent",
            "Continue tagged work",
            "--next-action",
            "Accept the handoff",
            "--json",
        ],
    );
    let task_accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            task_handoff["id"].as_str().unwrap(),
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert_eq!(task_accepted["rules"].as_array().unwrap().len(), 1);
    assert_eq!(task_accepted["rules"][0]["id"], scoped["id"]);

    let remove_in_use = fixture.run(
        &fixture.main,
        &["tag", "remove", "infra", "--as", "geo", "--force", "--json"],
    );
    assert!(
        !remove_in_use.status.success(),
        "force removed a tag that still scopes an active rule"
    );
    assert!(
        String::from_utf8_lossy(&remove_in_use.stderr).contains("silently widen"),
        "stderr: {}",
        String::from_utf8_lossy(&remove_in_use.stderr)
    );

    let updated = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            scoped["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--clear-tags",
            "--json",
        ],
    );
    assert_eq!(updated["body"], "Only tagged task context.");
    assert_eq!(updated["tags"], json!(["ALL"]));
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--rule", scoped["id"].as_str().unwrap(), "--json"],
    );
    assert_eq!(
        events[0]["payload"]["previousTags"],
        json!(["ALL", "infra", "queuer"])
    );
}

#[test]
fn registry_rejects_duplicate_names_before_creating_a_new_board() {
    let fixture = Fixture::new("board-name-uniqueness");

    let omega_root = fixture.root.join("omega-rooted");
    fs::create_dir_all(&omega_root).unwrap();
    fixture.ok_json(&omega_root, &["init", "--name", "OMEGA", "--json"]);
    let omega_root_str = omega_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let omega_rootless = fixture.root.join("omega-rootless");
    fs::create_dir_all(&omega_rootless).unwrap();
    let rooted_then_rootless = fixture.run(
        &omega_rootless,
        &["init", "--name", "OMEGA", "--rootless", "--json"],
    );
    assert!(
        !rooted_then_rootless.status.success(),
        "a rootless OMEGA board was created beside the rooted one"
    );
    let rooted_then_rootless_message =
        String::from_utf8_lossy(&rooted_then_rootless.stderr).into_owned();
    assert!(
        rooted_then_rootless_message.contains("a Kanban board is already named OMEGA"),
        "{rooted_then_rootless_message}"
    );

    let detached = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            omega_root_str.as_str(),
            "--as",
            "geo",
            "--json",
        ],
    );
    assert_eq!(detached["rootPath"], omega_root_str);
    let omega_rows = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        omega_rows
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "OMEGA" && row["rootless"] == true),
        "detaching the last root should keep the unique board reachable by name: {omega_rows}"
    );

    let sigma_rootless = fixture.root.join("sigma-rootless");
    fs::create_dir_all(&sigma_rootless).unwrap();
    fixture.ok_json(
        &sigma_rootless,
        &["init", "--name", "SIGMA", "--rootless", "--json"],
    );
    let sigma_rooted = fixture.root.join("sigma-rooted");
    fs::create_dir_all(&sigma_rooted).unwrap();
    let rootless_then_rooted = fixture.run(&sigma_rooted, &["init", "--name", "SIGMA", "--json"]);
    assert!(
        !rootless_then_rooted.status.success(),
        "a rooted SIGMA board was created beside the rootless one"
    );
    let rootless_then_rooted_message =
        String::from_utf8_lossy(&rootless_then_rooted.stderr).into_owned();
    assert!(
        rootless_then_rooted_message.contains("a Kanban board is already named SIGMA"),
        "{rootless_then_rooted_message}"
    );
}

#[test]
fn registry_v3_rules_migrate_to_the_unified_all_tag() {
    let fixture = Fixture::new("global-rule-target-migration");
    fs::create_dir_all(&fixture.data).unwrap();
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
            CREATE TABLE global_rules (
             id TEXT PRIMARY KEY NOT NULL,body TEXT NOT NULL,author TEXT NOT NULL,
             archived INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_global_rules_active ON global_rules(archived,created_at);
            CREATE TABLE global_rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,rule_id TEXT NOT NULL,kind TEXT NOT NULL,
             actor TEXT NOT NULL,payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_global_rule_events_rule_seq ON global_rule_events(rule_id,seq);
            INSERT INTO global_rules VALUES('g-old','Existing global rule.','geo',0,1,1);
            PRAGMA user_version=3;
            "#,
        )
        .unwrap();
    drop(registry);

    let rules = fixture.ok_json(&fixture.main, &["rule", "list", "--full", "--json"]);
    assert_eq!(rules[0]["tags"], json!(["ALL"]));
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        11
    );
}

#[test]
fn registry_v10_migration_records_discarded_alias_names() {
    let fixture = Fixture::new("rootless-name-drift");
    fs::create_dir_all(&fixture.data).unwrap();
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            CREATE TABLE registry_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
            CREATE TABLE workspace_alias_history (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             root_path TEXT NOT NULL,
             name TEXT NOT NULL,
             board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             last_used_at INTEGER NOT NULL,
             archived_at INTEGER NOT NULL,
             archived_by TEXT NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_alias_history_root ON workspace_alias_history(root_path,seq);
            CREATE TABLE rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             rule_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             actor TEXT NOT NULL,
             payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL,
             prev_hash TEXT,
             event_hash TEXT
            ) STRICT;
            CREATE INDEX idx_registry_rule_events_rule_seq ON rule_events(rule_id,seq);
            INSERT INTO workspaces VALUES('/workspace/alpha','Alpha','/boards/alpha.db',10,20);
            INSERT INTO workspace_aliases VALUES('/workspace/alpha/alias','Beta','/boards/alpha.db',30,40);
            PRAGMA user_version=10;
            "#,
        )
        .unwrap();
    drop(registry);

    fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        11
    );
    let (kind, actor, payload): (String, String, String) = registry
        .query_row(
            "SELECT kind,actor,payload FROM rule_events WHERE kind='workspace_alias_name_discarded'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "workspace_alias_name_discarded");
    assert_eq!(actor, "system@migration");
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["discardedName"], "Beta");
    assert_eq!(payload["boardName"], "Alpha");
    assert_eq!(payload["rootPath"], "/workspace/alpha/alias");
    assert_eq!(payload["boardPath"], "/boards/alpha.db");
}

#[test]
fn registry_v10_migration_records_discarded_alias_names_once_across_competing_processes() {
    let fixture = Fixture::new("rootless-name-drift-race");
    fs::create_dir_all(&fixture.data).unwrap();
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            CREATE TABLE registry_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
            CREATE TABLE workspace_alias_history (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             root_path TEXT NOT NULL,
             name TEXT NOT NULL,
             board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             last_used_at INTEGER NOT NULL,
             archived_at INTEGER NOT NULL,
             archived_by TEXT NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_alias_history_root ON workspace_alias_history(root_path,seq);
            CREATE TABLE rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             rule_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             actor TEXT NOT NULL,
             payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL,
             prev_hash TEXT,
             event_hash TEXT
            ) STRICT;
            CREATE INDEX idx_registry_rule_events_rule_seq ON rule_events(rule_id,seq);
            INSERT INTO workspaces VALUES('/workspace/alpha','Alpha','/boards/alpha.db',10,20);
            INSERT INTO workspace_aliases VALUES('/workspace/alpha/alias','Beta','/boards/alpha.db',30,40);
            PRAGMA user_version=10;
            "#,
        )
        .unwrap();
    drop(registry);

    fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            DELETE FROM registry_meta
            WHERE key='workspace_root_model_v11_name_drift_audited';
            DELETE FROM rule_events
            WHERE kind='workspace_alias_name_discarded';
            "#,
        )
        .unwrap();
    drop(registry);

    let outputs = std::thread::scope(|scope| {
        let start = Arc::new(Barrier::new(3));
        let fixture = &fixture;

        let first_start = Arc::clone(&start);
        let first = scope.spawn(move || {
            first_start.wait();
            fixture.run(&fixture.main, &["workspace", "list", "--json"])
        });

        let second_start = Arc::clone(&start);
        let second = scope.spawn(move || {
            second_start.wait();
            fixture.run(&fixture.main, &["workspace", "list", "--json"])
        });

        start.wait();
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert!(
        outputs.iter().all(|output| output.status.success()),
        "both concurrent opens must succeed: first={:?}\nsecond={:?}",
        outputs[0],
        outputs[1]
    );

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    let marker_count = registry
        .query_row(
            "SELECT count(*) FROM registry_meta WHERE key='workspace_root_model_v11_name_drift_audited'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(marker_count, 1, "the drift marker must be written once");
    let event_count = registry
        .query_row(
            "SELECT count(*) FROM rule_events WHERE kind='workspace_alias_name_discarded'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(event_count, 1, "the drift event must be written once");
}

#[test]
fn registry_rejects_last_root_detach_for_legacy_duplicate_names() {
    let fixture = Fixture::new("rootless-duplicate-name-detach");
    fs::create_dir_all(&fixture.data).unwrap();

    let omega_left = fixture.root.join("omega-left");
    let omega_right = fixture.root.join("omega-right");
    let sigma_root = fixture.root.join("sigma-root");
    for dir in [&omega_left, &omega_right, &sigma_root] {
        fs::create_dir_all(dir).unwrap();
    }
    let omega_left_root = omega_left
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let omega_right_root = omega_right
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let sigma_root_path = sigma_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            format!(
                r#"
            CREATE TABLE registry_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
            CREATE TABLE workspace_alias_history (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             root_path TEXT NOT NULL,
             name TEXT NOT NULL,
             board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             last_used_at INTEGER NOT NULL,
             archived_at INTEGER NOT NULL,
             archived_by TEXT NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_alias_history_root ON workspace_alias_history(root_path,seq);
            CREATE TABLE boards (
             board_path TEXT PRIMARY KEY NOT NULL,
             name TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_boards_name ON boards(name,board_path);
            CREATE TABLE workspace_roots (
             root_path TEXT PRIMARY KEY NOT NULL,
             board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             last_used_at INTEGER NOT NULL,
             FOREIGN KEY(board_path) REFERENCES boards(board_path)
            ) STRICT;
            CREATE INDEX idx_workspace_roots_board ON workspace_roots(board_path,last_used_at DESC);
            CREATE TABLE rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             rule_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             actor TEXT NOT NULL,
             payload TEXT NOT NULL DEFAULT '{{}}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL,
             prev_hash TEXT,
             event_hash TEXT
            ) STRICT;
            CREATE INDEX idx_registry_rule_events_rule_seq ON rule_events(rule_id,seq);
            INSERT INTO workspaces VALUES('{omega_left_root}','OMEGA','/boards/omega-left.db',10,20);
            INSERT INTO workspaces VALUES('{omega_right_root}','OMEGA','/boards/omega-right.db',30,40);
            INSERT INTO workspaces VALUES('{sigma_root_path}','SIGMA','/boards/sigma.db',50,60);
            INSERT INTO boards VALUES('/boards/omega-left.db','OMEGA',10,20);
            INSERT INTO boards VALUES('/boards/omega-right.db','OMEGA',30,40);
            INSERT INTO boards VALUES('/boards/sigma.db','SIGMA',50,60);
            INSERT INTO workspace_roots VALUES('{omega_left_root}','/boards/omega-left.db',10,20);
            INSERT INTO workspace_roots VALUES('{omega_right_root}','/boards/omega-right.db',30,40);
            INSERT INTO workspace_roots VALUES('{sigma_root_path}','/boards/sigma.db',50,60);
            PRAGMA user_version=11;
            "#
            )
            .as_str(),
        )
        .unwrap();
    drop(registry);

    let before = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert_eq!(before.as_array().unwrap().len(), 3);

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='workspace_alias_history'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "the archive table is missing from the hybrid fixture"
    );
    assert_eq!(
        registry
            .query_row(
                "SELECT count(*) FROM workspace_roots WHERE root_path=?",
                [sigma_root_path.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "the migrated unique root was not registered"
    );

    let refused = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "detach",
            "--root",
            &omega_left_root,
            "--as",
            "geo",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "detaching a legacy duplicate-name root was accepted"
    );
    let refused_message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        refused_message.contains("would create a second active board named OMEGA"),
        "{refused_message}"
    );

    let after = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert_eq!(after, before, "the rejected detach changed registry state");

    let detached = fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "detach",
            "--root",
            &sigma_root_path,
            "--as",
            "geo",
            "--json",
        ],
    );
    assert_eq!(detached["rootPath"], sigma_root_path);
    assert_eq!(detached["archived"], true);

    let sigma_rows = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        sigma_rows
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "SIGMA" && row["rootless"] == true),
        "detaching the unique board's last root should leave it rootless: {sigma_rows}"
    );
}

#[test]
fn compiled_binary_consolidates_board_rules_once_and_retires_the_sources() {
    let fixture = Fixture::new("unified-rule-consolidation");
    let second = fixture.root.join("second");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "ONE", "--json"]);
    fixture.ok_json(&second, &["init", "--name", "TWO", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "infra", "--as", "geo", "--json"],
    );
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    let board_path = |name: &str| {
        registry
            .query_row(
                "SELECT board_path FROM boards WHERE name=?",
                [name],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    let one_path = board_path("ONE");
    let two_path = board_path("TWO");
    registry
        .execute(
            "INSERT INTO global_rules(id,body,author,archived,created_at,updated_at,board_tags,task_tags) \
             VALUES('g-late','Late rolling-upgrade rule.','geo',0,3,3,'[\"ALL\"]','[\"infra\"]')",
            [],
        )
        .unwrap();
    registry
        .execute(
            "INSERT INTO global_rule_events(rule_id,kind,actor,payload,created_at) \
             VALUES('g-late','global_rule_added','geo','{\"ruleID\":\"g-late\"}',3)",
            [],
        )
        .unwrap();
    drop(registry);
    Connection::open(&one_path)
        .unwrap()
        .execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at,task_tags) VALUES('r-legacy-one','ONE infrastructure rule.','geo',0,1,1,'[\"infra\"]')",
            [],
        )
        .unwrap();
    Connection::open(&two_path)
        .unwrap()
        .execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at,task_tags) VALUES('r-legacy-two','TWO board rule.','geo',0,2,2,'[]')",
            [],
        )
        .unwrap();
    let one = json!({"id":"r-legacy-one"});
    let two = json!({"id":"r-legacy-two"});

    let first = fixture.ok_json(
        &fixture.root,
        &["rule", "consolidate", "--as", "geo", "--json"],
    );
    assert_eq!(first["boardsMigrated"], 2);
    assert_eq!(first["rulesImported"], 2);
    assert_eq!(first["sourceRulesRetired"], 2);
    assert_eq!(first["legacyRegistryMigrated"], true);
    assert_eq!(first["legacyRulesImported"], 1);
    assert_eq!(first["legacyEventsImported"], 1);
    assert_eq!(first["legacyRulesRetired"], 1);

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    let imported_one: (String, String, String) = registry
        .query_row(
            "SELECT tags,source_board,source_rule_id FROM rules WHERE source_board='ONE'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&imported_one.0).unwrap(),
        ["ONLY:ONE", "infra"]
    );
    assert_eq!(imported_one.1, "ONE");
    assert_eq!(imported_one.2, one["id"]);
    let imported_two: String = registry
        .query_row(
            "SELECT tags FROM rules WHERE source_board='TWO' AND source_rule_id=?",
            [two["id"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&imported_two).unwrap(),
        ["ONLY:TWO"]
    );
    let late_tags: String = registry
        .query_row("SELECT tags FROM rules WHERE id='g-late'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&late_tags).unwrap(),
        ["ALL", "infra"]
    );
    drop(registry);

    for path in [&one_path, &two_path] {
        let board = Connection::open(path).unwrap();
        let active: i64 = board
            .query_row("SELECT count(*) FROM rules WHERE archived=0", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(active, 0, "legacy source remained active in {path}");
    }

    let second_run = fixture.ok_json(
        &fixture.root,
        &["rule", "consolidate", "--as", "geo", "--json"],
    );
    assert_eq!(second_run["boardsAlreadyMigrated"], 2);
    assert_eq!(second_run["legacyRegistryAlreadyMigrated"], true);
    assert_eq!(second_run["rulesImported"], 0);
    assert_eq!(second_run["sourceRulesRetired"], 0);
}

#[test]
fn unified_rules_are_stored_once_and_frame_every_board_claim_and_context() {
    let fixture = Fixture::new("global-rules");
    let second = fixture.root.join("second");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "ONE", "--json"]);
    fixture.ok_json(&second, &["init", "--name", "TWO", "--json"]);

    let global = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Never store credentials in Kanban.\n\nKeep secrets in git-crypt.",
            "--as",
            "geo",
            "--json",
        ],
    );
    assert!(global["id"].as_str().unwrap().starts_with("r-"));
    assert_eq!(global["tags"], json!(["ALL"]));
    let global_id = global["id"].as_str().unwrap();

    for (cwd, board, task, local) in [
        (&fixture.main, "ONE", "t-one", "ONE uses Rust."),
        (&second, "TWO", "t-two", "TWO uses SQLite."),
    ] {
        fixture.ok_json(cwd, &["task", "add", task, "--id", task, "--json"]);
        fixture.ok_json(
            cwd,
            &[
                "rule", "add", local, "--board", board, "--as", "geo", "--json",
            ],
        );
        let claim = fixture.ok_json(cwd, &["claim", task, "--as", "worker", "--json"]);
        let rules = claim["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["id"], global["id"]);
        assert_eq!(rules[0]["tags"], json!(["ALL"]));
        assert_eq!(rules[1]["tags"], json!([format!("ONLY:{board}")]));
        let context = fixture.ok_json(cwd, &["context", task, "--json"]);
        assert_eq!(context["rules"], claim["rules"]);
    }

    let registry_path = fixture.data.join("registry.db");
    let registry = Connection::open(&registry_path).unwrap();
    let board_paths = registry
        .prepare("SELECT board_path FROM boards ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for board_path in board_paths {
        let board = Connection::open(board_path).unwrap();
        let copied: i64 = board
            .query_row(
                "SELECT count(*) FROM rules WHERE id=?",
                [global_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copied, 0, "a global rule was copied into a project board");
    }

    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            global_id,
            "--body",
            "Never store secrets in Kanban.",
            "--as",
            "geo",
            "--json",
        ],
    );
    let events = fixture.ok_json(&fixture.main, &["events", "--rule", global_id, "--json"]);
    assert!(
        events[0]["payload"]["previousBody"]
            .as_str()
            .unwrap()
            .starts_with("Never store credentials")
    );

    fixture.ok_json(
        &fixture.main,
        &["rule", "retire", global_id, "--as", "geo", "--json"],
    );
    let active = fixture.ok_json(&fixture.main, &["rule", "list", "--json"]);
    assert_eq!(active.as_array().unwrap().len(), 2);
    let retained = fixture.ok_json(
        &fixture.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert!(
        retained
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == global["id"] && rule["archived"] == true)
    );

    let conflicting = fixture.run(
        &fixture.main,
        &["rule", "list", "--global", "--project", "ONE", "--json"],
    );
    assert!(!conflicting.status.success());
    assert!(String::from_utf8_lossy(&conflicting.stderr).contains("superseded"));
}

#[test]
fn rule_selector_tags_target_named_boards_or_all_except_named_boards() {
    let fixture = Fixture::new("targeted-global-rules");
    let second = fixture.root.join("second");
    let third = fixture.root.join("third");
    fs::create_dir_all(&second).unwrap();
    fs::create_dir_all(&third).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "ONE", "--json"]);
    fixture.ok_json(&second, &["init", "--name", "TWO", "--json"]);
    fixture.ok_json(&third, &["init", "--name", "THREE", "--json"]);

    let all = fixture.ok_json(
        &fixture.main,
        &["rule", "add", "Every board.", "--as", "geo", "--json"],
    );
    let only = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Only one and two.",
            "--board",
            "ONE",
            "--board",
            "TWO",
            "--as",
            "geo",
            "--json",
        ],
    );
    let except = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Everything except one.",
            "--except-board",
            "ONE",
            "--as",
            "geo",
            "--json",
        ],
    );
    assert_eq!(only["tags"], json!(["ONLY:ONE", "ONLY:TWO"]));
    assert_eq!(except["tags"], json!(["ALL", "EXCEPT:ONE"]));

    for (cwd, id, expected) in [
        (
            &fixture.main,
            "t-one",
            vec![all["id"].clone(), only["id"].clone()],
        ),
        (
            &second,
            "t-two",
            vec![all["id"].clone(), only["id"].clone(), except["id"].clone()],
        ),
        (
            &third,
            "t-three",
            vec![all["id"].clone(), except["id"].clone()],
        ),
    ] {
        fixture.ok_json(cwd, &["task", "add", id, "--id", id, "--json"]);
        let claim = fixture.ok_json(cwd, &["claim", id, "--as", "worker", "--json"]);
        let actual = claim["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["id"].clone())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(
            fixture.ok_json(cwd, &["context", id, "--json"])["rules"],
            claim["rules"]
        );
    }

    let only_id = only["id"].as_str().unwrap();
    let retargeted = fixture.ok_json(
        &fixture.main,
        &[
            "rule", "update", only_id, "--board", "THREE", "--as", "geo", "--json",
        ],
    );
    assert_eq!(retargeted["body"], "Only one and two.");
    assert_eq!(retargeted["tags"], json!(["ONLY:THREE"]));
    let events = fixture.ok_json(&fixture.main, &["events", "--rule", only_id, "--json"]);
    assert_eq!(
        events[0]["payload"]["previousTags"],
        json!(["ONLY:ONE", "ONLY:TWO"])
    );
    assert_eq!(events[0]["payload"]["changed"], json!(["selectorTags"]));

    for args in [
        vec![
            "rule", "add", "Bad mix.", "--board", "ALL", "--board", "ONE", "--as", "geo", "--json",
        ],
        vec![
            "rule",
            "add",
            "Bad subtraction.",
            "--board",
            "ONE",
            "--except-board",
            "TWO",
            "--as",
            "geo",
            "--json",
        ],
        vec![
            "rule",
            "add",
            "Unknown board.",
            "--board",
            "MISSING",
            "--as",
            "geo",
            "--json",
        ],
        vec![
            "rule",
            "add",
            "Legacy scope.",
            "--global",
            "--as",
            "geo",
            "--json",
        ],
    ] {
        assert!(
            !fixture.run(&fixture.main, &args).status.success(),
            "accepted {args:?}"
        );
    }
}

#[test]
fn compiled_binary_archives_settled_history_without_deleting_it() {
    let fixture = Fixture::new("archive");
    fixture.ok_json(&fixture.main, &["init", "--name", "Archive", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Old closed work",
            "--id",
            "t-old",
            "--as",
            "geo",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-old",
            "durable evidence",
            "--as",
            "worker",
            "--kind",
            "evidence",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-old", "--as", "worker", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-old",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "worker",
            "--state",
            "done",
            "--summary",
            "finished",
            "--intent",
            "close it",
            "--next-action",
            "none",
            "--json",
        ],
    );
    let attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Review old work",
            "--as",
            "worker",
            "--kind",
            "review",
            "--task",
            "t-old",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            attention["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--note",
            "accepted",
            "--json",
        ],
    );

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE tasks SET completed_at=1,updated_at=1 WHERE id='t-old'",
            [],
        )
        .unwrap();

    let preview = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["tasks"], 1);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"])[0]["id"],
        "t-old",
        "a dry run changed the board"
    );

    let archived = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(archived["tasks"], 1);
    assert_eq!(archived["notes"], 1);
    assert_eq!(archived["checkpoints"], 1);
    assert_eq!(archived["attention"], 1);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        json!([])
    );
    let all = fixture.ok_json(&fixture.main, &["task", "list", "--all", "--json"]);
    assert_eq!(all[0]["id"], "t-old");
    assert_eq!(all[0]["archived"], true);
    assert!(all[0]["archivedAt"].as_i64().is_some());

    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["attention", "list", "--status", "resolved", "--json"]
        ),
        json!([])
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "list",
                "--status",
                "resolved",
                "--all",
                "--json"
            ]
        )[0]["archived"],
        true
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--task", "t-old", "--json"]),
        json!([])
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["events", "--task", "t-old", "--all", "--json"]
            )
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["archived"] == true)
    );

    let rewrite = fixture.run(
        &fixture.main,
        &[
            "task", "move", "t-old", "todo", "--as", "operator", "--json",
        ],
    );
    assert!(
        !rewrite.status.success(),
        "archived history was silently reactivated"
    );
    assert!(
        String::from_utf8_lossy(&rewrite.stderr).contains("archived history"),
        "the refusal did not explain the archival boundary"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        true,
        "archival changed immutable audit history"
    );

    let database = Connection::open(&board).unwrap();
    let index_sql: String = database
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_tasks_status_priority'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(index_sql.contains("WHERE archived=0"));
}

#[test]
fn compiled_binary_tracks_verified_deployments_and_self_archives_only_non_current_history() {
    let fixture = Fixture::new("deployment-archive");
    fixture.ok_json(&fixture.main, &["init", "--name", "Deployments", "--json"]);

    let start = |operation: &str, commit: &str| {
        fixture.ok_json(
            &fixture.main,
            &[
                "deploy",
                "start",
                "--repo",
                "geoyws/kanban",
                "--commit",
                commit,
                "--tier",
                "@_p",
                "--environment",
                "production",
                "--host",
                "hax",
                "--url",
                "https://kb.geoy.ws",
                "--operation-id",
                operation,
                "--as",
                "codex@e2e",
                "--json",
            ],
        )
    };
    let first = start("deploy-e2e-1", "1111111111111111111111111111111111111111");
    let first_id = first["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &first_id,
            "--token",
            first["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            "1111111111111111111111111111111111111111",
            "--receipt",
            "served first",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );

    let current = start("deploy-e2e-2", "2222222222222222222222222222222222222222");
    let current_id = current["id"].as_str().unwrap().to_owned();
    let mismatch = fixture.run(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &current_id,
            "--token",
            current["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            "3333333333333333333333333333333333333333",
            "--receipt",
            "mismatch",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    assert!(
        !mismatch.status.success(),
        "a mismatching served commit was accepted"
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &current_id,
            "--token",
            current["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            "2222222222222222222222222222222222222222",
            "--receipt",
            "served current",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    let replay = start("deploy-e2e-2", "2222222222222222222222222222222222222222");
    assert_eq!(replay["id"], current_id);
    assert_eq!(replay["idempotentReplay"], true);
    assert_eq!(replay["capabilityToken"], current["capabilityToken"]);

    let active = start(
        "deploy-e2e-active",
        "4444444444444444444444444444444444444444",
    );
    let active_id = active["id"].as_str().unwrap().to_owned();
    let failed = start(
        "deploy-e2e-failed",
        "5555555555555555555555555555555555555555",
    );
    let failed_id = failed["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &failed_id,
            "--token",
            failed["capabilityToken"].as_str().unwrap(),
            "--result",
            "failed",
            "--phase",
            "publish",
            "--receipt",
            "registry refused",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let database = Connection::open(&board).unwrap();
    database.execute("UPDATE deployments SET created_at=1,updated_at=1,completed_at=CASE WHEN status='started' THEN NULL ELSE 1 END", []).unwrap();
    database
        .execute(
            "UPDATE deployments SET created_at=2,updated_at=2,completed_at=2 WHERE id=?",
            [&current_id],
        )
        .unwrap();

    let projected = fixture.ok_json(&fixture.main, &["deploy", "current", "--json"]);
    assert_eq!(projected.as_array().unwrap().len(), 1);
    assert_eq!(projected[0]["id"], current_id);

    let archived = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(
        archived["deployments"], 2,
        "only superseded success and old failure should leave hot storage"
    );
    let hot = fixture.ok_json(&fixture.main, &["deploy", "list", "--json"]);
    assert_eq!(hot.as_array().unwrap().len(), 2);
    assert!(
        hot.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == current_id && row["archived"] == false)
    );
    assert!(
        hot.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == active_id && row["status"] == "started")
    );
    let all = fixture.ok_json(&fixture.main, &["deploy", "list", "--all", "--json"]);
    assert_eq!(all.as_array().unwrap().len(), 4);
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == first_id && row["archived"] == true)
    );
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == failed_id && row["archived"] == true)
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["search", &first_id, "--json"])["results"],
        json!([]),
        "archived deployment documents leaked into the hot search corpus"
    );
    let cold_search = fixture.ok_json(&fixture.main, &["search", &first_id, "--all", "--json"]);
    assert!(
        cold_search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["title"] == format!("deployment: {first_id}") && row["archived"] == true
            })
    );
    let hot_search = fixture.ok_json(&fixture.main, &["search", &current_id, "--json"]);
    assert!(hot_search["results"].as_array().unwrap().iter().any(|row| {
        row["title"] == format!("deployment: {current_id}") && row["archived"] == false
    }));

    let repeated = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(
        repeated["deployments"], 0,
        "the self-archive sweep must be idempotent"
    );
    let index_sql: String = database.query_row(
        "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_deployments_hot_target'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(index_sql.contains("WHERE archived=0"));
}
