use rusqlite::Connection;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(cwd)
            .env("KANBAN_DATA_DIR", &self.data)
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

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn compiled_binary_persists_across_processes_and_rotates_handoff_lease() {
    let fixture = Fixture::new("handoff");
    fixture.ok_json(&fixture.main, &["init", "--name", "E2E", "--json"]);
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
    assert_eq!(dashboard[0]["workspaceRoots"].as_array().unwrap().len(), 2);
    assert_eq!(dashboard[0]["taskCounts"]["done"], 1);
    assert!(fixture.ok_json(&fixture.main, &["doctor", "--json"])["healthy"] == true);
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
    assert!(compatible.status.success());
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
    fixture.ok_json(&alpha_twin, &["init", "--name", "Alpha", "--json"]);
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
            && ambiguous_message.contains(alpha_twin.canonicalize().unwrap().to_str().unwrap()),
        "{ambiguous_message}"
    );

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
    assert!(
        String::from_utf8_lossy(&fixture.run(&fixture.main, &["version"]).stdout)
            .contains("kanban"),
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
        fixture.ok_json(
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
    }

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
