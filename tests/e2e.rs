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

    // (5) Precedence — an explicit --project beats the cwd it is standing in,
    // and an explicit --db beats --project.
    assert_eq!(
        ids(&fixture.ok_json(
            &fixture.main,
            &["task", "list", "--project", "Beta", "--json"]
        )),
        vec!["t-beta".to_owned()]
    );
    assert_eq!(
        ids(&fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "list",
                "--project",
                "Beta",
                "--db",
                &alpha_board,
                "--json"
            ]
        )),
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
