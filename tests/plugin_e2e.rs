//! Process-boundary proof of slice PLUGIN (docs/specs/plugin.md): every case
//! runs the compiled `kanban` binary against a real data root and a real
//! plugin executable, and reads only what crosses the process boundary.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const SECRET_VALUE: &str = "plugin-secret-value-7f3a";
const SECRET_SOURCE: &str = "ACME_SOURCE_TOKEN";

/// The fixture plugin. Its environment is cleared down to the configured
/// secret, so it uses shell builtins and absolute paths only. `$1` selects
/// the behaviour, `$2` is where it leaves its start marker and the request.
const PLUGIN_SCRIPT: &str = r#"#!/bin/sh
mode="$1"
dir="$2"
: > "$dir/started"
IFS= read -r request || true
printf '%s' "$request" > "$dir/request.json"
case "$mode" in
  ok) printf '%s\n' '{"protocolVersion":1,"revision":"r1","output":{"b":2,"a":[1,{"d":4,"c":3}]}}' ;;
  r2) printf '%s' '{"protocolVersion":1,"revision":"r2","output":{}}' ;;
  trailing) printf '%s' '{"protocolVersion":1,"revision":"r1","output":{}} x' ;;
  exit3) exit 3 ;;
  sleep) exec /bin/sleep 30 ;;
  secret) printf '{"protocolVersion":1,"revision":"r1","output":{"seen":"%s"}}' "$ACME_TOKEN" ;;
esac
"#;

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    project: PathBuf,
    board: PathBuf,
    plugin: PathBuf,
    scratch: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-plugin-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let project = root.join("project");
        let scratch = root.join("scratch");
        for dir in [&root, &data, &project, &scratch] {
            fs::create_dir_all(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let plugin = root.join("acme-plugin.sh");
        fs::write(&plugin, PLUGIN_SCRIPT).unwrap();
        fs::set_permissions(&plugin, fs::Permissions::from_mode(0o755)).unwrap();
        let mut fixture = Self {
            root,
            data,
            project,
            board: PathBuf::new(),
            plugin,
            scratch,
        };
        let initialized = fixture.ok_json(&["init", "--name", "PLUGIN-E2E", "--json"]);
        fixture.board = PathBuf::from(initialized["boardPath"].as_str().unwrap());
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(&self.project)
            .env("KANBAN_DATA_DIR", &self.data)
            .env(SECRET_SOURCE, SECRET_VALUE)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn ok_json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Run a command that must be refused, and return its `--json` refusal
    /// sentence after checking stdout holds that object and nothing else.
    fn refusal(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(!output.status.success(), "{args:?} was accepted");
        let stdout: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?} stdout is not one JSON object ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        let object = stdout.as_object().unwrap();
        assert_eq!(object.len(), 1, "refusal carries more than error: {stdout}");
        stdout["error"].as_str().unwrap().to_owned()
    }

    fn plugin_sha256(&self) -> String {
        format!("{:x}", Sha256::digest(fs::read(&self.plugin).unwrap()))
    }

    fn write_config(&self, config: &Value) {
        let path = self.data.join("dispatchers.json");
        fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// The A3 configuration: consumer `acme` with plugin action `lookup` in
    /// `mode`, and a delivery action `notify`.
    fn plugin_config(&self, mode: &str, capabilities: &[&str], timeout_ms: i64) -> Value {
        json!({
            "version": 2,
            "consumers": {
                "acme": {
                    "capabilities": capabilities,
                    "secrets": {
                        "acme-token": {"sourceEnv": SECRET_SOURCE, "targetEnv": "ACME_TOKEN"}
                    },
                    "actions": {
                        "lookup": {
                            "kind": "plugin",
                            "capability": "plugin.read",
                            "executable": self.plugin.to_str().unwrap(),
                            "args": [mode, self.scratch.to_str().unwrap()],
                            "secret": "acme-token",
                            "revision": "r1",
                            "sha256": self.plugin_sha256(),
                            "timeoutMs": timeout_ms
                        },
                        "notify": {
                            "capability": "deliver",
                            "executable": self.plugin.to_str().unwrap(),
                            "args": []
                        }
                    }
                }
            }
        })
    }

    fn configure(&self, mode: &str) {
        self.write_config(&self.plugin_config(mode, &["plugin.read", "deliver"], 30_000));
    }

    fn started(&self) -> bool {
        self.scratch.join("started").exists()
    }

    fn clear_marker(&self) {
        let _ = fs::remove_file(self.scratch.join("started"));
    }

    /// Every byte Kanban owns on disk — the board and everything under the
    /// data root (registry, locks, journals) — as one digest. Excludes only
    /// the operator's `dispatchers.json` and SQLite's shared-memory files.
    fn state_digest(&self) -> String {
        fn collect(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        collect(&self.data, &mut files);
        for suffix in ["", "-wal"] {
            files.push(PathBuf::from(format!("{}{suffix}", self.board.display())));
        }
        files.sort();
        files.dedup();
        let mut hasher = Sha256::new();
        for path in files {
            let name = path.to_string_lossy().into_owned();
            if name.ends_with("dispatchers.json") || name.ends_with("-shm") {
                continue;
            }
            if let Ok(bytes) = fs::read(&path) {
                hasher.update(name.as_bytes());
                hasher.update(&bytes);
            }
        }
        format!("{:x}", hasher.finalize())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn expected_line(sha256: &str) -> String {
    format!(
        r#"{{"action":"lookup","consumer":"acme","output":{{"a":[1,{{"c":3,"d":4}}],"b":2}},"revision":"r1","schemaVersion":1,"sha256":"{sha256}"}}"#
    )
}

/// A1 (PLUGIN-04, PLUGIN-14 is the unit test beside the module): a host with
/// no plugins lists none, runs its board commands, and refuses a call.
#[test]
fn a_host_without_plugins_lists_none_and_refuses_a_call() {
    let fixture = Fixture::new("pluginless");
    assert_eq!(fixture.ok_json(&["plugin", "list", "--json"]), json!([]));
    let added = fixture.ok_json(&["task", "add", "Ordinary work", "--json"]);
    assert_eq!(added["title"], "Ordinary work");
    let watched = fixture.run(&["watch", "--cursor", "0", "--json"]);
    assert!(
        watched.status.success(),
        "watch failed on a pluginless host: {}",
        String::from_utf8_lossy(&watched.stderr)
    );
    assert_eq!(
        fixture.refusal(&["plugin", "call", "acme", "lookup", "--json"]),
        "no plugin acme/lookup is configured"
    );

    // A present file that declares only delivery actions is still "no plugins".
    fixture.write_config(&json!({
        "version": 1,
        "consumers": {"acme": {
            "capabilities": ["deliver"],
            "secrets": {},
            "actions": {"notify": {
                "capability": "deliver",
                "executable": fixture.plugin.to_str().unwrap(),
                "args": []
            }}
        }}
    }));
    assert_eq!(fixture.ok_json(&["plugin", "list", "--json"]), json!([]));
    assert_eq!(
        fixture.refusal(&["plugin", "call", "acme", "lookup", "--json"]),
        "no plugin acme/lookup is configured"
    );
}

/// A2 (PLUGIN-01, PLUGIN-02, PLUGIN-03): a violating file is refused at load,
/// naming the field or the capability rule, and nothing is spawned.
#[test]
fn a_violating_file_is_refused_at_load_and_spawns_nothing() {
    let fixture = Fixture::new("load-rules");
    let mut version_one = fixture.plugin_config("ok", &["plugin.read"], 30_000);
    version_one["version"] = json!(1);
    fixture.write_config(&version_one);
    let refused = fixture.refusal(&["plugin", "list", "--json"]);
    assert!(
        refused.contains("action acme/lookup: field kind needs dispatcher config version 2"),
        "{refused}"
    );

    let mut bad_hash = fixture.plugin_config("ok", &["plugin.read"], 30_000);
    bad_hash["consumers"]["acme"]["actions"]["lookup"]["sha256"] = json!("ABC");
    fixture.write_config(&bad_hash);
    let refused = fixture.refusal(&["plugin", "list", "--json"]);
    assert!(
        refused.contains("action acme/lookup: field sha256"),
        "{refused}"
    );
    let refused = fixture.refusal(&["plugin", "call", "acme", "lookup", "--json"]);
    assert!(refused.contains("field sha256"), "{refused}");

    let mut wrong_capability = fixture.plugin_config("ok", &["plugin.read", "deliver"], 30_000);
    wrong_capability["consumers"]["acme"]["actions"]["lookup"]["capability"] = json!("deliver");
    fixture.write_config(&wrong_capability);
    let refused = fixture.refusal(&["plugin", "list", "--json"]);
    assert!(
        refused.contains("plugin action acme/lookup must use capability plugin.read"),
        "{refused}"
    );

    let mut delivery_with_pin = fixture.plugin_config("ok", &["plugin.read", "deliver"], 30_000);
    delivery_with_pin["consumers"]["acme"]["actions"]["notify"]["revision"] = json!("r1");
    fixture.write_config(&delivery_with_pin);
    let refused = fixture.refusal(&["plugin", "list", "--json"]);
    assert!(
        refused.contains("action acme/notify: field revision is allowed only on a plugin action"),
        "{refused}"
    );

    let mut out_of_range = fixture.plugin_config("ok", &["plugin.read"], 30_000);
    out_of_range["consumers"]["acme"]["actions"]["lookup"]["timeoutMs"] = json!(300_001);
    fixture.write_config(&out_of_range);
    let refused = fixture.refusal(&["plugin", "list", "--json"]);
    assert!(refused.contains("field timeoutMs"), "{refused}");

    assert!(
        !fixture.started(),
        "a refused configuration spawned the plugin"
    );
}

/// A3 (PLUGIN-06 .. PLUGIN-11): two calls print the same canonical line, the
/// plugin saw the exact request, and the board did not move.
#[test]
fn a_call_prints_one_canonical_line_and_writes_nothing() {
    let fixture = Fixture::new("canonical");
    fixture.configure("ok");
    let before = fixture.state_digest();
    let mut lines = Vec::new();
    for json_flag in [true, false] {
        let mut args = vec![
            "plugin",
            "call",
            "acme",
            "lookup",
            "--input-json",
            r#"{"q":1}"#,
        ];
        if json_flag {
            args.push("--json");
        }
        let output = fixture.run(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        lines.push(String::from_utf8(output.stdout).unwrap());
    }
    let expected = format!("{}\n", expected_line(&fixture.plugin_sha256()));
    assert_eq!(lines[0], expected);
    assert_eq!(lines[1], expected);

    let request: Value =
        serde_json::from_slice(&fs::read(fixture.scratch.join("request.json")).unwrap()).unwrap();
    assert_eq!(
        request,
        json!({
            "protocolVersion": 1,
            "target": {"consumerID": "acme", "actionID": "lookup"},
            "revision": "r1",
            "input": {"q": 1},
        })
    );
    assert_eq!(
        fixture.state_digest(),
        before,
        "plugin call wrote to the board or the data root"
    );

    // The input must be one object, refused before the plugin is reached.
    fixture.clear_marker();
    assert_eq!(
        fixture.refusal(&[
            "plugin",
            "call",
            "acme",
            "lookup",
            "--input-json",
            "[1]",
            "--json"
        ]),
        "plugin input must be a JSON object"
    );
    assert!(!fixture.started());

    // The secret reaches the plugin, and only as the configured target name.
    fixture.configure("secret");
    let output = fixture.run(&["plugin", "call", "acme", "lookup", "--json"]);
    assert!(output.status.success());
    let line: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(line["output"]["seen"], SECRET_VALUE);
}

/// PLUGIN-12: one row per plugin action, sorted by consumer then action, and
/// delivery actions are not listed.
#[test]
fn the_listing_is_sorted_by_consumer_then_action() {
    let fixture = Fixture::new("sorted");
    let mut config = fixture.plugin_config("ok", &["plugin.read", "deliver"], 30_000);
    let lookup = config["consumers"]["acme"]["actions"]["lookup"].clone();
    config["consumers"]["acme"]["actions"]["alpha"] = lookup.clone();
    let mut zeta = config["consumers"]["acme"].clone();
    zeta["actions"] = json!({"lookup": lookup});
    config["consumers"]["zeta"] = zeta;
    fixture.write_config(&config);
    let listed = fixture.ok_json(&["plugin", "list", "--json"]);
    let order: Vec<(String, String)> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["consumer"].as_str().unwrap().to_owned(),
                row["action"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [("acme", "alpha"), ("acme", "lookup"), ("zeta", "lookup")]
            .map(|(c, a)| (c.to_owned(), a.to_owned()))
            .to_vec()
    );
}

/// A4 (PLUGIN-07, PLUGIN-12, PLUGIN-15): each failed recheck refuses the call
/// before the plugin runs, the listing shows the same reason, and no output
/// carries the secret's value or its source variable name.
#[test]
fn every_call_rechecks_pin_grant_and_secret_before_it_spawns() {
    let fixture = Fixture::new("recheck");
    fixture.configure("ok");
    let pinned = fixture.plugin_sha256();
    let listed = fixture.ok_json(&["plugin", "list", "--json"]);
    assert_eq!(
        listed,
        json!([{
            "consumer": "acme",
            "action": "lookup",
            "capability": "plugin.read",
            "revision": "r1",
            "sha256": pinned,
            "executable": fixture.plugin.to_str().unwrap(),
            "available": true,
            "reason": null,
        }])
    );

    let check = |expected: &str| {
        fixture.clear_marker();
        let call = fixture.run(&["plugin", "call", "acme", "lookup", "--json"]);
        let list = fixture.run(&["plugin", "list", "--json"]);
        for output in [&call, &list] {
            for stream in [&output.stdout, &output.stderr] {
                let text = String::from_utf8_lossy(stream);
                assert!(!text.contains(SECRET_VALUE), "secret value leaked: {text}");
                assert!(
                    !text.contains(SECRET_SOURCE),
                    "source variable leaked: {text}"
                );
            }
        }
        assert!(!call.status.success());
        let refusal: Value = serde_json::from_slice(&call.stdout).unwrap();
        assert_eq!(refusal["error"], expected);
        assert!(!fixture.started(), "the plugin ran despite: {expected}");
        let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
        assert_eq!(listed[0]["available"], false);
        assert_eq!(listed[0]["reason"], expected);
    };

    // One changed byte in the executable.
    let mut changed = fs::read(&fixture.plugin).unwrap();
    changed.extend_from_slice(b"# changed\n");
    fs::write(&fixture.plugin, &changed).unwrap();
    let found = fixture.plugin_sha256();
    check(&format!(
        "plugin acme/lookup is unavailable: executable sha256 {found} does not match pinned {pinned}"
    ));
    fs::write(&fixture.plugin, PLUGIN_SCRIPT).unwrap();

    // The consumer stops declaring plugin.read.
    fixture.write_config(&fixture.plugin_config("ok", &["deliver"], 30_000));
    check(
        "plugin acme/lookup is unavailable: consumer acme does not declare capability plugin.read",
    );

    // The secret's source variable is unset.
    fixture.configure("ok");
    fixture.clear_marker();
    let call = fixture
        .command()
        .env_remove(SECRET_SOURCE)
        .args(["plugin", "call", "acme", "lookup", "--json"])
        .output()
        .unwrap();
    assert!(!call.status.success());
    let refusal: Value = serde_json::from_slice(&call.stdout).unwrap();
    assert_eq!(
        refusal["error"],
        "plugin acme/lookup is unavailable: missing source env for secret acme-token"
    );
    assert!(!fixture.started());
}

/// A5 (PLUGIN-08, PLUGIN-09, PLUGIN-10): a wrong revision, a trailing token,
/// a non-zero exit and a timeout are each refused, and no output is printed.
#[test]
fn a_misbehaving_plugin_is_refused_and_its_output_discarded() {
    let fixture = Fixture::new("responses");
    for (mode, timeout_ms, expected) in [
        (
            "r2",
            30_000,
            "plugin acme/lookup reported revision r2, pinned r1",
        ),
        (
            "trailing",
            30_000,
            "plugin acme/lookup returned an invalid response: trailing characters",
        ),
        ("exit3", 30_000, "plugin acme/lookup failed: adapter_exit"),
        ("sleep", 500, "plugin acme/lookup failed: adapter_timeout"),
    ] {
        fixture.write_config(&fixture.plugin_config(mode, &["plugin.read", "deliver"], timeout_ms));
        let refusal = fixture.refusal(&["plugin", "call", "acme", "lookup", "--json"]);
        assert!(refusal.starts_with(expected), "{mode}: {refusal}");
    }
}

/// A6 (PLUGIN-05): a subscription cannot target a plugin action, and a plugin
/// call cannot target a delivery action.
#[test]
fn plugin_actions_and_subscription_delivery_never_cross() {
    let fixture = Fixture::new("never-cross");
    fixture.configure("ok");
    let refused = fixture.refusal(&[
        "subscription",
        "add",
        "--consumer",
        "acme",
        "--action",
        "lookup",
        "--timeout-ms",
        "1000",
        "--max-retries",
        "0",
        "--rate-per-minute",
        "1",
        "--max-concurrency",
        "1",
        "--as",
        "geoyws",
        "--json",
    ]);
    assert_eq!(
        refused,
        "action acme/lookup is a plugin; subscriptions deliver only to delivery actions"
    );
    assert_eq!(
        fixture.ok_json(&["subscription", "list", "--all", "--json"]),
        json!([])
    );
    assert_eq!(
        fixture.refusal(&["plugin", "call", "acme", "notify", "--json"]),
        "action acme/notify is not a plugin action"
    );
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn ask(&mut self, request: Value) -> Value {
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A7 (PLUGIN-13): both verbs are read-only MCP tools, and a call answers the
/// CLI's line or the CLI's refusal.
#[test]
fn both_verbs_are_read_only_mcp_tools_with_cli_answers() {
    let fixture = Fixture::new("mcp");
    fixture.configure("ok");
    let mut child = fixture
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut session = Session {
        child,
        stdin,
        stdout,
    };
    session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}}
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    for name in ["plugin_call", "plugin_list"] {
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"));
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{name}");
    }

    let called = session.ask(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "plugin_call", "arguments": {
            "consumer": "acme", "action": "lookup", "input-json": r#"{"q":1}"#
        }}
    }));
    assert_eq!(called["result"]["isError"], false, "{called}");
    assert_eq!(
        called["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .trim_end(),
        expected_line(&fixture.plugin_sha256())
    );

    let refused = session.ask(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "plugin_call", "arguments": {"consumer": "acme", "action": "nope"}}
    }));
    assert_eq!(refused["result"]["isError"], true);
    let text = refused["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no plugin acme/nope is configured"), "{text}");
}
