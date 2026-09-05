//! The ADR-038 compiled-process refusal matrix, as much of it as a macOS host
//! can prove across a real binary boundary.
//!
//! These tests spawn the compiled `kanban` binary and observe three of the nine
//! refusals at the process layer: the managed-mode selector-bypass gate
//! (clause 9 / ADR-033), the direct-`--db` policy isolation (clause 9), and the
//! backward-compatible unmanaged estate (clause 9 + the legacy-unmanaged stamp).
//!
//! The other six refusals — the two Linux-principal abstractions, UID reuse,
//! MCP socket identity, empty-policy bootstrap, least-privilege grant
//! administration, and policy epochs — live in `rust/policy.rs` and
//! `rust/broker.rs`, where they are library-level evidence. None of what runs
//! here establishes Linux `SO_PEERCRED` acceptance; that is a separately named
//! live check on a Linux host.

use rusqlite::Connection;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// The registry schema that introduced `enforcement_state` (ADR-038 clause 9).
/// Below this a registry is legacy-unmanaged, never ambiguous.
const ENFORCEMENT_STATE_SCHEMA: i64 = 14;

/// A scratch estate: a data root (`KANBAN_DATA_DIR`) and a canonical root
/// (`XDG_DATA_HOME`), both under one temp dir that is removed on drop even if
/// the test panics.
struct AccessFixture {
    root: PathBuf,
    data: PathBuf,
    xdg: PathBuf,
    main: PathBuf,
}

impl AccessFixture {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-access-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let xdg = root.join("xdg");
        let main = root.join("main");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&xdg).unwrap();
        fs::create_dir_all(&main).unwrap();
        Self {
            root,
            data,
            xdg,
            main,
        }
    }

    /// The canonical registry path (`$XDG_DATA_HOME/kanban/registry.db`), the
    /// one place the managed-mode gate reads.
    fn canonical_registry(&self) -> PathBuf {
        self.xdg.join("kanban").join("registry.db")
    }

    /// The scratch registry path (`$KANBAN_DATA_DIR/registry.db`), where the
    /// ordinary single-user estate lives.
    fn scratch_registry(&self) -> PathBuf {
        self.data.join("registry.db")
    }

    /// A spawned binary with both roots pinned to this fixture and every
    /// ambient selector default cleared.
    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(cwd)
            .env("KANBAN_DATA_DIR", &self.data)
            .env("XDG_DATA_HOME", &self.xdg)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
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

impl Drop for AccessFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Stamp a canonical registry with the given enforcement state and schema
/// version, enough for the gate's probe (`PRAGMA user_version` + a
/// `enforcement_state` row) and nothing more.
fn write_canonical_registry(path: &Path, schema_version: i64, state: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE enforcement_state (\
                 id INTEGER PRIMARY KEY NOT NULL CHECK(id = 1), \
                 state TEXT NOT NULL CHECK(state IN ('direct','prepared','managed'))\
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO enforcement_state(id,state) VALUES(1,?1)",
            [state],
        )
        .unwrap();
    connection
        .execute_batch(&format!("PRAGMA user_version = {schema_version}"))
        .unwrap();
}

// -- clause 9 / ADR-033: the managed-mode selector-bypass gate ---------------

/// A managed estate refuses every selector bypass by name, from a compiled
/// process. Each is a route around the broker: a typed board selector, a
/// repointed data root, or an environment default. None is a silent downgrade
/// to direct access.
#[test]
fn compiled_binary_refuses_each_selector_bypass_in_managed_mode() {
    let fixture = AccessFixture::new("managed-bypasses");
    write_canonical_registry(
        &fixture.canonical_registry(),
        ENFORCEMENT_STATE_SCHEMA,
        "managed",
    );

    // No ambient data-root override: each leg names exactly the bypass it
    // means to observe, so the refusal names that bypass and not the ambient
    // KANBAN_DATA_DIR this fixture otherwise pins.
    let command = |fixture: &AccessFixture| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(&fixture.main)
            .env("XDG_DATA_HOME", &fixture.xdg)
            .env_remove("KANBAN_DATA_DIR")
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };

    // The three typed-flag bypasses, each refused by name.
    for (flag, value) in [
        ("--db", "/tmp/nowhere.db"),
        ("--workspace", "/tmp/nowhere"),
        ("--project", "Alpha"),
    ] {
        let output = command(&fixture)
            .args(["task", "list", flag, value, "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{flag} was not refused");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("managed mode refuses") && stderr.contains(flag),
            "{flag} refused without naming the bypass: {stderr}"
        );
    }

    // The root-path bypass: KANBAN_DATA_DIR repoints the estate.
    let repointed = command(&fixture)
        .env("KANBAN_DATA_DIR", "/tmp/elsewhere")
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    assert!(!repointed.status.success());
    assert!(
        String::from_utf8_lossy(&repointed.stderr).contains("KANBAN_DATA_DIR"),
        "{}",
        String::from_utf8_lossy(&repointed.stderr)
    );

    // The environment-selector bypass: KANBAN_DB and KANBAN_PROJECT.
    for (key, value) in [("KANBAN_DB", "/tmp/forged.db"), ("KANBAN_PROJECT", "Alpha")] {
        let output = command(&fixture)
            .env(key, value)
            .args(["task", "list", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{key} was not refused");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(key),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// -- clause 9: direct --db is not a policy decision --------------------------

/// A `--db` open in `direct` enforcement writes no policy decision: the
/// registry's append-only journals stay empty. The claim is inspected in the
/// real registry database after the compiled process exits, not inferred from
/// the receipt.
#[test]
fn compiled_binary_direct_db_open_writes_no_policy_decision() {
    let fixture = AccessFixture::new("direct-db-isolation");
    let alpha = fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let board = alpha["boardPath"].as_str().unwrap().to_owned();

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "direct db probe",
            "--db",
            &board,
            "--as",
            "geoyws",
            "--json",
        ],
    );

    let connection = Connection::open(&fixture.scratch_registry()).unwrap();
    let audit_rows: i64 = connection
        .query_row("SELECT count(*) FROM access_audit", [], |row| row.get(0))
        .unwrap();
    let policy_rows: i64 = connection
        .query_row("SELECT count(*) FROM policy_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(audit_rows, 0, "a direct open wrote an access-audit row");
    assert_eq!(policy_rows, 0, "a direct open wrote a policy event");
}

// -- clause 9: backward-compatible unmanaged mode ---------------------------

/// An existing single-user install keeps working unchanged: a registry stamped
/// below `REGISTRY_V14` is legacy-unmanaged, not ambiguous, and the selector
/// gate is a no-op there. The stale `managed` row below is exactly the trap —
/// the schema stamp decides, so a half-migrated estate cannot lock anyone out.
#[test]
fn compiled_binary_legacy_registry_stays_unmanaged_and_keeps_working() {
    let fixture = AccessFixture::new("legacy-unmanaged");
    // A registry stamped below V14, but whose (ignored) enforcement row still
    // says `managed`: the stamp must win, so the estate reads as `direct`.
    write_canonical_registry(
        &fixture.canonical_registry(),
        ENFORCEMENT_STATE_SCHEMA - 1,
        "managed",
    );
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);

    // The very selector the managed gate would refuse is honoured instead.
    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--project", "Alpha", "--json"],
    );
    assert!(
        listed.is_array(),
        "a legacy estate must keep listing boards"
    );
}

/// An absent canonical registry is a fresh install — legitimately unmanaged —
/// so the selector gate is a no-op there too.
#[test]
fn compiled_binary_absent_registry_reads_as_unmanaged() {
    let fixture = AccessFixture::new("absent-unmanaged");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);

    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--project", "Alpha", "--json"],
    );
    assert!(listed.is_array(), "no registry means no managed estate");
}

/// The five reads must ANSWER on a machine that has never run `kanban`, not
/// leak SQLite's `unable to open database file`. `routing::enforcement_state_at`
/// already rules an absent registry an unmanaged `direct` estate, and
/// `access enforcement show` is among the first things an operator runs, so
/// disagreeing there is both a contradiction and a path disclosure.
#[test]
fn compiled_binary_access_reads_answer_before_any_registry_exists() {
    let fixture = AccessFixture::new("reads-before-registry");

    // Deliberately NO `init`: nothing has created a registry.
    let enforcement = fixture.ok_json(&fixture.main, &["access", "enforcement", "show", "--json"]);
    assert_eq!(enforcement["enforcementState"], "direct");
    assert_eq!(enforcement["epoch"], 0);
    assert!(enforcement["journalHead"].is_null());

    assert_eq!(
        fixture.ok_json(&fixture.main, &["access", "principal", "list", "--json"]),
        serde_json::json!([])
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["access", "audit", "--json"]),
        serde_json::json!([])
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &[
                    "access",
                    "principal",
                    "show",
                    "--principal",
                    "p-absent",
                    "--json"
                ],
            )
            .is_null()
    );

    // A denial with no registry says exactly what every other denial says, so
    // the absence of a registry is not itself an oracle.
    let explained = fixture.ok_json(
        &fixture.main,
        &[
            "access",
            "explain",
            "--principal",
            "p-absent",
            "--capability",
            "read",
            "--scope",
            "registry",
            "--json",
        ],
    );
    assert_eq!(explained["outcome"], "denied");
    assert_eq!(explained["denialReason"], "denied or not found");
    assert_eq!(explained["matchedGrantIDs"], serde_json::json!([]));

    // And no registry was conjured by reading.
    assert!(
        !fixture.data.join("registry.db").exists(),
        "a read must not create the registry it reports on"
    );
}
