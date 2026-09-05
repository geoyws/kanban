use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use headless_chrome::{Browser, LaunchOptionsBuilder};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Barrier, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use syn::parse::Parser;
use syn::{
    Attribute, Expr, ExprLit, ExprMethodCall, ForeignItemFn, ImplItemFn, Item, Lit, Meta,
    Path as SynPath, Token, TraitItemFn, Variant,
    punctuated::Punctuated,
    visit::{self, Visit},
};
use uuid::Uuid;

const MAX_CFG_ATOMS: usize = 12;

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
        // Both cwds are git repositories so provenance-bearing writes
        // (checkpoint, handoff create, sitrep post) capture a checkout instead
        // of refusing. Best-effort: without git the dirs stay plain, and only
        // tests that explicitly need provenance will fail.
        let _ = make_repo(&main);
        let _ = make_repo(&worktree);
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

/// What a `--json` refusal leaves on stdout: an object holding only `error`,
/// so a parser meets the refusal rather than an empty result, and no answer
/// rides along with it. Returns the message.
fn refusal_object(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "expected a refusal, got exit {:?}\nstdout: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "refusal stdout is not JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("refusal is not an object: {value}"));
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        ["error"],
        "a refusal carried more than its message: {value}"
    );
    object["error"]
        .as_str()
        .unwrap_or_else(|| panic!("refusal message is not a string: {value}"))
        .to_owned()
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

fn rule_transfer_item_fingerprint(rule: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(rule["sourceRegistryUuid"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update(rule["sourceRuleId"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update(rule["body"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update(rule["author"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update([rule["archived"].as_bool().unwrap() as u8]);
    hasher.update([0]);
    hasher.update(rule["createdAt"].as_i64().unwrap().to_le_bytes());
    hasher.update([0]);
    hasher.update(rule["updatedAt"].as_i64().unwrap().to_le_bytes());
    hasher.update([0]);
    let tags = rule["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\u{1f}");
    hasher.update(tags.as_bytes());
    Sha256::digest(hasher.finalize())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn external_source_board(fixture: &Fixture, label: &str, name: &str) -> PathBuf {
    let data = fixture.root.join(format!("{label}-data"));
    let cwd = fixture.root.join(format!("{label}-cwd"));
    fs::create_dir_all(&cwd).unwrap();
    let output = fixture
        .command_with_data_dir(&cwd, &data)
        .args(["init", "--name", name, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "source init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    PathBuf::from(value["boardPath"].as_str().unwrap())
}

fn adoption_marker_path(fixture: &Fixture) -> PathBuf {
    fixture.data.join(".workspace-adopt.json")
}

static WORKSPACE_ADOPT_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn workspace_adopt_test_guard() -> std::sync::MutexGuard<'static, ()> {
    // A panicking sibling must fail on its own assertion, not cascade a
    // PoisonError into every later adopt test that shares this lock.
    WORKSPACE_ADOPT_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static DB_LOCK_CONTENTION_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn db_lock_contention_test_guard() -> std::sync::MutexGuard<'static, ()> {
    DB_LOCK_CONTENTION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap()
}

const WORKSPACE_ADOPT_HELPER_ROOT_FD: i32 = 37;
const WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD: i32 = 38;

/// Occupy the helper's fixed fd numbers with close-on-exec `/dev/null`
/// handles, so the adopt binary starts with those numbers taken and then
/// freed at exec, exactly as a parent that happened to hold them would leave
/// it.
///
/// This runs in the FORKED CHILD, from `pre_exec`, never in the test process.
/// Every test in this binary is a thread of one process sharing one fd table,
/// and `dup2` onto a fixed low number there clobbers whatever a sibling thread
/// has on it mid-`spawn` — a pipe it is about to hand to its own child. Three
/// unrelated spawn-heavy tests failed `Command::spawn` with EBADF on
/// 2026-09-05, one per gate, once `Fixture::new` began spawning `git init`
/// twice per fixture and widened the window. Only async-signal-safe calls
/// belong here: `open`, `dup2`, `fcntl`, `close`.
fn occupy_helper_fds_in_child() -> std::io::Result<()> {
    unsafe {
        for target in [
            WORKSPACE_ADOPT_HELPER_ROOT_FD,
            WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD,
        ] {
            let source = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            if source < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(source, target) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if source != target && libc::close(source) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(target, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn wait_for_path(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

#[cfg(debug_assertions)]
fn wait_for_json_file(path: &Path) -> Value {
    for _ in 0..200 {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(value) = serde_json::from_str(&text)
        {
            return value;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for valid JSON in {}", path.display());
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
    stderr_thread: Option<std::thread::JoinHandle<()>>,
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
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

/// Mirrors `POLL_INTERVAL` in `rust/watch.rs`: how long `watch --follow` waits
/// between keep-alive heartbeats while it has no matching batch. Only used to
/// size a deliberate delay, so a drift between the two costs test time rather
/// than correctness.
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);

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

    fn try_next_stdout_json(&self, timeout: Duration) -> Option<Value> {
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(line) => Some(serde_json::from_str(&line).unwrap()),
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                None
            }
        }
    }

    /// Returns the next `event` envelope, discarding keep-alive heartbeats.
    ///
    /// `watch --follow` emits a heartbeat every poll interval while it has no
    /// matching batch, so any number of them can land between a mutation and
    /// the event that mutation produces. `timeout` stays a hard deadline for
    /// the event itself: heartbeats never extend it, so an event that never
    /// arrives fails instead of looping forever.
    fn next_stdout_event_json(&self, timeout: Duration) -> Value {
        self.next_stdout_event_json_with_drain_count(timeout).0
    }

    /// [`Self::next_stdout_event_json`], plus how many heartbeats it discarded.
    ///
    /// The count is what proves the drain actually ran, so a test that means to
    /// exercise the interleaved-heartbeat path can assert on it instead of
    /// silently degrading to the pass-through case.
    ///
    /// On timeout the panic reports what was seen rather than naming a cause:
    /// an event can be missing because stdout stalled, because the child died,
    /// or because the event was never delivered at all. The drained count and
    /// the last heartbeat's `state` and `cursor` are the evidence that tells
    /// those apart -- in particular a cursor that has moved past the awaited
    /// event distinguishes a lost event from a quiet stream.
    fn next_stdout_event_json_with_drain_count(&self, timeout: Duration) -> (Value, usize) {
        let deadline = Instant::now() + timeout;
        let mut drained = 0_usize;
        let mut last_heartbeat: Option<Value> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(envelope) = self.try_next_stdout_json(remaining) else {
                let seen = match &last_heartbeat {
                    Some(heartbeat) => {
                        let cursor = heartbeat["cursor"].as_str().unwrap_or_default();
                        // Decode leniently: this runs on the failure path, so a
                        // malformed cursor must still be reported, not panic.
                        let seq = URL_SAFE_NO_PAD
                            .decode(cursor)
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                            .map(|decoded| decoded["seq"].to_string())
                            .unwrap_or_else(|| format!("<undecodable {cursor}>"));
                        format!(
                            "drained {drained} heartbeat(s), last was state {} at cursor seq {seq}",
                            heartbeat["payload"]["state"]
                        )
                    }
                    None => "no heartbeat arrived either".to_owned(),
                };
                panic!("watch delivered no event within {timeout:?}: {seen}");
            };
            match envelope["type"].as_str().unwrap() {
                "event" => return (envelope, drained),
                "heartbeat" => {
                    drained += 1;
                    last_heartbeat = Some(envelope);
                }
                other => panic!("unexpected watch envelope type {other}"),
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
    let playwright_cache_root = playwright_cache_root();
    let candidates = chrome_binary_candidates(
        explicit.as_deref().and_then(Path::to_str),
        env::var_os("PATH")
            .as_deref()
            .and_then(std::ffi::OsStr::to_str),
        cfg!(target_os = "macos"),
        playwright_cache_root.as_deref(),
    );
    if let Some(path) = chrome_binary_from_candidates(candidates.clone(), is_regular_executable) {
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
    playwright_cache_root: Option<&Path>,
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
    candidates.extend(playwright_chromium_candidates(playwright_cache_root));
    candidates
}

fn playwright_cache_root() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
}

fn playwright_chromium_candidates(playwright_cache_root: Option<&Path>) -> Vec<PathBuf> {
    let Some(playwright_cache_root) = playwright_cache_root else {
        return Vec::new();
    };
    let cache_root = playwright_cache_root.join("ms-playwright");
    let Ok(entries) = fs::read_dir(&cache_root) else {
        return Vec::new();
    };
    let mut chromium_dirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(version) = file_name.strip_prefix("chromium-") else {
            continue;
        };
        let Some(version_key) = chromium_version_key(version) else {
            continue;
        };
        chromium_dirs.push((version_key, entry.path()));
    }
    chromium_dirs.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    chromium_dirs
        .into_iter()
        .map(|(_, chromium_dir)| chromium_dir.join("chrome-linux64").join("chrome"))
        .collect()
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ChromiumVersionKey(Vec<u64>);

fn chromium_version_key(version: &str) -> Option<ChromiumVersionKey> {
    let components = version
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|component| !component.is_empty())
        .map(|component| component.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if components.is_empty() {
        return None;
    }
    Some(ChromiumVersionKey(components))
}

fn chrome_binary_from_candidates<F>(candidates: Vec<PathBuf>, exists: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    candidates.into_iter().find(|path| exists(path))
}

fn is_regular_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    metadata.permissions().mode() & 0o111 != 0
}

fn browser_sandbox_enabled(effective_uid: u32) -> bool {
    effective_uid != 0
}

fn assert_reply_recorded(tab: &headless_chrome::Tab, origin: &str, reply_id: &str, label: &str) {
    let success = tab
        .wait_for_element("p.success")
        .unwrap_or_else(|error| panic!("{label} success element: {error}"));
    let success_text = success
        .get_inner_text()
        .unwrap_or_else(|error| panic!("{label} success text: {error}"));
    let expected_url = format!("{origin}?replied={reply_id}");
    assert!(
        success_text == format!("Reply recorded for {reply_id}."),
        "{label} success text: {success_text}"
    );
    assert!(
        tab.get_url() == expected_url,
        "{label} redirect url: {}",
        tab.get_url()
    );
}

#[test]
fn chrome_discovery_prefers_explicit_then_platform_then_path_then_defaults() {
    let cache_root = unique_test_dir("chrome-discovery-cache-root");
    let playwright_root = cache_root.join("ms-playwright");
    fs::create_dir_all(playwright_root.join("chromium-2/chrome-linux64")).unwrap();
    fs::create_dir_all(playwright_root.join("chromium-10/chrome-linux64")).unwrap();
    fs::create_dir_all(playwright_root.join("chromium-10.1/chrome-linux64")).unwrap();
    let candidates = chrome_binary_candidates(
        Some("/explicit/chrome"),
        Some("/a:/b"),
        true,
        Some(&cache_root),
    );
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
    assert_eq!(
        &candidates[candidates.len() - 3..],
        [
            playwright_root.join("chromium-10.1/chrome-linux64/chrome"),
            playwright_root.join("chromium-10/chrome-linux64/chrome"),
            playwright_root.join("chromium-2/chrome-linux64/chrome"),
        ]
    );
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
    fs::remove_dir_all(&cache_root).unwrap();
}

#[test]
fn chrome_discovery_rejects_missing_and_non_executable_candidates() {
    let root = unique_test_dir("chrome-discovery-executable-check");
    let missing = root.join("missing");
    let non_executable = root.join("non-executable");
    let executable = root.join("executable");
    fs::write(&non_executable, b"#!/bin/sh\n").unwrap();
    let mut non_executable_permissions = fs::metadata(&non_executable).unwrap().permissions();
    non_executable_permissions.set_mode(0o644);
    fs::set_permissions(&non_executable, non_executable_permissions).unwrap();
    fs::write(&executable, b"#!/bin/sh\n").unwrap();
    let mut executable_permissions = fs::metadata(&executable).unwrap().permissions();
    executable_permissions.set_mode(0o755);
    fs::set_permissions(&executable, executable_permissions).unwrap();
    let picked = chrome_binary_from_candidates(
        vec![missing, non_executable, executable.clone()],
        is_regular_executable,
    )
    .unwrap();
    assert_eq!(picked, executable);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn chrome_discovery_keeps_explicit_before_path_candidates() {
    let candidates = chrome_binary_candidates(Some("/explicit/chrome"), Some("/a:/b"), false, None);
    assert_eq!(candidates[0], PathBuf::from("/explicit/chrome"));
    assert_eq!(candidates[1], PathBuf::from("/a/google-chrome"));
    assert_eq!(candidates[2], PathBuf::from("/a/google-chrome-stable"));
    assert_eq!(candidates[3], PathBuf::from("/a/chromium"));
    assert_eq!(candidates[4], PathBuf::from("/a/chromium-browser"));
    assert_eq!(candidates[5], PathBuf::from("/b/google-chrome"));
}

#[test]
fn browser_sandbox_disables_only_for_root_uid() {
    assert!(browser_sandbox_enabled(1));
    assert!(browser_sandbox_enabled(1000));
    assert!(!browser_sandbox_enabled(0));
}

fn launch_browser(chrome_path: PathBuf) -> Browser {
    let options = LaunchOptionsBuilder::default()
        .path(Some(chrome_path))
        .headless(true)
        .sandbox(browser_sandbox_enabled(effective_uid()))
        .build()
        .expect("build Chrome launch options");
    Browser::new(options).expect("launch Chrome")
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() as u32 }
}

fn unique_test_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "kanban-chrome-test-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn browser_loopback_reservation_supported() -> Result<(), String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .map(drop)
        .map_err(|error| format!("reserve loopback port for browser tests: {error}"))
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = fs::metadata(&path).unwrap();
        if metadata.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(&path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            stack.extend(entries.into_iter().rev());
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            sources.push(path);
        }
    }
    sources.sort();
    sources
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SymbolReferenceKind {
    Definition,
    Use,
}

fn symbol_references_in_source(source: &str, symbol: &str) -> Vec<SymbolReferenceKind> {
    let file = syn::parse_file(source).unwrap();
    struct Finder<'a> {
        symbol: &'a str,
        references: Vec<SymbolReferenceKind>,
    }

    #[derive(Clone)]
    enum CfgExpr {
        Atom(String),
        All(Vec<CfgExpr>),
        Any(Vec<CfgExpr>),
        Not(Box<CfgExpr>),
    }

    fn meta_path_to_string(path: &syn::Path) -> String {
        path.segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::")
    }

    fn expr_to_cfg_string(expr: &Expr) -> String {
        match expr {
            Expr::Lit(ExprLit { lit, .. }) => match lit {
                Lit::Str(value) => format!("{:?}", value.value()),
                Lit::ByteStr(value) => format!("{:?}", value.value()),
                Lit::Byte(value) => value.value().to_string(),
                Lit::Char(value) => value.value().to_string(),
                Lit::Int(value) => value.base10_digits().to_owned(),
                Lit::Float(value) => value.base10_digits().to_owned(),
                Lit::Bool(value) => value.value.to_string(),
                _ => "lit".to_owned(),
            },
            Expr::Path(expr_path) => meta_path_to_string(&expr_path.path),
            Expr::Paren(expr_paren) => expr_to_cfg_string(&expr_paren.expr),
            Expr::Group(expr_group) => expr_to_cfg_string(&expr_group.expr),
            Expr::Unary(expr_unary) => match &expr_unary.op {
                syn::UnOp::Neg(_) => format!("-{}", expr_to_cfg_string(&expr_unary.expr)),
                syn::UnOp::Not(_) => format!("!{}", expr_to_cfg_string(&expr_unary.expr)),
                _ => "unary".to_owned(),
            },
            Expr::Macro(expr_macro) => expr_macro.mac.tokens.to_string(),
            _ => "expr".to_owned(),
        }
    }

    fn meta_to_cfg_expr(meta: Meta) -> Option<CfgExpr> {
        match meta {
            Meta::Path(path) => Some(CfgExpr::Atom(meta_path_to_string(&path))),
            Meta::NameValue(name_value) => Some(CfgExpr::Atom(format!(
                "{}={}",
                meta_path_to_string(&name_value.path),
                expr_to_cfg_string(&name_value.value)
            ))),
            Meta::List(list) => {
                let tokens = list.tokens.clone();
                let items = Punctuated::<Meta, Token![,]>::parse_terminated
                    .parse2(tokens.clone())
                    .ok()?;
                let nested = items
                    .into_iter()
                    .map(meta_to_cfg_expr)
                    .collect::<Option<Vec<_>>>()?;
                match meta_path_to_string(&list.path).as_str() {
                    "all" => Some(CfgExpr::All(nested)),
                    "any" => Some(CfgExpr::Any(nested)),
                    "not" => {
                        if nested.len() != 1 {
                            return None;
                        }
                        Some(CfgExpr::Not(Box::new(nested.into_iter().next().unwrap())))
                    }
                    _ => Some(CfgExpr::Atom(format!(
                        "{}({})",
                        meta_path_to_string(&list.path),
                        tokens
                    ))),
                }
            }
        }
    }

    fn cfg_expr_atoms(expr: &CfgExpr, atoms: &mut Vec<String>) {
        match expr {
            CfgExpr::Atom(atom) => atoms.push(atom.clone()),
            CfgExpr::All(children) | CfgExpr::Any(children) => {
                for child in children {
                    cfg_expr_atoms(child, atoms);
                }
            }
            CfgExpr::Not(child) => cfg_expr_atoms(child, atoms),
        }
    }

    fn cfg_expr_matches(
        expr: &CfgExpr,
        assignment: &std::collections::HashMap<String, bool>,
    ) -> bool {
        match expr {
            CfgExpr::Atom(atom) => assignment.get(atom).copied().unwrap_or(false),
            CfgExpr::All(children) => children
                .iter()
                .all(|child| cfg_expr_matches(child, assignment)),
            CfgExpr::Any(children) => children
                .iter()
                .any(|child| cfg_expr_matches(child, assignment)),
            CfgExpr::Not(child) => !cfg_expr_matches(child, assignment),
        }
    }

    fn has_test_only_cfg(attrs: &[Attribute]) -> bool {
        attrs.iter().any(|attr| {
            if !attr.path().is_ident("cfg") {
                return false;
            }
            let Meta::List(list) = &attr.meta else {
                return false;
            };
            let Ok(items) =
                Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone())
            else {
                return false;
            };
            let Some(expr) = items
                .into_iter()
                .map(meta_to_cfg_expr)
                .collect::<Option<Vec<_>>>()
                .map(CfgExpr::All)
            else {
                return false;
            };
            let mut atoms = Vec::new();
            cfg_expr_atoms(&expr, &mut atoms);
            atoms.sort();
            atoms.dedup();
            atoms.retain(|atom| atom != "test");
            if atoms.len() > MAX_CFG_ATOMS {
                return false;
            }
            let mut assignment = std::collections::HashMap::new();
            let Some(limit) = 1usize.checked_shl(atoms.len() as u32) else {
                return false;
            };
            for mask in 0..limit {
                assignment.clear();
                assignment.insert("test".to_owned(), false);
                for (index, atom) in atoms.iter().enumerate() {
                    assignment.insert(atom.clone(), (mask & (1usize << index)) != 0);
                }
                if cfg_expr_matches(&expr, &assignment) {
                    return false;
                }
            }
            true
        })
    }

    trait HasAttrs {
        fn attrs(&self) -> &[Attribute];
    }

    impl HasAttrs for Item {
        fn attrs(&self) -> &[Attribute] {
            match self {
                Item::Const(item) => &item.attrs,
                Item::Enum(item) => &item.attrs,
                Item::ExternCrate(item) => &item.attrs,
                Item::Fn(item) => &item.attrs,
                Item::ForeignMod(item) => &item.attrs,
                Item::Impl(item) => &item.attrs,
                Item::Macro(item) => &item.attrs,
                Item::Mod(item) => &item.attrs,
                Item::Static(item) => &item.attrs,
                Item::Struct(item) => &item.attrs,
                Item::Trait(item) => &item.attrs,
                Item::TraitAlias(item) => &item.attrs,
                Item::Type(item) => &item.attrs,
                Item::Union(item) => &item.attrs,
                Item::Use(item) => &item.attrs,
                Item::Verbatim(_) => &[],
                _ => &[],
            }
        }
    }

    impl HasAttrs for Variant {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl HasAttrs for TraitItemFn {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl HasAttrs for ForeignItemFn {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl HasAttrs for ImplItemFn {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl Visit<'_> for Finder<'_> {
        fn visit_item(&mut self, node: &Item) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if let Item::Fn(item_fn) = node
                && item_fn.sig.ident == self.symbol
            {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_item(self, node);
        }

        fn visit_trait_item_fn(&mut self, node: &TraitItemFn) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.sig.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_trait_item_fn(self, node);
        }

        fn visit_foreign_item_fn(&mut self, node: &ForeignItemFn) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.sig.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_foreign_item_fn(self, node);
        }

        fn visit_impl_item_fn(&mut self, node: &ImplItemFn) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.sig.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_impl_item_fn(self, node);
        }

        fn visit_field(&mut self, node: &syn::Field) {
            if has_test_only_cfg(&node.attrs) {
                return;
            }
            visit::visit_field(self, node);
        }

        fn visit_expr_method_call(&mut self, node: &ExprMethodCall) {
            if node.method == self.symbol {
                self.references.push(SymbolReferenceKind::Use);
            }
            visit::visit_expr_method_call(self, node);
        }

        fn visit_path(&mut self, node: &SynPath) {
            if node
                .segments
                .iter()
                .any(|segment| segment.ident == self.symbol)
            {
                self.references.push(SymbolReferenceKind::Use);
            }
            visit::visit_path(self, node);
        }

        fn visit_macro(&mut self, node: &syn::Macro) {
            if node.tokens.to_string().contains(self.symbol) {
                self.references.push(SymbolReferenceKind::Use);
            }
            visit::visit_macro(self, node);
        }

        fn visit_variant(&mut self, node: &Variant) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_variant(self, node);
        }
    }
    let mut finder = Finder {
        symbol,
        references: Vec::new(),
    };
    finder.visit_file(&file);
    finder.references
}

fn server_ready_banner(port: u16) -> String {
    format!("kanban serve: http://127.0.0.1:{port} (loopback only; front it with nginx)")
}

fn line_is_server_ready_banner(expected_banner: &str, line: &str) -> bool {
    line == expected_banner
}

fn spawn_server(fixture: &Fixture) -> ServerGuard {
    spawn_server_with_actor_header(fixture, None)
}

fn spawn_server_with_actor_header(fixture: &Fixture, actor_header: Option<&str>) -> ServerGuard {
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
        let port_arg = port.to_string();
        let mut command = fixture.command(&fixture.main);
        command.args(["serve", "--port", &port_arg]);
        if let Some(name) = actor_header {
            command.args(["--actor-header", name]);
        }
        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawn kanban serve on {port}: {error}"));
        let stderr = child.stderr.take().unwrap();
        let expected_banner = server_ready_banner(port);
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_sink = Arc::clone(&stderr_lines);
        let (stderr_tx, stderr_rx) = mpsc::channel();
        let stderr_thread = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                stderr_sink.lock().unwrap().push(line.clone());
                if stderr_tx.send(line).is_err() {
                    return;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|error| panic!("wait on kanban serve candidate {port}: {error}"))
            {
                let _ = child.wait();
                let _ = stderr_thread.join();
                let stderr = stderr_lines.lock().unwrap().join("\n");
                failures.push(format!(
                    "port {port} exited before readiness on attempt {attempt}: {status}: {stderr}"
                ));
                break;
            }
            match stderr_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => {
                    if line_is_server_ready_banner(&expected_banner, &line) {
                        match std::panic::catch_unwind(|| http_get(port, "/")) {
                            Ok((200, _body)) => {
                                return ServerGuard {
                                    child: Some(child),
                                    port,
                                    stderr_thread: Some(stderr_thread),
                                };
                            }
                            Ok((status, body)) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                let _ = stderr_thread.join();
                                let stderr = stderr_lines.lock().unwrap().join("\n");
                                failures.push(format!(
                                    "port {port} printed readiness banner but GET / returned {status} on attempt {attempt}: {body}\n{stderr}"
                                ));
                                break;
                            }
                            Err(_) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                let _ = stderr_thread.join();
                                let stderr = stderr_lines.lock().unwrap().join("\n");
                                failures.push(format!(
                                    "port {port} printed readiness banner but GET / panicked on attempt {attempt}\n{stderr}"
                                ));
                                break;
                            }
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stderr_thread.join();
                    let stderr = stderr_lines.lock().unwrap().join("\n");
                    failures.push(format!(
                        "port {port} stopped emitting stderr before readiness on attempt {attempt}: {stderr}"
                    ));
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_thread.join();
                let stderr = stderr_lines.lock().unwrap().join("\n");
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

fn project_command(fixture: &Fixture, board: &str) -> Command {
    let mut command = fixture.command(&fixture.main);
    command.env("KANBAN_PROJECT", board);
    command
}

fn project_ok_json(fixture: &Fixture, board: &str, args: &[&str]) -> Value {
    let output = project_command(fixture, board).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {:?}\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn http_post_with_headers(
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String) {
    use std::io::{Read, Write as _};
    use std::net::TcpStream;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to kanban serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )
    .unwrap();
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    write!(stream, "\r\n").unwrap();
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

#[test]
fn serve_readiness_banner_matches_exact_output() {
    let port = 14200;
    assert!(line_is_server_ready_banner(
        &server_ready_banner(port),
        &server_ready_banner(port)
    ));
    assert!(!line_is_server_ready_banner(
        &server_ready_banner(port),
        "kanban serve: http://127.0.0.1:14200/listening (loopback only; front it with nginx)"
    ));
    assert!(!line_is_server_ready_banner(
        &server_ready_banner(port),
        "tcp listener on 127.0.0.1:14200 is available"
    ));
    assert!(!line_is_server_ready_banner(
        &server_ready_banner(port + 1),
        &server_ready_banner(port)
    ));
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

/// Return a current fixture to the exact pre-subscription schema shape before
/// a historical migration test lowers `user_version` below V21.
fn remove_v21_subscription_schema(connection: &Connection) {
    connection
        .execute_batch(
            "DROP TABLE subscription_delivery_attempts;\
             DROP TABLE subscription_deliveries;\
             DROP TABLE board_materialization_cursor;\
             DROP INDEX idx_subscriptions_consumer;\
             DROP INDEX idx_subscriptions_status;\
             DROP TABLE subscriptions;",
        )
        .unwrap();
}

#[test]
fn compiled_binary_manages_audited_board_local_subscriptions_fail_closed() {
    let fixture = Fixture::new("subscriptions");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SUBSCRIPTIONS", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "pubsub", "--as", "geoyws", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Parent", "--id", "e-sub", "--type", "epic", "--status", "todo", "--as",
            "geoyws", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Subject",
            "--id",
            "t-subject",
            "--parent",
            "e-sub",
            "--tag",
            "pubsub",
            "--as",
            "geoyws",
            "--json",
        ],
    );

    let added = fixture.ok_json(
        &fixture.main,
        &[
            "subscription",
            "add",
            "--id",
            "sub-e2e",
            "--subject",
            "task:t-subject",
            "--relation",
            "parent:e-sub",
            "--kind",
            "checkpoint_added",
            "--prior-status",
            "todo",
            "--current-status",
            "in_progress",
            "--tag",
            "pubsub",
            "--consumer",
            "codex.queue",
            "--action",
            "enqueue-turn",
            "--timeout-ms",
            "30000",
            "--max-retries",
            "3",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--secret-ref",
            "codex_queue_token",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(added["id"], "sub-e2e");
    assert_eq!(added["protocolVersion"], 1);
    assert_eq!(added["subjectTaskID"], "t-subject");
    assert_eq!(added["consumerID"], "codex.queue");
    assert_eq!(added["actionID"], "enqueue-turn");
    assert_eq!(added["secretRef"], "codex_queue_token");

    // Read-only subscription commands must not run the mutating open path,
    // whose lease sweep would remove an expired claim and append an event.
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-subject", "--as", "lease-probe", "--json"],
    );
    let board_path = board_path_for_project(&fixture, &fixture.main, "SUBSCRIPTIONS");
    let connection = Connection::open(&board_path).unwrap();
    connection
        .execute(
            "UPDATE task_claims SET expires_at=1 WHERE task_id='t-subject'",
            [],
        )
        .unwrap();
    let claim_expired_before: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind='claim_expired'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);

    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["subscription", "show", "sub-e2e", "--json"]
        ),
        added
    );
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["subscription", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let connection = Connection::open(&board_path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM task_claims WHERE task_id='t-subject'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "subscription show/list swept an expired claim"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='claim_expired'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        claim_expired_before,
        "subscription show/list appended a claim_expired event"
    );
    drop(connection);

    let event = fixture.ok_json(
        &fixture.main,
        &[
            "watch",
            "--kind",
            "subscription_added",
            "--cursor",
            "0",
            "--limit",
            "1",
            "--json",
        ],
    );
    assert_eq!(event["type"], "event");
    assert_eq!(event["payload"]["kind"], "subscription_added");
    assert_eq!(event["payload"]["payload"]["subscriptionID"], "sub-e2e");
    let event_text = event.to_string();
    assert!(!event_text.contains("secretRef"), "{event_text}");
    assert!(!event_text.contains("codex_queue_token"), "{event_text}");

    let paused = fixture.ok_json(
        &fixture.main,
        &[
            "subscription",
            "pause",
            "sub-e2e",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(paused["status"], "paused");
    assert!(
        fixture
            .ok_json(&fixture.main, &["subscription", "list", "--json"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["subscription", "list", "--all", "--json"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let resumed = fixture.ok_json(
        &fixture.main,
        &[
            "subscription",
            "resume",
            "sub-e2e",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(resumed["status"], "active");

    for (label, args) in [
        (
            "unknown subject",
            vec![
                "subscription",
                "add",
                "--id",
                "sub-bad-subject",
                "--subject",
                "task:missing",
                "--consumer",
                "codex.queue",
                "--action",
                "enqueue-turn",
                "--timeout-ms",
                "1",
                "--max-retries",
                "0",
                "--rate-per-minute",
                "1",
                "--max-concurrency",
                "1",
                "--as",
                "geoyws",
                "--json",
            ],
        ),
        (
            "raw secret",
            vec![
                "subscription",
                "add",
                "--id",
                "sub-bad-secret",
                "--consumer",
                "codex.queue",
                "--action",
                "enqueue-turn",
                "--timeout-ms",
                "1",
                "--max-retries",
                "0",
                "--rate-per-minute",
                "1",
                "--max-concurrency",
                "1",
                "--secret-ref",
                "env:TOKEN=raw",
                "--as",
                "geoyws",
                "--json",
            ],
        ),
        (
            "id collision",
            vec![
                "subscription",
                "add",
                "--id",
                "sub-e2e",
                "--consumer",
                "codex.queue",
                "--action",
                "enqueue-turn",
                "--timeout-ms",
                "1",
                "--max-retries",
                "0",
                "--rate-per-minute",
                "1",
                "--max-concurrency",
                "1",
                "--as",
                "geoyws",
                "--json",
            ],
        ),
        (
            "missing pause target",
            vec![
                "subscription",
                "pause",
                "sub-missing",
                "--as",
                "geoyws",
                "--json",
            ],
        ),
        (
            "missing resume target",
            vec![
                "subscription",
                "resume",
                "sub-missing",
                "--as",
                "geoyws",
                "--json",
            ],
        ),
    ] {
        let output = fixture.run(&fixture.main, &args);
        assert!(!output.status.success(), "{label} was accepted");
    }

    let second = fixture.root.join("second");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&second, &["init", "--name", "SUBSCRIPTIONS-B", "--json"]);
    assert!(
        fixture
            .ok_json(&second, &["subscription", "list", "--all", "--json"])
            .as_array()
            .unwrap()
            .is_empty()
    );

    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert!(doctor["healthy"].as_bool().unwrap());
    assert!(
        doctor["projects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|project| project["schemaVersion"] == 24)
    );

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    for (name, read_only) in [
        ("subscription add", false),
        ("subscription list", true),
        ("subscription show", true),
        ("subscription pause", false),
        ("subscription resume", false),
    ] {
        let operation = schema["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|operation| operation["name"] == name)
            .unwrap_or_else(|| panic!("missing schema operation {name}"));
        assert_eq!(operation["readOnly"], read_only, "{name}");
        assert_eq!(operation["longRunning"], false, "{name}");
    }
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
            "geoyws",
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
            "geoyws",
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
    assert_eq!(doctor["registrySchemaVersion"], 14);
    assert_eq!(doctor["supportedRegistrySchemaVersion"], 14);
    assert_eq!(doctor["supportedBoardSchemaVersion"], 24);
    assert_eq!(doctor["projects"][0]["schemaVersion"], 24);
    assert_eq!(doctor["projects"][0]["supportedSchemaVersion"], 24);
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
            "geoyws",
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
            "geoyws",
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

    let tag_driven = fixture.ok_json(
        &fixture.main,
        &["search", "release ops", "--limit", "3", "--json"],
    );
    assert!(
        tag_driven["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["sourceId"] == "t-release"),
        "tag-driven search stopped returning the tagged task: {tag_driven}"
    );

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
    assert_eq!(
        doctor["projects"][0]["searchIndex"]["missingEmbeddings"], 0,
        "a source mutation left its document unembedded; the incremental path must re-embed inline"
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
    let board_path = board_path_for_project(&fixture, &fixture.main, "SEARCH-A");
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
fn compiled_binary_excludes_tag_only_canonical_id_collisions() {
    let fixture = Fixture::new("canonical-id-tag-collision");
    fixture.ok_json(&fixture.main, &["init", "--name", "SEARCH-C", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "sub-deadbeef", "--as", "tester", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Tagged collision",
            "--id",
            "t-tagged",
            "--body",
            "No literal match lives here.",
            "--tag",
            "sub-deadbeef",
            "--as",
            "tester",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Literal body hit",
            "--id",
            "t-literal",
            "--body",
            "Keep sub-deadbeef in the source body.",
            "--as",
            "tester",
            "--json",
        ],
    );

    let search = fixture.ok_json(
        &fixture.main,
        &["search", "sub-deadbeef", "--limit", "5", "--json"],
    );
    let results = search["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .any(|result| result["sourceId"] == "t-literal"),
        "literal source/body hit was not returned: {search}"
    );
    assert!(
        results
            .iter()
            .all(|result| result["sourceId"] != "t-tagged"),
        "tag-only canonical ID collision leaked into search: {search}"
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
fn doctor_flags_and_rebuild_repairs_a_search_index_without_embeddings() {
    let fixture = Fixture::new("search-embed-health");
    fixture.ok_json(&fixture.main, &["init", "--name", "EMBED-HEALTH", "--json"]);

    // One of each searchable source kind, written through the CLI so the
    // compiled binary's incremental path is what is under test.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Embedding health probe",
            "--id",
            "t-embed",
            "--body",
            "The semantic half of hybrid retrieval.",
            "--as",
            "tester",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-embed",
            "A note for the probe.",
            "--as",
            "tester",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-embed", "--as", "tester", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-embed",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "tester",
            "--state",
            "continue",
            "--summary",
            "probe checkpoint",
            "--intent",
            "exercise the incremental embed path",
            "--next-action",
            "check health",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "An attention row is a searchable document.",
            "--as",
            "tester",
            "--kind",
            "decision",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "A sitrep for the driver lane.",
            "--as",
            "tester",
            "--lane",
            "driver",
            "--json",
        ],
    );

    // (a) Every write embedded inline, so the index reports no missing vectors.
    let healthy = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(healthy["healthy"], true, "{healthy}");
    assert_eq!(
        healthy["projects"][0]["searchIndex"]["missingEmbeddings"], 0,
        "{healthy}"
    );
    assert_eq!(
        healthy["projects"][0]["searchIndex"]["healthy"], true,
        "{healthy}"
    );

    // (b) Null out the vectors directly; doctor must refuse to call it healthy
    // and must name the gap and the rebuild command in the reason.
    let board_path = board_path_for_project(&fixture, &fixture.main, "EMBED-HEALTH");
    Connection::open(&board_path)
        .unwrap()
        .execute("UPDATE search_documents SET embedding=NULL", [])
        .unwrap();

    let degraded = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !degraded.status.success(),
        "doctor must exit non-zero over a mostly-unembedded index"
    );
    let report: Value = serde_json::from_slice(&degraded.stdout).unwrap();
    assert_eq!(report["healthy"], false, "{report}");
    let search = &report["projects"][0]["searchIndex"];
    assert_eq!(search["healthy"], false, "{search}");
    assert!(
        search["missingEmbeddings"].as_i64().unwrap() > 0,
        "{search}"
    );
    let reasons = search["unhealthyBecause"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|reason| reason.as_str().unwrap().contains("have no embedding")),
        "reason does not name the missing vectors: {reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.as_str().unwrap().contains("search-rebuild")),
        "reason does not name the fix: {reasons:?}"
    );

    // (c) The explicit rebuild restores a clean bill of health.
    fixture.ok_json(
        &fixture.main,
        &["search-rebuild", "--as", "tester", "--json"],
    );
    let rebuilt = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(rebuilt["healthy"], true, "{rebuilt}");
    assert_eq!(
        rebuilt["projects"][0]["searchIndex"]["missingEmbeddings"], 0,
        "{rebuilt}"
    );
    assert_eq!(
        rebuilt["projects"][0]["searchIndex"]["healthy"], true,
        "{rebuilt}"
    );
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
    remove_v21_subscription_schema(&connection);
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
        24
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
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
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
fn a_json_refusal_reaches_stdout_as_an_error_object_and_exits_non_zero() {
    // `claim --candidates --json` without --as wrote its refusal to stderr
    // only, so a consumer piping stdout into a parser saw an empty result and
    // concluded there was no claimable work while P0 rows sat in todo. Absence
    // and error must not render identically on the surface a parser reads.
    let fixture = Fixture::new("json-refusal");
    fixture.ok_json(&fixture.main, &["init", "--name", "REFUSAL", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "claimable", "--id", "t-claimable", "--json"],
    );

    let refused = fixture.run(&fixture.main, &["claim", "--candidates", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(refusal_object(&refused), "--as is required");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--as is required"),
        "stderr still carries the refusal for the MCP layer and humans"
    );

    // The same refusal without --json stays prose on stderr and nothing on
    // stdout: a human reading a terminal did not ask for an object.
    let prose = fixture.run(&fixture.main, &["claim", "--candidates"]);
    assert_eq!(prose.status.code(), Some(1));
    assert!(prose.stdout.is_empty());

    // With --as the same command answers with the candidate list, so the two
    // outcomes a parser can meet are a bare array and an `error` object.
    let candidates = fixture.ok_json(
        &fixture.main,
        &["claim", "--candidates", "--as", "worker", "--json"],
    );
    assert_eq!(candidates.as_array().unwrap().len(), 1, "{candidates}");

    // A refusal raised before the parser has finished -- a flag missing its
    // value -- is still a refusal a --json caller asked to receive as JSON.
    let unparsed = fixture.run(&fixture.main, &["task", "list", "--json", "--status"]);
    assert_eq!(unparsed.status.code(), Some(1));
    assert_eq!(refusal_object(&unparsed), "--status requires a value");
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

/// A selector the caller typed outranks every environment default, and no read
/// stands a board up.
///
/// `direct_db` returned `--db` *or* `KANBAN_DB`, and `store_path` consulted it
/// before anything else, so with `KANBAN_DB` exported an explicit `--project`
/// was never reached. `task list --project Alpha` answered `[]` from the
/// environment's path — and created a fully migrated board there on the way.
/// The `[]` is the dangerous half: a plausible answer rather than an error, so
/// the caller acts on "no tasks" when the truth is "wrong board".
///
/// This has to cross a real process boundary. The defect is in how a process
/// reads its own environment against its own argv, and a unit test calling the
/// resolver in-process would inherit the harness's environment rather than a
/// controlled one — which is why the resolver's own tests never caught it.
#[test]
fn compiled_binary_lets_a_typed_selector_override_its_environment_default() {
    let fixture = Fixture::new("selector-precedence");
    let beta = fixture.root.join("beta");
    fs::create_dir_all(&beta).unwrap();

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
    // Every assertion naming this path also asserts it stayed absent, until the
    // last leg creates it deliberately.
    let ghost = fixture.root.join("ghost.db");
    let ghost_path = ghost.to_str().unwrap().to_owned();
    // Owns no project: resolution walks up from here and finds nothing, so any
    // leg that resolves a board did so through the selector under test.
    let outside = fixture.root.clone();

    let with_env = |key: &str, value: &str, args: &[&str]| -> Output {
        fixture
            .command(&outside)
            .env(key, value)
            .args(args)
            .output()
            .unwrap()
    };
    let ok_with_env = |key: &str, value: &str, args: &[&str]| -> Value {
        let output = with_env(key, value, args);
        assert!(
            output.status.success(),
            "{key}={value} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    // (1) KANBAN_DB set, --project typed: the flag wins, and the environment's
    // path is neither read nor created.
    assert_eq!(
        ids(&ok_with_env(
            "KANBAN_DB",
            &ghost_path,
            &["task", "list", "--project", "Alpha", "--json"]
        )),
        vec!["t-alpha".to_owned()],
        "KANBAN_DB outvoted an explicit --project"
    );
    assert!(
        !ghost.exists(),
        "an overridden KANBAN_DB still conjured a board"
    );

    // (2) Same for --workspace, which sat two rungs below KANBAN_DB.
    assert_eq!(
        ids(&ok_with_env(
            "KANBAN_DB",
            &ghost_path,
            &[
                "task",
                "list",
                "--workspace",
                beta.to_str().unwrap(),
                "--json"
            ]
        )),
        vec!["t-beta".to_owned()],
        "KANBAN_DB outvoted an explicit --workspace"
    );
    assert!(!ghost.exists());

    // (3) And the other direction: KANBAN_PROJECT is a default too.
    assert_eq!(
        ids(&ok_with_env(
            "KANBAN_PROJECT",
            "Beta",
            &["task", "list", "--db", &alpha_board, "--json"]
        )),
        vec!["t-alpha".to_owned()],
        "KANBAN_PROJECT outvoted an explicit --db"
    );
    assert_eq!(
        ids(&ok_with_env(
            "KANBAN_PROJECT",
            "Beta",
            &[
                "task",
                "list",
                "--workspace",
                fixture.main.to_str().unwrap(),
                "--json"
            ]
        )),
        vec!["t-alpha".to_owned()],
        "KANBAN_PROJECT outvoted an explicit --workspace"
    );

    // (4) A write obeys the same order. Answering from the wrong board is bad;
    // writing to it is the unrecoverable case ADR-007 exists to prevent.
    ok_with_env(
        "KANBAN_DB",
        &ghost_path,
        &[
            "task",
            "add",
            "typed",
            "--id",
            "t-typed",
            "--project",
            "Beta",
            "--json",
        ],
    );
    assert!(!ghost.exists(), "a write landed on the environment's board");
    assert!(
        ids(&fixture.ok_json(&outside, &["task", "list", "--project", "Beta", "--json"]))
            .contains(&"t-typed".to_owned()),
        "the write did not land on the board --project named"
    );

    // (5) With no flag to override it, each default still applies unchanged.
    assert_eq!(
        ids(&ok_with_env(
            "KANBAN_DB",
            &alpha_board,
            &["task", "list", "--json"]
        )),
        vec!["t-alpha".to_owned()],
        "KANBAN_DB stopped working as a default"
    );
    assert_eq!(
        ids(&ok_with_env(
            "KANBAN_PROJECT",
            "Alpha",
            &["task", "list", "--json"]
        )),
        vec!["t-alpha".to_owned()],
        "KANBAN_PROJECT stopped working as a default"
    );

    // (6) Two flags the caller typed stay a refusal. A default is not a second
    // request; a second flag is.
    let two_flags = fixture.run(
        &outside,
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
        "the two-flag refusal stopped firing"
    );
    let conflict = String::from_utf8_lossy(&two_flags.stderr).into_owned();
    assert!(conflict.contains("each name a board"), "{conflict}");

    // (7) A read reports a board file that is not there rather than creating
    // it, whichever selector named the path.
    let read_flag = fixture.run(&outside, &["task", "list", "--db", &ghost_path, "--json"]);
    assert!(
        !read_flag.status.success(),
        "a read created the board it was asked to read"
    );
    let read_message = String::from_utf8_lossy(&read_flag.stderr).into_owned();
    assert!(read_message.contains("does not exist"), "{read_message}");
    assert!(read_message.contains("never creates one"), "{read_message}");
    assert!(!ghost.exists(), "the refused read left a board behind");

    let read_env = with_env("KANBAN_DB", &ghost_path, &["task", "list", "--json"]);
    assert!(
        !read_env.status.success(),
        "a read through KANBAN_DB created the board it was asked to read"
    );
    assert!(!ghost.exists());

    // The read-only resolver reaches the same boards by a second code path, so
    // it carries both guarantees too.
    let read_only = fixture.run(
        &outside,
        &["subscription", "list", "--db", &ghost_path, "--json"],
    );
    assert!(
        !read_only.status.success(),
        "the read-only resolver created a board"
    );
    assert!(!ghost.exists());
    ok_with_env(
        "KANBAN_DB",
        &ghost_path,
        &["subscription", "list", "--project", "Alpha", "--json"],
    );
    assert!(!ghost.exists());

    // `watch` is the third caller of the same resolver, and it reaches it by a
    // branch of its own. `--limit 0` returns immediately, so this asks nothing
    // of the stream beyond which board it resolved. It emits no batch, so the
    // exit status is the whole assertion.
    let watched = with_env(
        "KANBAN_DB",
        &ghost_path,
        &["watch", "--limit", "0", "--project", "Alpha", "--json"],
    );
    assert!(
        watched.status.success(),
        "watch resolved the environment's board over an explicit --project: {}",
        String::from_utf8_lossy(&watched.stderr)
    );
    assert!(!ghost.exists());

    // (8) A write through KANBAN_DB does not create one either. An inherited or
    // mistyped default is not a request to make a board.
    let write_env = with_env(
        "KANBAN_DB",
        &ghost_path,
        &["task", "add", "ghost", "--json"],
    );
    assert!(
        !write_env.status.success(),
        "KANBAN_DB conjured a board on a write"
    );
    let write_message = String::from_utf8_lossy(&write_env.stderr).into_owned();
    assert!(write_message.contains("KANBAN_DB names"), "{write_message}");
    assert!(!ghost.exists());

    // (9) Naming the path on the command line still is such a request: that is
    // how a board outside the registry is made, and it keeps working.
    fixture.ok_json(
        &outside,
        &["task", "add", "deliberate", "--db", &ghost_path, "--json"],
    );
    assert!(
        ghost.is_file(),
        "--db on a command that writes no longer creates a board"
    );
    assert_eq!(
        ids(&fixture.ok_json(&outside, &["task", "list", "--db", &ghost_path, "--json"])).len(),
        1
    );
}

/// A board selector a command cannot honour is refused by name, not discarded.
///
/// `--db`, `--project` and `--workspace` are global flags, so every command
/// parses them and `reject_unknown` exempts them. A command that surveys the
/// registry instead of resolving one board therefore took a selector and threw
/// it away. `doctor --db /nowhere/absent.db --json` answered
/// `{"healthy": true, "projects": [...]}` — a survey of every registered board,
/// handed to an operator who had pointed the health check at one file that was
/// not even there. That is the worst shape a wrong answer can take: green, and
/// about a different subject. `backup --db` was the same defect with a quieter
/// receipt, and both skipped the data-root lock on the way, because
/// `lock::touches_data_root` asks whether the `--db` path lies inside the data
/// root and the command then ignored that path entirely — so
/// `restore --db /tmp/elsewhere.db --force` replaced the whole data root
/// without the exclusive lock that exists to keep readers off it.
///
/// This has to cross a real process boundary. The discard happens in argument
/// dispatch, above every resolver, so an in-process test of a resolver never
/// sees the command line that produced it.
#[test]
fn compiled_binary_refuses_a_board_selector_the_command_would_discard() {
    let fixture = Fixture::new("ignored-selectors");
    let alpha = fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let alpha_board = alpha["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(&fixture.worktree, &["init", "--name", "Beta", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "alpha work", "--id", "t-alpha", "--json"],
    );
    // Absent throughout: a refusal must not stand a board up on its way out.
    let ghost = fixture.root.join("ghost.db");
    let ghost_path = ghost.to_str().unwrap().to_owned();
    let worktree_path = fixture.worktree.to_str().unwrap().to_owned();

    // (1) The headline case, in both the shape that made it dangerous and the
    // shape that made it plausible: a path that is not there, and a real board.
    for path in [&ghost_path, &alpha_board] {
        let output = fixture.run(&fixture.main, &["doctor", "--db", path, "--json"]);
        assert!(
            !output.status.success(),
            "doctor --db {path} reported on every board and exited zero"
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            !stdout.contains("healthy"),
            "the refusal still printed a health receipt: {stdout}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("--db"),
            "the refusal does not name --db: {stderr}"
        );
        assert!(stderr.contains("doctor"), "{stderr}");
        assert!(
            stderr.contains("checks the registry and every board in it"),
            "the refusal does not say what doctor addresses instead: {stderr}"
        );
    }
    assert!(
        !ghost.exists(),
        "a refused doctor conjured the board it refused"
    );

    // (2) `backup` next, because its receipt is quiet enough to be believed:
    // `{"boards": []}` for a board that was never inspected. Nothing may be
    // written either — the refusal has to land before the snapshot starts.
    let output = fixture.run(&fixture.main, &["backup", "--db", &ghost_path, "--json"]);
    assert!(
        !output.status.success(),
        "backup --db snapshotted every board"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("--db"), "{stderr}");
    assert!(
        stderr.contains("snapshots the registry and every board in it"),
        "{stderr}"
    );
    assert!(
        !fixture.data.join("backups").exists(),
        "the refused backup wrote a snapshot anyway"
    );

    // (3) Every selector every command declares it discards, driven from the
    // manifest rather than restated here, and with values that are otherwise
    // perfectly good: `--project Beta` names a real project, and it is still
    // refused, because this command was never going to read it.
    let manifest = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let mut checked = 0;
    for operation in manifest["operations"].as_array().unwrap() {
        let ignored = operation["ignoredSelectors"].as_array().unwrap();
        if ignored.is_empty() {
            continue;
        }
        let command = operation["command"].as_str().unwrap();
        let sub = operation["subcommand"].as_str();
        for selector in ignored {
            let selector = selector.as_str().unwrap();
            let value = match selector {
                "db" => ghost_path.as_str(),
                "project" => "Beta",
                "workspace" => worktree_path.as_str(),
                other => panic!("unexpected selector {other}"),
            };
            let flag = format!("--{selector}");
            let mut args = vec![command];
            args.extend(sub);
            args.extend([flag.as_str(), value, "--json"]);
            let output = fixture.run(&fixture.main, &args);
            assert!(
                !output.status.success(),
                "{args:?} accepted a selector the manifest says it discards"
            );
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(
                stderr.contains(&flag),
                "{args:?} was refused without naming {flag}: {stderr}"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 30,
        "the manifest declared only {checked} ignored selectors; the table shrank"
    );
    assert!(!ghost.exists());

    // (4) `events --registry` and `events --rule` read the registry trail, and
    // `watch` has refused a board selector on that trail since it was written.
    // The two spoke differently about the same command line.
    for args in [
        vec![
            "events",
            "--registry",
            "--db",
            ghost_path.as_str(),
            "--json",
        ],
        vec![
            "events",
            "--rule",
            "r-nothing",
            "--project",
            "Beta",
            "--json",
        ],
    ] {
        let output = fixture.run(&fixture.main, &args);
        assert!(!output.status.success(), "{args:?} took a board selector");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("read the registry trail"),
            "{args:?}: {stderr}"
        );
    }

    // (5) Nothing that worked stopped working. The selectors these commands do
    // honour are the whole reason this is a per-command list.
    let attached = fixture.root.join("attached");
    fs::create_dir_all(&attached).unwrap();
    fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "attach",
            "--workspace",
            attached.to_str().unwrap(),
            "--to",
            "Alpha",
            "--json",
        ],
    );
    let fresh = fixture.root.join("fresh");
    fs::create_dir_all(&fresh).unwrap();
    fixture.ok_json(
        &fixture.main,
        &[
            "init",
            "--name",
            "Gamma",
            "--workspace",
            fresh.to_str().unwrap(),
            "--json",
        ],
    );
    for args in [
        vec!["task", "list", "--db", alpha_board.as_str(), "--json"],
        vec!["task", "list", "--project", "Beta", "--json"],
        vec![
            "task",
            "list",
            "--workspace",
            worktree_path.as_str(),
            "--json",
        ],
        vec!["events", "--db", alpha_board.as_str(), "--json"],
    ] {
        fixture.ok_json(&fixture.main, &args);
    }
    // And every command that refuses a selector still answers without one.
    for args in [
        vec!["doctor", "--json"],
        vec!["dashboard", "--json"],
        vec!["audit", "verify", "--json"],
        vec!["workspace", "list", "--json"],
        vec!["schema", "--json"],
        vec!["backup", "--json"],
        vec!["rule", "list", "--json"],
    ] {
        fixture.ok_json(&fixture.main, &args);
    }

    // (6) Ordering, which a table-driven guard placed early is easy to get
    // wrong: a flag that no longer exists outranks one that is merely
    // inapplicable, because "this flag was removed" is the more actionable
    // complaint about the same command line.
    let both = fixture.run(
        &fixture.main,
        &["rule", "list", "--global", "--project", "Beta", "--json"],
    );
    assert!(!both.status.success());
    let stderr = String::from_utf8_lossy(&both.stderr).into_owned();
    assert!(
        stderr.contains("superseded"),
        "the inapplicable --project outranked the superseded --global: {stderr}"
    );
    let only_selector = fixture.run(
        &fixture.main,
        &["rule", "list", "--project", "Beta", "--json"],
    );
    assert!(!only_selector.status.success());
    assert!(
        String::from_utf8_lossy(&only_selector.stderr).contains("--project"),
        "{}",
        String::from_utf8_lossy(&only_selector.stderr)
    );
}

/// No operation takes a board selector and answers without it.
///
/// The net behind the table: a command either resolves the board it was given —
/// and a board that is not there is an error — or refuses the flag. Neither
/// exits zero. A command added to neither list fails here rather than reaching
/// an operator with a confident answer about a board nobody named.
#[test]
fn compiled_binary_lets_no_operation_succeed_while_discarding_a_selector() {
    let fixture = Fixture::new("selector-net");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    // Inside a directory that does not exist, so even the one operation
    // permitted to create a board cannot bring this one into being.
    let absent = fixture.root.join("no-such-directory").join("absent.db");
    let absent_path = absent.to_str().unwrap().to_owned();
    let nowhere = fixture.root.join("no-such-tree");
    let nowhere_path = nowhere.to_str().unwrap().to_owned();

    let manifest = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    for operation in manifest["operations"].as_array().unwrap() {
        // `serve`, `mcp` and `watch` block until killed. The first two refuse
        // every selector before they start and are covered by the manifest
        // sweep above; `watch` resolves one, and its own tests cover it.
        if operation["longRunning"].as_bool().unwrap() {
            continue;
        }
        let command = operation["command"].as_str().unwrap();
        let sub = operation["subcommand"].as_str();
        for (flag, value) in [
            ("--db", absent_path.as_str()),
            ("--project", "no-such-project"),
            ("--workspace", nowhere_path.as_str()),
        ] {
            let mut args = vec![command];
            args.extend(sub);
            args.extend([flag, value, "--json"]);
            let output = fixture.run(&fixture.main, &args);
            assert!(
                !output.status.success(),
                "{args:?} exited zero: it neither resolved {flag} nor refused it\nstdout: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
    assert!(!absent.exists(), "the sweep created a board");
}

/// A command that discards `--db` locks the data root whatever `KANBAN_DB` says.
///
/// Refusing the flag narrowed this defect; it did not close it.
/// `reject_ignored_selectors` counts typed flags only — correctly, because every
/// agent cage exports `KANBAN_DB` and an exported default must not break
/// `doctor` — so the environment still reached `board_selection`, and
/// `touches_data_root` still decided the lock from a `--db` value the command
/// went on to ignore. `KANBAN_DB=/tmp/elsewhere.db kanban restore --from SNAP
/// --force` therefore replaced the entire data root with **no exclusive lock**,
/// the flag's only effect being to suppress the lock. On a box running many
/// agents against one data root that is a corruption route.
///
/// The lock lives in the kernel and the environment is read by the process
/// under test, so this can only be measured across a real process boundary:
/// the test holds the flock itself and watches the compiled binary contend.
#[test]
fn compiled_binary_locks_the_data_root_even_when_the_environment_names_a_board() {
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
    let fixture = Fixture::new("lock-vs-ignored-selector");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "survives", "--id", "t-keep", "--json"],
    );
    let snapshot = fixture.root.join("snap");
    fixture.ok_json(
        &fixture.main,
        &["backup", "--output", snapshot.to_str().unwrap(), "--json"],
    );
    // Outside the data root, which is what made `touches_data_root` answer
    // "this invocation touches nothing of mine".
    let elsewhere = fixture.root.join("elsewhere.db");
    let elsewhere_path = elsewhere.to_str().unwrap().to_owned();
    let with_env_db = |args: &[&str]| -> Output {
        fixture
            .command(&fixture.main)
            .env("KANBAN_DB", &elsewhere_path)
            .args(args)
            .output()
            .unwrap()
    };

    let lock_file = || {
        fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(fixture.data.join(".lock"))
            .unwrap()
    };

    // (1) One live board command holds the root shared. `restore` takes it
    // exclusively and refuses immediately, so it must not get past this.
    let held = lock_file();
    held.lock_shared().unwrap();
    let restore = with_env_db(&[
        "restore",
        "--from",
        snapshot.to_str().unwrap(),
        "--force",
        "--json",
    ]);
    assert!(
        !restore.status.success(),
        "restore replaced the data root with no exclusive lock, because KANBAN_DB \
         named a board it then ignored"
    );
    let stderr = String::from_utf8_lossy(&restore.stderr).into_owned();
    assert!(
        stderr.contains("another kanban process is using"),
        "restore did not contend for the exclusive lock: {stderr}"
    );
    drop(held);
    // The work state is still the live one, not the snapshot's.
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"])[0]["id"],
        "t-keep"
    );

    // (2) The other direction: a restore holds the root exclusively, and the
    // surveying commands must queue behind it rather than read through it.
    // Each waits out `lock::WAIT` before refusing.
    let held = lock_file();
    held.lock().unwrap();
    for command in ["doctor", "backup"] {
        let output = with_env_db(&[command, "--json"]);
        assert!(
            !output.status.success(),
            "{command} read the data root through a restore's exclusive lock"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("restore is replacing"),
            "{command} did not wait on the shared lock: {stderr}"
        );
    }
    drop(held);

    // (3) And none of this broke restore. With nothing holding the root it
    // still runs to completion under the same environment.
    let restored = with_env_db(&[
        "restore",
        "--from",
        snapshot.to_str().unwrap(),
        "--force",
        "--json",
    ]);
    assert!(
        restored.status.success(),
        "restore stopped working: {}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"])[0]["id"],
        "t-keep"
    );
    assert!(
        !elsewhere.exists(),
        "the ignored KANBAN_DB path was conjured into existence"
    );
}

/// A `--db` that reaches a data-root board through a symlink still locks it.
///
/// `lock::contains` compared lexically absolute paths, resolving `.` and `..`
/// textually and symlinks not at all. A board at `<data root>/boards/<uuid>.db`
/// addressed as `/tmp/link.db` therefore compared as *outside* the root and took
/// no lock, so `kanban task add --db /tmp/link.db` mutated a database file while
/// a `restore` holding the root exclusively believed it had every writer
/// excluded — and that restore renames whole files into place behind SQLite's
/// back, which is the one thing no transaction can protect against.
///
/// A symlink is one `ln -s` away in an agent cage, and the lock lives in the
/// kernel, so this can only be measured across a real process boundary: the
/// test holds the flock itself and watches the compiled binary contend.
#[test]
fn compiled_binary_locks_the_data_root_for_a_board_reached_through_a_symlink() {
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
    let fixture = Fixture::new("lock-vs-symlinked-board");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let board = board_path_for_project(&fixture, &fixture.main, "Alpha");
    assert!(
        board
            .canonicalize()
            .unwrap()
            .starts_with(fixture.data.canonicalize().unwrap()),
        "the registry put the board somewhere other than the data root: {}",
        board.display()
    );
    // The board is inside the data root. This name for it is not.
    let link = fixture.root.join("link.db");
    std::os::unix::fs::symlink(&board, &link).unwrap();
    let link_arg = link.to_str().unwrap().to_owned();
    let through_link = |args: &[&str]| -> Output {
        fixture
            .command(&fixture.main)
            .args(args)
            .args(["--db", &link_arg, "--json"])
            .output()
            .unwrap()
    };

    // A restore holds the root exclusively. Writing through the symlink is
    // writing to a file that restore is about to rename over, so it must queue
    // behind it rather than walk straight past.
    let held = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fixture.data.join(".lock"))
        .unwrap();
    held.lock().unwrap();
    let blocked = through_link(&["task", "add", "written under a restore"]);
    assert!(
        !blocked.status.success(),
        "a symlinked --db wrote to a data-root board through a restore's \
         exclusive lock"
    );
    let stderr = String::from_utf8_lossy(&blocked.stderr).into_owned();
    assert!(
        stderr.contains("restore is replacing"),
        "the symlinked --db did not wait on the shared lock: {stderr}"
    );
    drop(held);
    assert!(
        fixture
            .ok_json(&fixture.main, &["task", "list", "--json"])
            .as_array()
            .unwrap()
            .is_empty(),
        "the refused write landed on the board anyway"
    );

    // And nothing else changed: with the root free the same command works, and
    // it works on the board the symlink points at.
    let added = through_link(&[
        "task",
        "add",
        "written with the root free",
        "--id",
        "t-through-link",
    ]);
    assert!(
        added.status.success(),
        "the symlinked --db stopped working: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    assert_eq!(
        listed[0]["id"].as_str(),
        Some("t-through-link"),
        "the write did not land on the registered board: {listed}"
    );
}

/// A selector the manifest says an operation accepts actually works on it.
///
/// The mirror of the refusal sweep, and the one that is not self-referential.
/// Every other guard reads `IGNORED_SELECTORS` and checks something against it,
/// so a command missing from the table looks consistent to all of them: `rule`
/// refused all three selectors from two inline loops in the dispatcher while
/// declaring nothing, and the MCP tool builder — which withholds exactly what
/// the table names — went on advertising `project` on `rule_list`. An agent
/// could read the schema, send the argument it was offered, and be told
/// `--project does not select a rule collection`.
///
/// This asks the binary instead of the table: for every read-only operation
/// that needs no positional, each selector the manifest leaves out of
/// `ignoredSelectors` is passed a valid value and must be honoured. A command
/// that refuses a selector it never declared fails here.
#[test]
fn compiled_binary_honours_every_selector_the_manifest_says_it_accepts() {
    let fixture = Fixture::new("selector-applicability");
    let alpha = fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let board = alpha["boardPath"].as_str().unwrap().to_owned();
    let main = fixture.main.to_str().unwrap().to_owned();

    let manifest = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let mut checked = 0;
    for operation in manifest["operations"].as_array().unwrap() {
        // Read-only and positional-free, so a valid selector is the only input
        // the command needs and success is the whole assertion.
        if !operation["readOnly"].as_bool().unwrap()
            || operation["longRunning"].as_bool().unwrap()
            || !operation["positionals"].as_array().unwrap().is_empty()
        {
            continue;
        }
        let ignored = operation["ignoredSelectors"].as_array().unwrap();
        let command = operation["command"].as_str().unwrap();
        let sub = operation["subcommand"].as_str();
        for (selector, value) in [
            ("db", board.as_str()),
            ("project", "Alpha"),
            ("workspace", main.as_str()),
        ] {
            if ignored.iter().any(|declared| declared == selector) {
                continue;
            }
            let flag = format!("--{selector}");
            let mut args = vec![command];
            args.extend(sub);
            args.extend([flag.as_str(), value, "--json"]);
            let output = fixture.run(&fixture.main, &args);
            assert!(
                output.status.success(),
                "{args:?} refused a selector the manifest does not list as ignored\n\
                 stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 27,
        "only {checked} applicable selectors were exercised; the set shrank"
    );
}

/// A board file comes into existence only where creating one is the point.
///
/// Permission to create was derived from the `readOnly` bit in `COMMANDS`, and
/// that bit answers a different question — whether an operation writes anything
/// *anywhere* — which is why `backup` and `todo` are not read-only despite
/// changing no work state. Ask it about board creation and it answers about
/// file writes, and the two diverge exactly where it hurts.
///
/// Measured against the tree before this fix: `archive --dry-run
/// --older-than-days 30 --as me --db <typo>` reported zero rows, exited 0, and
/// left a 372736-byte migrated board at the typo — from a flag whose entire
/// promise is to change nothing. `todo --db <typo>` did the same.
#[test]
fn compiled_binary_creates_a_board_only_where_creation_is_the_point() {
    let fixture = Fixture::new("board-creation");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);

    // Each leg gets its own path, so "no file appeared" is about this command
    // and not about a neighbour having cleaned up.
    let refuses = |label: &str, args: &[&str]| {
        let ghost = fixture.root.join(format!("{label}.db"));
        let path = ghost.to_str().unwrap().to_owned();
        let mut argv = args.to_vec();
        argv.extend(["--db", path.as_str(), "--json"]);
        let output = fixture.run(&fixture.main, &argv);
        assert!(
            !output.status.success(),
            "{label} answered from a board it created instead of reporting"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("does not exist") && stderr.contains("never creates one"),
            "{label} refused for some other reason, so this proves nothing: {stderr}"
        );
        assert!(
            !ghost.exists(),
            "{label} left a board behind at a path it refused to answer from"
        );
    };

    // The two the `readOnly` derivation got wrong. `--dry-run` is the sharpest:
    // the flag exists to promise nothing changes.
    refuses(
        "archive-dry-run",
        &[
            "archive",
            "--dry-run",
            "--older-than-days",
            "30",
            "--as",
            "me",
        ],
    );
    refuses("todo", &["todo"]);
    // A plain read, which the derivation did get right — kept so a later
    // simplification cannot quietly lose it.
    refuses("task-list", &["task", "list"]);
    // `watch` reaches the board by a branch of its own that bypassed the guard
    // entirely. It was safe only because `Store::open_readonly` passes
    // SQLITE_OPEN_READ_ONLY and physically cannot create; the diagnosis was a
    // raw `Error code 14`.
    refuses("watch", &["watch", "--limit", "0"]);

    // And through the environment, where the honest complaint is different:
    // nobody typed this path.
    let env_ghost = fixture.root.join("watch-env.db");
    let watched = fixture
        .command(&fixture.main)
        .env("KANBAN_DB", env_ghost.to_str().unwrap())
        .args(["watch", "--limit", "0", "--json"])
        .output()
        .unwrap();
    assert!(!watched.status.success());
    let watched_stderr = String::from_utf8_lossy(&watched.stderr).into_owned();
    assert!(
        watched_stderr.contains("KANBAN_DB names"),
        "watch did not name the environment default as the problem: {watched_stderr}"
    );
    assert!(!env_ghost.exists());

    // The one command whose point is to put the first work state somewhere
    // still does, or a scratch board becomes uncreatable.
    let made = fixture.root.join("made.db");
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "first",
            "--db",
            made.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        made.is_file(),
        "task add --db no longer starts a board outside the registry"
    );
}

/// Widening the set of commands that may create a board is a deliberate act.
///
/// The allowlist has one entry and the dangerous direction is silent growth: a
/// command that creates when it should not answers from a board it just made,
/// which is indistinguishable from the empty board the caller meant. The
/// manifest publishes the bit, so this reads it back and fails until a new
/// creator is written down here too.
#[test]
fn the_only_board_creator_is_declared() {
    let fixture = Fixture::new("board-creators");
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operations = schema["operations"].as_array().unwrap();

    let creators = operations
        .iter()
        .filter(|operation| operation["createsBoard"] == true)
        .map(|operation| operation["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        creators,
        vec!["task add".to_owned()],
        "the set of commands that may bring a board into existence changed"
    );

    // And the bit is not a restatement of `readOnly`. These two are the reason
    // deriving one from the other was wrong, so pin the disagreement: someone
    // "fixing" this by flipping `readOnly` is changing the wrong thing.
    for name in ["todo", "archive"] {
        let operation = operations
            .iter()
            .find(|operation| operation["name"] == name)
            .unwrap_or_else(|| panic!("{name} is missing from the manifest"));
        assert_eq!(
            operation["readOnly"], false,
            "{name} writes something somewhere, which is exactly why readOnly \
             could not answer whether it may create a board"
        );
        assert_eq!(
            operation["createsBoard"], false,
            "{name} may create a board"
        );
    }
}

/// A mistyped `--db` never overwrites what is already at the path, and a
/// command that refuses leaves the filesystem as it found it.
///
/// Two defects, both measured against the tree before this fix:
///
/// The board guard asked `Path::is_file`, so a path naming an existing EMPTY
/// file passed it. `task list --db notes.txt` against a 0-byte file printed
/// `[]` and left 372736 bytes of SQLite where the operator's file had been —
/// a plausible wrong answer and a destroyed file in one command. A non-empty
/// non-SQLite file already failed loudly, so empty files were the whole hole.
///
/// And `open_store` ran before any command read its own positionals, so
/// `task add --db /new/deep/nest/board.db` with no title printed "task title is
/// required" *after* creating the board and both directories above it.
#[test]
fn compiled_binary_never_overwrites_a_file_it_was_pointed_at_by_mistake() {
    let fixture = Fixture::new("mistyped-db");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);

    // A file that exists and is not a board: the guard has to read it, not
    // merely stat it.
    let empty = fixture.root.join("notes.txt");
    fs::write(&empty, b"").unwrap();
    let prose = fixture.root.join("notes.md");
    fs::write(&prose, b"my important notes\n").unwrap();

    // And a real SQLite database belonging to something else. This is the one
    // a header check cannot catch: it IS SQLite, so `migrate` started from its
    // `user_version` of 0 and ran the whole ladder into it — measured at 8192
    // bytes in and 376832 bytes out, `bookmarks` still sitting among 26 kanban
    // tables. Being a database is not the same as being *this* database.
    let foreign = fixture.root.join("firefox.db");
    {
        let connection = Connection::open(&foreign).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE bookmarks(id INTEGER PRIMARY KEY, url TEXT);\
                 INSERT INTO bookmarks VALUES(1,'https://example.com');",
            )
            .unwrap();
    }

    for (label, victim) in [("empty", &empty), ("prose", &prose), ("sqlite", &foreign)] {
        let before = fs::read(victim).unwrap();
        // Both a pure read and the one command allowed to create a board.
        // Permission to start one where there is nothing is not permission to
        // overwrite something that is already there.
        for command in [
            vec!["task", "list"],
            vec!["task", "add", "clobber"],
            vec!["todo"],
        ] {
            let mut argv = command.clone();
            argv.extend(["--db", victim.to_str().unwrap(), "--json"]);
            let output = fixture.run(&fixture.main, &argv);
            assert!(
                !output.status.success(),
                "{label}: {command:?} opened a file that is not a board"
            );
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(
                stderr.contains("is not a Kanban board"),
                "{label}: {command:?} refused for some other reason: {stderr}"
            );
            assert_eq!(
                fs::read(victim).unwrap(),
                before,
                "{label}: {command:?} rewrote a file it was pointed at by mistake"
            );
        }
    }

    // The guard reads the SQLite header, so a real board is still just a board.
    let board = board_path_for_project(&fixture, &fixture.main, "Alpha");
    fixture.ok_json(
        &fixture.main,
        &["task", "list", "--db", board.to_str().unwrap(), "--json"],
    );

    // A command that cannot run creates nothing on its way to saying so —
    // not the board, and not the directories above it.
    let nest = fixture.root.join("deep/nest");
    let typo = nest.join("typo.db");
    let untitled = fixture.run(
        &fixture.main,
        &["task", "add", "--db", typo.to_str().unwrap(), "--json"],
    );
    assert!(!untitled.status.success());
    // The filesystem claim first: it is the one that matters, and asserting the
    // message first would let a mutation be caught by the wrong assertion.
    assert!(!typo.exists(), "a failed task add left a board behind");
    assert!(
        !nest.exists() && !fixture.root.join("deep").exists(),
        "a failed task add left the directories it would have needed"
    );
    let untitled_stderr = String::from_utf8_lossy(&untitled.stderr).into_owned();
    assert!(
        untitled_stderr.contains("title is required"),
        "{untitled_stderr}"
    );
    assert!(
        untitled_stderr.contains("usage: kanban task add TITLE"),
        "the refusal must say what the command wanted: {untitled_stderr}"
    );

    // With the title supplied, the same path is still created — absence is
    // what makes a new board legitimate, and that has not changed.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "titled",
            "--db",
            typo.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(typo.is_file(), "task add --db no longer starts a new board");
}

/// Classifying a board path answers promptly and does not say false things.
///
/// Two defects in the guard that replaced `Path::is_file`, both introduced by
/// the fix for the empty-file case:
///
/// It reached straight for `File::open`. `open(O_RDONLY)` on a FIFO blocks
/// until a writer appears, and Rust passes no `O_NONBLOCK` — and this runs in a
/// loop over every registered board in `doctor`, `dashboard`, `backup`,
/// `restore`, `audit verify` and both `--all-boards` searches. One FIFO would
/// stop the survey of all the others with no output and no timeout, which is
/// precisely what the survey design exists to avoid. `is_file()` answered in
/// microseconds and is false for a FIFO, so the stat goes first.
///
/// And an unreadable file was classified as "not a Kanban board", which is a
/// false statement about intact data and sends the operator hunting for
/// corruption instead of a permission bit.
#[test]
fn compiled_binary_classifies_a_board_path_promptly_and_truthfully() {
    let fixture = Fixture::new("board-path-classification");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let board = board_path_for_project(&fixture, &fixture.main, "Alpha");

    // A FIFO. The assertion is the deadline: without the stat this never
    // returns at all, so the test would hang rather than fail. Waiting with a
    // bound turns that into a reportable failure.
    let fifo = fixture.root.join("pipe.db");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be available to exercise the blocking-open case");
    assert!(made.success(), "mkfifo failed");

    let mut child = fixture
        .command(&fixture.main)
        .args(["task", "list", "--db", fifo.to_str().unwrap(), "--json"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let finished = loop {
        match child.try_wait().unwrap() {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let Some(status) = finished else {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("classifying a FIFO blocked; one bad path would hang every survey");
    };
    assert!(!status.success(), "a FIFO was accepted as a board");

    // An intact board that cannot be read is reported as unreadable, not as
    // something it is not.
    //
    // Root bypasses the mode bits, so the scenario is unreachable when this
    // runs privileged. Both branches assert a true thing rather than skipping:
    // if the harness itself can still read the file, so can the binary, and the
    // command must succeed.
    fs::set_permissions(&board, fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = fs::read(&board).is_ok();
    let denied = fixture.run(
        &fixture.main,
        &["task", "list", "--db", board.to_str().unwrap(), "--json"],
    );
    fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();

    if readable_anyway {
        assert!(
            denied.status.success(),
            "running privileged, so the board was readable and the read should have worked: {}",
            String::from_utf8_lossy(&denied.stderr)
        );
    } else {
        assert!(!denied.status.success());
        let stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
        assert!(
            stderr.contains("cannot be read"),
            "an unreadable board must say so: {stderr}"
        );
        assert!(
            !stderr.contains("is not a Kanban board"),
            "an intact board was reported as not being one: {stderr}"
        );
    }

    // The file survived being classified, either way.
    assert!(board.is_file());
    fixture.ok_json(
        &fixture.main,
        &["task", "list", "--db", board.to_str().unwrap(), "--json"],
    );

    // A SQLite failure is not a verdict about what the file holds. The probe
    // returned `bool`, so BUSY, CORRUPT, NOTADB and IOERR all reached the
    // operator as "this is not a Kanban board" — a claim about contents that
    // nothing had established. A damaged file gets that treatment immediately,
    // with no lock to wait on.
    let damaged = fixture.root.join("damaged.db");
    let mut bytes = b"SQLite format 3\0".to_vec();
    bytes.extend(std::iter::repeat_n(0xA5u8, 4096));
    fs::write(&damaged, &bytes).unwrap();
    let reported = fixture.run(
        &fixture.main,
        &["task", "list", "--db", damaged.to_str().unwrap(), "--json"],
    );
    assert!(!reported.status.success());
    let reported_stderr = String::from_utf8_lossy(&reported.stderr).into_owned();
    assert!(
        reported_stderr.contains("cannot be read"),
        "a damaged database must be reported as unreadable: {reported_stderr}"
    );
    assert!(
        !reported_stderr.contains("is not a Kanban board"),
        "a SQLite failure was turned into a claim about the file's contents: {reported_stderr}"
    );
    assert_eq!(
        fs::read(&damaged).unwrap(),
        bytes,
        "a damaged file was written to while being classified"
    );
}

/// An interrupted board creation is recoverable, not a permanent refusal.
///
/// `open` creates the file and sets `journal_mode=WAL` before the first
/// migration commits, so a Ctrl-C, a kill or ENOSPC in that window leaves a
/// database with no tables. Classified as a stranger's database, the retry of
/// the very command that was interrupted is refused forever, with a message
/// asserting the path holds something it does not.
///
/// `init` makes it worse: it commits the registry row before `Store::open` runs
/// the migrations, so an interrupt there strands a *registered* board that no
/// command can open.
#[test]
fn compiled_binary_finishes_a_board_creation_that_was_interrupted() {
    let fixture = Fixture::new("interrupted-creation");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);

    // Exactly what `open` leaves behind before the first migration commits.
    let half = fixture.root.join("half.db");
    {
        let connection = Connection::open(&half).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
    }

    // A command that does not create says what it found and both ways out.
    let reported = fixture.run(
        &fixture.main,
        &["task", "list", "--db", half.to_str().unwrap(), "--json"],
    );
    assert!(!reported.status.success());
    let stderr = String::from_utf8_lossy(&reported.stderr).into_owned();
    // What was observed, not what caused it.
    assert!(
        stderr.contains("no tables in it"),
        "the message must report what it saw: {stderr}"
    );
    // Both causes, because nothing here can tell them apart: under WAL this
    // probe sees last-committed state, so a creation running in another process
    // right now looks exactly like one abandoned an hour ago.
    assert!(
        stderr.contains("interrupted") && stderr.contains("another process"),
        "the message must name both causes, not assert one: {stderr}"
    );
    // And it must never call the file abandoned, or call removal safe. Both
    // are false during a concurrent creation, and the second is destructive
    // advice stated as fact.
    assert!(
        !stderr.contains("loses no work") && !stderr.contains("holds nothing"),
        "the message asserted that deleting the file is safe: {stderr}"
    );
    assert!(
        stderr.contains("confirm no other process is creating it"),
        "removal must be conditioned on the check only the operator can make: {stderr}"
    );

    // And the command that creates finishes the job rather than refusing.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "recovered",
            "--db",
            half.to_str().unwrap(),
            "--json",
        ],
    );
    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--db", half.to_str().unwrap(), "--json"],
    );
    assert_eq!(listed[0]["title"], "recovered");

    // A registered board interrupted the same way is not stranded: `init`
    // commits the registry row first, so refusing here would leave a project
    // no command could open.
    let stranded = fixture.root.join("stranded");
    fs::create_dir_all(&stranded).unwrap();
    fixture.ok_json(&stranded, &["init", "--name", "Stranded", "--json"]);
    let registered = board_path_for_project(&fixture, &stranded, "Stranded");
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", registered.display()));
    }
    {
        let connection = Connection::open(&registered).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
    }
    fixture.ok_json(&stranded, &["task", "add", "after the interrupt", "--json"]);
}

/// A board that is behind on migrations is still a board.
///
/// The schema check is the one that refuses a stranger's database, and the
/// risk it carries is refusing one of ours mid-upgrade. It looks for the three
/// tables `BOARD_V1` creates and nothing has dropped since, so every version
/// from v1 to current passes it and `open_board` migrates as it always did.
/// A stricter signal — `user_version` equal to the current schema — would have
/// turned every board due an upgrade into "not a Kanban board".
#[test]
fn compiled_binary_still_migrates_a_board_that_is_behind() {
    let fixture = Fixture::new("behind-schema");
    fixture.ok_json(&fixture.main, &["init", "--name", "Behind", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "older work", "--id", "t-old", "--json"],
    );
    let board = board_path_for_project(&fixture, &fixture.main, "Behind");

    let current: i64 = Connection::open(&board)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert!(current > 1, "expected a migrated board, got v{current}");
    Connection::open(&board)
        .unwrap()
        .execute_batch(&format!("PRAGMA user_version={}", current - 1))
        .unwrap();

    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--db", board.to_str().unwrap(), "--json"],
    );
    assert_eq!(
        listed[0]["id"], "t-old",
        "a board one version behind was refused"
    );
    let after: i64 = Connection::open(&board)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, current, "the board was not migrated forward");
}

#[test]
fn compiled_binary_keeps_rootless_boards_out_of_unreachable_roots() {
    let fixture = Fixture::new("rootless-doctor-repoint");
    let rootless = fixture.root.join("rootless");
    fs::create_dir_all(&rootless).unwrap();
    let rootless = rootless.canonicalize().unwrap();

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

    // A rootless board deliberately has no filesystem discovery hint. Neither
    // standing in the directory used to create it nor naming that directory
    // as a workspace may make it reachable by accident.
    for (label, output) in [
        (
            "bare cwd",
            fixture.run(&rootless, &["task", "show", "t-rootless", "--json"]),
        ),
        (
            "explicit --workspace",
            fixture.run(
                &fixture.main,
                &[
                    "task",
                    "show",
                    "t-rootless",
                    "--workspace",
                    rootless.to_str().unwrap(),
                    "--json",
                ],
            ),
        ),
    ] {
        assert!(
            !output.status.success(),
            "{label} reached a rootless board through its creation directory"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no Kanban project contains"),
            "{label}: {stderr}"
        );
        assert!(
            stderr.contains(rootless.to_str().unwrap()),
            "{label} did not identify the unresolved directory: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("t-rootless"),
            "{label} returned the rootless task while refusing"
        );
    }

    let by_project = fixture.ok_json(
        &rootless,
        &[
            "task",
            "show",
            "t-rootless",
            "--project",
            "ROOTLESS",
            "--json",
        ],
    );
    assert_eq!(by_project["id"], "t-rootless");
    let by_env = fixture
        .command(&rootless)
        .env("KANBAN_PROJECT", "ROOTLESS")
        .args(["task", "show", "t-rootless", "--json"])
        .output()
        .unwrap();
    assert!(
        by_env.status.success(),
        "KANBAN_PROJECT did not reach the rootless board: {}",
        String::from_utf8_lossy(&by_env.stderr)
    );
    let by_env: Value = serde_json::from_slice(&by_env.stdout).unwrap();
    assert_eq!(by_env["id"], "t-rootless");

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
        version.contains("board schema 24"),
        "version output: {version}"
    );
    assert!(
        version.contains("registry schema 14"),
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
    // Directories kanban does create are private from creation. The vehicle
    // has to be a command that writes: a read no longer stands a board up, so
    // `task list` would report the missing file instead of creating anything.
    let nested = shared.join("deep/nest/board.db");
    fixture.ok_json(
        &fixture.main,
        &[
            "--db",
            nested.to_str().unwrap(),
            "task",
            "add",
            "nested board",
            "--json",
        ],
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
fn compiled_binary_resolves_real_git_worktrees_and_submodules_by_registered_roots() {
    let fixture = Fixture::new("git-root-resolution");
    let neighbor = fixture.root.join("neighbor");
    let ordinary_child = fixture.main.join("ordinary-child");
    fs::create_dir_all(&neighbor).unwrap();
    fs::create_dir_all(&ordinary_child).unwrap();

    fixture.ok_json(&fixture.main, &["init", "--name", "GIT-MAIN", "--json"]);
    fixture.ok_json(&neighbor, &["init", "--name", "NEIGHBOR", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "main topology sentinel",
            "--id",
            "t-topology-main",
            "--json",
        ],
    );
    fixture.ok_json(
        &neighbor,
        &[
            "task",
            "add",
            "neighbor topology sentinel",
            "--id",
            "t-topology-neighbor",
            "--json",
        ],
    );

    // These filesystem assertions are the resolver contract and always run,
    // even on a host where Git cannot construct the topology-specific legs.
    let from_child = fixture.ok_json(
        &ordinary_child,
        &["task", "show", "t-topology-main", "--json"],
    );
    assert_eq!(from_child["title"], "main topology sentinel");
    assert_eq!(from_child["id"], "t-topology-main");
    let outside = fixture.run(
        &fixture.root,
        &["task", "show", "t-topology-main", "--json"],
    );
    assert!(
        !outside.status.success(),
        "an unattached sibling inherited a board below it"
    );
    let outside_stderr = String::from_utf8_lossy(&outside.stderr);
    assert!(
        outside_stderr.contains("no Kanban project contains"),
        "{outside_stderr}"
    );
    assert!(!String::from_utf8_lossy(&outside.stdout).contains("topology sentinel"));

    let git_probe = match Command::new("git").arg("--version").output() {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            eprintln!("git unavailable; skipping git topology assertions");
            return;
        }
        Err(error) => panic!("failed to probe git availability: {error}"),
    };
    assert!(
        git_probe.status.success(),
        "git --version failed: {}",
        String::from_utf8_lossy(&git_probe.stderr)
    );
    assert!(
        make_repo(&fixture.main),
        "git repository setup failed after git availability was proven"
    );
    let submodule_source = fixture.root.join("submodule-source");
    fs::create_dir_all(&submodule_source).unwrap();
    assert!(
        make_repo(&submodule_source),
        "local submodule repository setup failed after git availability was proven"
    );
    let submodule = Command::new("git")
        .arg("-c")
        .arg("protocol.file.allow=always")
        .arg("-C")
        .arg(&fixture.main)
        .args([
            "submodule",
            "add",
            "-q",
            submodule_source.to_str().unwrap(),
            "modules/local",
        ])
        .output()
        .expect("spawn git submodule add");
    assert!(
        submodule.status.success(),
        "git submodule add failed: {}",
        String::from_utf8_lossy(&submodule.stderr)
    );
    assert!(
        commit_all(&fixture.main, "add local submodule"),
        "git commit failed after local submodule setup"
    );

    let linked = fixture.root.join("linked-worktree");
    let worktree = Command::new("git")
        .arg("-C")
        .arg(&fixture.main)
        .args([
            "worktree",
            "add",
            "-q",
            "-b",
            "topology-linked",
            linked.to_str().unwrap(),
        ])
        .output()
        .expect("spawn git worktree add");
    assert!(
        worktree.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&worktree.stderr)
    );

    let unattached = fixture.run(&linked, &["task", "show", "t-topology-main", "--json"]);
    assert!(
        !unattached.status.success(),
        "an unattached linked worktree inherited the main worktree's board"
    );
    let unattached_stderr = String::from_utf8_lossy(&unattached.stderr);
    assert!(
        unattached_stderr.contains("no Kanban project contains"),
        "{unattached_stderr}"
    );
    assert!(!String::from_utf8_lossy(&unattached.stdout).contains("topology sentinel"));

    fixture.ok_json(
        &linked,
        &["workspace", "attach", "--to", "GIT-MAIN", "--json"],
    );
    for root in [&fixture.main, &linked] {
        let task = fixture.ok_json(root, &["task", "show", "t-topology-main", "--json"]);
        assert_eq!(task["title"], "main topology sentinel");
        assert_eq!(task["id"], "t-topology-main");
    }

    let local_submodule = fixture.main.join("modules/local");
    let from_submodule = fixture.ok_json(&local_submodule, &["task", "list", "--json"]);
    assert_eq!(from_submodule.as_array().unwrap().len(), 1);
    assert_eq!(from_submodule[0]["id"], "t-topology-main");
    assert!(
        from_submodule
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["id"] != "t-topology-neighbor"),
        "an unregistered submodule escaped to the neighbor board"
    );
    let neighbor_tasks = fixture.ok_json(&neighbor, &["task", "list", "--json"]);
    assert_eq!(neighbor_tasks.as_array().unwrap().len(), 1);
    assert_eq!(neighbor_tasks[0]["id"], "t-topology-neighbor");
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
        &["t", "mv", "t-1", "review", "--as", "geoyws", "--json"],
    );
    assert_eq!(
        kb_json(&fixture.main, &["t", "cat", "t-1", "--json"])["status"],
        "review"
    );
    kb_json(
        &fixture.main,
        &[
            "t",
            "up",
            "t-1",
            "--as",
            "geoyws",
            "--priority",
            "1",
            "--json",
        ],
    );
    kb_json(
        &fixture.main,
        &["n", "t-1", "a note", "--as", "geoyws", "--json"],
    );
    assert!(kb(&fixture.main, &["ctx", "t-1"]).status.success());
    assert!(kb(&fixture.main, &["dash"]).status.success());
    kb_json(&fixture.main, &["w", "ls", "--json"]);

    // Both binaries are one program over one board.
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["priority"],
        1
    );
    kb_json(
        &fixture.main,
        &["t", "rm", "t-1", "--as", "geoyws", "--json"],
    );
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
        &["n", "rm", "note on task rm", "--as", "geoyws", "--json"],
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
        &["rule", "add", "Keep evidence.", "--as", "geoyws", "--json"],
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
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
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
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
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
            "--to",
            "driver-2",
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
                "task", "update", "t-3", "--as", "geoyws", flag, value, "--json",
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
            "geoyws",
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
            "task", "update", "t-3", "--as", "geoyws", "--title", "renamed", "--json",
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
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
    let fixture = Fixture::new("busy");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Busy", "--json"]);
    let board = project["boardPath"].as_str().unwrap().to_owned();

    // Hold the write lock past the ceiling the binary used to give up at. A
    // swarm write that loses the race has to queue, not fail: an agent reads
    // an exit status and moves on, so a dropped write is lost work that
    // nothing downstream will notice is missing.
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let connection = Connection::open(&board).unwrap();
        connection
            .busy_handler(Some(|_| {
                std::thread::sleep(Duration::from_millis(50));
                true
            }))
            .unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        started_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(7_500));
        connection.execute_batch("COMMIT").unwrap();
    });
    started_rx.recv().unwrap();

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

/// Take a board's permissions away, and report whether that actually denied
/// this process.
///
/// `chmod 000` only stops a process the mode bits apply to. Root bypasses them
/// entirely, so under a privileged runner the unreadable case cannot be staged
/// at all. Skipping there would report green while measuring nothing, so the
/// callers branch instead and assert the true statement for the situation they
/// are really in: if this harness can still read the file, so can the binary,
/// and the board must come back readable.
fn deny_board_reads(board: &Path) -> bool {
    fs::set_permissions(board, fs::Permissions::from_mode(0o000)).unwrap();
    fs::read(board).is_err()
}

fn count_pre_restore_snapshots(data: &Path) -> usize {
    let backups = data.join("backups");
    if !backups.is_dir() {
        return 0;
    }
    fs::read_dir(backups)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("pre-restore-")
        })
        .count()
}

/// Every survey tells a board it could not open from one that is gone.
///
/// Before this, both answered `present: false` and nothing else — byte for
/// byte the same receipt for intact data behind a permission bit and for data
/// that had been deleted. The move an operator makes on `missing` is to restore
/// a snapshot over the path, so the receipt was the instruction to destroy the
/// board it was describing. Boards are created `0600`, which is all it takes:
/// one written by root, or living on a shared path, reads as gone.
#[test]
fn compiled_binary_tells_an_unreadable_board_from_a_missing_one() {
    let fixture = Fixture::new("unreadable-board");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Locked", "--json"]);
    let board = PathBuf::from(project["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "real work", "--id", "t-1", "--json"],
    );

    // A healthy board first, so the new field is measured against all three
    // answers and not just the one this is about.
    let healthy = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(healthy["healthy"], true);
    assert_eq!(healthy["projects"][0]["present"], true);
    assert_eq!(healthy["projects"][0]["boardState"], "readable");

    if !deny_board_reads(&board) {
        // Privileged runner: the file stayed readable, so the binary reads it
        // too and the only honest assertion is that nothing changed.
        let still_fine = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
        assert_eq!(still_fine["projects"][0]["boardState"], "readable");
        fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }

    // doctor: unhealthy, because nothing about this board was checked at all —
    // and unreadable rather than absent, with the reason that stopped it.
    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !checked.status.success(),
        "doctor certified a board it never opened"
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["projects"][0]["boardState"], "unreadable");
    assert_eq!(
        report["projects"][0]["unreadableReason"],
        "Permission denied (os error 13)"
    );
    assert!(board.is_file(), "doctor removed the board it inspected");

    // dashboard: never `boardMissing`, which is the flag a reader acts on.
    let dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert_eq!(dashboard[0]["boardState"], "unreadable");
    assert_eq!(
        dashboard[0]["boardUnreadableReason"],
        "Permission denied (os error 13)"
    );
    assert!(
        dashboard[0].get("boardMissing").is_none(),
        "dashboard called a board that is right there missing: {}",
        dashboard[0]
    );

    // backup: still snapshots what it can, and names what it left out as
    // unreadable rather than filing it under boards that no longer exist.
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    assert_eq!(snapshot["boards"].as_array().unwrap().len(), 0);
    assert_eq!(snapshot["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(snapshot["unreadableBoards"][0]["name"], "Locked");
    assert_eq!(
        snapshot["unreadableBoards"][0]["boardPath"],
        board.to_string_lossy().as_ref()
    );
    assert_eq!(
        snapshot["unreadableBoards"][0]["reason"],
        "Permission denied (os error 13)"
    );
    // The manifest carries it too, so a snapshot's own record says it is
    // incomplete rather than looking whole.
    let manifest: Value =
        serde_json::from_slice(&fs::read(snapshot["manifest"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(manifest["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(manifest["unreadableBoards"][0]["name"], "Locked");

    // audit verify: not healthy, because an unopened ledger has had nothing
    // verified about it and the whole receipt is a claim about ledgers checked.
    let audited = fixture.run(&fixture.main, &["audit", "verify", "--json"]);
    assert!(!audited.status.success());
    let audit: Value = serde_json::from_slice(&audited.stdout).unwrap();
    assert_eq!(audit["healthy"], false);
    assert_eq!(audit["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(audit["unreadableBoards"][0]["name"], "Locked");

    // search --all-boards
    let searched = fixture.ok_json(&fixture.main, &["search", "real", "--all-boards", "--json"]);
    assert_eq!(searched["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(searched["unreadableBoards"][0]["name"], "Locked");

    // search-rebuild --all-boards
    let rebuilt = fixture.ok_json(
        &fixture.main,
        &[
            "search-rebuild",
            "--all-boards",
            "--as",
            "codex@cli",
            "--json",
        ],
    );
    assert_eq!(rebuilt["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(rebuilt["unreadableBoards"][0]["name"], "Locked");

    // The same board deleted still reports missing, and reports nothing under
    // unreadable — the two answers stay apart in both directions.
    fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", board.display()));
    }

    let gone = fixture.run(&fixture.main, &["doctor", "--json"]);
    let gone_report: Value = serde_json::from_slice(&gone.stdout).unwrap();
    assert_eq!(gone_report["projects"][0]["boardState"], "missing");
    assert_eq!(gone_report["projects"][0]["present"], false);
    assert!(
        gone_report["projects"][0].get("unreadableReason").is_none(),
        "a deleted board carried a read failure"
    );

    let gone_dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert_eq!(gone_dashboard[0]["boardState"], "missing");
    assert_eq!(gone_dashboard[0]["boardMissing"], true);

    let gone_snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    assert_eq!(
        gone_snapshot["missingBoards"][0],
        board.to_string_lossy().as_ref()
    );
    assert_eq!(
        gone_snapshot["unreadableBoards"].as_array().unwrap().len(),
        0
    );

    let gone_audit = fixture.run(&fixture.main, &["audit", "verify", "--json"]);
    let gone_audit_report: Value = serde_json::from_slice(&gone_audit.stdout).unwrap();
    assert_eq!(gone_audit_report["missingBoards"][0], "Locked");
    assert_eq!(
        gone_audit_report["unreadableBoards"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let gone_search = fixture.ok_json(&fixture.main, &["search", "real", "--all-boards", "--json"]);
    assert_eq!(gone_search["missingBoards"][0], "Locked");
    assert_eq!(gone_search["unreadableBoards"].as_array().unwrap().len(), 0);

    let gone_rebuild = fixture.ok_json(
        &fixture.main,
        &[
            "search-rebuild",
            "--all-boards",
            "--as",
            "codex@cli",
            "--json",
        ],
    );
    assert_eq!(gone_rebuild["missingBoards"][0], "Locked");
    assert_eq!(
        gone_rebuild["unreadableBoards"].as_array().unwrap().len(),
        0
    );
}

/// `restore` stops rather than replacing a board it could not copy first.
///
/// Measured before the refusal existed: a live board at mode 000, holding a
/// task added after the snapshot was taken, was skipped by the pre-restore
/// rescue copy as "missing" and then replaced anyway — `replace_database`
/// renames over the path, which needs the directory's permissions and not the
/// file's. The command exited 0, the rescue snapshot had no `boards` directory
/// in it at all, and the post-snapshot task was gone with nothing to recover it
/// from. The rescue copy is the only thing that makes `--force` reversible.
#[test]
fn compiled_binary_refuses_to_restore_over_a_board_it_cannot_rescue() {
    let fixture = Fixture::new("unreadable-restore");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Rescue", "--json"]);
    let board = PathBuf::from(project["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "in the snapshot", "--id", "t-1", "--json"],
    );
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // Work committed after the snapshot: this is what the rescue copy exists to
    // preserve, and what the measured bug destroyed.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "added after the backup",
            "--id",
            "t-2",
            "--json",
        ],
    );

    if !deny_board_reads(&board) {
        // Privileged runner: the board is readable, so it is rescued the
        // ordinary way and the restore goes through.
        let done = fixture.ok_json(
            &fixture.main,
            &["restore", "--from", &snapshot, "--force", "--json"],
        );
        let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
        assert!(
            rescue.join("boards").is_dir(),
            "the rescue snapshot kept no copy of the live board"
        );
        fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }

    let refused = fixture.run(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    assert!(
        !refused.status.success(),
        "restore replaced a board it could not copy first"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("cannot copy into the rescue snapshot first"),
        "stderr: {message}"
    );
    assert!(
        message.contains("Permission denied (os error 13)"),
        "the refusal did not say what stopped the read: {message}"
    );
    assert!(
        message.contains("very likely intact"),
        "the refusal read as data loss rather than a permission problem: {message}"
    );
    assert_eq!(
        count_pre_restore_snapshots(&fixture.data),
        0,
        "a refused restore left a half-built rescue snapshot behind"
    );

    // The board was not touched, and the work added after the snapshot is
    // still there once the permission bit is back.
    fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-2", "--json"])["title"],
        "added after the backup"
    );

    // And with the board readable the restore runs, rescuing it on the way
    // through — so the refusal gated on the rescue copy, nothing else.
    let done = fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
    assert!(
        rescue.join("boards").is_dir(),
        "the rescue snapshot kept no copy of the live board"
    );
    let rescue_manifest: Value =
        serde_json::from_slice(&fs::read(rescue.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        rescue_manifest["unreadableBoards"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        rescue_manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|file| file["kind"] == "board")
            .count(),
        1,
        "the rescue manifest recorded no board copy"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"],
        "in the snapshot"
    );
}

/// `restore` rescues what it will overwrite, not what the registry happens to
/// list.
///
/// The destruction is keyed to the filesystem: the replacement loop renames a
/// snapshot file over `<root>/boards/<file name>` for every board in the
/// snapshot, registered or not. Restoring an older snapshot drops a project
/// from the registry while leaving its file on disk, so restoring a newer one
/// then renames over a file that nothing classifies. Measured before this was
/// keyed correctly: the work committed after that snapshot was destroyed with
/// no rescue copy, and the unreadable-board refusal could not fire either,
/// because it was keyed to the registry too.
#[test]
fn compiled_binary_rescues_a_board_file_the_registry_no_longer_lists() {
    let fixture = Fixture::new("unregistered-overwrite");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let first = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    let bee = fixture.ok_json(&fixture.worktree, &["init", "--name", "Bee", "--json"]);
    let bee_board = PathBuf::from(bee["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.worktree,
        &[
            "task",
            "add",
            "in the second snapshot",
            "--id",
            "b-1",
            "--json",
        ],
    );
    let second = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // Committed after the second snapshot: exactly what a rescue copy is for.
    fixture.ok_json(
        &fixture.worktree,
        &[
            "task",
            "add",
            "after the second snapshot",
            "--id",
            "b-2",
            "--json",
        ],
    );

    // Restoring the older snapshot drops Bee from the registry and leaves its
    // file where it is — the state that hides the next overwrite from anything
    // classifying by registry membership.
    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &first, "--force", "--json"],
    );
    let listed = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        !listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "Bee"),
        "the older snapshot did not drop Bee, so this never reaches the case: {listed}"
    );
    assert!(
        bee_board.is_file(),
        "restoring an older snapshot deleted a board it never mentioned"
    );

    // Restoring the newer snapshot renames over that unregistered file, so it
    // has to reach the rescue snapshot first.
    let done = fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &second, "--force", "--json"],
    );
    let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
    let manifest: Value =
        serde_json::from_slice(&fs::read(rescue.join("manifest.json")).unwrap()).unwrap();
    let rescued = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["kind"] == "board")
        .collect::<Vec<_>>();
    assert_eq!(
        rescued.len(),
        2,
        "a board about to be overwritten was left out of the rescue snapshot: {manifest}"
    );
    let unregistered = rescued
        .iter()
        .find(|file| file["project"] == Value::Null)
        .unwrap_or_else(|| {
            panic!("the file the registry no longer lists was not rescued: {manifest}")
        });
    assert_eq!(
        unregistered["path"],
        format!(
            "boards/{}",
            bee_board.file_name().unwrap().to_string_lossy()
        ),
        "the unnamed rescue copy is not the file that was overwritten"
    );

    // The rescue copy holds the work the overwrite destroyed. This is the whole
    // guarantee: the live file is gone, and it is recoverable.
    let copy = rescue.join(unregistered["path"].as_str().unwrap());
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "show",
                "b-2",
                "--db",
                copy.to_str().unwrap(),
                "--json"
            ],
        )["title"],
        "after the second snapshot"
    );
    // And the live file really was replaced, as restore promises.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "show",
                "b-1",
                "--db",
                bee_board.to_str().unwrap(),
                "--json",
            ],
        )["title"],
        "in the second snapshot"
    );
}

/// A file that is not a board is copied out of the way, not silently replaced.
///
/// `BoardFile::Foreign`'s contract is "Never opened, never migrated, never
/// overwritten", and it exists because `task list --db notes.txt` once left
/// 372736 bytes of SQLite where an operator's file had been. `restore` renames
/// over `<root>/boards/<file name>` for every board in the snapshot, so it can
/// destroy such a file just as completely.
///
/// Refusing was the wrong way to keep that promise: a board whose header is
/// damaged classifies as foreign too, so refusing would block recovery of
/// exactly the disaster `restore` exists for. Copying keeps the file
/// recoverable, which is what the promise protects, and the receipt names it so
/// the replacement is never silent.
#[test]
fn compiled_binary_copies_a_foreign_file_out_of_the_way_before_replacing_it() {
    let fixture = Fixture::new("foreign-overwrite");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let bee = fixture.ok_json(&fixture.worktree, &["init", "--name", "Bee", "--json"]);
    let bee_board = PathBuf::from(bee["boardPath"].as_str().unwrap());
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // Something that is not a board, sitting exactly where the restore writes.
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", bee_board.display()));
    }
    fs::write(&bee_board, "operator notes, not a database\n").unwrap();

    let done = fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    let unparsed = done["rescuedUnparsed"].as_array().unwrap();
    assert_eq!(
        unparsed.len(),
        1,
        "a file that was never a board was replaced with no copy: {done}"
    );
    assert_eq!(
        unparsed[0]["originalPath"],
        bee_board.to_string_lossy().as_ref()
    );
    assert_eq!(unparsed[0]["reason"], "not a Kanban board");

    // The bytes survive verbatim in the rescue snapshot.
    let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(rescue.join(unparsed[0]["path"].as_str().unwrap())).unwrap(),
        "operator notes, not a database\n",
        "the operator's file was destroyed rather than copied"
    );

    // And the restore did its job: the path is a board again.
    assert_eq!(
        fixture.ok_json(&fixture.worktree, &["doctor", "--json"])["healthy"],
        true
    );
}

/// Corrupt the pages of a board while leaving the file readable.
///
/// `offset` selects how deep the damage goes, which decides which layer
/// notices: the 16-byte header is checked by the classifier, the schema lives
/// in the first page, and anything past that is invisible until every page is
/// read.
fn corrupt_board_bytes(board: &Path, offset: usize, fill: u8) {
    let mut bytes = fs::read(board).unwrap();
    let end = bytes.len().min(65536);
    assert!(
        end > offset,
        "board too small to corrupt at {offset}: {} bytes",
        bytes.len()
    );
    for byte in &mut bytes[offset..end] {
        *byte = fill;
    }
    fs::write(board, &bytes).unwrap();
}

/// A corrupt board is what `restore` is for, so it must not block on one.
///
/// Measured before this predicate was corrected: `restore` refused with
/// `database disk image is malformed` and told the operator to check
/// permissions that were fine — the one command that recovers from disk
/// corruption, refusing because of disk corruption. The rescue copy does not
/// need SQLite to parse a file, only to read it, so the question is whether the
/// bytes can be read, and every readable file is copied out of the way.
///
/// Three depths, because three different layers notice: a damaged header stops
/// the classifier, a damaged first page stops the online backup, and damage
/// past the first page is invisible until the copy's audit chain is read back.
#[test]
fn compiled_binary_restores_over_a_corrupt_board_and_keeps_a_copy_of_it() {
    for (label, offset, fill) in [
        ("damaged-header", 0usize, b'!'),
        ("schema-page", 100, 0x5A),
        ("past-the-first-page", 4096, 0xAA),
    ] {
        let fixture = Fixture::new(&format!("corrupt-restore-{label}"));
        let project = fixture.ok_json(&fixture.main, &["init", "--name", "Ord", "--json"]);
        let board = PathBuf::from(project["boardPath"].as_str().unwrap());
        fixture.ok_json(
            &fixture.main,
            &["task", "add", "good work", "--id", "t-1", "--json"],
        );
        let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
            .as_str()
            .unwrap()
            .to_owned();

        corrupt_board_bytes(&board, offset, fill);
        let damaged = fs::read(&board).unwrap();
        assert!(
            fs::read(&board).is_ok(),
            "{label}: the harness cannot read the file, so this measures the wrong thing"
        );

        // The recovery must run, not refuse.
        let done = fixture.ok_json(
            &fixture.main,
            &["restore", "--from", &snapshot, "--force", "--json"],
        );

        // The corrupt file was copied out of the way before being replaced,
        // byte for byte, and the receipt says so rather than staying silent.
        let unparsed = done["rescuedUnparsed"].as_array().unwrap();
        assert_eq!(
            unparsed.len(),
            1,
            "{label}: the corrupt board was replaced without a copy: {done}"
        );
        assert_eq!(
            unparsed[0]["originalPath"],
            board.to_string_lossy().as_ref(),
            "{label}"
        );
        assert!(
            !unparsed[0]["reason"].as_str().unwrap().is_empty(),
            "{label}: no reason recorded"
        );

        let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
        let copy = rescue.join(unparsed[0]["path"].as_str().unwrap());
        assert_eq!(
            fs::read(&copy).unwrap(),
            damaged,
            "{label}: the rescue copy is not the file that was replaced"
        );

        // The rescue manifest records it, and the rescue snapshot as a whole
        // still verifies — an unparsed copy must not make it unrestorable.
        let manifest: Value =
            serde_json::from_slice(&fs::read(rescue.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest["unparsedFiles"].as_array().unwrap().len(),
            1,
            "{label}"
        );
        fixture.ok_json(
            &fixture.main,
            &[
                "audit",
                "verify",
                "--against",
                rescue.join("manifest.json").to_str().unwrap(),
                "--json",
            ],
        );

        // And the restore actually recovered the board.
        assert_eq!(
            fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"],
            "good work",
            "{label}"
        );
        assert_eq!(
            fixture.ok_json(&fixture.main, &["doctor", "--json"])["healthy"],
            true,
            "{label}"
        );
    }
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
        &["task", "move", "t-1", "todo", "--as", "geoyws", "--json"],
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
        &["task", "move", "s-1", "done", "--as", "geoyws", "--json"],
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
        &["story", "advance", "s-1", "--as", "geoyws", "--json"],
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
        &["task", "move", "s-1", "blocked", "--as", "geoyws", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "s-1", "--json"])["status"],
        "blocked"
    );

    // --force overwrites the projection and says so in the ledger.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "s-1", "done", "--as", "geoyws", "--force", "--json",
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
        &["task", "move", "t-1", "done", "--as", "geoyws", "--json"],
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
            "task", "update", "s-ok", "--as", "geoyws", "--parent", "t-1", "--json",
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
            "geoyws",
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
    fixture.ok_json(
        &fixture.main,
        &[
            "subscription",
            "add",
            "--id",
            "sub-schema-readonly",
            "--consumer",
            "schema.probe",
            "--action",
            "observe",
            "--timeout-ms",
            "1000",
            "--max-retries",
            "0",
            "--rate-per-minute",
            "1",
            "--max-concurrency",
            "1",
            "--as",
            "schema@e2e",
            "--json",
        ],
    );
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
            "subscription list" => vec!["subscription", "list"],
            "subscription show" => {
                vec!["subscription", "show", "sub-schema-readonly"]
            }
            "deploy show" => vec!["deploy", "show", &deployment_id],
            "deploy list" => vec!["deploy", "list"],
            "deploy current" => vec!["deploy", "current"],
            "schema" => vec!["schema"],
            "events" => vec!["events"],
            "stale" => vec!["stale"],
            "context" => vec!["context", "t-1"],
            "access principal show" => {
                vec!["access", "principal", "show", "--principal", "p-00000000"]
            }
            "access principal list" => vec!["access", "principal", "list"],
            "access explain" => vec![
                "access",
                "explain",
                "--principal",
                "p-00000000",
                "--capability",
                "read",
                "--scope",
                "registry",
            ],
            "access audit" => vec!["access", "audit"],
            "access enforcement show" => vec!["access", "enforcement", "show"],
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
    assert_eq!(flag_kind("limit"), "value");
    assert_eq!(flag_kind("follow"), "boolean");
    assert_eq!(flag_kind("all"), "boolean");
    assert_eq!(flag_kind("task"), "value");
    assert_eq!(flag_kind("rule"), "value");
    assert_eq!(flag_kind("registry"), "boolean");
    for name in ["kind", "relation", "prior-status", "current-status", "tag"] {
        assert_eq!(flag_kind(name), "list", "{name} is not repeatable");
    }

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
fn compiled_binary_events_after_before_archive_and_schema_match() {
    let fixture = Fixture::new("events-after-before");
    fixture.ok_json(&fixture.main, &["init", "--name", "EVENTS", "--json"]);

    for (id, title) in [
        ("e-1", "first event"),
        ("e-2", "second event"),
        ("e-3", "third event"),
        ("e-4", "fourth event"),
    ] {
        fixture.ok_json(&fixture.main, &["task", "add", title, "--id", id, "--json"]);
    }

    let board_path = board_path_for_project(&fixture, &fixture.main, "EVENTS");
    let seqs = {
        let connection = Connection::open(&board_path).unwrap();
        let seqs = connection
            .prepare("SELECT seq FROM events ORDER BY seq")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            seqs.len() >= 4,
            "expected at least four ledger events, found {}",
            seqs.len()
        );
        for (index, seq) in seqs.iter().enumerate() {
            connection
                .execute(
                    "UPDATE events SET created_at=?, archived=? WHERE seq=?",
                    params![
                        1000_i64 * (index as i64 + 1),
                        if index == 0 { 1_i64 } else { 0_i64 },
                        seq,
                    ],
                )
                .unwrap();
        }
        seqs
    };
    let board_db = board_path.to_string_lossy().into_owned();

    let help = fixture.run(&fixture.main, &["events", "--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    for flag in [
        "--task",
        "--rule",
        "--registry",
        "--kind",
        "--after",
        "--before",
        "--limit",
        "--all",
    ] {
        assert!(
            help_text.contains(flag),
            "help text is missing {flag}: {help_text}"
        );
    }

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let events = schema["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|operation| operation["name"] == "events")
        .expect("events is missing from the manifest");
    let flags = events["flags"].as_array().unwrap();
    let flag_kind = |name: &str| -> &str {
        flags.iter().find(|flag| flag["name"] == name).unwrap()["kind"]
            .as_str()
            .unwrap()
    };
    for name in ["task", "rule", "kind", "after", "before", "limit"] {
        assert_eq!(flag_kind(name), "value", "--{name} should remain scalar");
    }
    assert_eq!(flag_kind("registry"), "boolean");
    assert_eq!(flag_kind("all"), "boolean");

    let half_open = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "2000", "--before", "3000", "--limit", "10",
            "--json",
        ],
    );
    assert!(half_open.status.success());
    let half_open_json: Value = serde_json::from_slice(&half_open.stdout).unwrap();
    let half_open_rows = half_open_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        half_open_rows,
        vec![seqs[1]],
        "half-open bounds should include the start and exclude the end: {}",
        String::from_utf8_lossy(&half_open.stdout)
    );

    let bounded = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "2000", "--before", "4000", "--limit", "1",
            "--json",
        ],
    );
    assert!(bounded.status.success());
    let bounded_json: Value = serde_json::from_slice(&bounded.stdout).unwrap();
    let bounded_rows = bounded_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        bounded_rows,
        vec![seqs[2]],
        "SQL filtering should happen before limit: {}",
        String::from_utf8_lossy(&bounded.stdout)
    );

    let equal_bounds = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "3000", "--before", "3000", "--limit", "10",
            "--json",
        ],
    );
    assert!(equal_bounds.status.success());
    let equal_bounds_json: Value = serde_json::from_slice(&equal_bounds.stdout).unwrap();
    assert!(
        equal_bounds_json.as_array().unwrap().is_empty(),
        "equal bounds should be empty: {}",
        String::from_utf8_lossy(&equal_bounds.stdout)
    );

    let after_only = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "3000", "--limit", "10", "--json",
        ],
    );
    assert!(after_only.status.success());
    let after_only_json: Value = serde_json::from_slice(&after_only.stdout).unwrap();
    let after_only_rows = after_only_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        after_only_rows,
        seqs[2..].iter().rev().copied().collect::<Vec<_>>(),
        "after-only bounds should include the lower edge and all later rows: {}",
        String::from_utf8_lossy(&after_only.stdout)
    );

    let before_only = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--before", "4000", "--limit", "10", "--json",
        ],
    );
    assert!(before_only.status.success());
    let before_only_json: Value = serde_json::from_slice(&before_only.stdout).unwrap();
    let before_only_rows = before_only_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        before_only_rows,
        vec![seqs[2], seqs[1]],
        "before-only bounds should exclude rows at or after the upper edge: {}",
        String::from_utf8_lossy(&before_only.stdout)
    );

    let default_rows = fixture.ok_json(
        &fixture.main,
        &["events", "--db", &board_db, "--limit", "10", "--json"],
    );
    let default_seqs = default_rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        default_seqs,
        seqs[1..].iter().rev().copied().collect::<Vec<_>>(),
        "default output lost seq-desc ordering"
    );
    assert!(
        default_rows
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !row["archived"].as_bool().unwrap()),
        "default events output leaked archived history: {default_rows}"
    );

    let all_rows = fixture.ok_json(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--all", "--limit", "10", "--json",
        ],
    );
    let all_seqs = all_rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        all_seqs,
        seqs.iter().rev().copied().collect::<Vec<_>>(),
        "--all did not preserve seq-desc ordering"
    );
    assert!(
        all_rows
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["archived"] == json!(true)),
        "--all hid the archived event: {all_rows}"
    );

    for args in [
        ["events", "--db", &board_db, "--after", "-1", "--json"].as_slice(),
        ["events", "--db", &board_db, "--before", "-1", "--json"].as_slice(),
    ] {
        let rejected = fixture.run(&fixture.main, args);
        assert!(!rejected.status.success());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            stderr.contains("must be non-negative"),
            "negative bound was not rejected correctly: {stderr}"
        );
    }

    let reversed = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "3000", "--before", "2000", "--json",
        ],
    );
    assert!(!reversed.status.success());
    assert!(
        String::from_utf8_lossy(&reversed.stderr)
            .contains("--after must not be later than --before")
    );

    let registry_rejected = fixture.run(
        &fixture.main,
        &["events", "--registry", "--after", "2000", "--json"],
    );
    assert!(!registry_rejected.status.success());
    assert!(
        String::from_utf8_lossy(&registry_rejected.stderr)
            .contains("--after and --before only apply to board events")
    );

    let rule_rejected = fixture.run(
        &fixture.main,
        &["events", "--rule", "r-missing", "--after", "2000", "--json"],
    );
    assert!(!rule_rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rule_rejected.stderr)
            .contains("--after and --before only apply to board events")
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
            "geoyws",
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
            "geoyws",
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
            "geoyws",
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
        // Only the refusal reaches stdout: no events from the other stream.
        assert!(
            refusal_object(&mismatch).contains("different watch stream"),
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
        "geoyws",
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
    let next_event = watch.next_stdout_event_json(Duration::from_secs(10));
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
fn watch_emits_truthful_bounded_semantic_envelopes() {
    let fixture = Fixture::new("watch-semantic-envelope");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-SEMANTIC-ENVELOPE", "--json"],
    );
    for tag in ["alpha", "zeta"] {
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Root epic",
            "--id",
            "e-root",
            "--type",
            "epic",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Parent story",
            "--id",
            "s-parent",
            "--type",
            "story",
            "--parent",
            "e-root",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Completed dependency",
            "--id",
            "t-base",
            "--status",
            "done",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Child task",
            "--id",
            "t-child",
            "--parent",
            "s-parent",
            "--depends-on",
            "t-base",
            "--tag",
            "zeta",
            "--tag",
            "alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "done", "--as", "geoyws", "--json",
        ],
    );

    let audit = fixture.ok_json(&fixture.main, &["audit", "verify", "--json"]);
    assert_eq!(audit["healthy"], true, "{audit}");
    assert_eq!(audit["boards"][0]["audit"]["healthy"], true, "{audit}");

    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-SEMANTIC-ENVELOPE");
    let board_id = fs::canonicalize(&board_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let legacy_seq = insert_raw_board_event(
        &board_path,
        Some("t-child"),
        "legacy_semantic_probe",
        "geoyws",
        json!({
            "token": "outer-secret",
            "tokenized": "visible",
            "nested": {
                "secretValue": "inner-secret",
                "keep": "visible",
                "items": [{"materialValue": "deep-secret", "keep": "still-visible"}]
            }
        }),
    );
    let oversized_seq = insert_raw_board_event(
        &board_path,
        Some("t-child"),
        "legacy_oversized_probe",
        "geoyws",
        json!({"blob": "x".repeat(20_000)}),
    );

    let output = fixture.run(
        &fixture.main,
        &[
            "watch", "--task", "t-child", "--cursor", "0", "--limit", "16", "--json",
        ],
    );
    assert!(
        output.status.success(),
        "watch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = ndjson_values(&output);
    let find = |kind: &str| {
        rows.iter()
            .find(|row| row["payload"]["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {rows:#?}"))
    };
    let added = find("task_added");
    let moved = find("task_moved");

    for envelope in [added, moved] {
        let event = &envelope["payload"];
        assert_eq!(envelope["version"], 1, "{envelope}");
        assert_eq!(envelope["type"], "event", "{envelope}");
        assert_eq!(event["schemaVersion"], 1, "{event}");
        assert_eq!(event["board"]["id"], board_id, "{event}");
        assert_eq!(event["board"]["name"], "WATCH-SEMANTIC-ENVELOPE", "{event}");
        assert_eq!(event["eventID"], event["eventHash"], "{event}");
        assert!(
            event["eventID"].as_str().is_some_and(|id| !id.is_empty()),
            "{event}"
        );
        assert!(event["seq"].as_i64().is_some_and(|seq| seq > 0), "{event}");
        assert_eq!(event["timestamp"], event["createdAt"], "{event}");
        assert!(
            event["timestamp"].as_i64().is_some_and(|at| at > 0),
            "{event}"
        );
        assert_eq!(event["actor"], "geoyws", "{event}");
        assert_eq!(event["subject"], json!({"type":"task","id":"t-child"}));
        assert_eq!(event["tags"], json!(["alpha", "zeta"]));
        assert!(event["payload"].get("_semanticV1").is_none(), "{event}");
        assert!(serde_json::to_vec(&event["metadata"]).unwrap().len() <= 16_384);
        let relations = event["relations"].as_array().unwrap();
        for relation in [
            json!({"kind":"ancestor","type":"epic","id":"e-root"}),
            json!({"kind":"depends-on","type":"task","id":"t-base"}),
            json!({"kind":"parent","type":"story","id":"s-parent"}),
        ] {
            assert!(
                relations.contains(&relation),
                "missing {relation} in {event}"
            );
        }
    }
    assert_eq!(added["payload"]["priorStatus"], Value::Null);
    assert_eq!(added["payload"]["currentStatus"], "todo");
    assert_eq!(moved["payload"]["priorStatus"], "todo");
    assert_eq!(moved["payload"]["currentStatus"], "done");

    let legacy = &find("legacy_semantic_probe")["payload"];
    assert_eq!(legacy["seq"], legacy_seq);
    for field in [
        "subject",
        "relations",
        "priorStatus",
        "currentStatus",
        "tags",
    ] {
        assert!(
            legacy[field].is_null(),
            "{field} was reconstructed: {legacy}"
        );
    }
    assert!(legacy["payload"].get("token").is_none(), "{legacy}");
    assert_eq!(legacy["payload"]["tokenized"], "visible");
    assert!(
        legacy["payload"]["nested"].get("secretValue").is_none(),
        "{legacy}"
    );
    assert!(
        legacy["payload"]["nested"]["items"][0]
            .get("materialValue")
            .is_none(),
        "{legacy}"
    );
    assert_eq!(
        legacy["payload"]["nested"]["items"][0]["keep"],
        "still-visible"
    );
    assert_eq!(legacy["metadata"]["value"], legacy["payload"]);
    assert_eq!(legacy["metadata"]["truncated"], false);
    assert!(serde_json::to_vec(&legacy["metadata"]).unwrap().len() <= 16_384);

    let oversized = &find("legacy_oversized_probe")["payload"];
    assert_eq!(oversized["seq"], oversized_seq);
    assert_eq!(oversized["metadata"]["value"], Value::Null);
    assert_eq!(oversized["metadata"]["truncated"], true);
    assert!(oversized["metadata"]["bytes"].as_u64().unwrap() > 16_384);
    assert!(serde_json::to_vec(&oversized["metadata"]).unwrap().len() <= 16_384);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("_semanticV1"));
}

#[test]
fn watch_filters_sparse_history_and_binds_normalized_predicates_to_cursors() {
    let fixture = Fixture::new("watch-semantic-filters");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-SEMANTIC-FILTERS", "--json"],
    );
    for tag in ["alpha", "zeta"] {
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Root epic",
            "--id",
            "e-root",
            "--type",
            "epic",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Parent story",
            "--id",
            "s-parent",
            "--type",
            "story",
            "--parent",
            "e-root",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Other relation target",
            "--id",
            "t-other",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Dependency",
            "--id",
            "t-base",
            "--status",
            "done",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Filtered child",
            "--id",
            "t-child",
            "--parent",
            "s-parent",
            "--depends-on",
            "t-base",
            "--tag",
            "zeta",
            "--tag",
            "alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "done", "--as", "geoyws", "--json",
        ],
    );

    let kinds = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--kind",
            "task_moved",
            "--kind",
            "task_added",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(
        kinds.status.success(),
        "{}",
        String::from_utf8_lossy(&kinds.stderr)
    );
    let kind_rows = ndjson_values(&kinds);
    let kind_names = kind_rows
        .iter()
        .map(|row| row["payload"]["kind"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        kind_names,
        std::collections::BTreeSet::from(["task_added", "task_moved"])
    );

    let filtered = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--kind",
            "task_moved",
            "--kind",
            "task_added",
            "--relation",
            "parent:s-parent",
            "--relation",
            "parent:t-other",
            "--prior-status",
            "todo",
            "--current-status",
            "done",
            "--tag",
            "alpha",
            "--tag",
            "zeta",
            "--cursor",
            "0",
            "--limit",
            "1",
            "--json",
        ],
    );
    assert!(
        filtered.status.success(),
        "sparse filtered watch failed: {}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    let filtered_rows = ndjson_values(&filtered);
    assert_eq!(
        filtered_rows.len(),
        1,
        "{}",
        String::from_utf8_lossy(&filtered.stdout)
    );
    assert_eq!(filtered_rows[0]["payload"]["kind"], "task_moved");
    assert_eq!(filtered_rows[0]["payload"]["priorStatus"], "todo");
    assert_eq!(filtered_rows[0]["payload"]["currentStatus"], "done");
    let cursor = filtered_rows[0]["cursor"].as_str().unwrap().to_owned();
    let cursor_json = decode_watch_cursor(&cursor);
    assert_eq!(cursor_json["kinds"], json!(["task_added", "task_moved"]));
    assert_eq!(
        cursor_json["relations"],
        json!(["parent:s-parent", "parent:t-other"])
    );
    assert_eq!(cursor_json["priorStatuses"], json!(["todo"]));
    assert_eq!(cursor_json["currentStatuses"], json!(["done"]));
    assert_eq!(cursor_json["tags"], json!(["alpha", "zeta"]));

    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "todo", "--as", "geoyws", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "done", "--as", "geoyws", "--json",
        ],
    );
    let resumed = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--tag",
            "zeta",
            "--tag",
            "alpha",
            "--current-status",
            "done",
            "--prior-status",
            "todo",
            "--relation",
            "parent:t-other",
            "--relation",
            "parent:s-parent",
            "--kind",
            "task_added",
            "--kind",
            "task_moved",
            "--cursor",
            &cursor,
            "--limit",
            "1",
            "--json",
        ],
    );
    assert!(
        resumed.status.success(),
        "normalized cursor did not resume: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed_rows = ndjson_values(&resumed);
    assert_eq!(resumed_rows.len(), 1);
    assert_eq!(resumed_rows[0]["payload"]["kind"], "task_moved");
    assert!(
        resumed_rows[0]["payload"]["seq"].as_i64().unwrap()
            > filtered_rows[0]["payload"]["seq"].as_i64().unwrap()
    );

    let mismatch = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--kind",
            "task_added",
            "--kind",
            "task_moved",
            "--relation",
            "parent:s-parent",
            "--relation",
            "parent:t-other",
            "--prior-status",
            "todo",
            "--current-status",
            "done",
            "--tag",
            "alpha",
            "--cursor",
            &cursor,
            "--json",
        ],
    );
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("different watch stream"));

    let invalid_cases = [
        (vec!["--task", "t-never-existed"], "not present"),
        (vec!["--relation", "child:t-other"], "KIND:ID"),
        (
            vec!["--relation", "parent:t-never-existed"],
            "historical relation target",
        ),
        (
            vec!["--kind", "not_a_real_event"],
            "unknown watch event kind",
        ),
        (vec!["--prior-status", "not-a-status"], "must be one of"),
        (vec!["--current-status", "not-a-status"], "must be one of"),
        (vec!["--tag", "not-a-tag"], "master file"),
    ];
    for (flags, expected) in invalid_cases {
        let mut args = vec!["watch"];
        args.extend(flags);
        args.extend(["--cursor", "0", "--json"]);
        let rejected = fixture.run(&fixture.main, &args);
        assert!(
            !rejected.status.success(),
            "invalid watch succeeded: {args:?}"
        );
        let error = refusal_object(&rejected);
        assert!(
            error.contains(expected),
            "{args:?}: expected {expected:?} in {error}"
        );
    }
}

#[test]
fn watch_follow_delivers_an_event_queued_behind_interleaved_heartbeats() {
    let fixture = Fixture::new("watch-heartbeat-interleave");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-INTERLEAVE", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Queued behind heartbeats",
            "--id",
            "t-interleaved",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let preflight = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-interleaved",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    let start_cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let watcher = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &[
            "--task",
            "t-interleaved",
            "--cursor",
            &start_cursor,
            "--follow",
            "--limit",
            "8",
            "--json",
        ],
    );
    let idle = watcher.next_stdout_json(Duration::from_secs(5));
    assert_eq!(idle["type"], "heartbeat");
    assert_eq!(idle["payload"]["state"], "idle");

    // Hold the mutation back for several poll intervals so the follow loop
    // queues keep-alive heartbeats ahead of the event. Under real load the
    // same interleaving happens on its own but far too rarely to rely on, so
    // the delay is what makes the window deterministic. It constructs the
    // condition; the drained-count assertion below is what proves the reader
    // handled it.
    std::thread::sleep(WATCH_POLL_INTERVAL * 6);

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-interleaved",
            "in_progress",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let (moved, drained) = watcher.next_stdout_event_json_with_drain_count(Duration::from_secs(10));
    assert!(
        drained >= 1,
        "no heartbeat interleaved, so the drain never ran and this test silently \
         degraded to the pass-through case"
    );
    assert_eq!(moved["type"], "event");
    assert_eq!(moved["payload"]["kind"], "task_moved");
    assert_eq!(
        moved["payload"]["subject"],
        json!({"type":"task","id":"t-interleaved"})
    );
    assert_eq!(moved["payload"]["actor"], "geoyws");
    assert_eq!(moved["payload"]["priorStatus"], "todo");
    assert_eq!(moved["payload"]["currentStatus"], "in_progress");
    assert!(watcher.finish().is_empty());
}

#[test]
fn watch_replays_removed_subjects_and_keeps_registry_semantics_separate() {
    let fixture = Fixture::new("watch-removed-and-registry");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-REMOVED-REGISTRY", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Root epic",
            "--id",
            "e-root",
            "--type",
            "epic",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Historical parent",
            "--id",
            "s-parent",
            "--type",
            "story",
            "--parent",
            "e-root",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Removed subject",
            "--id",
            "t-removed",
            "--parent",
            "s-parent",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let preflight = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-removed",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    let start_cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let watcher = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &[
            "--task",
            "t-removed",
            "--cursor",
            &start_cursor,
            "--follow",
            "--limit",
            "8",
            "--json",
        ],
    );
    let idle = watcher.next_stdout_json(Duration::from_secs(5));
    assert_eq!(idle["type"], "heartbeat");
    assert_eq!(idle["payload"]["state"], "idle");
    assert_eq!(idle["cursor"], start_cursor);

    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "t-removed", "--as", "geoyws", "--json"],
    );
    let removed = watcher.next_stdout_event_json(Duration::from_secs(10));
    assert_eq!(removed["type"], "event");
    assert_eq!(removed["payload"]["kind"], "task_removed");
    assert_eq!(
        removed["payload"]["subject"],
        json!({"type":"task","id":"t-removed"})
    );
    assert_eq!(removed["payload"]["priorStatus"], "todo");
    assert!(removed["payload"]["currentStatus"].is_null());
    assert!(watcher.finish().is_empty());

    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "s-parent", "--as", "geoyws", "--json"],
    );
    let replay = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-removed",
            "--relation",
            "parent:s-parent",
            "--kind",
            "task_removed",
            "--cursor",
            "0",
            "--limit",
            "8",
            "--json",
        ],
    );
    assert!(
        replay.status.success(),
        "historical replay failed: {}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay_rows = ndjson_values(&replay);
    assert_eq!(replay_rows.len(), 1);
    assert_eq!(replay_rows[0]["payload"]["kind"], "task_removed");
    let replay_cursor = replay_rows[0]["cursor"].as_str().unwrap();
    let resumed = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-removed",
            "--relation",
            "parent:s-parent",
            "--kind",
            "task_removed",
            "--cursor",
            replay_cursor,
            "--limit",
            "8",
            "--json",
        ],
    );
    assert!(resumed.status.success());
    assert!(ndjson_values(&resumed).is_empty());

    let rule = fixture.ok_json(
        &fixture.main,
        &["rule", "add", "Registry event", "--as", "geoyws", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            rule["id"].as_str().unwrap(),
            "--body",
            "Updated registry event",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let registry = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--registry",
            "--kind",
            "rule_updated",
            "--kind",
            "rule_added",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(registry.status.success());
    let registry_rows = ndjson_values(&registry);
    let registry_kinds = registry_rows
        .iter()
        .map(|row| row["payload"]["kind"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        registry_kinds,
        std::collections::BTreeSet::from(["rule_added", "rule_updated"])
    );
    assert!(
        registry_rows
            .iter()
            .all(|row| row["payload"]["board"].is_null())
    );

    for (flag, value) in [
        ("--relation", "parent:s-parent"),
        ("--prior-status", "todo"),
        ("--current-status", "done"),
        ("--tag", "alpha"),
    ] {
        let rejected = fixture.run(
            &fixture.main,
            &[
                "watch",
                "--registry",
                flag,
                value,
                "--cursor",
                "0",
                "--json",
            ],
        );
        assert!(!rejected.status.success(), "registry accepted {flag}");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("apply only to board watch events")
        );
    }
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
        &["note", "t-one", "Backlog note", "--as", "geoyws", "--json"],
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
    child: Option<std::process::Child>,
    outgoing: Option<std::process::ChildStdin>,
    incoming: std::sync::mpsc::Receiver<String>,
    reader: Option<std::thread::JoinHandle<()>>,
}

enum ShutdownResult {
    Clean(std::process::ExitStatus),
    TimedOut,
}

impl Session {
    fn start(binary: &Path, cwd: &Path, data: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut backoff = Duration::from_millis(10);
        let mut child = loop {
            match Command::new(binary)
                .arg("mcp")
                .current_dir(cwd)
                .env("KANBAN_DATA_DIR", data)
                .env_remove("KANBAN_DB")
                .env_remove("KANBAN_PROJECT")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => break child,
                Err(error) if error.kind() == ErrorKind::ExecutableFileBusy => {
                    if Instant::now() >= deadline {
                        panic!(
                            "spawn kanban mcp kept failing with ETXTBSY past the 750ms deadline: {error}"
                        );
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_millis(50));
                }
                Err(error) => panic!("spawn kanban mcp: {error}"),
            }
        };
        let outgoing = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, incoming) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
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
            child: Some(child),
            outgoing: Some(outgoing),
            incoming,
            reader: Some(reader),
        }
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    fn writer(&mut self) -> &mut std::process::ChildStdin {
        self.outgoing.as_mut().unwrap()
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
        self.writer().write_all(frame.as_bytes()).unwrap();
        self.writer().flush().unwrap();
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
        writeln!(self.writer(), "{request}").unwrap();
        self.writer().flush().unwrap();
        let line = self
            .incoming
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no reply to {request}"));
        serde_json::from_str(&line).unwrap()
    }

    fn finish(mut self) {
        match self.shutdown(Duration::from_secs(5)) {
            ShutdownResult::Clean(status) => {
                assert!(
                    status.success(),
                    "session exited nonzero after a clean EOF: {status}"
                );
            }
            ShutdownResult::TimedOut => {
                panic!("session did not exit cleanly before the 5-second timeout");
            }
        }
    }

    fn shutdown(&mut self, timeout: Duration) -> ShutdownResult {
        let Some(mut child) = self.child.take() else {
            return ShutdownResult::TimedOut;
        };
        let _ = self.outgoing.take();
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(reader) = self.reader.take() {
                        let _ = reader.join();
                    }
                    return ShutdownResult::Clean(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        let _ = child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        ShutdownResult::TimedOut
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown(Duration::ZERO);
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

fn copy_executable(source: &Path, target: &Path) {
    // Stage the executable beside the target, then rename it into place so
    // initial publication has the same atomic boundary as later replacements.
    let staging = target.with_extension("staging");
    fs::copy(source, &staging).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&staging, target).unwrap();
}

fn file_sha256(path: &Path) -> String {
    let mut file = fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer).unwrap();
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    format!("{:x}", hasher.finalize())
}

fn clone_release_package(source: &Path, target: &Path, source_commit: &str) {
    fs::create_dir_all(target).unwrap();
    for name in [
        "kanban",
        "kb",
        "kanban-dispatcher",
        "kanban-codex-queue-adapter",
        "kanban-codex-app-server-adapter",
        "kanban-claude-print-adapter",
    ] {
        fs::copy(source.join(name), target.join(name)).unwrap();
    }
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(source.join("manifest.json")).unwrap()).unwrap();
    manifest["sourceCommit"] = json!(source_commit);
    fs::write(
        target.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut receipt: Value =
        serde_json::from_slice(&fs::read(source.with_extension("receipt.json")).unwrap()).unwrap();
    receipt["sourceCommit"] = json!(source_commit);
    receipt["manifestSha256"] = json!(file_sha256(&target.join("manifest.json")));
    fs::write(
        target.with_extension("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
}

struct HaxInstallContext<'a> {
    fixture: &'a Fixture,
    script: &'a Path,
    path: &'a str,
    hostname_bin: &'a Path,
    fake_repo_root: &'a Path,
    remote_root: &'a Path,
}

fn install_matching_hax_package(
    ctx: &HaxInstallContext<'_>,
    package_dir: &Path,
    commit: &str,
    label: &str,
) -> PathBuf {
    let hax_install_root = ctx.fixture.root.join(format!("{label}-install-hax"));
    let hax_bin_dir = ctx.fixture.root.join(format!("{label}-bin-hax"));
    let installed = Command::new("bash")
        .current_dir(&ctx.fixture.main)
        .env("PATH", ctx.path)
        .env("HOSTNAME_BIN", ctx.hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", ctx.fake_repo_root)
        .env("FAKE_GIT_HEAD", commit)
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", ctx.remote_root)
        .arg(ctx.script)
        .args([
            "install",
            "hax",
            "--package",
            package_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "HAX install for {label} failed: {}\nstderr: {}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(
        PathBuf::from(installed_json["installRoot"].as_str().unwrap()),
        hax_install_root
    );
    hax_install_root
}

fn release_id_from_package(package_dir: &Path) -> String {
    let receipt: Value =
        serde_json::from_slice(&fs::read(package_dir.with_extension("receipt.json")).unwrap())
            .unwrap();
    format!(
        "{}-{}",
        receipt["sourceCommit"].as_str().unwrap(),
        receipt["manifestSha256"].as_str().unwrap()
    )
}

/// Byte-level picture of a tree that never follows symlinks: a planted link is
/// recorded as its target text, so a write that went THROUGH it shows up in
/// the snapshot of the directory it pointed at, not here.
fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (&'static str, Vec<u8>)> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, (&'static str, Vec<u8>)>) {
        let relative = path.strip_prefix(root).unwrap().to_path_buf();
        let meta = fs::symlink_metadata(path).unwrap();
        if meta.file_type().is_symlink() {
            let target = fs::read_link(path).unwrap();
            out.insert(
                relative,
                ("link", target.to_string_lossy().into_owned().into_bytes()),
            );
        } else if meta.is_dir() {
            out.insert(relative, ("dir", Vec::new()));
            for entry in fs::read_dir(path).unwrap() {
                walk(root, &entry.unwrap().path(), out);
            }
        } else {
            out.insert(relative, ("file", fs::read(path).unwrap()));
        }
    }
    let mut out = BTreeMap::new();
    match fs::symlink_metadata(root) {
        Ok(_) => walk(root, root, &mut out),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => panic!("snapshot {}: {error}", root.display()),
    }
    out
}

fn capture_release_links(install_root: &Path, bin_dir: &Path) -> BTreeMap<String, PathBuf> {
    let mut links = BTreeMap::new();
    links.insert(
        "current".to_string(),
        fs::read_link(install_root.join("current")).unwrap(),
    );
    for name in [
        "kanban",
        "kb",
        "kanban-dispatcher",
        "kanban-codex-queue-adapter",
        "kanban-codex-app-server-adapter",
        "kanban-claude-print-adapter",
    ] {
        links.insert(name.to_string(), fs::read_link(bin_dir.join(name)).unwrap());
    }
    links
}

fn assert_release_view(install_root: &Path, bin_dir: &Path, release_dir: &Path) {
    let current_link = install_root.join("current");
    assert!(current_link.is_symlink(), "current symlink missing");
    assert_eq!(fs::read_link(&current_link).unwrap(), release_dir);
    for name in [
        "kanban",
        "kb",
        "kanban-dispatcher",
        "kanban-codex-queue-adapter",
        "kanban-codex-app-server-adapter",
        "kanban-claude-print-adapter",
    ] {
        let symlink = bin_dir.join(name);
        assert!(symlink.is_symlink(), "missing bin symlink {name}");
        assert_eq!(fs::read_link(&symlink).unwrap(), current_link.join(name));
    }
}

fn write_release_tool_stubs(
    fixture: &Fixture,
    fake_repo_root: &Path,
    fake_git_head: &str,
    fake_release_binary: &str,
    fake_host: &str,
) -> PathBuf {
    let stubs = fixture.root.join("release-stubs");
    fs::create_dir_all(&stubs).unwrap();
    let kb_skill = fake_repo_root.join("skills/kb/SKILL.md");
    fs::create_dir_all(kb_skill.parent().unwrap()).unwrap();
    fs::write(&kb_skill, "# kb skill fixture\n").unwrap();
    write_executable(
        &stubs.join("hostname"),
        r#"#!/bin/sh
set -eu
printf '%s\n' "${FAKE_HOST:?}"
"#,
    );
    write_executable(
        &stubs.join("git"),
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "-C" ]; then
  shift 2
fi
case "${1:-}" in
  status)
    exit 0
    ;;
  rev-parse)
    case "${2:-}" in
      --show-toplevel)
        printf '%s\n' "${FAKE_REPO_ROOT:?}"
        ;;
      HEAD)
        printf '%s\n' "${FAKE_GIT_HEAD:?}"
        ;;
      *)
        printf 'unexpected git rev-parse %s\n' "$*" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    printf 'unexpected git %s\n' "$*" >&2
    exit 1
    ;;
esac
"#,
    );
    write_executable(
        &stubs.join("cargo"),
        r#"#!/bin/sh
set -eu
case "${1:-}" in
  build)
    ;;
  *)
    printf 'unexpected cargo %s\n' "$*" >&2
    exit 1
    ;;
esac
target_root="${CARGO_TARGET_DIR:?}/release"
mkdir -p "$target_root"
for binary in kanban kb kanban-dispatcher kanban-codex-queue-adapter kanban-codex-app-server-adapter kanban-claude-print-adapter; do
  source="${FAKE_RELEASE_BINARY:?}"
  if [ -n "${FAKE_RELEASE_BINARY_DIR:-}" ]; then
    source="$FAKE_RELEASE_BINARY_DIR/$binary"
  fi
  cp "$source" "$target_root/$binary"
  chmod 0755 "$target_root/$binary"
done
"#,
    );
    write_executable(
        &stubs.join("date"),
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "+%s" ] && [ -n "${FAKE_RELEASE_DATE_SECONDS:-}" ]; then
  printf '%s\n' "$FAKE_RELEASE_DATE_SECONDS"
  exit 0
fi
command -p date "$@"
"#,
    );
    write_executable(
        &stubs.join("install"),
        r#"#!/bin/sh
set -eu
mode=0755
while [ "$#" -gt 0 ]; do
  case "$1" in
    -m)
      mode="$2"
      shift 2
      ;;
    -*)
      printf 'unexpected install flag %s\n' "$1" >&2
      exit 1
      ;;
    *)
      break
      ;;
  esac
done
src="$1"
dest="$2"
mkdir -p "$(dirname "$dest")"
cp "$src" "$dest"
chmod "$mode" "$dest"
"#,
    );
    write_executable(
        &stubs.join("ssh"),
        r#"#!/bin/sh
set -eu
host="$1"
shift
if [ -n "${FAKE_SSH_INVOCATION_LOG:-}" ]; then
  printf '%s\n' "$host $*" >> "$FAKE_SSH_INVOCATION_LOG"
fi
case "${1:-}" in
  hostname)
    printf '%s\n' "$host"
    ;;
  mktemp\ -d*)
    mktemp -d "${FAKE_REMOTE_ROOT:?}/$host.XXXXXX"
    ;;
  bash)
    shift 3
    hidden_path=""
    restore_hidden_path() {
      if [ -n "$hidden_path" ]; then
        mv "$hidden_path" "${FAKE_SSH_HIDE_PATH:?}"
        hidden_path=""
      fi
    }
    if [ -n "${FAKE_SSH_HIDE_PATH:-}" ]; then
      hidden_path="${FAKE_SSH_HIDE_PATH}.fake-ssh-hidden"
      [ ! -e "$hidden_path" ]
      mv "$FAKE_SSH_HIDE_PATH" "$hidden_path"
      [ ! -e "$FAKE_SSH_HIDE_PATH" ]
      trap restore_hidden_path EXIT HUP INT TERM
    fi
    FAKE_HOST="$host" bash -s -- "$@"
    ;;
  *)
    FAKE_HOST="$host" bash -lc "$*"
    ;;
esac
"#,
    );
    let _ = (
        fake_repo_root,
        fake_git_head,
        fake_release_binary,
        fake_host,
    );
    stubs
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
            "body": "MCP-created rule.", "board": ["MCP"], "as": "geoyws"
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
        "params": { "name": "task_move", "arguments": { "id": "t-wire", "status": "nonsense", "as": "geoyws" } }
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
        session.writer(),
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    session.writer().flush().unwrap();
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

    session.finish();
}

#[test]
fn the_mcp_server_reports_protocol_edges_over_stdio() {
    let fixture = Fixture::new("mcp-protocol-edge");
    fixture.ok_json(&fixture.main, &["init", "--name", "EDGE", "--json"]);

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );

    let default_initialize = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize"
    }));
    assert_eq!(
        default_initialize["result"]["protocolVersion"],
        "2024-11-05"
    );

    writeln!(session.writer(), "{{not-json").unwrap();
    session.writer().flush().unwrap();
    let malformed = session.recv();
    assert_eq!(malformed["error"]["code"], -32700);
    assert_eq!(malformed["id"], Value::Null);

    let missing_name = session.ask(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "arguments": {} }
    }));
    assert_eq!(missing_name["error"]["code"], -32602);
    assert!(
        missing_name["error"]["message"]
            .as_str()
            .unwrap()
            .contains("name")
    );

    session.finish();
}

#[test]
fn the_mcp_server_replaces_itself_without_dropping_the_session() {
    let fixture = Fixture::new("mcp-reload");
    fixture.ok_json(&fixture.main, &["init", "--name", "RELOAD", "--json"]);

    // Serve from a copy, so the test can replace the binary underneath it the
    // way `install` does.
    let binary = fixture.root.join("kanban");
    copy_executable(Path::new(env!("CARGO_BIN_EXE_kanban")), &binary);

    let mut session = Session::start(&binary, &fixture.main, &fixture.data);
    let pid = session.pid();
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
        assert_eq!(session.pid(), pid, "the process was replaced anyway");
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
        session.pid(),
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
    copy_executable(Path::new(env!("CARGO_BIN_EXE_kanban")), &binary);

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
            "--to",
            "driver-2",
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
        &[
            "task", "remove", "t-1", "--as", "geoyws", "--force", "--json",
        ],
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
fn session_handoff_requires_an_addressee_but_a_task_handoff_does_not() {
    let fixture = Fixture::new("session-handoff-addressee");
    fixture.ok_json(&fixture.main, &["init", "--name", "ADDR", "--json"]);

    // A session handoff (no task id) with no --to is refused, naming the fix.
    let refused = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "claude@driver-2",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    let message = refusal_object(&refused);
    assert!(
        message.contains("--to"),
        "the refusal must name --to: {message}"
    );
    assert!(
        message.contains("driver-2"),
        "the refusal must give an example lane: {message}"
    );

    // With --to it is created and carries the addressee.
    let created = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "claude@driver-2",
            "--to",
            "driver-2",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    assert_eq!(created["toAgent"], "driver-2");
    assert_eq!(created["status"], "pending");

    // A task handoff keeps --to optional: the task is the address.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-1", "--as", "outgoing", "--json"],
    );
    let task_handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-1",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "outgoing",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    assert!(task_handoff["toAgent"].is_null());
    assert_eq!(task_handoff["taskID"], "t-1");
}

#[test]
fn handoff_retire_closes_a_pending_handoff_without_deleting_it() {
    let fixture = Fixture::new("handoff-retire");
    fixture.ok_json(&fixture.main, &["init", "--name", "RETIRE", "--json"]);

    let session = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "claude@driver-2",
            "--to",
            "driver-2",
            "--summary",
            "Phase landed",
            "--intent",
            "continue",
            "--next-action",
            "merge",
            "--json",
        ],
    );
    let id = session["id"].as_str().unwrap().to_owned();

    let retired = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "retire",
            &id,
            "--as",
            "geoyws",
            "--note",
            "repo and branch gone",
            "--json",
        ],
    );
    assert_eq!(retired["status"], "retired");
    assert_eq!(retired["retiredBy"], "geoyws");
    assert_eq!(retired["retireNote"], "repo and branch gone");
    assert!(retired["retiredAt"].is_i64());

    // `--status pending` no longer shows it; `--status retired` does, with the
    // note and the actor.
    let pending = fixture.ok_json(
        &fixture.main,
        &["handoff", "list", "--status", "pending", "--json"],
    );
    assert!(
        pending
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["id"] != id.as_str()),
        "a retired handoff stayed in the pending resume queue"
    );
    let retired_list = fixture.ok_json(
        &fixture.main,
        &["handoff", "list", "--status", "retired", "--json"],
    );
    let row = retired_list
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == id.as_str())
        .expect("the retired handoff is not listed under --status retired");
    assert_eq!(row["retireNote"], "repo and branch gone");
    assert_eq!(row["retiredBy"], "geoyws");

    // Accepting a retired handoff refuses, naming retired, by whom, and when.
    let accepted = fixture.run(
        &fixture.main,
        &["handoff", "accept", &id, "--as", "driver-2", "--json"],
    );
    let message = refusal_object(&accepted);
    assert!(message.contains("retired"), "{message}");
    assert!(message.contains("geoyws"), "{message}");
    assert!(
        message.contains("epoch ms"),
        "the refusal must name when it was retired: {message}"
    );

    // Retiring twice refuses.
    let twice = fixture.run(
        &fixture.main,
        &[
            "handoff", "retire", &id, "--as", "geoyws", "--note", "again", "--json",
        ],
    );
    let twice_message = refusal_object(&twice);
    assert!(twice_message.contains("already retired"), "{twice_message}");

    // Retiring an accepted one refuses.
    let other = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "a",
            "--to",
            "b",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    let other_id = other["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &["handoff", "accept", &other_id, "--as", "b", "--json"],
    );
    let retire_accepted = fixture.run(
        &fixture.main,
        &[
            "handoff", "retire", &other_id, "--as", "geoyws", "--note", "x", "--json",
        ],
    );
    let retire_accepted_message = refusal_object(&retire_accepted);
    assert!(
        retire_accepted_message.contains("not pending"),
        "{retire_accepted_message}"
    );

    // The retirement is on the durable audit trail, note included.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let connection = Connection::open(&board).unwrap();
    let payload: String = connection
        .query_row(
            "SELECT payload FROM events WHERE kind='handoff_retired' ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(payload.contains("repo and branch gone"), "{payload}");
    assert!(payload.contains(&id), "{payload}");
}

#[test]
fn deploy_start_refuses_a_tier_host_pair_the_canonical_table_forbids() {
    let fixture = Fixture::new("deploy-tier-host");
    fixture.ok_json(&fixture.main, &["init", "--name", "TIERHOST", "--json"]);
    let start = |tier: &str, host: &str| {
        fixture.run(
            &fixture.main,
            &[
                "deploy",
                "start",
                "--repo",
                "geoyws/kanban",
                "--commit",
                "1111111111111111111111111111111111111111",
                "--tier",
                tier,
                "--environment",
                "env",
                "--host",
                host,
                "--url",
                "https://x",
                "--as",
                "e2e",
                "--json",
            ],
        )
    };

    // An MBP tier on a Hetzner host is refused, naming tier, host, and row.
    let refused = start("@_bdt", "hig");
    let message = refusal_object(&refused);
    assert!(message.contains("@_bdt"), "{message}");
    assert!(message.contains("hig"), "{message}");
    assert!(message.contains("geoywsMBP"), "{message}");

    // The same MBP tier on the MBP host is accepted.
    let accepted = start("@_bdt", "geoywsMBP");
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    // A Hetzner tier on an MBP host is refused.
    let refused = start("@_p", "geoywsMBP");
    let message = refusal_object(&refused);
    assert!(message.contains("@_p"), "{message}");
    assert!(message.contains("geoywsMBP"), "{message}");
    assert!(message.contains("Hetzner"), "{message}");

    // The same Hetzner tier on a Hetzner host is accepted.
    let accepted = start("@_p", "hax");
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
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
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
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
    assert!(String::from_utf8_lossy(&unauthorized.stderr).contains("only geoyws"));
    let missing_note = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "geoyws",
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
            "geoyws",
            "--note",
            "approved and pushed",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["resolvedBy"], "geoyws");
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
    assert!(String::from_utf8_lossy(&wrong_reopener.stderr).contains("only geoyws"));
    let reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            approval["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--note",
            "The wrong item was resolved.",
            "--json",
        ],
    );
    assert_eq!(reopened["status"], "open");
    assert_eq!(reopened["resolvedBy"], "geoyws");
    assert_eq!(reopened["resolution"], "approved and pushed");
    assert!(!reopened["reopenedAt"].is_null());
    assert_eq!(reopened["reopenedBy"], "geoyws");
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "geoyws",
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
    assert!(String::from_utf8_lossy(&again.stderr).contains("already resolved by geoyws"));

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
        &[
            "task", "remove", "t-1", "--as", "geoyws", "--force", "--json",
        ],
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
            "geoyws",
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
    remove_v21_subscription_schema(&connection);
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
        24
    );
}

#[test]
fn the_operator_actor_is_geoyws_and_geo_is_not_an_alias() {
    // George, 2026-09-05: "make sure that I'm geoyws and not geo so it's less
    // ambiguous." `geo` is retired outright, not kept as a second spelling.
    let fixture = Fixture::new("operator-actor");
    fixture.ok_json(&fixture.main, &["init", "--name", "OPERATOR", "--json"]);
    let raise = |body: &str| {
        fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "raise",
                body,
                "--as",
                "someone@lane",
                "--kind",
                "decision",
                "--json",
            ],
        )
    };

    // The retired spelling is refused exactly like any other non-raiser, and
    // the refusal names the actor that would have been accepted.
    let retired = raise("the old spelling must not slip through");
    let refused = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            retired["id"].as_str().unwrap(),
            "--as",
            "geo",
            "--note",
            "Approved.",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "`--as geo` resolved an item raised by someone else"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("only geoyws or that same raiser may resolve it"),
        "{stderr}"
    );
    let still_open = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "open", "--json"],
    );
    assert!(
        still_open
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == retired["id"])
    );

    let operator = raise("needs the operator");
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            operator["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--note",
            "Approved.",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["resolvedBy"], "geoyws");

    // The raiser settling its own row is unchanged by the rename.
    let own = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            retired["id"].as_str().unwrap(),
            "--as",
            "someone@lane",
            "--note",
            "Withdrawn by the raiser.",
            "--json",
        ],
    );
    assert_eq!(own["status"], "resolved");
    assert_eq!(own["resolvedBy"], "someone@lane");
}

#[test]
fn a_listing_can_be_narrowed_to_a_lane_and_projected_to_the_keys_asked_for() {
    // `task list --json` on the px board is a megabyte of bodies, and a remote
    // caller who wanted ids and titles paid for all of it. The board answers a
    // smaller question now: a lane, and the keys the caller names.
    let fixture = Fixture::new("lane-projection");
    fixture.ok_json(&fixture.main, &["init", "--name", "LANES", "--json"]);
    let body = "plan ".repeat(200);
    for (id, lane) in [
        ("t-two-a", "driver-2"),
        ("t-two-b", "driver-2"),
        ("t-three", "driver-3"),
    ] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task", "add", "Work", "--id", id, "--lane", lane, "--body", &body, "--json",
            ],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Unlaned", "--id", "t-none", "--json"],
    );

    let listed = |args: &[&str]| fixture.run(&fixture.main, args);
    let full = listed(&["task", "list", "--json"]);
    assert!(full.status.success());
    let projected = listed(&[
        "task",
        "list",
        "--lane",
        "driver-2",
        "--fields",
        "id,lane,title",
        "--json",
    ]);
    assert!(
        projected.status.success(),
        "{}",
        String::from_utf8_lossy(&projected.stderr)
    );
    let rows: Value = serde_json::from_slice(&projected.stdout).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["t-two-a", "t-two-b"],
        "the lane filter keeps that lane and nothing else"
    );
    for row in rows {
        let keys = row.as_object().unwrap().keys().collect::<Vec<_>>();
        assert_eq!(keys, ["id", "lane", "title"], "exactly the keys asked for");
        assert_eq!(row["lane"], "driver-2");
    }
    assert!(
        projected.stdout.len() * 10 < full.stdout.len(),
        "projected {} bytes is not small against the full {} bytes",
        projected.stdout.len(),
        full.stdout.len()
    );

    let without_body: Value =
        fixture.ok_json(&fixture.main, &["task", "list", "--no-body", "--json"]);
    assert_eq!(without_body.as_array().unwrap().len(), 4);
    for row in without_body.as_array().unwrap() {
        assert!(row.get("body").is_none(), "--no-body left a body on {row}");
        assert!(
            row.get("metadata").is_some(),
            "--no-body dropped more than the body"
        );
    }
    // The body is still there for whoever asks for the row.
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-two-a", "--json"])["body"],
        body
    );

    // A misspelt key is refused naming the keys that exist, before any row
    // is read, so an empty board refuses it the same way.
    let refused = listed(&["task", "list", "--fields", "id,titel", "--json"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("titel"), "{stderr}");
    assert!(stderr.contains("title"), "{stderr}");
    assert!(stderr.contains("staleMinutes"), "{stderr}");

    // Attention: a row is in a lane through its raiser or through its task.
    let raise = |body: &str, raiser: &str, task: Option<&str>| {
        let mut args = vec!["attention", "raise", body, "--as", raiser];
        if let Some(task) = task {
            args.extend(["--task", task]);
        }
        args.push("--json");
        fixture.ok_json(&fixture.main, &args)["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let by_raiser = raise("raiser route", "worker@driver-2", None);
    let by_task = raise("task route", "geoyws", Some("t-two-a"));
    raise("elsewhere", "worker@driver-3", Some("t-three"));
    let attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--lane",
            "driver-2",
            "--fields",
            "id,raisedBy",
            "--json",
        ],
    );
    let mut ids = attention
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(
                row.as_object().unwrap().keys().collect::<Vec<_>>(),
                ["id", "raisedBy"]
            );
            row["id"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    ids.sort();
    let mut expected = vec![by_raiser, by_task];
    expected.sort();
    assert_eq!(ids, expected);

    // The MCP manifest is projected from the same table, so the new flags
    // reach a tool without anyone restating them (ADR-010).
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    for name in ["task list", "attention list"] {
        let operation = schema["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|operation| operation["name"] == name)
            .unwrap();
        let kind = |flag: &str| {
            operation["flags"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == flag)
                .unwrap_or_else(|| panic!("{name} does not advertise --{flag}"))["kind"]
                .clone()
        };
        assert_eq!(kind("lane"), "value");
        assert_eq!(kind("fields"), "value");
        assert_eq!(kind("no-body"), "boolean");
    }
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

/// One capped listing, for the ADR-037 property below. Adding a listing is
/// one row here; the test refuses to pass until every `--limit`-taking
/// operation the binary publishes has one.
struct CappedListing {
    label: &'static str,
    /// The listing as invoked, without `--limit` or `--json`.
    argv: &'static [&'static str],
    /// The default it must not pass off as the whole.
    default: usize,
    /// Where the rows sit in the reply: the bare array, or this key of an object.
    rows: Option<&'static str>,
    /// Stand up whatever the rows hang off; what it returns is handed to `seed`.
    prepare: fn(&Fixture) -> String,
    /// Add one more row the listing would return.
    seed: fn(&Fixture, &str, usize),
}

fn seed_nothing(_: &Fixture) -> String {
    String::new()
}

fn seed_task(fixture: &Fixture) -> String {
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "capped history", "--id", "t-1", "--json"],
    );
    String::new()
}

fn seed_task_and_lease(fixture: &Fixture) -> String {
    seed_task(fixture);
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "agent", "--json"])["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn seed_task_and_board_path(fixture: &Fixture) -> String {
    seed_task(fixture);
    board_path_for_project(fixture, &fixture.main, "CAPPED")
        .to_str()
        .unwrap()
        .to_owned()
}

const CAPPED_LISTINGS: &[CappedListing] = &[
    CappedListing {
        label: "events",
        argv: &["events"],
        default: 50,
        rows: None,
        prepare: seed_task,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "note",
                    "t-1",
                    &format!("event {index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "registry-events",
        argv: &["events", "--registry"],
        default: 50,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "rule",
                    "add",
                    &format!("rule {index}"),
                    "--as",
                    "geo",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "sitrep-list",
        argv: &["sitrep", "list"],
        default: 20,
        rows: None,
        prepare: seed_nothing,
        // One lane each: a lane keeps only its ten newest current, and the
        // rest are archived out of the default view.
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "sitrep",
                    "post",
                    &format!("sitrep {index}"),
                    "--as",
                    "agent",
                    "--lane",
                    &format!("lane-{index}"),
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "attention-list",
        argv: &["attention", "list"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "attention",
                    "raise",
                    &format!("item {index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "deploy-list",
        argv: &["deploy", "list"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "deploy",
                    "start",
                    "--repo",
                    "geoyws/kanban",
                    "--commit",
                    "1111111111111111111111111111111111111111",
                    "--tier",
                    "@_p",
                    "--environment",
                    "production",
                    "--host",
                    "hax",
                    "--url",
                    "https://kb.geoy.ws",
                    "--operation-id",
                    &format!("op-{index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "claim-candidates",
        argv: &["claim", "--candidates", "--as", "agent"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "task",
                    "add",
                    &format!("candidate {index}"),
                    "--id",
                    &format!("t-c{index}"),
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "search",
        argv: &["search", "quokka", "--source", "task"],
        default: 10,
        rows: Some("results"),
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "task",
                    "add",
                    &format!("quokka {index}"),
                    "--id",
                    &format!("t-q{index}"),
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "handoff-list",
        argv: &["handoff", "list"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "handoff",
                    "create",
                    "--as",
                    "agent",
                    "--to",
                    "driver-2",
                    "--summary",
                    &format!("handoff {index}"),
                    "--intent",
                    "carry on",
                    "--next-action",
                    "resume",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "task-show-notes",
        argv: &["task", "show", "t-1"],
        default: 100,
        rows: Some("notes"),
        prepare: seed_task,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "note",
                    "t-1",
                    &format!("note {index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "task-show-checkpoints",
        argv: &["task", "show", "t-1"],
        default: 20,
        rows: Some("checkpoints"),
        prepare: seed_task_and_lease,
        seed: |fixture, lease, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "checkpoint",
                    "t-1",
                    "--lease",
                    lease,
                    "--as",
                    "agent",
                    "--summary",
                    &format!("checkpoint {index}"),
                    "--intent",
                    "carry on",
                    "--next-action",
                    "resume",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "task-show-handoffs",
        argv: &["task", "show", "t-1"],
        default: 100,
        rows: Some("handoffs"),
        prepare: seed_task_and_board_path,
        // Written straight into the table: every task handoff the CLI creates
        // also writes a checkpoint, so a hundred of them through the binary
        // would trip the twenty-checkpoint cap first and this cap could never
        // be observed on its own. The rows are real; only the checkpoint
        // side-effect is skipped.
        seed: |_, board, index| {
            Connection::open(board)
                .unwrap()
                .execute(
                    "INSERT INTO handoffs(id,task_id,checkpoint_seq,reason,status,from_agent,\
                     summary,intent,next_action,created_at) \
                     VALUES(?,'t-1',NULL,'manual','pending','agent',?,'carry on','resume',?)",
                    params![
                        format!("h-{index:08}"),
                        format!("handoff {index}"),
                        index as i64
                    ],
                )
                .unwrap();
        },
    },
    CappedListing {
        label: "access-audit",
        argv: &["access", "audit"],
        default: 50,
        rows: None,
        prepare: seed_nothing,
        // A refused `access` attempt appends exactly one denied access-audit
        // row and no policy event (clause 4), so one denied grant per index
        // seeds exactly one row of what `access audit` lists.
        seed: |fixture, _, _index| {
            let _ = fixture.run(
                &fixture.main,
                &[
                    "access",
                    "grant",
                    "--principal",
                    "p-deadbeef",
                    "--capability",
                    "read",
                    "--scope",
                    "registry",
                    "--as",
                    "geoyws",
                    "--reason",
                    "seed",
                    "--json",
                ],
            );
        },
    },
];

/// ADR-037: a capped listing that would exceed its default without `--limit`
/// refuses and names the flag; exactly the default is complete and answered;
/// an explicit `--limit` is honoured as-is.
///
/// Read bottom-up: the refusal is asserted on a board holding one row more
/// than the default, which is the one case every day of the silent-cap bug
/// answered with a full-looking page and exit 0. A test that only seeded
/// under the cap would have passed throughout.
#[test]
fn every_capped_listing_refuses_a_default_it_would_exceed_and_answers_one_it_meets() {
    // Enumerated from the surface the binary publishes, so a listing that
    // grows `--limit` later has to join the table or fail here. `watch` is
    // the one exception: its `--limit` sizes a batch, and it computes and
    // reports truncation itself.
    let fixture = Fixture::new("capped-surface");
    fixture.ok_json(&fixture.main, &["init", "--name", "SURFACE", "--json"]);
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    for operation in schema["operations"].as_array().unwrap() {
        let name = operation["name"].as_str().unwrap();
        let takes_limit = operation["flags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|flag| flag["name"] == "limit");
        if !takes_limit || name == "watch" {
            continue;
        }
        let words = name.split(' ').collect::<Vec<_>>();
        assert!(
            CAPPED_LISTINGS
                .iter()
                .any(|listing| listing.argv.starts_with(&words)),
            "`{name}` takes --limit but no CAPPED_LISTINGS row proves it refuses its default"
        );
    }
    drop(fixture);

    for listing in CAPPED_LISTINGS {
        let fixture = Fixture::new(&format!("capped-{}", listing.label));
        fixture.ok_json(&fixture.main, &["init", "--name", "CAPPED", "--json"]);
        let context = (listing.prepare)(&fixture);
        let rows = |value: &Value| -> Vec<Value> {
            match listing.rows {
                Some(key) => value[key].as_array(),
                None => value.as_array(),
            }
            .unwrap_or_else(|| {
                panic!(
                    "{}: no rows at {:?} in {value}",
                    listing.label, listing.rows
                )
            })
            .clone()
        };
        let with_limit = |limit: usize| -> Vec<Value> {
            let mut argv = listing.argv.to_vec();
            let limit = limit.to_string();
            argv.extend(["--limit", &limit, "--json"]);
            rows(&fixture.ok_json(&fixture.main, &argv))
        };
        let mut bare = listing.argv.to_vec();
        bare.push("--json");

        // Whatever `init` and `prepare` already wrote counts toward the cap.
        let baseline = with_limit(listing.default + 1).len();
        assert!(
            baseline <= listing.default,
            "{}: the board starts past the cap ({baseline})",
            listing.label
        );
        for index in baseline..listing.default {
            (listing.seed)(&fixture, &context, index);
        }

        // Exactly the default, no extra row: complete, and answered as such.
        // This is the false refusal a count-equals-limit check would commit.
        let complete = fixture.run(&fixture.main, &bare);
        assert!(
            complete.status.success(),
            "{}: exactly {} rows were refused as if more existed\nstderr: {}",
            listing.label,
            listing.default,
            String::from_utf8_lossy(&complete.stderr)
        );
        assert_eq!(
            rows(&serde_json::from_slice(&complete.stdout).unwrap()).len(),
            listing.default,
            "{}: a complete page at the cap was cut",
            listing.label
        );

        // One past it, no --limit: refuse, naming the cap and the flag.
        (listing.seed)(&fixture, &context, listing.default);
        let refused = fixture.run(&fixture.main, &bare);
        let message = refusal_object(&refused);
        assert!(
            message.contains("--limit"),
            "{}: the refusal does not name its fix: {message}",
            listing.label
        );
        assert!(
            message.contains(&listing.default.to_string()),
            "{}: the refusal does not name the cap it stopped at: {message}",
            listing.label
        );
        let plain = fixture.run(&fixture.main, listing.argv);
        assert!(
            !plain.status.success(),
            "{}: without --json the same listing answered",
            listing.label
        );
        assert!(
            String::from_utf8_lossy(&plain.stderr).contains("--limit"),
            "{}: the plain refusal does not name --limit",
            listing.label
        );

        // An explicit bound is honoured as stated, hit or not, with no marker.
        assert_eq!(
            with_limit(listing.default).len(),
            listing.default,
            "{}: --limit at the cap did not return exactly the cap",
            listing.label
        );
        assert_eq!(
            with_limit(listing.default + 1).len(),
            listing.default + 1,
            "{}: --limit above the cap did not reach the row past it",
            listing.label
        );
    }
}

/// One enum-valued argument, for the ADR-008 property below. Adding one is a
/// row here; the test refuses to pass until every argument the binary
/// publishes with a closed set has one.
struct EnumArgument {
    label: &'static str,
    /// The `schema --json` operation name the argument belongs to.
    operation: &'static str,
    /// The flag name (without `--`) or the positional name.
    argument: &'static str,
    /// Whether `argument` is a positional (`task move`'s `status`).
    positional: bool,
    /// Stand up whatever the command needs to reach the enum validation; the
    /// returned string substitutes `@ctx@` in `argv`.
    prepare: fn(&Fixture) -> String,
    /// A full invocation whose enum argument is the `@bogus@` token. The test
    /// swaps in a deliberately bad value, so the only refusal is the enum.
    argv: &'static [&'static str],
}

fn seed_story(fixture: &Fixture) -> String {
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "gated story",
            "--type",
            "story",
            "--id",
            "s-1",
            "--json",
        ],
    );
    String::new()
}

const ENUM_ARGUMENTS: &[EnumArgument] = &[
    EnumArgument {
        label: "checkpoint-state",
        operation: "checkpoint",
        argument: "state",
        positional: false,
        prepare: seed_task_and_lease,
        argv: &[
            "checkpoint",
            "t-1",
            "--lease",
            "@ctx@",
            "--as",
            "agent",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--state",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "task-add-type",
        operation: "task add",
        argument: "type",
        positional: false,
        prepare: seed_nothing,
        argv: &["task", "add", "titled", "--type", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "task-add-status",
        operation: "task add",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["task", "add", "titled", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "task-list-status",
        operation: "task list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["task", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "task-move-status",
        operation: "task move",
        argument: "status",
        positional: true,
        prepare: seed_nothing,
        argv: &["task", "move", "t-1", "@bogus@", "--as", "agent", "--json"],
    },
    EnumArgument {
        label: "note-kind",
        operation: "note",
        argument: "kind",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "note", "t-1", "body", "--as", "agent", "--kind", "@bogus@", "--json",
        ],
    },
    EnumArgument {
        label: "attention-raise-kind",
        operation: "attention raise",
        argument: "kind",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "attention",
            "raise",
            "needs a look",
            "--as",
            "agent",
            "--kind",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "attention-list-kind",
        operation: "attention list",
        argument: "kind",
        positional: false,
        prepare: seed_nothing,
        argv: &["attention", "list", "--kind", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "attention-list-status",
        operation: "attention list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["attention", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "deploy-start-tier",
        operation: "deploy start",
        argument: "tier",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "deploy",
            "start",
            "--repo",
            "r",
            "--commit",
            "c",
            "--tier",
            "@bogus@",
            "--environment",
            "e",
            "--host",
            "h",
            "--url",
            "u",
            "--as",
            "agent",
            "--json",
        ],
    },
    EnumArgument {
        label: "deploy-list-tier",
        operation: "deploy list",
        argument: "tier",
        positional: false,
        prepare: seed_nothing,
        argv: &["deploy", "list", "--tier", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "deploy-list-status",
        operation: "deploy list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["deploy", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "deploy-finish-result",
        operation: "deploy finish",
        argument: "result",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "deploy", "finish", "d-1", "--token", "x", "--result", "@bogus@", "--phase", "build",
            "--as", "agent", "--json",
        ],
    },
    EnumArgument {
        label: "deploy-finish-phase",
        operation: "deploy finish",
        argument: "phase",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "deploy",
            "finish",
            "d-1",
            "--token",
            "x",
            "--result",
            "succeeded",
            "--phase",
            "@bogus@",
            "--as",
            "agent",
            "--json",
        ],
    },
    EnumArgument {
        label: "handoff-create-reason",
        operation: "handoff create",
        argument: "reason",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "handoff",
            "create",
            "--as",
            "agent",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--reason",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "handoff-list-status",
        operation: "handoff list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["handoff", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "subscription-add-relation",
        operation: "subscription add",
        argument: "relation",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "subscription",
            "add",
            "--consumer",
            "c",
            "--action",
            "a",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "agent",
            "--relation",
            "@bogus@:id",
            "--json",
        ],
    },
    EnumArgument {
        label: "subscription-add-prior-status",
        operation: "subscription add",
        argument: "prior-status",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "subscription",
            "add",
            "--consumer",
            "c",
            "--action",
            "a",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "agent",
            "--prior-status",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "subscription-add-current-status",
        operation: "subscription add",
        argument: "current-status",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "subscription",
            "add",
            "--consumer",
            "c",
            "--action",
            "a",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "agent",
            "--current-status",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "subscription-list-status",
        operation: "subscription list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["subscription", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "story-advance-to",
        operation: "story advance",
        argument: "to",
        positional: false,
        prepare: seed_story,
        argv: &[
            "story", "advance", "s-1", "--as", "agent", "--to", "@bogus@", "--json",
        ],
    },
    EnumArgument {
        label: "watch-relation",
        operation: "watch",
        argument: "relation",
        positional: false,
        prepare: seed_nothing,
        argv: &["watch", "--relation", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "watch-prior-status",
        operation: "watch",
        argument: "prior-status",
        positional: false,
        prepare: seed_nothing,
        argv: &["watch", "--prior-status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "watch-current-status",
        operation: "watch",
        argument: "current-status",
        positional: false,
        prepare: seed_nothing,
        argv: &["watch", "--current-status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "access-grant-capability",
        operation: "access grant",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "access",
            "grant",
            "--principal",
            "p",
            "--capability",
            "@bogus@",
            "--scope",
            "registry",
            "--as",
            "geoyws",
            "--reason",
            "r",
            "--json",
        ],
    },
    EnumArgument {
        label: "access-revoke-capability",
        operation: "access revoke",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "access",
            "revoke",
            "--principal",
            "p",
            "--capability",
            "@bogus@",
            "--scope",
            "registry",
            "--as",
            "geoyws",
            "--reason",
            "r",
            "--json",
        ],
    },
    EnumArgument {
        label: "access-explain-capability",
        operation: "access explain",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "access",
            "explain",
            "--principal",
            "p",
            "--capability",
            "@bogus@",
            "--scope",
            "registry",
            "--json",
        ],
    },
    EnumArgument {
        label: "access-audit-capability",
        operation: "access audit",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &["access", "audit", "--capability", "@bogus@", "--json"],
    },
];

/// ADR-008: an enum-valued refusal names the whole set it accepts, so the
/// caller reads the exit status and the fix in one message. The values are
/// read back out of `schema --json`, which is the same surface an adapter
/// validates against (ADR-010), so a set the manifest advertises but the
/// refusal omits fails here — and an argument the binary publishes with a
/// closed set but no table row fails the completeness check above the loop.
///
/// Read bottom-up: the refusal is asserted against a deliberately bad value,
/// which is the one input every silent "invalid X" of the old shape answered
/// with no hint of what would work.
#[test]
fn every_enum_argument_refusal_names_the_whole_set() {
    let fixture = Fixture::new("enum-surface");
    fixture.ok_json(&fixture.main, &["init", "--name", "ENUM", "--json"]);
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);

    // (operation, argument, positional) -> the accepted values, projected from
    // the manifest. This is the "join the table or fail" guard: every argument
    // the binary publishes with a closed set must have a row, and every row
    // must be published, so the two cannot drift in either direction.
    let mut schema_values: BTreeMap<(String, String, bool), Vec<String>> = BTreeMap::new();
    for operation in schema["operations"].as_array().unwrap() {
        let name = operation["name"].as_str().unwrap().to_owned();
        for flag in operation["flags"].as_array().unwrap() {
            if let Some(values) = flag.get("values").and_then(Value::as_array) {
                schema_values.insert(
                    (
                        name.clone(),
                        flag["name"].as_str().unwrap().to_owned(),
                        false,
                    ),
                    values
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect(),
                );
            }
        }
        if let Some(positionals) = operation.get("positionalValues").and_then(Value::as_object) {
            for (positional, values) in positionals {
                schema_values.insert(
                    (name.clone(), positional.clone(), true),
                    values
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect(),
                );
            }
        }
    }
    for arg in ENUM_ARGUMENTS {
        let key = (
            arg.operation.to_owned(),
            arg.argument.to_owned(),
            arg.positional,
        );
        assert!(
            schema_values.contains_key(&key),
            "{}: no schema --json row publishes values for {}.{}",
            arg.label,
            arg.operation,
            arg.argument
        );
        let values = schema_values.get(&key).unwrap();
        assert!(
            !values.is_empty(),
            "{}: schema --json publishes an empty set",
            arg.label
        );
    }
    for (key, _) in &schema_values {
        let (operation, argument, positional) = key;
        assert!(
            ENUM_ARGUMENTS.iter().any(|arg| {
                arg.operation == operation
                    && arg.argument == argument
                    && arg.positional == *positional
            }),
            "schema --json publishes values for {operation} {argument} but no ENUM_ARGUMENTS row proves its refusal names them"
        );
    }
    drop(fixture);

    for arg in ENUM_ARGUMENTS {
        let fixture = Fixture::new(&format!("enum-{}", arg.label));
        fixture.ok_json(&fixture.main, &["init", "--name", "ENUM", "--json"]);
        let context = (arg.prepare)(&fixture);
        let bogus = "bogus-value";
        let argv: Vec<String> = arg
            .argv
            .iter()
            .map(|token| match *token {
                "@ctx@" => context.clone(),
                "@bogus@" => bogus.to_owned(),
                _ => (*token).to_owned(),
            })
            .collect();
        let borrowed = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let refused = fixture.run(&fixture.main, &borrowed);
        let message = refusal_object(&refused);
        let values = schema_values
            .get(&(
                arg.operation.to_owned(),
                arg.argument.to_owned(),
                arg.positional,
            ))
            .unwrap();
        for value in values {
            assert!(
                message.contains(value.as_str()),
                "{}: the refusal omits {value:?}: {message}",
                arg.label
            );
        }
    }
}

/// `story advance` on an epic names the verb that does move it: an epic has no
/// gate, and the answer the claim refusal already gives — "claim one of its
/// children instead" — has no story-advance equivalent, so the discoverable
/// fix is `task move`. The old refusal said only "not a story".
#[test]
fn story_advance_on_an_epic_names_the_working_verb() {
    let fixture = Fixture::new("story-advance-epic");
    fixture.ok_json(&fixture.main, &["init", "--name", "STORY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "the plan", "--type", "epic", "--id", "e-1", "--json",
        ],
    );
    let refused = fixture.run(
        &fixture.main,
        &["story", "advance", "e-1", "--as", "agent", "--json"],
    );
    let message = refusal_object(&refused);
    assert!(
        message.contains("is an epic") && message.contains("only a story advances"),
        "{message}"
    );
    assert!(
        message.contains("task move e-1 <status>"),
        "the refusal does not name the verb that moves an epic: {message}"
    );
    assert!(!message.contains("a epic"), "{message}");
}

/// The survey's numbers are counted, not fetched: `pendingHandoffs` was the
/// length of a 100-row listing page and `openAttention` of a 1000-row one, so
/// a board holding 101 and 1001 reported 100 and 1000 with nothing to say
/// either had stopped.
///
/// Read bottom-up: the boards are seeded one row past each old page, which is
/// the one case the silent cap answered wrong, and the same numbers are read
/// back through `dashboard --json` and the served Boards page.
#[test]
fn dashboard_and_boards_page_count_past_the_listing_page() {
    let fixture = Fixture::new("survey-counts");
    fixture.ok_json(&fixture.main, &["init", "--name", "SURVEY", "--json"]);
    let board = board_path_for_project(&fixture, &fixture.main, "SURVEY");
    {
        // Written straight into the tables, as the task-show-handoffs cap
        // above is: eleven hundred spawns of the binary would seed the same
        // rows, only slowly, and it is the survey's read that is under test.
        let connection = Connection::open(&board).unwrap();
        connection.execute_batch("BEGIN").unwrap();
        for index in 0..101 {
            connection
                .execute(
                    "INSERT INTO handoffs(id,task_id,checkpoint_seq,reason,status,from_agent,\
                     summary,intent,next_action,created_at) \
                     VALUES(?,NULL,NULL,'manual','pending','agent',?,'carry on','resume',?)",
                    params![
                        format!("h-{index:08}"),
                        format!("handoff {index}"),
                        index as i64
                    ],
                )
                .unwrap();
        }
        for index in 0..1001 {
            connection
                .execute(
                    "INSERT INTO attention(id,task_id,kind,body,raised_by,created_at,status) \
                     VALUES(?,NULL,'decision',?,'agent',?,'open')",
                    params![
                        format!("a-{index:08}"),
                        format!("item {index}"),
                        index as i64
                    ],
                )
                .unwrap();
        }
        connection.execute_batch("COMMIT").unwrap();
    }

    let dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert_eq!(dashboard[0]["pendingHandoffs"], 101);
    assert_eq!(dashboard[0]["openAttention"], 1001);

    // The count and the listing describe the same rows.
    let handoffs = fixture.ok_json(
        &fixture.main,
        &["handoff", "list", "--limit", "101", "--json"],
    );
    assert_eq!(handoffs.as_array().unwrap().len(), 101);
    let attention = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--limit", "1001", "--json"],
    );
    assert_eq!(attention.as_array().unwrap().len(), 1001);

    // The served Boards page is the same projection.
    let server = spawn_server(&fixture);
    let (status, body) = http_get(server.port, "/boards");
    assert_eq!(status, 200);
    assert!(
        body.contains("<td class=\"n waiting\">1001</td>"),
        "open attention on the Boards page:\n{body}"
    );
    assert!(
        body.contains("<td class=n>101</td>"),
        "pending handoffs on the Boards page:\n{body}"
    );
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
        &[
            "task", "move", "t-draft", "todo", "--as", "geoyws", "--json",
        ],
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
        &[
            "task", "remove", "e-1", "--as", "geoyws", "--force", "--json",
        ],
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
    remove_v21_subscription_schema(&connection);
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
        24
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
            "geoyws",
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
            "geoyws",
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
/// Idempotent: a directory already carrying a commit is left untouched, because
/// a second `commit` over a clean tree would fail and be misread as no repo.
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
    if run(&["rev-parse", "--show-toplevel"]) {
        return run(&["rev-parse", "HEAD"]);
    }
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
    // Claim is best-effort: running outside a repository is not an error -- it
    // simply has no git context, and recording none is the truthful outcome.
    // (A checkpoint/handoff/sitrep would refuse here; a claim is legitimate.)
    let fixture = Fixture::new("provenance-none");
    // The fixture's cwds are git repositories now, so stand in a plain sibling
    // directory to observe the no-repository path.
    let plain = fixture.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    fixture.ok_json(&plain, &["init", "--name", "NONE", "--json"]);
    fixture.ok_json(&plain, &["task", "add", "Work", "--id", "t-1", "--json"]);

    let claim = fixture.ok_json(&plain, &["claim", "t-1", "--as", "worker", "--json"]);
    assert!(claim["worktree"].is_null(), "provenance was invented");
    assert!(claim["branch"].is_null());
    assert!(claim["headSha"].is_null());

    // And the command itself is unaffected.
    assert_eq!(claim["taskID"], "t-1");
    assert_eq!(
        fixture.ok_json(&plain, &["task", "show", "t-1", "--json"])["status"],
        "in_progress"
    );
}

#[test]
fn a_provenance_write_outside_a_checkout_is_refused_and_flags_are_validated() {
    // ADR-008: a field that says something and holds nothing is refused. Run
    // each provenance-bearing write from a plain (non-repository) directory
    // with no flags, and require a refusal that names every flag and kb-board.
    let fixture = Fixture::new("provenance-refused");
    let plain = fixture.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    fixture.ok_json(&plain, &["init", "--name", "REFUSED", "--json"]);
    fixture.ok_json(&plain, &["task", "add", "Work", "--id", "t-1", "--json"]);
    let claim = fixture.ok_json(&plain, &["claim", "t-1", "--as", "worker", "--json"]);
    let token = claim["leaseToken"].as_str().unwrap().to_owned();

    let refuses_blank = |args: &[&str]| {
        let output = fixture.run(&plain, args);
        assert!(
            !output.status.success(),
            "a provenance write from outside a checkout succeeded: {args:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        for needle in ["--repo", "--branch", "--head", "--dirty", "kb-board"] {
            assert!(
                stderr.contains(needle),
                "the refusal must name {needle}: {stderr}"
            );
        }
    };

    refuses_blank(&[
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
    ]);
    refuses_blank(&[
        "handoff",
        "create",
        "--as",
        "worker",
        "--summary",
        "s",
        "--intent",
        "i",
        "--next-action",
        "n",
        "--json",
    ]);
    refuses_blank(&[
        "sitrep",
        "post",
        "Where I stand",
        "--as",
        "worker",
        "--lane",
        "driver-2",
        "--json",
    ]);

    // An explicit flag that smuggles garbage is refused by its shape, not
    // trusted: a HEAD must be hex (full 40 or at least 7), and --dirty must
    // read like `git status` (the exact wording kb-board writes).
    let head_shape = fixture.run(
        &plain,
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
            "--repo",
            "/tmp/r",
            "--branch",
            "b",
            "--head",
            "zzz",
            "--dirty",
            "clean",
            "--json",
        ],
    );
    assert!(
        !head_shape.status.success(),
        "a non-hex --head was accepted"
    );
    assert!(
        String::from_utf8_lossy(&head_shape.stderr).contains("--head"),
        "the shape refusal must name --head: {}",
        String::from_utf8_lossy(&head_shape.stderr)
    );

    let dirty_wording = fixture.run(
        &plain,
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
            "--repo",
            "/tmp/r",
            "--branch",
            "b",
            "--head",
            "0123456789abcdef0123456789abcdef01234567",
            "--dirty",
            "3 files",
            "--json",
        ],
    );
    assert!(
        !dirty_wording.status.success(),
        "a malformed --dirty was accepted"
    );
    assert!(
        String::from_utf8_lossy(&dirty_wording.stderr).contains("--dirty"),
        "the wording refusal must name --dirty: {}",
        String::from_utf8_lossy(&dirty_wording.stderr)
    );
}

#[test]
fn provenance_flags_round_trip_exactly_and_capture_matches_the_checkout() {
    // (b) Explicit flags are stored verbatim, so a caller whose checkout is not
    // the process cwd can still ship the truth across a process boundary.
    let fixture = Fixture::new("provenance-roundtrip");
    fixture.ok_json(&fixture.main, &["init", "--name", "RT", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    let claim = fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let token = claim["leaseToken"].as_str().unwrap().to_owned();

    let head = "0123456789abcdef0123456789abcdef01234567";
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
            "--repo",
            "/tmp/example-repo",
            "--branch",
            "feature-x",
            "--head",
            head,
            "--dirty",
            "2 files changed",
            "--json",
        ],
    );
    assert_eq!(checkpoint["repoPath"], "/tmp/example-repo");
    assert_eq!(checkpoint["branch"], "feature-x");
    assert_eq!(checkpoint["headSha"], head);
    assert_eq!(checkpoint["dirtySummary"], "2 files changed");
    // Read back through the same surface a resuming agent would use.
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(shown["checkpoints"][0]["repoPath"], "/tmp/example-repo");
    assert_eq!(shown["checkpoints"][0]["headSha"], head);
    assert_eq!(shown["checkpoints"][0]["dirtySummary"], "2 files changed");

    fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "worker",
            "--to",
            "driver-2",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--repo",
            "/tmp/example-repo",
            "--branch",
            "feature-x",
            "--head",
            head,
            "--dirty",
            "2 files changed",
            "--json",
        ],
    );
    let listed = fixture.ok_json(&fixture.main, &["handoff", "list", "--json"]);
    let row = &listed.as_array().unwrap()[0];
    assert_eq!(row["repoPath"], "/tmp/example-repo");
    assert_eq!(row["branch"], "feature-x");
    assert_eq!(row["headSha"], head);
    assert_eq!(row["dirtySummary"], "2 files changed");

    let sitrep = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Where I stand",
            "--as",
            "worker",
            "--lane",
            "driver-2",
            "--repo",
            "/tmp/example-repo",
            "--branch",
            "feature-x",
            "--head",
            head,
            "--dirty",
            "2 files changed",
            "--json",
        ],
    );
    assert_eq!(sitrep["worktree"], "/tmp/example-repo");
    assert_eq!(sitrep["branch"], "feature-x");
    assert_eq!(sitrep["headSha"], head);
    assert_eq!(sitrep["dirtySummary"], "2 files changed");
    let sitrep_listed = fixture.ok_json(&fixture.main, &["sitrep", "list", "--json"]);
    assert_eq!(
        sitrep_listed.as_array().unwrap()[0]["worktree"],
        "/tmp/example-repo"
    );

    // (c) With no flags, capture resolves the checkout the command runs in.
    // `main` is a git repository (the fixture made it one), so the recorded
    // values equal that checkout's real HEAD, branch and dirty count.
    let real_head = Command::new("git")
        .arg("-C")
        .arg(&fixture.main)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(
        real_head.status.success(),
        "git rev-parse HEAD failed: {}",
        String::from_utf8_lossy(&real_head.stderr)
    );
    let real_head = String::from_utf8(real_head.stdout)
        .unwrap()
        .trim()
        .to_owned();

    let captured = fixture.ok_json(
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
    assert!(
        captured["repoPath"].as_str().unwrap().ends_with("main"),
        "captured repoPath is not the checkout: {}",
        captured["repoPath"]
    );
    assert_eq!(captured["branch"], "work");
    assert_eq!(captured["headSha"], real_head);
    assert_eq!(captured["dirtySummary"], "clean");
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
        &["task", "move", "e-plan", "todo", "--as", "geoyws", "--json"],
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
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(registered["name"], "infra");
    assert_eq!(registered["description"], "hosts, containers, deploys");
    assert_eq!(registered["createdBy"], "geoyws");
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
            "task", "update", "t-chat", "--tag", "askiee", "--as", "geoyws", "--json",
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
            "geoyws",
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
            "task", "update", "e-plan", "--tag", "infra", "--as", "geoyws", "--json",
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
            "tag", "remove", "queuer", "--force", "--as", "geoyws", "--json",
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
    let active = fixture.ok_json(&original, &["workspace", "list", "--json"]);
    let mut moved_roots = active
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "MOVED")
        .map(|row| {
            assert_eq!(
                row["boardPath"], board_path,
                "repoint changed board identity"
            );
            assert_eq!(row["archived"], false);
            row["rootPath"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    moved_roots.sort();
    let mut expected_roots = vec![
        moved.canonicalize().unwrap().to_string_lossy().into_owned(),
        moved
            .join("lane")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    expected_roots.sort();
    assert_eq!(
        moved_roots, expected_roots,
        "repoint did not preserve exactly the intended canonical roots"
    );

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
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(detached["rootPath"], retired_root);
    assert_eq!(detached["archived"], true);
    assert_eq!(detached["archivedBy"], "geoyws");
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
    assert_eq!(lifecycle[0]["actor"], "geoyws");
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
    fs::create_dir_all(&retired).unwrap();
    let recreated_alias = fixture.run(&retired, &["task", "show", "t-kept", "--json"]);
    assert!(
        !recreated_alias.status.success(),
        "recreating a detached alias silently reattached it"
    );
    let recreated_stderr = String::from_utf8_lossy(&recreated_alias.stderr);
    assert!(
        recreated_stderr.contains("no Kanban project contains"),
        "{recreated_stderr}"
    );
    assert!(!String::from_utf8_lossy(&recreated_alias.stdout).contains("t-kept"));
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
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(detached_root["rootPath"], project_root);
    assert_eq!(detached_root["archived"], true);
    assert_eq!(detached_root["archivedBy"], "geoyws");
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
    for cwd in [&project, &retired] {
        let bare = fixture.run(cwd, &["task", "show", "t-kept", "--json"]);
        assert!(
            !bare.status.success(),
            "{} still resolved the board after its final root was detached",
            cwd.display()
        );
        let stderr = String::from_utf8_lossy(&bare.stderr);
        assert!(
            stderr.contains("no Kanban project contains"),
            "{}: {stderr}",
            cwd.display()
        );
        assert!(!String::from_utf8_lossy(&bare.stdout).contains("t-kept"));
    }
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
            "geoyws",
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

#[test]
fn workspace_adopt_copies_a_source_board_from_another_registry_and_preserves_it() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt");
    let source_data = fixture.root.join("source-data");
    let source_cwd = fixture.root.join("source-cwd");
    fs::create_dir_all(&source_cwd).unwrap();
    fs::create_dir_all(&source_data).unwrap();

    let source_init = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["init", "--name", "Alpha", "--json"])
        .output()
        .unwrap();
    assert!(
        source_init.status.success(),
        "source init failed: {}\nstderr: {}",
        String::from_utf8_lossy(&source_init.stdout),
        String::from_utf8_lossy(&source_init.stderr)
    );
    let source_init_json: Value = serde_json::from_slice(&source_init.stdout).unwrap();
    let source_board = PathBuf::from(source_init_json["boardPath"].as_str().unwrap());

    let source_task = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["task", "add", "keep this state", "--id", "t-live", "--json"])
        .output()
        .unwrap();
    assert!(
        source_task.status.success(),
        "source task add failed: {}\nstderr: {}",
        String::from_utf8_lossy(&source_task.stdout),
        String::from_utf8_lossy(&source_task.stderr)
    );

    let source_board = source_board.canonicalize().unwrap();
    let source_bytes = fs::read(&source_board).unwrap();

    let second_root = fixture.root.join("adopted-sibling");
    let neighbor = fixture.root.join("registered-neighbor");
    fs::create_dir_all(&second_root).unwrap();
    fs::create_dir_all(&neighbor).unwrap();
    fixture.ok_json(&neighbor, &["init", "--name", "Neighbor", "--json"]);
    fixture.ok_json(
        &neighbor,
        &[
            "task",
            "add",
            "neighbor sentinel",
            "--id",
            "t-neighbor",
            "--json",
        ],
    );

    let adopt_root = fixture.root.join("adopted");
    fs::create_dir_all(&adopt_root).unwrap();
    let receipt = fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source_board.to_str().unwrap(),
            "--name",
            "Alpha",
            "--workspace",
            adopt_root.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );

    let adopt_root = adopt_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(receipt["name"], "Alpha");
    assert_eq!(receipt["rootPath"], adopt_root.as_str());
    assert_eq!(
        receipt["sourceBoardPath"],
        source_board.to_string_lossy().as_ref()
    );
    assert_eq!(receipt["workspaceRoots"], json!([adopt_root.clone()]));
    let adopted_board = PathBuf::from(receipt["boardPath"].as_str().unwrap());
    assert_eq!(
        adopted_board.parent(),
        Some(fixture.data.join("boards").as_path()),
        "adopted destination escaped registry-owned boards storage"
    );
    assert_eq!(
        adopted_board.extension().and_then(|value| value.to_str()),
        Some("db")
    );
    assert!(
        adopted_board
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok()),
        "adopted destination is not UUID-named: {}",
        adopted_board.display()
    );
    let adopted_bytes = fs::read(&adopted_board).unwrap();
    assert_eq!(
        receipt["sourceSha256"],
        format!("{:x}", Sha256::digest(&adopted_bytes)),
        "receipt hash did not describe the exact registered bytes"
    );
    assert_eq!(receipt["sourceBytes"], json!(adopted_bytes.len() as u64));

    let adopted_task = fixture.ok_json(
        Path::new(&adopt_root),
        &["task", "show", "t-live", "--json"],
    );
    assert_eq!(adopted_task["id"], "t-live");
    assert_eq!(adopted_task["title"], "keep this state");

    let attached = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "attach",
            "--to",
            "Alpha",
            "--workspace",
            second_root.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        attached["boardPath"],
        adopted_board.to_string_lossy().as_ref()
    );
    let second_root = second_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let from_second = fixture.ok_json(
        Path::new(&second_root),
        &["task", "show", "t-live", "--json"],
    );
    assert_eq!(from_second["id"], "t-live");
    assert_eq!(from_second["title"], "keep this state");
    let by_project = fixture.ok_json(
        &neighbor,
        &["task", "show", "t-live", "--project", "Alpha", "--json"],
    );
    assert_eq!(by_project["id"], "t-live");
    assert_eq!(by_project["title"], "keep this state");

    fixture.ok_json(
        Path::new(&second_root),
        &[
            "task",
            "add",
            "written from adopted sibling",
            "--id",
            "t-adopted-sibling",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            Path::new(&adopt_root),
            &["task", "show", "t-adopted-sibling", "--json"],
        )["title"],
        "written from adopted sibling"
    );
    let neighbor_tasks = fixture.ok_json(&neighbor, &["task", "list", "--json"]);
    assert_eq!(neighbor_tasks.as_array().unwrap().len(), 1);
    assert_eq!(neighbor_tasks[0]["id"], "t-neighbor");

    let listed = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    let mut alpha_roots = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "Alpha")
        .map(|row| {
            assert_eq!(
                row["boardPath"],
                adopted_board.to_string_lossy().as_ref(),
                "an adopted root points at a different board"
            );
            assert_eq!(row["archived"], false);
            row["rootPath"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    alpha_roots.sort();
    let mut expected_roots = vec![adopt_root.clone(), second_root.clone()];
    expected_roots.sort();
    assert_eq!(alpha_roots, expected_roots);

    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--registry", "--kind", "board_adopted", "--json"],
    );
    assert_eq!(events[0]["actor"], "geoyws");
    assert_eq!(events[0]["payload"]["name"], "Alpha");
    assert_eq!(events[0]["payload"]["rootPath"], adopt_root.as_str());
    assert_eq!(
        events[0]["payload"]["sourceBoardPath"],
        source_board.to_string_lossy().as_ref()
    );
    assert_eq!(
        events[0]["payload"]["sourceSha256"],
        receipt["sourceSha256"]
    );
    assert_eq!(events[0]["payload"]["sourceBytes"], receipt["sourceBytes"]);
    assert_eq!(fs::read(&source_board).unwrap(), source_bytes);
    assert!(!fixture.data.join(".workspace-adopt.json").exists());
}

#[test]
fn workspace_adopt_requires_an_explicit_actor_before_opening_registry_state() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-missing-actor");
    let source_data = fixture.root.join("source-data");
    let source_cwd = fixture.root.join("source-cwd");
    fs::create_dir_all(&source_cwd).unwrap();
    fs::create_dir_all(&source_data).unwrap();
    let source_init = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["init", "--name", "Alpha", "--json"])
        .output()
        .unwrap();
    assert!(
        source_init.status.success(),
        "{}",
        String::from_utf8_lossy(&source_init.stderr)
    );
    let source_init_json: Value = serde_json::from_slice(&source_init.stdout).unwrap();
    let source_board = source_init_json["boardPath"].as_str().unwrap();

    let adopt = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source_board,
            "--name",
            "Alpha",
            "--rootless",
            "--json",
        ],
    );
    assert!(!adopt.status.success(), "adopt without --as succeeded");
    assert!(
        String::from_utf8_lossy(&adopt.stderr).contains("--as is required"),
        "{}",
        String::from_utf8_lossy(&adopt.stderr)
    );
    assert!(
        !fixture.data.join("registry.db").exists(),
        "missing-actor refusal opened or created the registry database"
    );
    assert!(
        !fixture.data.join("boards").exists(),
        "missing-actor refusal created registry-owned board storage"
    );
}

#[test]
fn workspace_adopt_missing_or_invalid_source_creates_no_live_registry_state() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    for (label, source) in [
        ("missing", None),
        ("invalid", Some(b"not a sqlite database".as_slice())),
        ("large", Some(vec![b'x'; 2 * 1024 * 1024].leak())),
    ] {
        let fixture = Fixture::new(&format!("workspace-adopt-preflight-{label}"));
        let source_path = fixture.root.join(format!("{label}.db"));
        if let Some(bytes) = source {
            fs::write(&source_path, bytes).unwrap();
        }
        let output = fixture.run(
            &fixture.main,
            &[
                "workspace",
                "adopt",
                "--from-board",
                source_path.to_str().unwrap(),
                "--name",
                "Alpha",
                "--rootless",
                "--as",
                "geoyws",
                "--json",
            ],
        );
        assert!(
            !output.status.success(),
            "{label} source unexpectedly adopted"
        );
        assert!(
            !fixture.data.exists(),
            "{label} source created live registry root before preflight refusal"
        );
    }
}

#[test]
fn workspace_adopt_helper_stays_hidden_from_help_schema_and_mcp() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-helper-hidden");
    let help = fixture.run(&fixture.main, &["--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(
        !help_text.contains("__workspace-adopt-helper"),
        "helper leaked into the public help surface: {help_text}"
    );

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operations = schema["operations"].as_array().unwrap();
    assert!(
        operations
            .iter()
            .all(|operation| operation["name"] != "__workspace_adopt_helper"),
        "helper leaked into the generated schema: {schema}"
    );

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .all(|tool| tool["name"] != "__workspace_adopt_helper"),
        "helper leaked into the MCP tool list: {listed}"
    );
}

// `KANBAN_TEST_WORKSPACE_ADOPT_HOOK` is honoured only by the debug pause seam
// `workspace_adopt_test_hook` in rust/registry.rs (`#[cfg(debug_assertions)]`).
// A release binary completes adoption instead of pausing, so this test exists
// only in debug test binaries; see
// `release_binary_ignores_workspace_adopt_pause_hook` for the release-side proof.
#[cfg(debug_assertions)]
#[test]
fn workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-crash-before-commit");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);
    let mut first = fixture.command(&fixture.main);
    first
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_marker");
    let mut first = first.spawn().unwrap();
    wait_for_path(&marker);
    let marker_json: Value = wait_for_json_file(&marker);
    let staging_dir = PathBuf::from(marker_json["stagingDir"].as_str().unwrap());

    let second = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !second.status.success(),
        "concurrent adopt unexpectedly won"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another kanban process is using"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    first.kill().unwrap();
    let first = first.wait_with_output().unwrap();
    assert!(
        !first.status.success(),
        "paused adopt unexpectedly completed"
    );

    let boards = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        boards
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "Alpha"),
        "crash recovery left a board registered unexpectedly: {boards}"
    );
    assert!(!marker.exists());
    assert!(!staging_dir.exists());
    assert_eq!(
        fs::read_dir(fixture.data.join("boards")).unwrap().count(),
        0
    );
}

#[test]
fn workspace_adopt_handles_helper_fd_collisions_and_cloexec() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-helper-fd-collision");
    let source = external_source_board(&fixture, "source", "Alpha");

    let mut command = fixture.command(&fixture.main);
    command.args([
        "workspace",
        "adopt",
        "--from-board",
        source.to_str().unwrap(),
        "--name",
        "Alpha",
        "--rootless",
        "--as",
        "geoyws",
        "--json",
    ]);
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, occupy_helper_fds_in_child);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "fd collision and cloexec handoff failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// Debug-only: depends on the `after_marker` pause seam (see the note on
// `workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash`).
#[cfg(debug_assertions)]
#[test]
fn workspace_adopt_refuses_while_the_canonical_data_root_lock_is_held() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-lock-held");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);

    let mut holder = fixture.command(&fixture.main);
    holder
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_marker");
    let mut holder = holder.spawn().unwrap();

    wait_for_path(&marker);

    let blocked = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !blocked.status.success(),
        "second adopt unexpectedly succeeded while the canonical data-root lock was held"
    );
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains("another kanban process is using"),
        "canonical lock refusal missing from stderr: {stderr}"
    );
    assert!(
        !stderr.contains("database is locked"),
        "raw SQLite contention leaked through instead of the canonical lock refusal: {stderr}"
    );

    holder.kill().unwrap();
    let holder = holder.wait_with_output().unwrap();
    assert!(
        !holder.status.success(),
        "paused adopt unexpectedly completed while testing lock refusal"
    );
}

// Debug-only: depends on the `after_publish` pause seam (see the note on
// `workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash`).
#[cfg(debug_assertions)]
#[test]
fn workspace_adopt_recovers_after_publishing_before_commit() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-crash-after-rename");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);
    let mut child = fixture.command(&fixture.main);
    child
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_publish");
    let mut child = child.spawn().unwrap();
    // wait_for_path only proves the file exists; the writing process may still
    // be mid-write, and reading it then fails with "EOF while parsing a value"
    // (observed under a loaded gate on 2026-09-05). wait_for_json_file retries
    // until the content parses.
    let marker_json: Value = wait_for_json_file(&marker);
    let board_path = PathBuf::from(marker_json["boardPath"].as_str().unwrap());
    wait_for_path(&board_path);

    child.kill().unwrap();
    let child = child.wait_with_output().unwrap();
    assert!(
        !child.status.success(),
        "paused adopt unexpectedly completed"
    );

    let boards = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        boards
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "Alpha"),
        "recovery left a board registered unexpectedly: {boards}"
    );
    assert!(!marker.exists());
    assert!(!board_path.exists());
    assert_eq!(
        fs::read_dir(fixture.data.join("boards")).unwrap().count(),
        0
    );
}

// Release-side counterpart of the three `#[cfg(debug_assertions)]` adopt tests
// above. A release binary compiles `workspace_adopt_test_hook` down to `Ok(())`,
// so the pause env var must be inert: adoption completes, the marker is gone,
// and the board is registered. This is what a release run reports instead of
// silently running three fewer tests.
#[cfg(not(debug_assertions))]
#[test]
fn release_binary_ignores_workspace_adopt_pause_hook() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-release-hook-inert");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);
    let mut command = fixture.command(&fixture.main);
    command
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_marker");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "release binary honoured the debug-only pause hook; the debug-only tests \
         workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash, \
         workspace_adopt_refuses_while_the_canonical_data_root_lock_is_held and \
         workspace_adopt_recovers_after_publishing_before_commit are intentionally \
         excluded from release test binaries (cfg(debug_assertions)); run `cargo test` \
         without --release to exercise the seam. stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "adoption marker lingered after a completed adopt"
    );
    let boards = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        boards
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "Alpha"),
        "release adopt did not register the board: {boards}"
    );
}

#[test]
fn workspace_adopt_rejects_boards_symlink_without_external_write_lock_or_event() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-boards-symlink");
    let source = external_source_board(&fixture, "source", "Alpha");
    let external = fixture.root.join("external");
    fs::create_dir_all(&fixture.data).unwrap();
    fs::create_dir(&external).unwrap();
    symlink(&external, fixture.data.join("boards")).unwrap();

    let output = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !output.status.success(),
        "boards symlink unexpectedly followed"
    );
    assert_eq!(
        fs::read_dir(&external).unwrap().count(),
        0,
        "external target was written"
    );
    assert!(
        !fixture.data.join(".lock").exists(),
        "symlink refusal created the live lock"
    );
    assert!(
        !fixture.data.join("registry.db").exists(),
        "symlink refusal created registry state"
    );
    assert!(
        fs::symlink_metadata(fixture.data.join("boards"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn workspace_adopt_compiled_process_refuses_source_symlink_traversal_fk_audit_and_newer_schema() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-fail-closed-sources");
    let valid = external_source_board(&fixture, "valid", "Alpha");
    let link = fixture.root.join("source-link.db");
    symlink(&valid, &link).unwrap();
    let traversal_dir = valid.parent().unwrap().join("traversal");
    fs::create_dir(&traversal_dir).unwrap();
    let traversal = traversal_dir.join("..").join(valid.file_name().unwrap());

    let fk = external_source_board(&fixture, "fk", "Alpha");
    let task = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "orphan",
            "--id",
            "t-orphan",
            "--db",
            fk.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        task.status.success(),
        "failed to seed FK fixture: {}",
        String::from_utf8_lossy(&task.stderr)
    );
    let fk_connection = Connection::open(&fk).unwrap();
    fk_connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    fk_connection
        .execute(
            "UPDATE tasks SET parent_id='t-missing' WHERE id='t-orphan'",
            [],
        )
        .unwrap();
    drop(fk_connection);

    let audit = external_source_board(&fixture, "audit", "Alpha");
    let audit_connection = Connection::open(&audit).unwrap();
    audit_connection
        .execute(
            "UPDATE events SET event_hash='bad' WHERE seq=(SELECT max(seq) FROM events)",
            [],
        )
        .unwrap();
    drop(audit_connection);

    let newer = external_source_board(&fixture, "newer", "Alpha");
    let newer_connection = Connection::open(&newer).unwrap();
    newer_connection
        .pragma_update(None, "user_version", 25_i64)
        .unwrap();
    drop(newer_connection);

    for (label, path, expected) in [
        ("symlink", link, "symlink"),
        ("traversal", traversal, "parent traversal"),
        ("foreign key", fk, "foreign key violations"),
        ("audit", audit, "invalid audit chain"),
        ("newer schema", newer, "newer than supported"),
    ] {
        let output = fixture.run(
            &fixture.main,
            &[
                "workspace",
                "adopt",
                "--from-board",
                path.to_str().unwrap(),
                "--name",
                "Alpha",
                "--rootless",
                "--as",
                "geoyws",
                "--json",
            ],
        );
        assert!(
            !output.status.success(),
            "{label} source unexpectedly adopted"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{label}: {stderr}");
        assert!(
            !fixture.data.exists(),
            "{label} refusal created live registry state"
        );
    }
}

#[test]
fn workspace_adopt_rejects_a_duplicate_active_board_name_across_processes() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-duplicate");
    let source_data = fixture.root.join("source-data");
    let source_cwd = fixture.root.join("source-cwd");
    fs::create_dir_all(&source_cwd).unwrap();
    fs::create_dir_all(&source_data).unwrap();

    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let source_init = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["init", "--name", "Alpha", "--json"])
        .output()
        .unwrap();
    assert!(
        source_init.status.success(),
        "{}",
        String::from_utf8_lossy(&source_init.stderr)
    );
    let source_init_json: Value = serde_json::from_slice(&source_init.stdout).unwrap();
    let source_board = source_init_json["boardPath"].as_str().unwrap();

    let adopt = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source_board,
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(!adopt.status.success(), "adopt unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&adopt.stderr).contains("already named Alpha"),
        "{}",
        String::from_utf8_lossy(&adopt.stderr)
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
            "geoyws",
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
            "geoyws",
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
            "geoyws",
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
            "geoyws",
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
            "geoyws",
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
            "geoyws",
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
            "geoyws",
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
                "--to",
                "driver-2",
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

struct ActorHeaderFixture {
    success_task_id: String,
    success_attention_id: String,
    negative_task_id: String,
    negative_attention_id: String,
    open_epic_id: String,
    default_task_id: String,
    default_attention_id: String,
}

fn seed_actor_header_fixture(fixture: &Fixture, board: &str) -> ActorHeaderFixture {
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", board, "--rootless", "--json"],
    );

    project_ok_json(
        fixture,
        board,
        &[
            "task",
            "add",
            "Custom resolve target",
            "--id",
            "t-custom-resolve",
            "--type",
            "task",
            "--status",
            "todo",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let success_attention = project_ok_json(
        fixture,
        board,
        &[
            "attention",
            "raise",
            "Custom resolve needed",
            "--task",
            "t-custom-resolve",
            "--kind",
            "decision",
            "--as",
            "geoyws",
            "--json",
        ],
    );

    project_ok_json(
        fixture,
        board,
        &[
            "task",
            "add",
            "Negative resolve target",
            "--id",
            "t-negative-resolve",
            "--type",
            "task",
            "--status",
            "todo",
            "--as",
            "ifca-sso",
            "--json",
        ],
    );
    let negative_attention = project_ok_json(
        fixture,
        board,
        &[
            "attention",
            "raise",
            "Negative resolve target",
            "--task",
            "t-negative-resolve",
            "--kind",
            "decision",
            "--as",
            "ifca-sso",
            "--json",
        ],
    );

    project_ok_json(
        fixture,
        board,
        &[
            "task",
            "add",
            "Custom open target",
            "--id",
            "e-custom-open",
            "--type",
            "epic",
            "--status",
            "draft",
            "--as",
            "ifca-sso",
            "--json",
        ],
    );

    project_ok_json(
        fixture,
        board,
        &[
            "task",
            "add",
            "Default resolve target",
            "--id",
            "t-default-resolve",
            "--type",
            "task",
            "--status",
            "todo",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let default_attention = project_ok_json(
        fixture,
        board,
        &[
            "attention",
            "raise",
            "Default resolve target",
            "--task",
            "t-default-resolve",
            "--kind",
            "decision",
            "--as",
            "geoyws",
            "--json",
        ],
    );

    ActorHeaderFixture {
        success_task_id: "t-custom-resolve".to_owned(),
        success_attention_id: success_attention["id"].as_str().unwrap().to_owned(),
        negative_task_id: "t-negative-resolve".to_owned(),
        negative_attention_id: negative_attention["id"].as_str().unwrap().to_owned(),
        open_epic_id: "e-custom-open".to_owned(),
        default_task_id: "t-default-resolve".to_owned(),
        default_attention_id: default_attention["id"].as_str().unwrap().to_owned(),
    }
}

fn assert_attention_resolution(
    fixture: &Fixture,
    board: &str,
    task_id: &str,
    expected_actor: &str,
) {
    let board_path = board_path_for_project(fixture, &fixture.main, board);
    let rows = project_ok_json(
        fixture,
        board,
        &[
            "attention",
            "list",
            "--status",
            "resolved",
            "--task",
            task_id,
            "--all",
            "--json",
        ],
    );
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "expected one resolved attention row");
    assert_eq!(rows[0]["resolvedBy"].as_str(), Some(expected_actor));
    assert!(rows[0]["resolution"].as_str().is_some());
    let events = project_ok_json(
        fixture,
        board,
        &[
            "events",
            "--task",
            task_id,
            "--kind",
            "attention_resolved",
            "--all",
            "--json",
        ],
    );
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["actor"].as_str(), Some(expected_actor));
    let connection = Connection::open(board_path).unwrap();
    let resolved_by: String = connection
        .query_row(
            "SELECT resolved_by FROM attention WHERE task_id=? AND status='resolved'",
            params![task_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resolved_by, expected_actor);
}

fn assert_task_moved_by(fixture: &Fixture, board: &str, id: &str, expected_actor: &str) {
    let board_path = board_path_for_project(fixture, &fixture.main, board);
    let task = project_ok_json(fixture, board, &["task", "show", id, "--json"]);
    assert_eq!(task["status"].as_str(), Some("todo"));
    let events = project_ok_json(
        fixture,
        board,
        &[
            "events",
            "--task",
            id,
            "--kind",
            "task_moved",
            "--all",
            "--json",
        ],
    );
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["actor"].as_str(), Some(expected_actor));
    let connection = Connection::open(board_path).unwrap();
    let status: String = connection
        .query_row("SELECT status FROM tasks WHERE id=?", params![id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(status, "todo");
}

#[test]
fn serve_actor_header_uses_trusted_edge_value_and_refuses_bad_requests() {
    let fixture = Fixture::new("serve-actor-header");
    let board = "SERVE-ACTOR";
    let seeded = seed_actor_header_fixture(&fixture, board);
    let server = spawn_server_with_actor_header(&fixture, Some("X-Kanban-Actor"));
    let port = server.port;
    let origin = server.origin();

    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.success_attention_id),
        &[
            ("Origin", "https://hostile.example"),
            ("X-Kanban-Actor", "ifca-sso"),
        ],
        b"decision=approve&reply=done",
    );
    assert_eq!(status, 403, "{response}");
    let still_open = project_ok_json(
        &fixture,
        board,
        &[
            "attention",
            "list",
            "--status",
            "open",
            "--task",
            &seeded.success_task_id,
            "--json",
        ],
    );
    assert_eq!(still_open.as_array().unwrap().len(), 1);

    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.success_attention_id),
        &[("Origin", &origin), ("X-Kanban-Actor", "ifca-sso")],
        b"decision=approve&reply=done",
    );
    assert_eq!(status, 303, "{response}");
    assert_attention_resolution(&fixture, board, &seeded.success_task_id, "ifca-sso");

    let (status, response) = http_post_with_headers(
        port,
        &format!("/plan/{board}/{}/open", seeded.open_epic_id),
        &[("Origin", &origin), ("X-Kanban-Actor", "ifca-sso")],
        b"",
    );
    assert_eq!(status, 303, "{response}");
    assert_task_moved_by(&fixture, board, &seeded.open_epic_id, "ifca-sso");

    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.negative_attention_id),
        &[("Origin", &origin)],
        b"decision=approve&reply=still-open",
    );
    assert_eq!(status, 400, "{response}");

    let negative = project_ok_json(
        &fixture,
        board,
        &[
            "attention",
            "list",
            "--status",
            "open",
            "--task",
            &seeded.negative_task_id,
            "--json",
        ],
    );
    assert_eq!(negative.as_array().unwrap().len(), 1);

    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.negative_attention_id),
        &[
            ("Origin", &origin),
            ("X-Kanban-Actor", "ifca-sso"),
            ("X-Kanban-Actor", "spoofed-second-copy"),
        ],
        b"decision=approve&reply=still-open",
    );
    assert_eq!(status, 400, "{response}");

    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.negative_attention_id),
        &[("X-Kanban-Actor", "ifca-sso")],
        b"decision=approve&reply=still-open",
    );
    assert_eq!(status, 403, "{response}");

    let oversized = "x".repeat(300);
    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.negative_attention_id),
        &[("Origin", &origin), ("X-Kanban-Actor", &oversized)],
        b"decision=approve&reply=still-open",
    );
    assert_eq!(status, 400, "{response}");

    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.negative_attention_id),
        &[("Origin", &origin), ("X-Kanban-Actor", "bad\tactor")],
        b"decision=approve&reply=still-open",
    );
    assert_eq!(status, 400, "{response}");

    let negative = project_ok_json(
        &fixture,
        board,
        &[
            "attention",
            "list",
            "--status",
            "open",
            "--task",
            &seeded.negative_task_id,
            "--json",
        ],
    );
    assert_eq!(negative.as_array().unwrap().len(), 1);

    let unauthorized_cli = project_command(&fixture, board)
        .args([
            "attention",
            "resolve",
            &seeded.default_attention_id,
            "--as",
            "ifca-sso",
            "--note",
            "still blocked",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        !unauthorized_cli.status.success(),
        "CLI resolve by non-geoyws/non-raiser actor was accepted"
    );
    let stderr = String::from_utf8_lossy(&unauthorized_cli.stderr);
    assert!(
        stderr.contains("only geoyws or that same raiser may resolve"),
        "{stderr}"
    );
}

#[test]
fn serve_actor_header_defaults_to_geo_when_flag_is_absent() {
    let fixture = Fixture::new("serve-actor-default");
    let board = "SERVE-DEFAULT";
    let seeded = seed_actor_header_fixture(&fixture, board);
    let server = spawn_server_with_actor_header(&fixture, None);
    let port = server.port;
    let origin = server.origin();

    let (status, response) = http_post_with_headers(
        port,
        &format!("/attention/{board}/{}/reply", seeded.default_attention_id),
        &[("Origin", &origin)],
        b"decision=approve&reply=done",
    );
    assert_eq!(status, 303, "{response}");
    assert_attention_resolution(&fixture, board, &seeded.default_task_id, "geoyws");
}

#[test]
fn trusted_edge_resolution_stays_on_the_single_web_call_site() {
    let rust_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("rust")
        .canonicalize()
        .unwrap();
    let serve_path = rust_dir.join("serve.rs");
    let store_path = rust_dir.join("store.rs");
    let mut definitions = Vec::new();
    let mut uses = Vec::new();
    for path in rust_sources(&rust_dir) {
        let path = path.canonicalize().unwrap();
        let source = fs::read_to_string(&path).unwrap();
        for reference in symbol_references_in_source(&source, "resolve_attention_from_trusted_edge")
        {
            match reference {
                SymbolReferenceKind::Definition => definitions.push(path.clone()),
                SymbolReferenceKind::Use => uses.push(path.clone()),
            }
        }
    }
    assert_eq!(definitions, vec![store_path]);
    assert_eq!(uses, vec![serve_path]);
}

#[test]
fn rust_sources_walk_nested_directories() {
    let root = std::env::temp_dir().join(format!("kanban-rust-source-walk-{}", std::process::id()));
    let nested = root.join("bin");
    fs::create_dir_all(&nested).unwrap();
    let nested_file = nested.join("tool.rs");
    fs::write(&nested_file, "fn main() {}\n").unwrap();

    let sources = rust_sources(&root);
    assert_eq!(sources, vec![nested_file]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbol_inventory_counts_associated_function_and_function_pointer_uses() {
    let associated_function = r#"
        struct Store;
        impl Store {
            fn resolve_attention_from_trusted_edge() {}
        }

        fn exercise() {
            let _ = Store::resolve_attention_from_trusted_edge;
        }
    "#;
    assert_eq!(
        symbol_references_in_source(associated_function, "resolve_attention_from_trusted_edge"),
        vec![SymbolReferenceKind::Definition, SymbolReferenceKind::Use]
    );

    let function_pointer = r#"
        fn resolve_attention_from_trusted_edge() {}

        fn exercise() {
            let _handler = resolve_attention_from_trusted_edge;
        }
    "#;
    assert_eq!(
        symbol_references_in_source(function_pointer, "resolve_attention_from_trusted_edge"),
        vec![SymbolReferenceKind::Definition, SymbolReferenceKind::Use]
    );
}

#[test]
fn symbol_inventory_skips_test_only_struct_type_and_static_items() {
    let source = r#"
        mod helper {
            pub struct resolve_attention_from_trusted_edge;
        }

        fn resolve_attention_from_trusted_edge() {}

        fn exercise() {
            let _ = resolve_attention_from_trusted_edge;
        }

        #[cfg(test)]
        struct TestOnlyStruct {
            field: helper::resolve_attention_from_trusted_edge,
        }

        #[cfg(all(test, feature = "inventory"))]
        type TestOnlyType = helper::resolve_attention_from_trusted_edge;

        #[cfg(any(
            all(test, feature = "inventory"),
            all(test, feature = "alternate")
        ))]
        static TEST_ONLY_STATIC: helper::resolve_attention_from_trusted_edge =
            helper::resolve_attention_from_trusted_edge;

        #[cfg(test)]
        struct TestOnlyStructField {
            field: helper::resolve_attention_from_trusted_edge,
        }

        #[cfg(test)]
        enum TestOnlyVariant {
            Hidden(helper::resolve_attention_from_trusted_edge),
        }

        #[cfg(all(feature = "alpha", not(feature = "beta")))]
        struct LiveFeatureField {
            field: helper::resolve_attention_from_trusted_edge,
        }
    "#;
    assert_eq!(
        symbol_references_in_source(source, "resolve_attention_from_trusted_edge"),
        vec![
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Use,
            SymbolReferenceKind::Use,
        ]
    );
}

#[test]
fn symbol_inventory_keeps_malformed_not_cfg_live() {
    let source = r#"
        fn resolve_attention_from_trusted_edge() {}

        #[cfg(not(test, feature = "inventory"))]
        struct MalformedNotField {
            field: resolve_attention_from_trusted_edge,
        }

        #[cfg(not())]
        enum EmptyNotVariant {
            Visible(resolve_attention_from_trusted_edge),
        }
    "#;
    assert_eq!(
        symbol_references_in_source(source, "resolve_attention_from_trusted_edge"),
        vec![
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Use,
            SymbolReferenceKind::Use,
        ]
    );
}

#[test]
fn symbol_inventory_treats_over_cap_cfg_as_live() {
    let cfg_atoms = (0..=MAX_CFG_ATOMS)
        .map(|index| format!("atom{index}"))
        .collect::<Vec<_>>();
    let source = format!(
        r#"
        fn resolve_attention_from_trusted_edge() {{}}

        #[cfg(all({cfg}))]
        struct OverCapLiveField {{
            field: resolve_attention_from_trusted_edge,
        }}
    "#,
        cfg = cfg_atoms.join(", ")
    );
    assert_eq!(
        symbol_references_in_source(&source, "resolve_attention_from_trusted_edge"),
        vec![SymbolReferenceKind::Definition, SymbolReferenceKind::Use]
    );
}

#[test]
fn symbol_inventory_catches_trait_foreign_and_macro_token_references() {
    let source = r#"
        trait Audit {
            fn resolve_attention_from_trusted_edge();
        }

        extern "C" {
            fn resolve_attention_from_trusted_edge();
        }

        macro_rules! capture {
            ($name:ident) => {
                $name
            };
        }

        fn resolve_attention_from_trusted_edge() {}

        fn exercise() {
            capture!(resolve_attention_from_trusted_edge);
        }
    "#;
    assert_eq!(
        symbol_references_in_source(source, "resolve_attention_from_trusted_edge"),
        vec![
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Use,
        ]
    );
}

#[test]
fn serve_actor_header_duplicate_cli_flags_fail_closed() {
    let fixture = Fixture::new("serve-actor-duplicate-cli");
    let output = fixture
        .command(&fixture.main)
        .args([
            "serve",
            "--port",
            "14200",
            "--actor-header",
            "X-Kanban-Actor",
            "--actor-header",
            "X-Other",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "duplicate actor header was accepted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("given more than once") || stderr.contains("takes a single value"),
        "{stderr}"
    );
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
    assert_eq!(resolved[0]["resolvedBy"], "geoyws");
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
    assert_reply_recorded(&tab, &origin, approve_id, "approve");
    let resolved = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "resolved", "--json"],
    );
    assert_eq!(resolved.as_array().unwrap().len(), 1);
    assert_eq!(resolved[0]["resolvedBy"], "geoyws");
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
    assert_reply_recorded(&tab, &origin, reject_id, "reject");
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
    assert_reply_recorded(&tab, &origin, reply_id, "reply");
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
    // cwd is a git repository, so the branch is captured rather than invented;
    // outside any checkout this write is refused, never stored blank.
    assert_eq!(mine[0]["branch"], "work");

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
            "geoyws",
            "--json",
        ],
    );
    assert!(first["id"].as_str().unwrap().starts_with("r-"));
    assert_eq!(first["author"], "geoyws");
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
            "geoyws",
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
        &["rule", "retire", first_id, "--as", "geoyws", "--json"],
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
        vec!["rule", "add", "", "--as", "geoyws", "--json"],
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
            "geoyws",
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
            "geoyws",
            "--json",
        ],
    );
    let long_body = format!(
        "crm-react only; PX repos are read-only references.\n\n{}",
        "supporting detail ".repeat(160)
    );
    let long = fixture.ok_json(
        &fixture.main,
        &[
            "rule", "add", "--body", &long_body, "--as", "geoyws", "--json",
        ],
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
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
    }
    for cwd in [&fixture.main, &second, &third] {
        fixture.ok_json(cwd, &["tag", "add", "shared", "--as", "geoyws", "--json"]);
    }

    let scoped = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Only tagged task context.",
            "--as",
            "geoyws",
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
            "geoyws",
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
        "geoyws",
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
        &[
            "tag", "remove", "infra", "--as", "geoyws", "--force", "--json",
        ],
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
            "geoyws",
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
            "geoyws",
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
            INSERT INTO global_rules VALUES('g-old','Existing global rule.','geoyws',0,1,1);
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
        14
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
        14
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
            "geoyws",
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
            "geoyws",
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
        &["tag", "add", "infra", "--as", "geoyws", "--json"],
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
             VALUES('g-late','Late rolling-upgrade rule.','geoyws',0,3,3,'[\"ALL\"]','[\"infra\"]')",
            [],
        )
        .unwrap();
    registry
        .execute(
            "INSERT INTO global_rule_events(rule_id,kind,actor,payload,created_at) \
             VALUES('g-late','global_rule_added','geoyws','{\"ruleID\":\"g-late\"}',3)",
            [],
        )
        .unwrap();
    drop(registry);
    Connection::open(&one_path)
        .unwrap()
        .execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at,task_tags) VALUES('r-legacy-one','ONE infrastructure rule.','geoyws',0,1,1,'[\"infra\"]')",
            [],
        )
        .unwrap();
    Connection::open(&two_path)
        .unwrap()
        .execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at,task_tags) VALUES('r-legacy-two','TWO board rule.','geoyws',0,2,2,'[]')",
            [],
        )
        .unwrap();
    let one = json!({"id":"r-legacy-one"});
    let two = json!({"id":"r-legacy-two"});

    let first = fixture.ok_json(
        &fixture.root,
        &["rule", "consolidate", "--as", "geoyws", "--json"],
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
        &["rule", "consolidate", "--as", "geoyws", "--json"],
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
            "geoyws",
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
                "rule", "add", local, "--board", board, "--as", "geoyws", "--json",
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
            "geoyws",
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
        &["rule", "retire", global_id, "--as", "geoyws", "--json"],
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
        &["rule", "add", "Every board.", "--as", "geoyws", "--json"],
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
            "geoyws",
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
            "geoyws",
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
            "rule", "update", only_id, "--board", "THREE", "--as", "geoyws", "--json",
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
            "rule", "add", "Bad mix.", "--board", "ALL", "--board", "ONE", "--as", "geoyws",
            "--json",
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
            "geoyws",
            "--json",
        ],
        vec![
            "rule",
            "add",
            "Unknown board.",
            "--board",
            "MISSING",
            "--as",
            "geoyws",
            "--json",
        ],
        vec![
            "rule",
            "add",
            "Legacy scope.",
            "--global",
            "--as",
            "geoyws",
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
fn compiled_binary_exports_and_imports_allowlisted_rules_without_mutating_source() {
    let source = Fixture::new("rule-transfer-source");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    source.ok_json(
        &source.main,
        &["tag", "add", "alpha", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &["tag", "add", "beta", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--tag",
            "alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    source.ok_json(
        &source_second,
        &[
            "rule",
            "add",
            "Beta source rule.",
            "--board",
            "BETA",
            "--tag",
            "beta",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let source_before =
        source.ok_json(&source.main, &["rule", "list", "--all", "--full", "--json"]);
    let bundle_path = source.root.join("rule-transfer.json");
    let export = source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(export["written"], json!(bundle_path.to_str().unwrap()));
    assert_eq!(export["sourceBoards"], json!(["ALPHA", "BETA"]));
    assert_eq!(export["rulesExported"], 2);

    let bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    assert_eq!(bundle["formatVersion"], 1);
    assert_eq!(bundle["exportedBy"], "geoyws");
    assert_eq!(bundle["sourceBoards"], json!(["ALPHA", "BETA"]));
    assert_eq!(bundle["rules"].as_array().unwrap().len(), 2);
    assert_eq!(
        source.ok_json(&source.main, &["rule", "list", "--all", "--full", "--json"]),
        source_before,
        "export mutated the source registry"
    );

    let destination = Fixture::new("rule-transfer-destination");
    let destination_second = destination.root.join("second");
    fs::create_dir_all(&destination_second).unwrap();
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    destination.ok_json(&destination_second, &["init", "--name", "BETA", "--json"]);

    let imported = destination.ok_json(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(imported["importedRules"], 2);
    assert_eq!(imported["alreadyImportedRules"], 0);
    assert_eq!(imported["destinationBoardsVerified"], 2);

    let imported_rules = destination.ok_json(
        &destination.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    let mut imported_by_source = BTreeMap::new();
    for rule in imported_rules.as_array().unwrap() {
        imported_by_source.insert(
            (
                rule["sourceBoard"].as_str().unwrap().to_owned(),
                rule["sourceRuleId"].as_str().unwrap().to_owned(),
            ),
            rule.clone(),
        );
    }
    for rule in bundle["rules"].as_array().unwrap() {
        let source_board = rule["sourceBoard"].as_str().unwrap().to_owned();
        let source_rule_id = rule["sourceRuleId"].as_str().unwrap().to_owned();
        let imported_rule = imported_by_source
            .get(&(source_board.clone(), source_rule_id.clone()))
            .unwrap_or_else(|| panic!("missing imported rule {source_board}/{source_rule_id}"));
        assert_ne!(imported_rule["id"], rule["sourceRuleId"]);
        assert_eq!(imported_rule["body"], rule["body"]);
        assert_eq!(imported_rule["author"], rule["author"]);
        assert_eq!(imported_rule["tags"], rule["tags"]);
        assert_eq!(imported_rule["sourceBoard"], rule["sourceBoard"]);
        assert_eq!(imported_rule["sourceRuleId"], rule["sourceRuleId"]);
    }

    let imported_again = destination.ok_json(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(imported_again["importedRules"], 0);
    assert_eq!(imported_again["alreadyImportedRules"], 2);
    assert_eq!(
        destination.ok_json(
            &destination.main,
            &["rule", "list", "--all", "--full", "--json"],
        ),
        imported_rules,
        "import was not idempotent"
    );
}

#[test]
fn compiled_binary_refuses_rule_import_when_a_bundle_item_source_registry_uuid_differs() {
    let source = Fixture::new("rule-transfer-source-tamper");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    source.ok_json(
        &source.main,
        &["tag", "add", "alpha", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &["tag", "add", "beta", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--tag",
            "alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    source.ok_json(
        &source_second,
        &[
            "rule",
            "add",
            "Beta source rule.",
            "--board",
            "BETA",
            "--tag",
            "beta",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let bundle_path = source.root.join("rule-transfer.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );
    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    bundle["rules"][0]["sourceRegistryUuid"] = json!(Uuid::new_v4().to_string());
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let destination = Fixture::new("rule-transfer-destination-tamper");
    let destination_second = destination.root.join("second");
    fs::create_dir_all(&destination_second).unwrap();
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    destination.ok_json(&destination_second, &["init", "--name", "BETA", "--json"]);

    let failed = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !failed.status.success(),
        "tampered bundle unexpectedly imported"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(
        stderr.contains("claims source registry") || stderr.contains("sourceRegistryUuid"),
        "{stderr}"
    );
    assert!(
        destination
            .ok_json(
                &destination.main,
                &["rule", "list", "--all", "--full", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused import mutated the destination registry"
    );
    let registry = Connection::open(destination.data.join("registry.db")).unwrap();
    let rule_count: i64 = registry
        .query_row("SELECT count(*) FROM rules", [], |row| row.get(0))
        .unwrap();
    let ledger_count: i64 = registry
        .query_row("SELECT count(*) FROM rule_import_ledger", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rule_count, 0, "a refused import wrote destination rules");
    assert_eq!(ledger_count, 0, "a refused import left ledger residue");
}

#[test]
fn compiled_binary_refuses_rule_import_when_destination_lacks_an_exported_board() {
    let source = Fixture::new("rule-transfer-missing-destination");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    source.ok_json(
        &source_second,
        &[
            "rule",
            "add",
            "Beta source rule.",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let bundle_path = source.root.join("rule-transfer.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );

    let destination = Fixture::new("rule-transfer-missing-destination-target");
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    let failed = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !failed.status.success(),
        "import without BETA unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(
        stderr.contains("not registered in this registry"),
        "{stderr}"
    );
    assert!(
        destination
            .ok_json(
                &destination.main,
                &["rule", "list", "--all", "--full", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused import mutated the destination registry"
    );
}

#[test]
fn hig_registry_refuses_absent_named_selectors_on_add_and_refingerprinted_import() {
    let source = Fixture::new("rule-selector-source-px-only");
    source.ok_json(&source.main, &["init", "--name", "px", "--json"]);
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "PX-only source rule.",
            "--board",
            "px",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let bundle_path = source.root.join("px-rules.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "px",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );

    let destination = Fixture::new("rule-selector-destination-px-only");
    destination.ok_json(&destination.main, &["init", "--name", "px", "--json"]);
    for board in ["kanban", "unum"] {
        let refused = destination.run(
            &destination.main,
            &[
                "rule",
                "add",
                "Wrong-host rule.",
                "--board",
                board,
                "--as",
                "geoyws",
                "--json",
            ],
        );
        assert!(
            !refused.status.success(),
            "a px-only registry accepted selector {board}"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains(&format!("no registered Kanban board named {board}")),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    bundle["rules"][0]["tags"]
        .as_array_mut()
        .unwrap()
        .push(json!("ONLY:unum"));
    let fingerprint = rule_transfer_item_fingerprint(&bundle["rules"][0]);
    bundle["rules"][0]["sourceContentSha256"] = json!(fingerprint);
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let refused = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "a correctly re-fingerprinted absent selector was imported"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("selector ONLY:unum"), "{stderr}");
    assert!(
        stderr.contains("outside the bundle sourceBoards allowlist [px]"),
        "{stderr}"
    );

    let registry = Connection::open(destination.data.join("registry.db")).unwrap();
    let rule_count: i64 = registry
        .query_row("SELECT count(*) FROM rules", [], |row| row.get(0))
        .unwrap();
    let ledger_count: i64 = registry
        .query_row("SELECT count(*) FROM rule_import_ledger", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rule_count, 0, "a refused import wrote a destination rule");
    assert_eq!(
        ledger_count, 0,
        "a refused import wrote an import-ledger row"
    );
}

#[test]
fn compiled_binary_refuses_refingerprinted_import_whose_only_selector_targets_an_active_board_outside_source_boards()
 {
    let source = Fixture::new("rule-transfer-scope-source");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    let added = source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha-only source rule.",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let source_rule_id = added["id"].as_str().unwrap().to_owned();
    let bundle_path = source.root.join("alpha-rules.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );

    // BETA is a live, uniquely named board in the destination, so the
    // active-selector check alone would accept ONLY:BETA; only the
    // sourceBoards allowlist can refuse it.
    let destination = Fixture::new("rule-transfer-scope-destination");
    let destination_second = destination.root.join("second");
    fs::create_dir_all(&destination_second).unwrap();
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    destination.ok_json(&destination_second, &["init", "--name", "BETA", "--json"]);

    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    assert_eq!(bundle["sourceBoards"], json!(["ALPHA"]));
    bundle["rules"][0]["tags"]
        .as_array_mut()
        .unwrap()
        .push(json!("ONLY:BETA"));
    let fingerprint = rule_transfer_item_fingerprint(&bundle["rules"][0]);
    bundle["rules"][0]["sourceContentSha256"] = json!(fingerprint);
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let registry = Connection::open(destination.data.join("registry.db")).unwrap();
    let audit_head = |registry: &Connection| -> (i64, Option<String>) {
        registry
            .query_row(
                "SELECT count(*), (SELECT event_hash FROM rule_events ORDER BY seq DESC LIMIT 1) FROM rule_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    let audit_before = audit_head(&registry);

    let refused = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "a re-fingerprinted ONLY selector outside sourceBoards was imported"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!("item {source_rule_id} selector ONLY:BETA")),
        "{stderr}"
    );
    assert!(
        stderr.contains("outside the bundle sourceBoards allowlist [ALPHA]"),
        "{stderr}"
    );
    assert!(
        stderr.contains("`rule export --board ALPHA --board BETA --as ACTOR`"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("found 0"),
        "refusal came from the absent-selector check, not the allowlist: {stderr}"
    );

    let rule_count: i64 = registry
        .query_row("SELECT count(*) FROM rules", [], |row| row.get(0))
        .unwrap();
    let ledger_count: i64 = registry
        .query_row("SELECT count(*) FROM rule_import_ledger", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rule_count, 0, "a refused import wrote a destination rule");
    assert_eq!(
        ledger_count, 0,
        "a refused import wrote an import-ledger row"
    );
    assert_eq!(
        audit_head(&registry),
        audit_before,
        "a refused import appended to the destination audit chain"
    );
    assert!(
        destination
            .ok_json(
                &destination.main,
                &["rule", "list", "--all", "--full", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused import mutated the destination registry"
    );
}

#[test]
fn compiled_binary_refuses_duplicate_rule_export_selectors_and_missing_boards() {
    let fixture = Fixture::new("rule-export-refusals");
    fixture.ok_json(&fixture.main, &["init", "--name", "ALPHA", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let duplicate_bundle = fixture.root.join("duplicate-bundle.json");

    let duplicate = fixture.run(
        &fixture.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--output",
            duplicate_bundle.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8_lossy(&duplicate.stderr).contains("given more than once"),
        "{:?}",
        String::from_utf8_lossy(&duplicate.stderr)
    );

    let missing_bundle = fixture.root.join("missing-bundle.json");
    let missing = fixture.run(
        &fixture.main,
        &[
            "rule",
            "export",
            "--board",
            "MISSING",
            "--as",
            "geoyws",
            "--output",
            missing_bundle.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("not registered in this registry"),
        "{:?}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn hig_release_script_requires_the_initialized_kb_skill_submodule() {
    let fixture = Fixture::new("hig-release-kb-submodule");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let release_binary_dir = Path::new(env!("CARGO_BIN_EXE_kanban")).parent().unwrap();
    let kb_skill = fake_repo_root.join("skills/kb/SKILL.md");
    fs::remove_file(&kb_skill).unwrap();

    let run_package = || {
        Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_RELEASE_BINARY_DIR", release_binary_dir)
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .arg(&script)
            .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
            .output()
            .unwrap()
    };

    let refused = run_package();
    assert!(
        !refused.status.success(),
        "package unexpectedly succeeded without skills/kb"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("skills/kb is not initialized"), "{stderr}");
    assert!(
        stderr.contains("git submodule update --init skills/kb"),
        "{stderr}"
    );
    assert!(!output_dir.exists(), "refused package left output behind");

    fs::write(&kb_skill, "# initialized kb skill\n").unwrap();
    let packaged = run_package();
    assert!(
        packaged.status.success(),
        "initialized package failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
}

#[test]
fn hig_release_script_installs_six_real_binaries_without_remote_hax_access_and_refuses_partial_activation()
 {
    let fixture = Fixture::new("hig-release");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let broken_install_root = fixture.root.join("broken-install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let release_binaries = [
        ("kanban", Path::new(env!("CARGO_BIN_EXE_kanban"))),
        ("kb", Path::new(env!("CARGO_BIN_EXE_kb"))),
        (
            "kanban-dispatcher",
            Path::new(env!("CARGO_BIN_EXE_kanban-dispatcher")),
        ),
        (
            "kanban-codex-queue-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-codex-queue-adapter")),
        ),
        (
            "kanban-codex-app-server-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-codex-app-server-adapter")),
        ),
        (
            "kanban-claude-print-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-claude-print-adapter")),
        ),
    ];
    let release_binary_dir = release_binaries[0].1.parent().unwrap();

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_RELEASE_BINARY_DIR", release_binary_dir)
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "package failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
    let manifest_path = output_dir.join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["formatVersion"], 1);
    assert_eq!(manifest["targets"], json!(["hax", "hig"]));
    assert_eq!(
        manifest["sourceCommit"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(manifest["sourceTreeClean"], true);
    assert_eq!(
        manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "kanban",
            "kb",
            "kanban-dispatcher",
            "kanban-codex-queue-adapter",
            "kanban-codex-app-server-adapter",
            "kanban-claude-print-adapter",
        ]
    );
    let receipt_path = output_dir.with_extension("receipt.json");
    let receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["host"], "hax");
    assert_eq!(receipt["targets"], json!(["hax", "hig"]));
    assert_eq!(
        receipt["manifestSha256"],
        json!(file_sha256(&manifest_path))
    );
    assert_eq!(
        receipt["sourceCommit"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    for (name, source) in &release_binaries {
        let packaged_binary = output_dir.join(name);
        assert!(packaged_binary.is_file(), "missing package binary {name}");
        assert_eq!(
            fs::read(&packaged_binary).unwrap(),
            fs::read(source).unwrap(),
            "package binary {name} did not come from its Cargo binary target"
        );
    }

    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "HAX install failed: {}\nstderr: {}",
        String::from_utf8_lossy(&hax_installed.stdout),
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let hax_installed_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();
    let hax_release_dir = PathBuf::from(hax_installed_json["releaseDir"].as_str().unwrap());
    assert!(hax_release_dir.is_dir(), "HAX release dir missing");
    assert!(
        hax_release_dir.join("manifest.json").is_file(),
        "HAX release manifest missing"
    );
    let hax_release_receipt = PathBuf::from(hax_installed_json["receipt"].as_str().unwrap());
    let hax_release_receipt_json: Value =
        serde_json::from_slice(&fs::read(&hax_release_receipt).unwrap()).unwrap();
    assert_eq!(
        hax_release_receipt_json["releaseDir"],
        json!(hax_release_dir.to_str().unwrap())
    );
    assert_eq!(hax_release_receipt_json["target"], "hax");
    assert_eq!(hax_release_receipt_json["targets"], json!(["hax", "hig"]));
    assert_eq!(
        hax_release_receipt_json["manifestSha256"],
        json!(file_sha256(&manifest_path))
    );
    let hax_release_receipt_bytes = fs::read(&hax_release_receipt).unwrap();
    let hax_installed_again = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed_again.status.success(),
        "HAX reinstall failed: {}\nstderr: {}",
        String::from_utf8_lossy(&hax_installed_again.stdout),
        String::from_utf8_lossy(&hax_installed_again.stderr)
    );
    assert_eq!(
        fs::read(&hax_release_receipt).unwrap(),
        hax_release_receipt_bytes,
        "HAX receipt bytes changed on reactivation"
    );
    assert_release_view(&hax_install_root, &hax_bin_dir, &hax_release_dir);
    for name in [
        "kanban",
        "kb",
        "kanban-dispatcher",
        "kanban-codex-queue-adapter",
        "kanban-codex-app-server-adapter",
        "kanban-claude-print-adapter",
        "manifest.json",
    ] {
        assert!(
            hax_release_dir.join(name).exists(),
            "missing HAX installed file {name}"
        );
    }

    let installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_SSH_HIDE_PATH", &hax_install_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "HIG install failed: {}\nstderr: {}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        hax_install_root.is_dir(),
        "fake SSH did not restore the hidden HAX install root"
    );
    assert!(
        String::from_utf8_lossy(&installed.stderr).trim().is_empty(),
        "HIG install wrote unexpected stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
    let release_dir = PathBuf::from(installed_json["releaseDir"].as_str().unwrap());
    assert!(release_dir.is_dir(), "release dir missing");
    assert!(
        release_dir.join("manifest.json").is_file(),
        "release manifest missing"
    );
    let release_receipt = PathBuf::from(installed_json["receipt"].as_str().unwrap());
    let release_receipt_json: Value =
        serde_json::from_slice(&fs::read(&release_receipt).unwrap()).unwrap();
    assert_eq!(
        release_receipt_json["releaseDir"],
        json!(release_dir.to_str().unwrap())
    );
    assert_eq!(release_receipt_json["target"], "hig");
    assert_eq!(
        release_receipt_json["manifestSha256"],
        hax_release_receipt_json["manifestSha256"]
    );
    for field in [
        "formatVersion",
        "host",
        "targets",
        "manifestSha256",
        "sourceCommit",
        "sourceTreeClean",
        "files",
    ] {
        assert_eq!(
            release_receipt_json[field], hax_release_receipt_json[field],
            "HIG release receipt changed canonical field {field}"
        );
    }
    assert_release_view(&install_root, &bin_dir, &release_dir);
    for name in [
        "kanban",
        "kb",
        "kanban-dispatcher",
        "kanban-codex-queue-adapter",
        "kanban-codex-app-server-adapter",
        "kanban-claude-print-adapter",
        "manifest.json",
    ] {
        assert!(
            release_dir.join(name).exists(),
            "missing installed file {name}"
        );
        assert_eq!(
            fs::read(release_dir.join(name)).unwrap(),
            fs::read(hax_release_dir.join(name)).unwrap(),
            "HIG installed bytes differ from the HAX release for {name}"
        );
    }

    let installed_again = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed_again.status.success(),
        "idempotent install failed: {}\nstderr: {}",
        String::from_utf8_lossy(&installed_again.stdout),
        String::from_utf8_lossy(&installed_again.stderr)
    );

    let partial_package = fixture.root.join("partial-package");
    fs::create_dir_all(&partial_package).unwrap();
    for entry in fs::read_dir(&output_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some("kb") {
            continue;
        }
        fs::copy(&path, partial_package.join(path.file_name().unwrap())).unwrap();
    }
    fs::copy(
        &receipt_path,
        partial_package.with_extension("receipt.json"),
    )
    .unwrap();
    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            partial_package.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            broken_install_root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "partial package activated successfully"
    );
    assert!(
        !broken_install_root.exists(),
        "partial package left an activated install root behind"
    );
}

#[test]
fn hig_release_script_keeps_the_previous_view_when_reactivation_fails_after_current() {
    let fixture = Fixture::new("hig-release-reactivation-failure");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );

    let installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&installed.stderr).trim().is_empty(),
        "HIG install wrote unexpected stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
    let release_dir = PathBuf::from(installed_json["releaseDir"].as_str().unwrap());
    let release_receipt = PathBuf::from(installed_json["receipt"].as_str().unwrap());
    let release_receipt_bytes = fs::read(&release_receipt).unwrap();
    let stable_links = capture_release_links(&install_root, &bin_dir);

    let failed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("HIG_RELEASE_FAIL_AFTER_CURRENT", "1")
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !failed.status.success(),
        "reactivation failure unexpectedly succeeded"
    );
    assert_eq!(
        capture_release_links(&install_root, &bin_dir),
        stable_links,
        "reactivation failure changed the public release view"
    );
    assert!(
        release_dir.is_dir(),
        "reactivation failure removed the release tree"
    );
    assert_eq!(
        fs::read(&release_receipt).unwrap(),
        release_receipt_bytes,
        "reactivation failure rewrote the release receipt"
    );
    let receipt_count = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(
        receipt_count, 1,
        "reactivation failure left receipt residue"
    );
}

#[test]
fn hig_release_script_usage_refuses_missing_and_unknown_commands_with_exit_64() {
    let fixture = Fixture::new("hig-release-usage");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");

    for args in [Vec::<&str>::new(), vec!["bogus", "hax"]] {
        let output = Command::new("bash")
            .current_dir(&fixture.main)
            .arg(&script)
            .args(args.iter().copied())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(64),
            "unexpected exit code for {:?}",
            args
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("usage:"), "{stderr}");
        assert!(
            stderr.contains("hig-release.sh package hax [--output DIR]"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("package <hax|hig>"),
            "stale usage text leaked into stderr: {stderr}"
        );
    }
}

#[test]
fn hig_release_script_rejects_package_target_hig() {
    let fixture = Fixture::new("hig-release-package-hig");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hig", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !packaged.status.success(),
        "package hig unexpectedly succeeded"
    );
    assert_eq!(packaged.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&packaged.stderr).contains("package target must be hax"),
        "stderr: {}",
        String::from_utf8_lossy(&packaged.stderr)
    );
}

#[test]
fn hig_release_script_rejects_hig_install_without_hax_install_root() {
    let fixture = Fixture::new("hig-release-hig-before-hax");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "HIG install without hax install root succeeded"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("--hax-install-root is required for hig installs"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn hig_release_script_rejects_the_build_provenance_receipt_for_hig_install() {
    let fixture = Fixture::new("hig-release-build-receipt");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let build_receipt = output_dir.with_extension("receipt.json");
    let fake_hax_install_root = fixture.root.join("fake-hax-install");
    let fake_release_dir = fake_hax_install_root
        .join("releases")
        .join(release_id_from_package(&output_dir));
    fs::create_dir_all(&fake_release_dir).unwrap();
    for entry in fs::read_dir(&output_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        fs::copy(&path, fake_release_dir.join(path.file_name().unwrap())).unwrap();
    }
    fs::copy(
        &build_receipt,
        fake_release_dir.with_extension("receipt.json"),
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fake_release_dir, fake_hax_install_root.join("current")).unwrap();
    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            fake_hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "build provenance receipt unexpectedly authorized HIG install"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("hax activation receipt is incomplete or mismatched")
            || String::from_utf8_lossy(&refused.stderr)
                .contains("hax activation receipt is missing"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn hig_release_script_rejects_a_mismatched_hax_install_receipt_before_ssh() {
    let fixture = Fixture::new("hig-release-hax-receipt-mismatch");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let ssh_invocation_log = fixture.root.join("unexpected-ssh-invocation.log");

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let hax_install_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();
    let hax_receipt_path = PathBuf::from(hax_install_json["receipt"].as_str().unwrap());
    let mut forged: Value = serde_json::from_slice(&fs::read(&hax_receipt_path).unwrap()).unwrap();
    forged["releaseId"] = json!("mismatched-release-id");
    fs::write(
        &hax_receipt_path,
        serde_json::to_vec_pretty(&forged).unwrap(),
    )
    .unwrap();

    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_SSH_INVOCATION_LOG", &ssh_invocation_log)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "mismatched hax receipt unexpectedly authorized HIG install"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("hax activation receipt is incomplete or mismatched"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !ssh_invocation_log.exists(),
        "HIG staging began before local HAX activation validation"
    );
}

#[test]
fn hig_release_script_prunes_to_ten_and_rolls_back_to_the_previous_release() {
    let fixture = Fixture::new("hig-release-rollback");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef00000000",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let release_second = "1700000000";
    let commit = |index: usize| format!("0123456789abcdef0123456789abcdef{:08x}", index);
    let hax_ctx = HaxInstallContext {
        fixture: &fixture,
        script: &script,
        path: &path,
        hostname_bin: &hostname_bin,
        fake_repo_root: &fake_repo_root,
        remote_root: &remote_root,
    };

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let mut release_dirs = Vec::new();
    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let _hax_install_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();

    for index in 0..5 {
        let package_dir = if index == 0 {
            output_dir.clone()
        } else {
            let cloned = fixture.root.join(format!("package-{index:02}"));
            clone_release_package(&output_dir, &cloned, &commit(index));
            cloned
        };
        let hax_install_root = if index == 0 {
            hax_install_root.clone()
        } else {
            install_matching_hax_package(
                &hax_ctx,
                &package_dir,
                &commit(index),
                &format!("rollback-hax-{index:02}"),
            )
        };
        let installed = Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", commit(index))
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .env("FAKE_RELEASE_DATE_SECONDS", release_second)
            .arg(&script)
            .args([
                "install",
                "hig",
                "--package",
                package_dir.to_str().unwrap(),
                "--hax-install-root",
                hax_install_root.to_str().unwrap(),
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            installed.status.success(),
            "install {index} failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
        release_dirs.push(PathBuf::from(
            installed_json["releaseDir"].as_str().unwrap(),
        ));
    }

    let failed_package = fixture.root.join("package-failed");
    clone_release_package(&output_dir, &failed_package, &commit(99));
    let failed_hax_install_root = install_matching_hax_package(
        &hax_ctx,
        &failed_package,
        &commit(99),
        "rollback-hax-failed",
    );
    let stable_links = capture_release_links(&install_root, &bin_dir);
    let failed_release_dir = install_root.join(format!(
        "releases/{}",
        release_id_from_package(&failed_package)
    ));
    let failed_install = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(99))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .env("HIG_RELEASE_FAIL_AFTER_CURRENT", "1")
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            failed_package.to_str().unwrap(),
            "--hax-install-root",
            failed_hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !failed_install.status.success(),
        "failed activation unexpectedly succeeded"
    );
    assert_eq!(
        capture_release_links(&install_root, &bin_dir),
        stable_links,
        "failed activation changed the public release view"
    );
    assert!(
        !failed_release_dir.exists(),
        "failed release directory was retained"
    );
    assert!(
        !failed_release_dir.with_extension("receipt.json").exists(),
        "failed release receipt was retained"
    );
    let release_receipts_after_failure = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(
        release_receipts_after_failure, 5,
        "failed release counted toward retention"
    );

    for index in 5..11 {
        let cloned = fixture.root.join(format!("package-{index:02}"));
        clone_release_package(&output_dir, &cloned, &commit(index));
        let hax_install_root = install_matching_hax_package(
            &hax_ctx,
            &cloned,
            &commit(index),
            &format!("rollback-hax-{index:02}"),
        );
        let installed = Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", commit(index))
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .env("FAKE_RELEASE_DATE_SECONDS", release_second)
            .arg(&script)
            .args([
                "install",
                "hig",
                "--package",
                cloned.to_str().unwrap(),
                "--hax-install-root",
                hax_install_root.to_str().unwrap(),
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            installed.status.success(),
            "install {index} failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
        release_dirs.push(PathBuf::from(
            installed_json["releaseDir"].as_str().unwrap(),
        ));
    }

    let release_receipts = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(
        release_receipts, 10,
        "retention did not stop at ten releases"
    );

    let rollback = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(10))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .arg(&script)
        .args([
            "rollback",
            "hig",
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
            "--steps",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        rollback.status.success(),
        "rollback failed: {}\nstderr: {}",
        String::from_utf8_lossy(&rollback.stdout),
        String::from_utf8_lossy(&rollback.stderr)
    );
    let rollback_json: Value = serde_json::from_slice(&rollback.stdout).unwrap();
    assert_eq!(
        PathBuf::from(rollback_json["releaseDir"].as_str().unwrap()),
        release_dirs[9]
    );
    let current_link = install_root.join("current");
    assert_eq!(fs::read_link(&current_link).unwrap(), release_dirs[9]);
    assert_release_view(&install_root, &bin_dir, &release_dirs[9]);
}

#[test]
fn hig_release_script_restores_the_previous_view_when_rollback_fails_mid_cutover() {
    let fixture = Fixture::new("hig-release-rollback-fail");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef11111111",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let commit = |index: usize| format!("0123456789abcdef0123456789abcdef{:08x}", index);
    let hax_ctx = HaxInstallContext {
        fixture: &fixture,
        script: &script,
        path: &path,
        hostname_bin: &hostname_bin,
        fake_repo_root: &fake_repo_root,
        remote_root: &remote_root,
    };

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let mut release_dirs = Vec::new();
    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let _hax_install_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();

    for index in 1..3 {
        let package_dir = if index == 0 {
            output_dir.clone()
        } else {
            let cloned = fixture.root.join(format!("rollback-package-{index:02}"));
            clone_release_package(&output_dir, &cloned, &commit(index));
            cloned
        };
        let hax_install_root = if index == 0 {
            hax_install_root.clone()
        } else {
            install_matching_hax_package(
                &hax_ctx,
                &package_dir,
                &commit(index),
                &format!("rollback-fail-hax-{index:02}"),
            )
        };
        let installed = Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", commit(index))
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .arg(&script)
            .args([
                "install",
                "hig",
                "--package",
                package_dir.to_str().unwrap(),
                "--hax-install-root",
                hax_install_root.to_str().unwrap(),
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            installed.status.success(),
            "install {index} failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
        release_dirs.push(PathBuf::from(
            installed_json["releaseDir"].as_str().unwrap(),
        ));
    }

    let stable_links = capture_release_links(&install_root, &bin_dir);
    let failed_rollback = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(1))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("HIG_RELEASE_FAIL_AFTER_CURRENT", "1")
        .arg(&script)
        .args([
            "rollback",
            "hig",
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
            "--steps",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        !failed_rollback.status.success(),
        "injected rollback failure unexpectedly succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&failed_rollback.stdout),
        String::from_utf8_lossy(&failed_rollback.stderr)
    );
    assert_eq!(
        capture_release_links(&install_root, &bin_dir),
        stable_links,
        "rollback failure changed the public release view"
    );
    assert_eq!(
        fs::read_link(install_root.join("current")).unwrap(),
        release_dirs[1],
        "rollback failure changed current\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&failed_rollback.stdout),
        String::from_utf8_lossy(&failed_rollback.stderr)
    );
    let release_receipts = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(release_receipts, 2, "rollback failure altered retention");
}

/// A packaged release plus a HAX activation of it, so both install paths can
/// be driven: `install hax` runs `install_release_tree` in this process's
/// bash, `install hig` ships the embedded remote script through the ssh stub.
struct ReleaseGuardHarness {
    fixture: Fixture,
    script: PathBuf,
    path: String,
    hostname_bin: PathBuf,
    fake_repo_root: PathBuf,
    remote_root: PathBuf,
    package_dir: PathBuf,
    hax_install_root: PathBuf,
}

impl ReleaseGuardHarness {
    fn new(label: &str) -> Self {
        let fixture = Fixture::new(label);
        let fake_repo_root = fixture.root.join("fake-repo");
        fs::create_dir_all(&fake_repo_root).unwrap();
        let remote_root = fixture.root.join("remote-root");
        fs::create_dir_all(&remote_root).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
        let stubs = write_release_tool_stubs(
            &fixture,
            &fake_repo_root,
            "0123456789abcdef0123456789abcdef01234567",
            env!("CARGO_BIN_EXE_kanban"),
            "hax",
        );
        let hostname_bin = stubs.join("hostname");
        let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
        let package_dir = fixture.root.join("package");
        let hax_install_root = fixture.root.join("install-hax");
        let harness = Self {
            fixture,
            script,
            path,
            hostname_bin,
            fake_repo_root,
            remote_root,
            package_dir,
            hax_install_root,
        };
        let packaged = harness
            .command()
            .args([
                "package",
                "hax",
                "--output",
                harness.package_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            packaged.status.success(),
            "{}",
            String::from_utf8_lossy(&packaged.stderr)
        );
        let hax_bin_dir = harness.fixture.root.join("bin-hax");
        let hax_installed = harness.install("hax", &harness.hax_install_root, &hax_bin_dir);
        assert!(
            hax_installed.status.success(),
            "{}",
            String::from_utf8_lossy(&hax_installed.stderr)
        );
        harness
    }

    fn command(&self) -> Command {
        let mut command = Command::new("bash");
        command
            .current_dir(&self.fixture.main)
            .env("PATH", &self.path)
            .env("HOSTNAME_BIN", &self.hostname_bin)
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &self.fake_repo_root)
            .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &self.remote_root)
            .arg(&self.script);
        command
    }

    /// `target` is `hax` for the local install path or `hig` for the embedded
    /// remote script.
    fn install(&self, target: &str, install_root: &Path, bin_dir: &Path) -> Output {
        let mut command = self.command();
        command.args([
            "install",
            target,
            "--package",
            self.package_dir.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ]);
        if target == "hig" {
            command.args([
                "--hax-install-root",
                self.hax_install_root.to_str().unwrap(),
            ]);
        }
        command.output().unwrap()
    }

    /// Runs an install that must be refused, and proves every watched tree is
    /// byte-identical afterwards: the refusal happened before any mutation.
    fn assert_refused_without_mutation(
        &self,
        target: &str,
        install_root: &Path,
        bin_dir: &Path,
        refusal: &str,
        watched: &[&Path],
    ) {
        let before: Vec<_> = watched.iter().map(|path| snapshot_tree(path)).collect();
        let refused = self.install(target, install_root, bin_dir);
        assert!(
            !refused.status.success(),
            "{target}: install succeeded through an unsafe view\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(refusal),
            "{target}: expected {refusal:?} in:\n{stderr}"
        );
        for (path, before) in watched.iter().zip(before) {
            assert_eq!(
                snapshot_tree(path),
                before,
                "{target}: refused install changed {}",
                path.display()
            );
        }
    }
}

#[test]
fn hig_release_script_refuses_a_planted_releases_symlink_before_writing_outside_the_tree() {
    let harness = ReleaseGuardHarness::new("hig-release-releases-symlink");
    for target in ["hax", "hig"] {
        let install_root = harness.fixture.root.join(format!("planted-{target}"));
        let bin_dir = harness.fixture.root.join(format!("planted-bin-{target}"));
        let outside = harness.fixture.root.join(format!("outside-{target}"));
        fs::create_dir_all(&install_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("operator.txt"), b"do not touch\n").unwrap();
        symlink(&outside, install_root.join("releases")).unwrap();

        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to install through a symlink at {}/releases; remove it so releases/ is a real directory inside {}",
                install_root.display(),
                install_root.display()
            ),
            &[&outside, &install_root, &bin_dir],
        );
        assert_eq!(
            fs::read(outside.join("operator.txt")).unwrap(),
            b"do not touch\n",
            "{target}: the directory behind the planted symlink was written"
        );
        assert!(
            fs::symlink_metadata(&bin_dir).is_err(),
            "{target}: refused install created the bin dir"
        );
    }
}

#[test]
fn hig_release_script_refuses_to_replace_operator_files_and_foreign_links_at_current_and_bin_destinations()
 {
    let harness = ReleaseGuardHarness::new("hig-release-operator-files");
    let outside = harness.fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("kanban"), b"#!/bin/sh\necho operator kanban\n").unwrap();

    for target in ["hax", "hig"] {
        // A regular file where the managed `current` symlink belongs.
        let install_root = harness.fixture.root.join(format!("current-file-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("current-file-bin-{target}"));
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("current"), b"operator notes\n").unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/current: it is not a symlink into {}/releases managed by this installer; move it aside before installing",
                install_root.display(),
                install_root.display()
            ),
            &[&install_root, &bin_dir],
        );
        assert_eq!(
            fs::read(install_root.join("current")).unwrap(),
            b"operator notes\n",
            "{target}: the operator's current file was clobbered"
        );
        assert!(
            fs::symlink_metadata(install_root.join("releases")).is_err(),
            "{target}: refused install created releases/"
        );

        // A symlink at `current` that the installer did not write.
        let install_root = harness
            .fixture
            .root
            .join(format!("current-foreign-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("current-foreign-bin-{target}"));
        fs::create_dir_all(&install_root).unwrap();
        symlink(&outside, install_root.join("current")).unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/current: it is not a symlink into {}/releases managed by this installer",
                install_root.display(),
                install_root.display()
            ),
            &[&outside, &install_root, &bin_dir],
        );
        assert_eq!(
            fs::read_link(install_root.join("current")).unwrap(),
            outside,
            "{target}: the operator's current link was repointed"
        );

        // A regular file at a public binary destination.
        let install_root = harness.fixture.root.join(format!("bin-file-{target}"));
        let bin_dir = harness.fixture.root.join(format!("bin-file-bin-{target}"));
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("kb"), b"#!/bin/sh\necho operator kb\n").unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/kb: it is not a symlink into {}/current managed by this installer; move it aside before installing",
                bin_dir.display(),
                install_root.display()
            ),
            &[&install_root, &bin_dir],
        );
        assert_eq!(
            fs::read(bin_dir.join("kb")).unwrap(),
            b"#!/bin/sh\necho operator kb\n",
            "{target}: the operator's kb file was clobbered"
        );
        assert!(
            fs::symlink_metadata(&install_root).is_err(),
            "{target}: refused install created the install root"
        );

        // A symlink at a public binary destination that points somewhere else.
        let install_root = harness.fixture.root.join(format!("bin-foreign-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("bin-foreign-bin-{target}"));
        fs::create_dir_all(&bin_dir).unwrap();
        symlink(outside.join("kanban"), bin_dir.join("kanban")).unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/kanban: it is not a symlink into {}/current managed by this installer",
                bin_dir.display(),
                install_root.display()
            ),
            &[&outside, &install_root, &bin_dir],
        );
        assert_eq!(
            fs::read_link(bin_dir.join("kanban")).unwrap(),
            outside.join("kanban"),
            "{target}: the operator's kanban link was repointed"
        );
    }
}

#[test]
fn hig_release_script_local_and_remote_install_guards_are_identical() {
    let script =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh"))
            .unwrap();
    let remote_start = script.find("<<'REMOTE'\n").unwrap();
    let remote_end = script[remote_start..].find("\nREMOTE\n").unwrap() + remote_start;
    for name in [
        "physical_dir",
        "managed_symlink",
        "ensure_safe_release_view",
        "atomic_symlink",
    ] {
        let header = format!("\n{name}() {{\n");
        let definitions: Vec<(usize, &str)> = script
            .match_indices(&header)
            .map(|(at, _)| {
                let start = at + 1;
                let end = script[start..].find("\n}\n").unwrap() + start + 3;
                (start, &script[start..end])
            })
            .collect();
        assert_eq!(
            definitions.len(),
            2,
            "{name} must be defined exactly twice: locally and inside the embedded remote script"
        );
        assert!(
            definitions[0].0 < remote_start
                && (remote_start..remote_end).contains(&definitions[1].0),
            "{name}: expected one local definition and one inside the REMOTE heredoc"
        );
        assert_eq!(
            definitions[0].1, definitions[1].1,
            "{name} drifted between the local and embedded remote install paths"
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
            "geoyws",
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
            "geoyws",
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

#[test]
fn compiled_binary_lists_retired_rootless_boards_once_in_workspace_list_all() {
    let fixture = Fixture::new("rootless-retire-list");
    let rootless = fixture.root.join("rootless");
    fs::create_dir_all(&rootless).unwrap();

    let created = fixture.ok_json(
        &rootless,
        &["init", "--name", "ROOTLESS", "--rootless", "--json"],
    );
    let rootless_path = created["boardPath"].as_str().unwrap().to_owned();

    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ROOTLESS",
            "--as",
            "geoyws",
            "--note",
            "retire rootless board",
            "--json",
        ],
    );
    assert_eq!(retired["archived"], true);

    let active_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        active_list
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ROOTLESS"),
        "the retired rootless board leaked into the default inventory: {active_list}"
    );

    let all_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--all", "--json"]);
    let rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "ROOTLESS")
        .collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        1,
        "retired rootless board duplicated in all inventory"
    );
    let row = rows[0];
    assert_eq!(row["archived"], true);
    assert_eq!(row["rootless"], true);
    assert_eq!(row["rootPath"], "");
    assert_eq!(row["boardPath"], rootless_path);

    let restored = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "unretire",
            "ROOTLESS",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(restored["archived"], false);
    assert!(
        restored["workspaceRoots"].as_array().unwrap().is_empty(),
        "unretiring a rootless board should not fabricate roots"
    );

    let restored_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        restored_list
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "ROOTLESS" && row["rootless"] == true),
        "unretiring the rootless board did not restore the active inventory: {restored_list}"
    );
}

#[test]
fn compiled_binary_keeps_rootless_retired_boards_visible_after_previous_detach_history() {
    let fixture = Fixture::new("rootless-retire-history");
    let project = fixture.root.join("project");
    fs::create_dir_all(&project).unwrap();

    let created = fixture.ok_json(&project, &["init", "--name", "ROOTLESS-HISTORY", "--json"]);
    let board_path = created["boardPath"].as_str().unwrap().to_owned();
    let root_path = created["workspaceRoots"][0]
        .as_str()
        .expect("rootless history root")
        .to_owned();

    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &root_path,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ROOTLESS-HISTORY",
            "--as",
            "geoyws",
            "--note",
            "retire after detach",
            "--json",
        ],
    );
    assert_eq!(retired["archived"], true);
    assert!(
        retired["workspaceRoots"].as_array().unwrap().is_empty(),
        "retiring a rootless board must not invent roots"
    );

    let active_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        active_list
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ROOTLESS-HISTORY"),
        "the retired rootless board leaked into the default inventory"
    );

    let all_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--all", "--json"]);
    let rootless_rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["boardPath"] == board_path && row["rootless"] == true)
        .collect::<Vec<_>>();
    assert_eq!(
        rootless_rows.len(),
        1,
        "retired rootless board with prior history disappeared from all inventory"
    );
    let row = rootless_rows[0];
    assert_eq!(row["archived"], true);
    assert_eq!(row["rootPath"], "");
    assert_eq!(row["boardPath"], board_path);
}

#[test]
fn compiled_binary_keeps_same_name_retired_boards_distinct_by_path() {
    let fixture = Fixture::new("retired-same-name");
    let first = fixture.root.join("first");
    let second = fixture.root.join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();

    let first_created = fixture.ok_json(&first, &["init", "--name", "SAME", "--json"]);
    let first_path = first_created["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "SAME",
            "--as",
            "geoyws",
            "--note",
            "retire first same-name board",
            "--json",
        ],
    );

    let second_created = fixture.ok_json(&second, &["init", "--name", "SAME", "--json"]);
    let second_path = second_created["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "SAME",
            "--as",
            "geoyws",
            "--note",
            "retire second same-name board",
            "--json",
        ],
    );

    let all_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--all", "--json"]);
    let same_name_rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "SAME")
        .collect::<Vec<_>>();
    assert_eq!(
        same_name_rows.len(),
        2,
        "same-name retired boards were deduped"
    );
    let mut paths = same_name_rows
        .iter()
        .map(|row| row["boardPath"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    paths.sort();
    let mut expected = vec![first_path, second_path];
    expected.sort();
    assert_eq!(paths, expected);
    assert!(
        same_name_rows.iter().all(|row| row["archived"] == true),
        "retired same-name boards must remain archived in the all inventory"
    );
}

#[test]
fn the_mcp_server_rejects_retired_direct_board_paths_over_stdio() {
    let fixture = Fixture::new("mcp-retired-db");
    let active = fixture.root.join("active");
    let retired = fixture.root.join("retired");
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&retired).unwrap();

    fixture.ok_json(&active, &["init", "--name", "MCP", "--json"]);
    let retired_board = fixture.ok_json(&retired, &["init", "--name", "RETIRED", "--json"]);
    let retired_path = retired_board["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "retire",
            "RETIRED",
            "--as",
            "geoyws",
            "--note",
            "retire MCP board",
            "--json",
        ],
    );

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let refused = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "task_list", "arguments": { "db": retired_path } }
    }));
    assert_eq!(refused["result"]["isError"], true);
    let text = refused["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("retire MCP board"), "{text}");

    session.finish();
}

#[test]
fn hax_registry_requires_rule_retirement_before_retiring_a_named_board() {
    let fixture = Fixture::new("active-rule-selector-retirement");
    let px = fixture.root.join("px");
    let kanban = fixture.root.join("kanban");
    fs::create_dir_all(&px).unwrap();
    fs::create_dir_all(&kanban).unwrap();
    fixture.ok_json(&px, &["init", "--name", "px", "--json"]);
    fixture.ok_json(&kanban, &["init", "--name", "kanban", "--json"]);
    let only = fixture.ok_json(
        &fixture.root,
        &[
            "rule",
            "add",
            "PX host rule.",
            "--board",
            "px",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let except = fixture.ok_json(
        &fixture.root,
        &[
            "rule",
            "add",
            "All hosts except PX.",
            "--except-board",
            "px",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.root,
        &[
            "rule",
            "add",
            "Kanban host rule.",
            "--board",
            "kanban",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let only_id = only["id"].as_str().unwrap();
    let except_id = except["id"].as_str().unwrap();
    let mut blocker_ids = [only_id, except_id];
    blocker_ids.sort_unstable();

    let refused = fixture.run(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "px",
            "--as",
            "geoyws",
            "--note",
            "split host registries",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "workspace retirement left active named selectors behind"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("ONLY:px"), "{stderr}");
    assert!(stderr.contains("EXCEPT:px"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "blocking rule IDs: {}, {}",
            blocker_ids[0], blocker_ids[1]
        )),
        "{stderr}"
    );
    assert!(stderr.contains("update or retire those rules"), "{stderr}");
    let still_active = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        still_active
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "px" && row["archived"] == false),
        "refused retirement mutated the board"
    );
    assert!(
        fixture
            .ok_json(
                &fixture.root,
                &[
                    "events",
                    "--registry",
                    "--kind",
                    "workspace_retired",
                    "--json",
                ],
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "refused retirement appended an audit event"
    );

    for id in [only_id, except_id] {
        fixture.ok_json(
            &fixture.root,
            &["rule", "retire", id, "--as", "geoyws", "--json"],
        );
    }
    let retired_board = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "px",
            "--as",
            "geoyws",
            "--note",
            "split host registries",
            "--json",
        ],
    );
    assert_eq!(retired_board["archived"], true);

    let all_rules = fixture.ok_json(
        &fixture.root,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert!(all_rules.as_array().unwrap().iter().any(|rule| {
        rule["id"] == only["id"]
            && rule["archived"] == true
            && rule["body"] == "PX host rule."
            && rule["tags"] == json!(["ONLY:px"])
    }));
    let shown = fixture.ok_json(&fixture.root, &["rule", "show", only_id, "--json"]);
    assert_eq!(shown["body"], "PX host rule.");
    assert_eq!(shown["tags"], json!(["ONLY:px"]));
    let rule_events = fixture.ok_json(&fixture.root, &["events", "--rule", only_id, "--json"]);
    assert!(
        rule_events
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "rule_retired")
    );

    let all_workspaces = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    assert!(
        all_workspaces
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "px" && row["archived"] == true)
    );
    let doctor = fixture.ok_json(&fixture.root, &["doctor", "--all", "--json"]);
    assert_eq!(doctor["healthy"], true, "{doctor}");
    assert_eq!(doctor["activeRuleSelectors"]["healthy"], true);
    assert!(
        doctor["activeRuleSelectors"]["errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        doctor["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "px" && row["archived"] == true)
    );
}

#[test]
fn doctor_reports_stale_active_selectors_without_blocking_rule_history() {
    let fixture = Fixture::new("doctor-active-rule-selectors");
    fixture.ok_json(&fixture.main, &["init", "--name", "px", "--json"]);
    let rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Recoverable rule.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let rule_id = rule["id"].as_str().unwrap();
    {
        let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
        registry
            .execute(
                "UPDATE rules SET tags='[\"ONLY:unum\",\"ONLY:unum\"]' WHERE id=?",
                [rule_id],
            )
            .unwrap();
    }

    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !checked.status.success(),
        "doctor certified a stale active selector"
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["activeRuleSelectors"]["healthy"], false);
    assert_eq!(
        report["activeRuleSelectors"]["errors"],
        json!([{
            "ruleId": rule_id,
            "selector": "ONLY:unum",
            "activeBoardCount": 0
        }])
    );

    let listed = fixture.ok_json(
        &fixture.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert!(listed.as_array().unwrap().iter().any(|row| {
        row["id"] == rule["id"] && row["tags"] == json!(["ONLY:unum", "ONLY:unum"])
    }));
    let shown = fixture.ok_json(&fixture.main, &["rule", "show", rule_id, "--json"]);
    assert_eq!(shown["tags"], json!(["ONLY:unum", "ONLY:unum"]));

    let update = fixture.run(
        &fixture.main,
        &[
            "rule",
            "update",
            rule_id,
            "--body",
            "An edit must not preserve a stale active selector.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !update.status.success(),
        "an active-rule edit retained a stale selector"
    );
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains(rule_id), "{stderr}");
    assert!(stderr.contains("selector ONLY:unum"), "{stderr}");
    assert!(stderr.contains("found 0"), "{stderr}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["rule", "show", rule_id, "--json"])["body"],
        "Recoverable rule.",
        "refused update changed the active rule"
    );

    fixture.ok_json(
        &fixture.main,
        &["rule", "retire", rule_id, "--as", "geoyws", "--json"],
    );
    let recovered = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(recovered["healthy"], true, "{recovered}");
    assert_eq!(recovered["activeRuleSelectors"]["healthy"], true);
}

#[test]
fn retiring_and_unretiring_a_workspace_hides_it_by_default_and_rolls_back_conflicts() {
    let fixture = Fixture::new("workspace-retire-unretire");
    let alpha = fixture.root.join("alpha");
    let alpha_spare = fixture.root.join("alpha-spare");
    let beta = fixture.root.join("beta");
    let alpha_nested = alpha.join("nested");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&alpha_spare).unwrap();
    fs::create_dir_all(&beta).unwrap();
    fs::create_dir_all(&alpha_nested).unwrap();
    let alpha_spare = alpha_spare.canonicalize().unwrap();

    let alpha_registered = fixture.ok_json(&alpha, &["init", "--name", "ALPHA", "--json"]);
    let alpha_root = alpha_registered["workspaceRoots"][0]
        .as_str()
        .expect("alpha root")
        .to_owned();
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "attach",
            "--to",
            "ALPHA",
            "--workspace",
            alpha_spare.to_str().unwrap(),
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            alpha_spare.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &alpha,
        &[
            "task",
            "add",
            "Retired needle 77",
            "--id",
            "t-retired-77",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(&beta, &["init", "--name", "BETA", "--json"]);
    fixture.ok_json(
        &beta,
        &[
            "task",
            "add",
            "BETA wrong-board sentinel",
            "--id",
            "t-retired-77",
            "--json",
        ],
    );
    let retirement_note = "moved-to-hig";

    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ALPHA",
            "--as",
            "geoyws",
            "--note",
            retirement_note,
            "--json",
        ],
    );
    assert_eq!(retired["name"], "ALPHA");
    assert_eq!(retired["archivedBy"], "geoyws");
    assert_eq!(retired["archivedNote"], retirement_note);
    assert_eq!(retired["workspaceRoots"], json!([alpha_root.clone()]));
    assert!(retired["archivedAt"].as_i64().is_some());
    let retired_path = retired["boardPath"].as_str().unwrap().to_owned();

    let with_env = |key: &str, value: &str, args: &[&str]| -> Output {
        fixture
            .command(&fixture.root)
            .env(key, value)
            .args(args)
            .output()
            .unwrap()
    };

    let direct_write = fixture.run(
        &fixture.root,
        &[
            "task",
            "add",
            "Blocked by retirement",
            "--db",
            &retired_path,
            "--json",
        ],
    );
    assert!(
        !direct_write.status.success(),
        "a retired board path still answered writes"
    );
    let direct_write_stderr = String::from_utf8_lossy(&direct_write.stderr).into_owned();
    assert!(
        direct_write_stderr.contains(retirement_note),
        "{direct_write_stderr}"
    );

    let direct_watch = fixture.run(
        &fixture.root,
        &["watch", "--db", &retired_path, "--limit", "0", "--json"],
    );
    assert!(
        !direct_watch.status.success(),
        "a retired board path still answered watch"
    );
    let direct_watch_stderr = String::from_utf8_lossy(&direct_watch.stderr).into_owned();
    assert!(
        direct_watch_stderr.contains(retirement_note),
        "{direct_watch_stderr}"
    );

    let env_list = with_env("KANBAN_DB", &retired_path, &["task", "list", "--json"]);
    assert!(
        !env_list.status.success(),
        "KANBAN_DB still answered from a retired board"
    );
    let env_list_stderr = String::from_utf8_lossy(&env_list.stderr).into_owned();
    assert!(
        env_list_stderr.contains(retirement_note),
        "{env_list_stderr}"
    );

    let explicit_workspace = fixture.run(
        &beta,
        &[
            "task",
            "show",
            "t-retired-77",
            "--workspace",
            &alpha_root,
            "--json",
        ],
    );
    let env_project = fixture
        .command(&beta)
        .env("KANBAN_PROJECT", "ALPHA")
        .args(["task", "show", "t-retired-77", "--json"])
        .output()
        .unwrap();
    let readonly_workspace = fixture.run(
        &beta,
        &["subscription", "list", "--workspace", &alpha_root, "--json"],
    );
    for (label, selector, output) in [
        (
            "explicit --workspace",
            alpha_root.as_str(),
            explicit_workspace,
        ),
        ("KANBAN_PROJECT", "ALPHA", env_project),
        (
            "read-only subscription --workspace",
            alpha_root.as_str(),
            readonly_workspace,
        ),
    ] {
        assert!(
            !output.status.success(),
            "{label} fell through to the active BETA board"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(selector),
            "{label} did not identify {selector}: {stderr}"
        );
        assert!(
            stderr.contains(retirement_note),
            "{label} omitted the recorded retirement note: {stderr}"
        );
        // The refusal is the only thing on stdout: no answer from BETA rides
        // along with it.
        assert!(
            refusal_object(&output).contains(selector),
            "{label} returned a wrong-board answer while refusing: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let active_list = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        active_list
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ALPHA"),
        "retired board leaked into the default workspace list"
    );
    let all_list = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    let retired_rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["boardPath"] == retired_path && row["rootless"] == true)
        .collect::<Vec<_>>();
    assert_eq!(
        retired_rows.len(),
        0,
        "retired rooted board still gained a rootless summary row"
    );
    let retired_row = all_list
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["boardPath"] == retired_path && row["rootPath"] == alpha_root)
        .expect("archived ALPHA retirement row");
    assert_eq!(retired_row["archived"], true);
    assert_eq!(retired_row["archivedBy"], "geoyws");
    assert_eq!(retired_row["archivedNote"], retirement_note);
    assert_eq!(retired_row["rootless"], false);

    let dashboard = fixture.ok_json(&fixture.root, &["dashboard", "--json"]);
    assert!(
        dashboard
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ALPHA"),
        "retired board leaked into the default dashboard"
    );
    let dashboard_all = fixture.ok_json(&fixture.root, &["dashboard", "--all", "--json"]);
    let dashboard_alpha = dashboard_all
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "ALPHA")
        .expect("archived ALPHA dashboard row");
    assert_eq!(dashboard_alpha["archived"], true);
    assert_eq!(dashboard_alpha["archivedNote"], retirement_note);

    let doctor = fixture.ok_json(&fixture.root, &["doctor", "--json"]);
    assert!(
        doctor["projects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ALPHA"),
        "retired board leaked into the default doctor report"
    );
    let doctor_all = fixture.ok_json(&fixture.root, &["doctor", "--all", "--json"]);
    let doctor_alpha = doctor_all["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "ALPHA")
        .expect("archived ALPHA doctor row");
    assert_eq!(doctor_alpha["archived"], true);
    assert_eq!(doctor_alpha["archivedBy"], "geoyws");
    assert_eq!(doctor_alpha["archivedNote"], retirement_note);

    let search_default = fixture.ok_json(
        &fixture.root,
        &["search", "Retired needle 77", "--all-boards", "--json"],
    );
    assert!(
        search_default["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["board"] != "ALPHA"),
        "default all-board search inspected a retired board"
    );
    let rebuilt_default = fixture.ok_json(
        &fixture.root,
        &["search-rebuild", "--all-boards", "--as", "geoyws", "--json"],
    );
    assert!(
        rebuilt_default["reports"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["board"] != "ALPHA"),
        "default all-board search rebuild inspected a retired board"
    );
    let search_all = fixture.ok_json(
        &fixture.root,
        &[
            "search",
            "Retired needle 77",
            "--all-boards",
            "--all",
            "--json",
        ],
    );
    let search_alpha = search_all["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["board"] == "ALPHA")
        .expect("archived ALPHA search result");
    assert_eq!(search_alpha["board"], "ALPHA");

    let denied_name = fixture.run(
        &beta,
        &[
            "task",
            "add",
            "Blocked by retirement",
            "--id",
            "t-retired-write",
            "--project",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !denied_name.status.success(),
        "a retired board name was still writable"
    );
    assert!(
        String::from_utf8_lossy(&denied_name.stderr).contains(retirement_note),
        "{}",
        String::from_utf8_lossy(&denied_name.stderr)
    );
    let denied_name_stderr = String::from_utf8_lossy(&denied_name.stderr);
    assert!(denied_name_stderr.contains("ALPHA"), "{denied_name_stderr}");
    assert!(refusal_object(&denied_name).contains("ALPHA"));
    let beta_tasks = fixture.ok_json(&beta, &["task", "list", "--json"]);
    assert_eq!(beta_tasks.as_array().unwrap().len(), 1);
    assert_eq!(beta_tasks[0]["id"], "t-retired-77");
    assert_eq!(beta_tasks[0]["title"], "BETA wrong-board sentinel");

    let denied_root = fixture.run(&alpha_nested, &["task", "show", "t-retired-77", "--json"]);
    assert!(
        !denied_root.status.success(),
        "a retired root still resolved"
    );
    assert!(
        String::from_utf8_lossy(&denied_root.stderr).contains(retirement_note),
        "{}",
        String::from_utf8_lossy(&denied_root.stderr)
    );
    let denied_root_stderr = String::from_utf8_lossy(&denied_root.stderr);
    assert!(denied_root_stderr.contains("ALPHA"), "{denied_root_stderr}");
    assert!(refusal_object(&denied_root).contains("ALPHA"));

    fixture.ok_json(
        &beta,
        &[
            "workspace",
            "attach",
            "--to",
            "BETA",
            "--workspace",
            &alpha_root,
            "--json",
        ],
    );
    let conflict = fixture.run(
        &fixture.root,
        &["workspace", "unretire", "ALPHA", "--as", "geoyws", "--json"],
    );
    assert!(
        !conflict.status.success(),
        "a conflicting unretire was accepted"
    );
    let conflict_stderr = String::from_utf8_lossy(&conflict.stderr).into_owned();
    assert!(
        conflict_stderr.contains("cannot be unretired"),
        "{conflict_stderr}"
    );
    let failed_unretire_events = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_unretired",
            "--json",
        ],
    );
    assert!(
        failed_unretire_events.as_array().unwrap().is_empty(),
        "a failed unretire wrote an audit event"
    );

    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &alpha_root,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let restored = fixture.ok_json(
        &fixture.root,
        &["workspace", "unretire", "ALPHA", "--as", "geoyws", "--json"],
    );
    assert_eq!(restored["name"], "ALPHA");
    assert_eq!(restored["workspaceRoots"], json!([alpha_root.clone()]));
    assert!(restored.get("archivedAt").is_none());
    assert!(restored.get("archivedBy").is_none());
    assert!(restored.get("archivedNote").is_none());

    let restored_list = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        restored_list
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "ALPHA" && row["archived"] == false),
        "unretire did not restore the default workspace list"
    );
    let restored_search = fixture.ok_json(
        &fixture.root,
        &["search", "Retired needle 77", "--all-boards", "--json"],
    );
    assert!(
        restored_search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["board"] == "ALPHA" && row["archived"] == false),
        "unretire did not restore all-board search access"
    );
    assert_eq!(
        fixture.ok_json(&alpha_nested, &["task", "show", "t-retired-77", "--json"])["id"],
        "t-retired-77",
        "unretire did not restore the retired workspace path"
    );

    let retired_events = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_retired",
            "--json",
        ],
    );
    assert_eq!(retired_events.as_array().unwrap().len(), 1);
    assert_eq!(retired_events[0]["actor"], "geoyws");
    assert!(
        !retired_events[0]["payload"]["retirementId"]
            .as_str()
            .expect("retirement id")
            .is_empty()
    );
    assert_eq!(
        retired_events[0]["payload"]["archivedNote"],
        retirement_note
    );
    let unretired_events = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_unretired",
            "--json",
        ],
    );
    assert_eq!(unretired_events.as_array().unwrap().len(), 1);
    assert_eq!(unretired_events[0]["actor"], "geoyws");
    assert_eq!(
        retired_events[0]["payload"]["retirementId"],
        unretired_events[0]["payload"]["retirementId"]
    );
    assert_eq!(
        unretired_events[0]["payload"]["restoredRoots"],
        json!([alpha_root])
    );
}

#[test]
fn retired_direct_db_refuses_when_registry_is_corrupt_or_stale_but_external_db_without_registry_still_works()
 {
    let fixture = Fixture::new("retired-direct-db-boundary");
    let managed = fixture.root.join("managed");
    let workspace = managed.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let created = fixture.ok_json(&workspace, &["init", "--name", "ALPHA", "--json"]);
    assert_eq!(created["name"], "ALPHA");
    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ALPHA",
            "--as",
            "geoyws",
            "--note",
            "moved-to-hig",
            "--json",
        ],
    );
    let retired_path = retired["boardPath"].as_str().unwrap().to_owned();
    let registry_source = fixture.data.join("registry.db");

    let corrupt_root = fixture.root.join("corrupt-data");
    fs::create_dir_all(&corrupt_root).unwrap();
    let corrupt_registry = corrupt_root.join("registry.db");
    fs::copy(&registry_source, &corrupt_registry).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&corrupt_registry)
        .unwrap()
        .set_len(32)
        .unwrap();

    let corrupt_flag = fixture
        .command_with_data_dir(&fixture.root, &corrupt_root)
        .args(["task", "list", "--db", &retired_path, "--json"])
        .output()
        .unwrap();
    assert!(
        !corrupt_flag.status.success(),
        "a corrupt registry still allowed a retired direct board path"
    );

    let corrupt_env = fixture
        .command_with_data_dir(&fixture.root, &corrupt_root)
        .env("KANBAN_DB", &retired_path)
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        !corrupt_env.status.success(),
        "a corrupt registry still allowed KANBAN_DB on a retired board"
    );

    let stale_root = fixture.root.join("stale-data");
    fs::create_dir_all(&stale_root).unwrap();
    let stale_registry = stale_root.join("registry.db");
    fs::copy(&registry_source, &stale_registry).unwrap();
    let connection = Connection::open(&stale_registry).unwrap();
    connection.execute_batch("PRAGMA user_version=11;").unwrap();

    let stale_flag = fixture
        .command_with_data_dir(&fixture.root, &stale_root)
        .args(["task", "list", "--db", &retired_path, "--json"])
        .output()
        .unwrap();
    assert!(
        !stale_flag.status.success(),
        "a stale registry still allowed a retired direct board path"
    );

    let stale_env = fixture
        .command_with_data_dir(&fixture.root, &stale_root)
        .env("KANBAN_DB", &retired_path)
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        !stale_env.status.success(),
        "a stale registry still allowed KANBAN_DB on a retired board"
    );

    let external_root = fixture.root.join("external-data");
    fs::create_dir_all(&external_root).unwrap();
    let external_db = fixture.root.join("external.db");
    let external_added = fixture
        .command_with_data_dir(&fixture.root, &external_root)
        .args([
            "task",
            "add",
            "External control task",
            "--id",
            "t-external",
            "--db",
            external_db.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        external_added.status.success(),
        "a truly external direct board file should stay usable without a registry"
    );
    let external_added_json: Value = serde_json::from_slice(&external_added.stdout).unwrap();
    assert_eq!(external_added_json["id"], "t-external");

    let external_list = fixture
        .command_with_data_dir(&fixture.root, &external_root)
        .args([
            "task",
            "list",
            "--db",
            external_db.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        external_list.status.success(),
        "a truly external direct board file should stay usable without a registry"
    );
    let external_list_json: Value = serde_json::from_slice(&external_list.stdout).unwrap();
    assert!(
        external_list_json
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "t-external"),
        "the external control board lost its task after a successful list"
    );
}

#[test]
fn serve_hides_retired_boards_from_the_board_index_and_board_route() {
    let fixture = Fixture::new("serve-retired-board");
    let active = fixture.root.join("active");
    let retired = fixture.root.join("retired");
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&retired).unwrap();

    fixture.ok_json(&active, &["init", "--name", "ACTIVE", "--json"]);
    fixture.ok_json(&retired, &["init", "--name", "RETIRED", "--json"]);
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "RETIRED",
            "--as",
            "geoyws",
            "--note",
            "retire served board",
            "--json",
        ],
    );

    let server = spawn_server(&fixture);
    let port = server.port;

    let (status, boards) = http_get(port, "/boards");
    assert_eq!(status, 200, "{boards}");
    assert!(
        boards.contains("ACTIVE"),
        "the active board disappeared from the board index: {boards}"
    );
    assert!(
        !boards.contains("RETIRED"),
        "the retired board leaked into the board index: {boards}"
    );

    let (status, retired_page) = http_get(port, "/board/RETIRED");
    assert_eq!(status, 500, "{retired_page}");
    assert!(
        retired_page.contains("retire served board"),
        "{retired_page}"
    );
}

#[test]
fn a_listing_says_whether_each_task_is_held_and_by_whom() {
    // Measured 2026-09-04 across eight boards: `task list --status
    // in_progress` carried no claim key at all, so 32 leased tasks read as
    // free and "no such claim exists anywhere" was reported twice before
    // `task show` on the same ids turned up live leases. A listing that
    // structurally cannot show a lease sits on the surface an agent uses to
    // decide whether work is free to take, and its absence renders as an
    // answer. Every row now says whether it is held; --with-claims says by
    // whom, in the shape `task show` already emits.
    let fixture = Fixture::new("listed-claims");
    fixture.ok_json(&fixture.main, &["init", "--name", "HELD", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Held", "--id", "t-held", "--json"],
    );
    // The free task carries an assignee, which is an inviting wrong answer
    // to "who holds it": on the px board a task read assignee=driver-3 while
    // the lease sat elsewhere. Assignee is intent; the claim is possession.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Wanted",
            "--id",
            "t-free",
            "--assignee",
            "driver-3",
            "--json",
        ],
    );
    let lease = fixture.ok_json(
        &fixture.main,
        &["claim", "t-held", "--as", "driver-2", "--json"],
    );
    assert_eq!(lease["agentID"], "driver-2");

    let row = |rows: &Value, id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} is not in {rows}"))
            .clone()
    };

    // The question as it is naturally asked: which in-progress work is held?
    let in_progress = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--status", "in_progress", "--json"],
    );
    let held = row(&in_progress, "t-held");
    assert_eq!(held["claimed"], true, "{held}");
    assert!(
        held.get("claim").is_none(),
        "the full summary is opt-in through --with-claims: {held}"
    );

    let with_claims = fixture.ok_json(&fixture.main, &["task", "list", "--with-claims", "--json"]);
    let held = row(&with_claims, "t-held");
    assert_eq!(held["claimed"], true, "{held}");
    assert_eq!(held["claim"]["agentID"], "driver-2", "{held}");
    assert_eq!(held["claim"]["taskID"], "t-held", "{held}");
    assert_eq!(held["claim"]["expiresAt"], lease["expiresAt"], "{held}");
    assert_eq!(held["claim"]["claimedAt"], lease["claimedAt"], "{held}");
    assert!(
        held["claim"].get("leaseToken").is_none(),
        "a listing never carries the token that authorizes writes: {held}"
    );
    let free = row(&with_claims, "t-free");
    assert_eq!(free["claimed"], false, "{free}");
    assert_eq!(free["claim"], Value::Null, "{free}");
    assert_eq!(
        free["assignee"], "driver-3",
        "assignee is intent and stays its own field: {free}"
    );

    // `claimed` is a row key like any other, so it projects.
    let projected = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--fields", "claimed,id", "--json"],
    );
    let mut seen = Vec::new();
    for row in projected.as_array().unwrap() {
        let keys = row.as_object().unwrap().keys().collect::<Vec<_>>();
        assert_eq!(keys, ["claimed", "id"], "exactly the keys asked for");
        seen.push((
            row["id"].as_str().unwrap().to_owned(),
            row["claimed"].as_bool().unwrap(),
        ));
    }
    seen.sort();
    assert_eq!(
        seen,
        [("t-free".to_owned(), false), ("t-held".to_owned(), true)]
    );

    // The wrong field names for the holder are refused where a name is typed,
    // naming the keys that exist. Reading `claim.actor` off the JSON would
    // yield null on every task including live ones, indistinguishable from
    // "no holder"; the binary cannot refuse a missing-key read, so it refuses
    // the name at the only place it sees one.
    for wrong in ["actor", "claim.actor"] {
        let refused = fixture.run(
            &fixture.main,
            &["task", "list", "--with-claims", "--fields", wrong, "--json"],
        );
        assert!(!refused.status.success(), "--fields {wrong} was accepted");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(wrong), "{stderr}");
        assert!(stderr.contains("claimed, claim"), "{stderr}");
    }
    // Without the flag `claim` is not on the row, and the refusal says which
    // flag puts it there rather than listing keys that omit it (ADR-008).
    let refused = fixture.run(
        &fixture.main,
        &["task", "list", "--fields", "id,claim", "--json"],
    );
    assert!(
        !refused.status.success(),
        "--fields claim was accepted without --with-claims"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--with-claims"), "{stderr}");

    // Releasing the lease is visible on the next listing; nothing is cached.
    let released = fixture.run(
        &fixture.main,
        &[
            "release",
            "t-held",
            "--lease",
            lease["leaseToken"].as_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        released.status.success(),
        "{}",
        String::from_utf8_lossy(&released.stderr)
    );
    let after = fixture.ok_json(&fixture.main, &["task", "list", "--with-claims", "--json"]);
    let held = row(&after, "t-held");
    assert_eq!(held["claimed"], false, "{held}");
    assert_eq!(held["claim"], Value::Null, "{held}");

    // --help is where a caller who never read the docs learns the flag, and
    // the one place to say which key is the holder and that assignee is not.
    let help = fixture.run(&fixture.main, &["task", "list", "--help"]);
    let usage = String::from_utf8_lossy(&help.stdout);
    assert!(usage.contains("--with-claims"), "{usage}");
    assert!(usage.contains("claim.agentID"), "{usage}");
}
