use rusqlite::Connection;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

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
