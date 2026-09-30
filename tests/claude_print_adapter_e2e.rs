use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

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

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    claude: PathBuf,
    capture: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "kanban-claude-adapter-e2e-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir(&cwd).unwrap();
        for p in [&root, &home, &cwd] {
            let mut m = fs::metadata(p).unwrap().permissions();
            m.set_mode(0o700);
            fs::set_permissions(p, m).unwrap();
        }
        let claude = root.join("claude");
        compile_fake(
            "tests/fixtures/claude_print_adapter_fake_claude.rs",
            &claude,
        );
        let capture = root.join("capture.ndjson");
        let _ = fs::remove_file(&capture);
        Self {
            root,
            home,
            cwd,
            claude,
            capture,
        }
    }
    fn run(&self, scenario: &str) -> Output {
        self.run_request(scenario, request())
    }
    fn run_request(&self, scenario: &str, request: Value) -> Output {
        fs::write(self.home.join("scenario.txt"), scenario).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_kanban-claude-print-adapter"));
        c.args(["--claude"])
            .arg(&self.claude)
            .args(["--home"])
            .arg(&self.home)
            .args(["--cwd"])
            .arg(&self.cwd)
            .args(["--required-version", "2.1.236"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn binary_help_and_version_are_exact() {
    let binary = env!("CARGO_BIN_EXE_kanban-claude-print-adapter");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert_eq!(
        String::from_utf8(help.stdout).unwrap(),
        "kanban-claude-print-adapter --claude ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --required-version VERSION\n"
    );
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!(
            "kanban-claude-print-adapter {}\n",
            env!("CARGO_PKG_VERSION")
        )
    );
}

fn request() -> Value {
    json!({"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","attempt":2,"createdAt":1720000000_i64},"target":{"consumerID":"claude.print","actionID":"start-readonly-turn"},"event":{"eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","eventHash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","timestamp":1720000000_i64,"body":"must not reach Claude"}})
}
#[test]
fn compiled_process_contract_and_fail_closed_boundaries() {
    let f = Fixture::new();
    for scenario in ["object", "array"] {
        let o = f.run(scenario);
        assert!(
            o.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(o.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&o.stdout).unwrap(),
            json!({"protocolVersion":1,"subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","createdAt":1720000000_i64,"replay":true})
        );
    }
    let capture = fs::read_to_string(&f.capture).unwrap();
    let records = capture
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 6);
    let expected_env = json!([
        ["HOME", fs::canonicalize(&f.home).unwrap().to_string_lossy()],
        ["PATH", "/usr/bin:/bin"]
    ]);
    for record in &records {
        assert_eq!(record["env"], expected_env);
        assert_eq!(
            record["cwd"],
            fs::canonicalize(&f.cwd).unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(record["stdin"], "");
    }
    assert_eq!(records[0]["argv"], json!(["--version"]));
    assert_eq!(records[1]["argv"], json!(["--help"]));
    let argv = records[2]["argv"].as_array().unwrap();
    assert_eq!(argv,json!(["--safe-mode","--print","Reply with exactly this acknowledgement and nothing else: ACK sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","--output-format","json","--tools","","--disallowedTools","mcp__*","--no-session-persistence","--permission-mode","dontAsk"]).as_array().unwrap());
    assert!(
        !argv
            .iter()
            .any(|v| v.as_str().unwrap().contains("must not reach"))
    );
    for scenario in [
        "api-error",
        "mismatch",
        "tool",
        "overflow",
        "trailing",
        "stderr",
        "nonzero",
    ] {
        let o = f.run(scenario);
        assert!(!o.status.success(), "{scenario} unexpectedly succeeded");
        assert!(o.stdout.is_empty(), "{scenario} leaked adapter response");
    }
    let captures_before_wrong_target = fs::read_to_string(&f.capture).unwrap().lines().count();
    let mut wrong_target = request();
    wrong_target["target"]["consumerID"] = json!("codex.queue");
    wrong_target["target"]["actionID"] = json!("enqueue-turn");
    let output = f.run_request("object", wrong_target);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let capture = fs::read_to_string(&f.capture).unwrap();
    let modes = capture
        .lines()
        .skip(captures_before_wrong_target)
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["mode"].clone())
        .collect::<Vec<_>>();
    assert_eq!(modes, vec![json!("version"), json!("help")]);
}
