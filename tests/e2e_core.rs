//! Compiled-binary E2E: board subscriptions, search, claims, init, selectors,
//! board creation, permissions, leases and the audit trail.
//!
//! One of the `e2e_*` area targets (t-2aeec40c). Each is serial inside
//! (`--test-threads=1`); `scripts/release-gate.sh` runs the areas side by
//! side because every case owns its fixture under a pid-unique temp root.
//! Helpers more than one area uses live in `tests/e2e_support/`.

// Each area uses only some of the shared helpers and imports.
#[allow(dead_code, unused_imports)]
mod e2e_support;
use e2e_support::*;

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

#[test]
fn compiled_binary_manages_audited_board_local_subscriptions_fail_closed() {
    let fixture = Fixture::new("subscriptions");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SUBSCRIPTIONS", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/pubsub", "--as", "geoyws", "--json"],
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
            "geoyws/pubsub",
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
            "geoyws/pubsub",
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
    assert_eq!(doctor["registrySchemaVersion"], 16);
    assert_eq!(doctor["supportedRegistrySchemaVersion"], 16);
    assert_eq!(doctor["supportedBoardSchemaVersion"], 39);
    assert_eq!(doctor["projects"][0]["schemaVersion"], 39);
    assert_eq!(doctor["projects"][0]["supportedSchemaVersion"], 39);
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
            "geoyws/handoff",
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
            "geoyws/handoff",
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
                "geoyws/handoff",
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
                    .any(|tag| tag == "geoyws/handoff")),
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
fn sprint_search_is_board_scoped_fresh_rebuildable_and_exactly_cited() {
    let fixture = Fixture::new("sprint-search");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SEARCH-SPRINT-A", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "new",
            "Photon release boundary",
            "--id",
            "sp-deadbeef",
            "--body",
            "Deliver the cobalt navigation contract.",
            "--target-version",
            "9.4.7",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "operator",
            "--json",
        ],
    );

    for query in ["Photon release boundary", "cobalt navigation", "9.4.7"] {
        let found = fixture.ok_json(
            &fixture.main,
            &["search", query, "--source", "sprint", "--json"],
        );
        assert_eq!(found["results"][0]["sourceKind"], "sprint", "{found}");
        assert_eq!(found["results"][0]["sourceId"], "sp-deadbeef", "{found}");
        assert_eq!(
            found["results"][0]["citation"], "kanban://SEARCH-SPRINT-A/sprint/sp-deadbeef",
            "{found}"
        );
        assert_eq!(found["results"][0]["tags"], json!([]), "{found}");
    }

    fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "add",
            "geoyws/sp-deadbeef",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Generated ID tag collision",
            "--id",
            "t-collision",
            "--tag",
            "geoyws/sp-deadbeef",
            "--as",
            "operator",
            "--json",
        ],
    );
    let exact = fixture.ok_json(&fixture.main, &["search", "sp-deadbeef", "--json"]);
    assert!(
        exact["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["sourceId"] != "t-collision"),
        "{exact}"
    );
    assert_eq!(exact["results"][0]["sourceKind"], "sprint", "{exact}");

    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-deadbeef",
            "--body",
            "Deliver the cobalt navigation contract.\nAcceptance: exact sprint search stays fresh.",
            "--empty-scope",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "start",
            "sp-deadbeef",
            "--as",
            "operator",
            "--json",
        ],
    );
    let fresh = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "Photon release boundary",
            "--source",
            "sprint",
            "--status",
            "current",
            "--json",
        ],
    );
    assert_eq!(fresh["results"].as_array().unwrap().len(), 1, "{fresh}");
    let stale = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "Photon release boundary",
            "--source",
            "sprint",
            "--status",
            "planned",
            "--json",
        ],
    );
    assert!(stale["results"].as_array().unwrap().is_empty(), "{stale}");

    let beta = fixture.root.join("beta-search-sprint");
    fs::create_dir_all(&beta).unwrap();
    fixture.ok_json(&beta, &["init", "--name", "SEARCH-SPRINT-B", "--json"]);
    fixture.ok_json(
        &beta,
        &[
            "sprint",
            "new",
            "Photon release boundary",
            "--id",
            "sp-feedface",
            "--target-version",
            "10.0.0",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "operator",
            "--json",
        ],
    );
    let local = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "Photon release boundary",
            "--source",
            "sprint",
            "--json",
        ],
    );
    assert!(
        local["results"].as_array().unwrap().iter().all(|row| {
            row["citation"]
                .as_str()
                .unwrap()
                .starts_with("kanban://SEARCH-SPRINT-A/")
        }),
        "{local}"
    );
    let global = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "Photon release boundary",
            "--source",
            "sprint",
            "--all-boards",
            "--json",
        ],
    );
    let citations = global["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["citation"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        citations.contains(&"kanban://SEARCH-SPRINT-A/sprint/sp-deadbeef"),
        "{global}"
    );
    assert!(
        citations.contains(&"kanban://SEARCH-SPRINT-B/sprint/sp-feedface"),
        "{global}"
    );

    let rebuilt = fixture.ok_json(
        &fixture.main,
        &["search-rebuild", "--as", "operator", "--json"],
    );
    assert_eq!(rebuilt["documents"], rebuilt["embedded"], "{rebuilt}");
    let after_rebuild = fixture.ok_json(
        &fixture.main,
        &[
            "search",
            "cobalt navigation",
            "--source",
            "sprint",
            "--json",
        ],
    );
    assert_eq!(
        after_rebuild["results"].as_array().unwrap().len(),
        1,
        "{after_rebuild}"
    );
    assert_eq!(after_rebuild["results"][0]["sourceId"], "sp-deadbeef");
    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    let health = &doctor["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["name"] == "SEARCH-SPRINT-A")
        .unwrap()["searchIndex"];
    assert_eq!(health["healthy"], true, "{doctor}");
    assert_eq!(health["sourceRows"], health["documents"], "{doctor}");
    assert_eq!(health["documents"], health["ftsRows"], "{doctor}");
    assert_eq!(health["missingEmbeddings"], 0, "{doctor}");
}

#[test]
fn v27_board_migrates_sprint_search_with_citation_cache_identity_and_index_health() {
    let fixture = Fixture::new("sprint-search-v27");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SEARCH-SPRINT-V27", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Preserve indexed cache",
            "--id",
            "t-v27-cache",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "new",
            "Migration release",
            "--id",
            "sp-7654abcd",
            "--body",
            "Existing heliotrope sprint knowledge.",
            "--target-version",
            "8.1.0",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "operator",
            "--json",
        ],
    );
    let board = board_path_for_project(&fixture, &fixture.main, "SEARCH-SPRINT-V27");
    let connection = Connection::open(&board).unwrap();
    let cached: (i64, String, String, Vec<u8>) = connection
        .query_row(
            "SELECT seq,source_hash,embedding_model,embedding FROM search_documents \
         WHERE source_kind='task' AND source_id='t-v27-cache'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    remove_v28_sprint_search_schema(&connection);
    connection.execute_batch("PRAGMA user_version=27;").unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM search_documents WHERE source_kind='sprint'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sprints WHERE id='sp-7654abcd'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    drop(connection);

    let found = fixture.ok_json(
        &fixture.main,
        &["search", "heliotrope", "--source", "sprint", "--json"],
    );
    assert_eq!(found["results"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found["results"][0]["sourceId"], "sp-7654abcd");
    assert_eq!(
        found["results"][0]["citation"],
        "kanban://SEARCH-SPRINT-V27/sprint/sp-7654abcd"
    );

    let connection = Connection::open(&board).unwrap();
    let preserved: (i64, String, String, Vec<u8>) = connection
        .query_row(
            "SELECT seq,source_hash,embedding_model,embedding FROM search_documents \
         WHERE source_kind='task' AND source_id='t-v27-cache'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(preserved, cached);
    connection
        .execute(
            "INSERT INTO search_fts(search_fts) VALUES('integrity-check')",
            [],
        )
        .unwrap();
    drop(connection);
    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    let health = &doctor["projects"][0]["searchIndex"];
    assert_eq!(health["healthy"], true, "{doctor}");
    assert_eq!(health["sourceRows"], health["documents"], "{doctor}");
    assert_eq!(health["documents"], health["ftsRows"], "{doctor}");
    assert_eq!(health["missingEmbeddings"], 0, "{doctor}");
}

#[test]
fn compiled_binary_searches_hybrid_knowledge_across_cli_and_boards() {
    let fixture = Fixture::new("rag-search");
    fixture.ok_json(&fixture.main, &["init", "--name", "SEARCH-A", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/release", "--as", "tester", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/ops", "--as", "tester", "--json"],
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
            "geoyws/release",
            "--tag",
            "geoyws/ops",
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
            "geoyws/release",
            "--tag",
            "geoyws/ops",
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
            "geoyws/release",
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
            "geoyws/release",
            "--json",
        ],
    );
    assert_eq!(rule_search["results"][0]["sourceKind"], "rule");
    assert_eq!(rule_search["results"][0]["sourceId"], rule["id"]);
    assert_eq!(
        rule_search["results"][0]["citation"],
        format!("kanban://rules/rule/{}", rule["id"].as_str().unwrap())
    );
    assert_eq!(
        rule_search["results"][0]["tags"],
        json!(["ALL", "geoyws/release"])
    );

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
        &[
            "tag",
            "add",
            "geoyws/sub-deadbeef",
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
            "Tagged collision",
            "--id",
            "t-tagged",
            "--body",
            "No literal match lives here.",
            "--tag",
            "geoyws/sub-deadbeef",
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
        &["tag", "add", "geoyws/release", "--as", "tester", "--json"],
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
            "geoyws/release",
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
    remove_v27_sprint_schema(&connection);
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
    // An ordinary open of a registered board stops at the pre-CROSS schema;
    // the CROSS step is the owner's, taken by `init` (ADR-056 §5).
    assert_eq!(
        reopened
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        38
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

/// The same-name race above, with its losing interleaving pinned rather than
/// left to scheduling. An `init` in flight holds `.init.lock` and the root
/// `.lock` shared while the registry it is creating sits at user_version 0 —
/// on its way to birth, and to a version probe indistinguishable from a
/// pre-CROSS file. A second `init` that probed for a pending CROSS step
/// before serializing on `.init.lock` read that newborn as an upgrade, took
/// the root exclusively against the first one's shared hold, and refused
/// "upgrade is pending ... needs <root> to itself" instead of waiting its
/// turn and reaching the duplicate-name refusal (ADR-008 2026-09-01,
/// ADR-056 §5: new registration keeps shared root plus init lock).
#[test]
fn compiled_binary_init_waits_for_an_in_flight_init_instead_of_seeing_a_pending_upgrade() {
    use std::os::unix::fs::DirBuilderExt;

    let fixture = Fixture::new("init-in-flight");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&fixture.data)
        .unwrap();
    let open_marker = |name: &str| {
        fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(fixture.data.join(name))
            .unwrap()
    };
    let init_lock = open_marker(".init.lock");
    init_lock.lock().unwrap();
    let root_lock = open_marker(".lock");
    root_lock.lock_shared().unwrap();
    // The first thing a fresh registry open writes: the header, at version 0.
    {
        let newborn = Connection::open(fixture.data.join("registry.db")).unwrap();
        newborn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
            .unwrap();
    }

    let mut second = fixture
        .command(&fixture.main)
        .args(["init", "--name", "SAME", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Before the fix the second init refused at once. Waiting on the first
    // init's `.init.lock` (15s budget), it cannot exit while that is held.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if second.try_wait().unwrap().is_some() {
            let output = second.wait_with_output().unwrap();
            panic!(
                "init did not wait for the in-flight init; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(root_lock);
    drop(init_lock);

    let output = second.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row("SELECT count(*) FROM boards WHERE name='SAME'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
}

/// The race above with a NON-init command on the other side, pinned. Any
/// command on an empty data root creates the registry, holding only the root
/// `.lock` shared — never `.init.lock`, so serializing inits on it does not
/// help. Its birth transaction is in flight: the file stands at user_version
/// 0, write-locked. Stepped one commit at a time, that birth also passed
/// through every pre-CROSS version. An `init` probing for a pending CROSS
/// step read the newborn as a legacy registry, took the root exclusively
/// against the command's shared hold, and refused "upgrade is pending ...
/// needs <root> to itself". A newborn is born whole, so 0 is an unfinished
/// birth and nothing is pending: `init` takes the root shared and waits on
/// the birth instead (ADR-056 §5, fresh files are born CROSS-aware).
#[test]
fn compiled_binary_init_waits_for_a_registry_another_command_is_creating() {
    use std::os::unix::fs::DirBuilderExt;

    let fixture = Fixture::new("init-newborn-registry");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&fixture.data)
        .unwrap();
    let root_lock = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fixture.data.join(".lock"))
        .unwrap();
    root_lock.lock_shared().unwrap();
    // The creating command's open, at version 0, with its one birth
    // transaction begun and not yet committed.
    let newborn = Connection::open(fixture.data.join("registry.db")).unwrap();
    newborn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
        .unwrap();
    newborn.execute_batch("BEGIN IMMEDIATE").unwrap();

    let mut init = fixture
        .command(&fixture.main)
        .args(["init", "--name", "Newborn", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Before the fix init refused at once. Waiting on the birth transaction
    // (15s busy budget), it cannot exit while that is held.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if init.try_wait().unwrap().is_some() {
            let output = init.wait_with_output().unwrap();
            panic!(
                "init did not wait for the registry being born; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    newborn.execute_batch("ROLLBACK").unwrap();
    drop(newborn);
    drop(root_lock);

    let output = init.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        16,
        "the registry init created must be born CROSS-aware"
    );
    assert_eq!(
        registry
            .query_row(
                "SELECT count(*) FROM boards WHERE name='Newborn' AND registration_token IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
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
    for tag in ["geoyws/claims", "geoyws/other"] {
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
    }
    let add = |id: &str, extra: &[&str]| {
        let mut args = vec!["task", "add", id, "--id", id, "--tag", "geoyws/claims"];
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
            "geoyws/other",
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
            "geoyws/claims",
            "--limit",
            "10",
            "--json",
        ],
    );
    assert_eq!(candidates.as_array().unwrap().len(), 2);
    assert_eq!(candidates[0]["id"], "t-ready-first");
    assert_eq!(candidates[1]["id"], "t-base");
    assert_eq!(candidates[0]["tags"], json!(["geoyws/claims"]));
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
            args.extend([flag.as_str(), value]);
            // `batch` publishes no positionals but still needs its item list
            // (docs/specs/batch.md BA-01): one valid read item, so the run
            // proves the selector was honoured instead of refused.
            if command == "batch" {
                args.extend(["--items", r#"[{"name":"tag_list","arguments":{}}]"#]);
            }
            args.push("--json");
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
    let connection = Connection::open(&board).unwrap();
    remove_v27_sprint_schema(&connection);
    connection.execute_batch("PRAGMA user_version=26;").unwrap();
    drop(connection);

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
    // An ordinary open of a registered board migrates forward only to the
    // pre-CROSS schema; the CROSS step is the owner's, taken by `init`
    // (ADR-056 §5). `current` above is 39 because `init` took it.
    assert_eq!(after, 38, "the board was not migrated forward");
    let rewound = Connection::open(&board).unwrap();
    rewound
        .pragma_update(None, "user_version", after - 1)
        .unwrap();
    drop(rewound);
    let listed_after_last_step = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--db", board.to_str().unwrap(), "--json"],
    );
    assert_eq!(listed_after_last_step[0]["id"], "t-old");
    let after_last_step: i64 = Connection::open(&board)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        after_last_step, after,
        "the final migration step did not rerun"
    );
}

/// Whether mode 0400 actually stops THIS process from writing.
///
/// The question has to be asked rather than assumed, because it is answered by
/// the caller's uid: an ordinary user is denied by the kernel, and root ignores
/// the mode bits entirely. Asked by probe rather than by `geteuid`, so the
/// answer covers a read-only mount and an ACL too, and so it is the exact
/// question the tests below care about instead of a proxy for it.
fn mode_0400_denies_writes(dir: &Path) -> bool {
    let probe = dir.join("mode-probe");
    fs::write(&probe, b"probe").unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o400)).unwrap();
    let denied = fs::OpenOptions::new().write(true).open(&probe).is_err();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&probe).unwrap();
    denied
}

/// A board file and a `registry.db` this process cannot write, with the rows
/// every read in `docs/testing/bench/fixture-frozen.json` needs.
struct SealedEstate {
    fixture: Fixture,
    board: PathBuf,
    registry: PathBuf,
    deployment: String,
    /// `doctor --json`, taken while the estate is still writable and BEFORE
    /// the lapsed lease below exists. Read for the two schema versions, which
    /// decide which open a read takes. It cannot be taken later: `doctor` is a
    /// whole-estate survey that opens every board writably, so running it
    /// after the lapse would sweep the very row these tests need present.
    doctor: Value,
}

impl SealedEstate {
    fn new(label: &str) -> Self {
        let fixture = Fixture::new(label);
        fixture.ok_json(&fixture.main, &["init", "--name", "SEALED", "--json"]);
        fixture.ok_json(&fixture.main, &["tag", "add", "geoyws/infra", "--json"]);
        for id in ["t-read-1", "t-read-2"] {
            fixture.ok_json(
                &fixture.main,
                &[
                    "task",
                    "add",
                    "Readable work",
                    "--id",
                    id,
                    "--tag",
                    "geoyws/infra",
                    "--json",
                ],
            );
        }
        fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "raise",
                "Needs a decision",
                "--as",
                "geoyws",
                "--task",
                "t-read-1",
                "--json",
            ],
        );
        fixture.ok_json(
            &fixture.main,
            &[
                "sitrep",
                "post",
                "Lane is moving",
                "--as",
                "bench@driver",
                "--lane",
                "driver",
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
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "--tier",
                "@_s",
                "--environment",
                "staging",
                "--host",
                "hax",
                "--url",
                "https://kb.geoy.ws",
                "--as",
                "bench@driver",
                "--json",
            ],
        )["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let board = board_path_for_project(&fixture, &fixture.main, "SEALED");
        let registry = fixture.data.join("registry.db");
        let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);

        // LAST, and the reason every test here is a real guard: a lease on
        // `t-read-2` that has run out and that no sweep has tidied. Without it
        // `sweep_expired_claims` short-circuits on its `COUNT(*)` probe and
        // writes nothing, so a read path that swept would still pass every
        // assertion below. With it, a sweep on the read path has to open a
        // write transaction against a file that will not take one.
        fixture.ok_json(
            &fixture.main,
            &[
                "claim",
                "t-read-2",
                "--as",
                "ghost",
                "--session",
                "ghost-session",
                "--json",
            ],
        );
        Connection::open(&board)
            .unwrap()
            .execute("UPDATE task_claims SET expires_at=1", [])
            .unwrap();

        Self {
            fixture,
            board,
            registry,
            deployment,
            doctor,
        }
    }

    /// Every path a `chmod` has to reach: the database file and any WAL
    /// sidecar beside it. SQLite creates `-wal` and `-shm` with the main
    /// file's mode, so a sidecar left at 0400 keeps the database unwritable
    /// after the main file is opened up again — which is a property of SQLite,
    /// not of the code under test, and would otherwise read as a failure of it.
    fn sealable(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for base in [&self.board, &self.registry] {
            paths.push(base.clone());
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", base.display()));
                if sidecar.exists() {
                    paths.push(sidecar);
                }
            }
        }
        paths
    }

    /// Take away every write bit on the database files, and nothing else. The
    /// directory above them stays writable, because the data root is where the
    /// process lock lives and taking that away tests a different thing.
    fn seal(&self) {
        for path in self.sealable() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// `chmod` on ONE file, which is what an operator sealing a board types.
    /// Deliberately not [`SealedEstate::seal`]: the sidecars stay writable, so
    /// the mode of the file under test is the only thing that can decide
    /// whether a second write lands.
    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn unseal(&self) {
        for path in self.sealable() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

/// Every path the ownership rule could reach, and what it looks like now.
///
/// `databases` are swept with their WAL sidecars; `plain` are files that have
/// none — the data root's `.lock` and `.init.lock`, which every command opens
/// before it reaches SQLite.
///
/// A sidecar that is not there between commands is not a gap: SQLite removes
/// `-wal` and `-shm` when the last connection closes cleanly, so what exists
/// depends on what else has the board open. The record is therefore of what
/// is on disk, keyed by path, and the assertions below compare like with like.
fn ownership_snapshot(databases: &[&Path], plain: &[&Path]) -> BTreeMap<PathBuf, (u32, u32, u32)> {
    let mut seen = BTreeMap::new();
    let mut record = |path: PathBuf| {
        if let Ok(metadata) = fs::metadata(&path) {
            seen.insert(
                path,
                (
                    metadata.uid(),
                    metadata.gid(),
                    metadata.permissions().mode() & 0o777,
                ),
            );
        }
    };
    for base in databases {
        for suffix in ["", "-wal", "-shm"] {
            record(PathBuf::from(format!("{}{suffix}", base.display())));
        }
    }
    for path in plain {
        record(PathBuf::from(path));
    }
    seen
}

/// A run that is not root leaves every ownership bit exactly as it found it.
///
/// The change this guards makes a ROOT cli hand the files it touches to the
/// owner of the directory they live in. kb.geoy.ws answered 500 on every
/// route on the morning of 2026-09-08 because one board was `root:root 0600`
/// while the web service runs as `kanban`: the board had been created by a
/// root `kb init` over ssh, and a later root read left root-owned `-wal` and
/// `-shm` beside it. The data root's `.lock` and `.init.lock` are in here for
/// the same reason and cost a second outage to find: mirroring only the
/// databases left the marker every command opens before SQLite at
/// `root:root`, and the service met `open lock file <root>/.lock: Permission
/// denied` instead of a 500.
///
/// The root half cannot be measured from a test — a process cannot become
/// root, and one already running as root cannot conjure a directory owned by
/// somebody else — so it is proved on the pure decision function in
/// `db::tests::ownership_target_mirrors_only_what_root_left_in_another_user_s_directory`.
/// This is the other half, and it is the one that would regress for every
/// ordinary user at once: on a read and on a write, through the compiled
/// binary, nothing about the board's ownership or its mode may move.
///
/// Mode is asserted beside ownership because they fail together. `chown` and
/// `chmod` are both writes to the inode, the seal tests below exist because a
/// stray `chmod` once undid an operator's 0400, and a board this process can
/// no longer open is the same outage whichever bit caused it.
#[test]
fn a_non_root_open_leaves_board_ownership_and_mode_untouched() {
    let fixture = Fixture::new("ownership-untouched");
    fixture.ok_json(&fixture.main, &["init", "--name", "OWNED", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Seed", "--id", "t-own-seed", "--json"],
    );
    let board = board_path_for_project(&fixture, &fixture.main, "OWNED");
    let registry = fixture.data.join("registry.db");

    // Who this process is, read off a file it just created rather than from
    // `geteuid`, so the expectation is stated in the same terms as the
    // measurement. Correct for root too: root's own files come out `0:0`, and
    // the temp tree is root's, so the rule is a no-op there as well.
    let probe = fixture.root.join("identity-probe");
    fs::write(&probe, b"probe").unwrap();
    let identity = fs::metadata(&probe).unwrap();
    let (my_uid, my_gid) = (identity.uid(), identity.gid());
    fs::remove_file(&probe).unwrap();

    // Both lock files, because they are what a root `init` leaves behind
    // ahead of any database and what the service opens first.
    let lock = fixture.data.join(".lock");
    let init_lock = fixture.data.join(".init.lock");
    let before = ownership_snapshot(&[&board, &registry], &[&lock, &init_lock]);
    for required in [&board, &registry, &lock, &init_lock] {
        assert!(
            before.contains_key(required),
            "{} was not on disk, so this measured nothing about it: {before:?}",
            required.display()
        );
    }

    // A read and a write, because they take different opens: the read-only
    // one, and `db::open` through the store.
    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == "t-own-seed"),
        "the read answered nothing, so it exercised no open: {listed}"
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "After", "--id", "t-own-after", "--json"],
    );

    let after = ownership_snapshot(&[&board, &registry], &[&lock, &init_lock]);
    for (path, now) in &after {
        assert_eq!(
            (now.0, now.1),
            (my_uid, my_gid),
            "{} changed hands during a non-root run",
            path.display()
        );
        assert_eq!(
            now.2,
            0o600,
            "{} came out of a non-root run at mode {:04o}",
            path.display(),
            now.2
        );
        if let Some(then) = before.get(path) {
            assert_eq!(
                now,
                then,
                "{} was re-owned or re-permissioned",
                path.display()
            );
        }
    }
}

/// A board and a registry the caller cannot write are still a board and a
/// registry the caller can read.
///
/// Measured on 2026-09-07 against the tree before this fix, with both files at
/// mode 0400: eleven of the twelve reads in
/// `docs/testing/bench/fixture-frozen.json` exited 1 with `attempt to write a
/// readonly database`, and none of them had read anything yet. Two writes sat
/// on the read path — the `last_used_at` stamp that board selection made
/// through a writable registry open, and `sweep_expired_claims` on every
/// `Store::open_as_caller` — plus the `chmod 0600` re-assert inside `db::open`,
/// which silently returned the operator's 0400 file to 0600 on the way past.
/// `claim --candidates` was the twelfth and the only one that answered, because
/// it already took the read-only open.
///
/// Two independent proofs here, because they fail for different reasons:
///
/// Every read answers — exit 0, and `--json` output a parser accepts.
///
/// And neither file was touched: byte-identical contents AND an unchanged mode.
/// The mode is the load-bearing half of that pair, and it holds whatever the
/// caller's uid is: `chmod` is a write that root performs too, so a read path
/// that re-permissions the file is caught here even where the mode bits would
/// not have denied it. Where they do deny it — any non-root caller, which is
/// what the report measured on hax — the exits above are a kernel refusal
/// rather than a convention, and the test says which case it ran in.
#[test]
fn read_only_commands_answer_from_a_board_and_registry_at_mode_0400() {
    let estate = SealedEstate::new("sealed-reads");
    let fixture = &estate.fixture;

    // The read-only branch is the one under test, so pin that this board is at
    // the schema that selects it. A board BEHIND the schema takes the writable
    // open instead, which is a different test
    // (`compiled_binary_still_migrates_a_board_that_is_behind`, and
    // `a_board_behind_the_schema_that_cannot_be_migrated_says_both_halves`).
    let schema = &estate.doctor;
    assert_eq!(
        schema["projects"][0]["schemaVersion"], schema["supportedBoardSchemaVersion"],
        "this test only exercises the read-only open on a current board"
    );
    assert_eq!(
        schema["registrySchemaVersion"], schema["supportedRegistrySchemaVersion"],
        "this test only exercises the read-only registry open on a current registry"
    );

    let kernel_enforced = mode_0400_denies_writes(&fixture.root);
    estate.seal();
    let board_before = fs::read(&estate.board).unwrap();
    let registry_before = fs::read(&estate.registry).unwrap();

    // The twelve reads of the frozen bench fixture, in its order, with this
    // board's ids substituted for the three `context` reads. Every one of them
    // is a read a resuming driver performs before it has written anything.
    let json_reads: [&[&str]; 12] = [
        &["workspace", "list"],
        &["handoff", "list", "--status", "pending"],
        &["attention", "list", "--status", "open", "--limit", "100"],
        &["stale"],
        &["attention", "list", "--status", "open", "--limit", "500"],
        &[
            "attention",
            "list",
            "--status",
            "resolved",
            "--limit",
            "500",
        ],
        &["task", "list"],
        &["context", "t-read-1"],
        &["context", "t-read-2"],
        &[
            "claim",
            "--candidates",
            "--as",
            "bench@driver",
            "--lane",
            "driver",
            "--limit",
            "100",
        ],
        &["sitrep", "list", "--lane", "driver", "--limit", "20"],
        &["events", "--limit", "50"],
    ];
    for read in json_reads {
        let mut argv = read.to_vec();
        argv.push("--json");
        let output = fixture.run(&fixture.main, &argv);
        assert!(
            output.status.success(),
            "{read:?} was refused against a sealed estate (kernel-enforced: \
             {kernel_enforced})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{read:?} answered something that is not JSON: {error}\nstdout: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    }

    // Answering is not enough: the answer has to be the one a writable open
    // would have given. `t-read-2` holds a lease that has run out, and the
    // scheduler's candidate list is where the two paths used to disagree — the
    // untidied claim row hid the task from `claim --candidates` while `claim
    // --next` handed it straight out.
    let candidates = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "--candidates",
            "--as",
            "bench@driver",
            "--lane",
            "driver",
            "--limit",
            "100",
            "--json",
        ],
    );
    assert!(
        candidates
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == "t-read-2"),
        "a lapsed lease hid a claimable task from the read-only open: {candidates}"
    );

    // The rest of the read-only surface, which the bench fixture does not
    // cover but an operator and an adapter both reach for.
    for read in [
        vec!["task", "show", "t-read-1", "--json"],
        vec!["tag", "list", "--json"],
        vec!["deploy", "list", "--json"],
        vec!["deploy", "current", "--json"],
        vec!["deploy", "show", estate.deployment.as_str(), "--json"],
        vec!["rule", "list", "--json"],
    ] {
        let output = fixture.run(&fixture.main, &read);
        assert!(
            output.status.success(),
            "{read:?} was refused against a sealed estate\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|error| {
            panic!("{read:?} answered something that is not JSON: {error}")
        });
    }

    // `todo` renders a document rather than JSON, and writes the board no more
    // than the rest of them.
    let todo = fixture.run(&fixture.main, &["todo"]);
    assert!(
        todo.status.success(),
        "todo was refused against a sealed estate\nstderr: {}",
        String::from_utf8_lossy(&todo.stderr)
    );
    assert!(
        !todo.stdout.is_empty(),
        "todo answered nothing at all against a sealed estate"
    );

    assert_eq!(
        fs::read(&estate.board).unwrap(),
        board_before,
        "a read changed the board's bytes"
    );
    assert_eq!(
        fs::read(&estate.registry).unwrap(),
        registry_before,
        "a read changed the registry's bytes"
    );
    assert_eq!(
        SealedEstate::mode(&estate.board),
        0o400,
        "a read re-permissioned the board"
    );
    assert_eq!(
        SealedEstate::mode(&estate.registry),
        0o400,
        "a read re-permissioned the registry"
    );

    estate.unseal();
}

/// A write against a board it cannot write refuses, and leaves the board alone.
///
/// The other half of the contract above, and the reason the read-only routing
/// is not a permission bypass: the read-only open carries `PRAGMA query_only`,
/// so a mutating command routed through it by mistake would fail rather than
/// silently drop the write, and a mutating command routed correctly meets the
/// filesystem.
#[test]
fn a_write_against_an_unwritable_board_refuses_and_writes_nothing() {
    let estate = SealedEstate::new("sealed-write");
    let fixture = &estate.fixture;
    if !mode_0400_denies_writes(&fixture.root) {
        // Root ignores the mode bits, so there is no refusal to observe and
        // nothing here would be evidence either way. Said out loud rather than
        // asserted into a pass.
        eprintln!(
            "skipped: this process writes through mode 0400, so a sealed board is not sealed for it"
        );
        return;
    }
    estate.seal();
    let before = fs::read(&estate.board).unwrap();

    let refused = fixture.run(
        &fixture.main,
        &["task", "add", "must not land", "--id", "t-nope", "--json"],
    );
    let message = refusal_object(&refused);
    assert!(
        message.contains("readonly database") || message.contains("read-only"),
        "a refusal on an unwritable board must say the file could not be written: {message}"
    );
    assert!(
        !message.contains("panic"),
        "a refusal must be a refusal, not a panic: {message}"
    );
    assert_eq!(
        fs::read(&estate.board).unwrap(),
        before,
        "a refused write left something behind"
    );

    // And the row genuinely is not there once the board is readable again.
    estate.unseal();
    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    assert!(
        !listed
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == "t-nope"),
        "a refused write stored its row anyway: {listed}"
    );
}

/// A seal survives the write it refused, so the RETRY is refused too.
///
/// The retry is the property. The refusal above was already correct before
/// this fix and is not what was broken: `db::open` asserted mode 0600 on every
/// writable open — "re-assert for databases created before this rule, or by
/// another tool" — so the same command that correctly refused to write a board
/// sealed at 0400 returned that board to 0600 on its way out, and the next
/// attempt landed. Measured against the tree before this fix: attempt one
/// exited 1 with `attempt to write a readonly database`, the board came back
/// `-rw-------`, and attempt two exited 0 and stored its row. A seal that
/// holds for exactly one attempt is a delay, not a seal, and an operator who
/// sealed a board would have read the refusal as proof that it held.
///
/// So both halves are asserted after each attempt: the mode the operator set,
/// and a second refusal that could only happen if that mode were still there.
#[test]
fn a_refused_write_leaves_a_sealed_board_sealed_and_refuses_the_retry() {
    let estate = SealedEstate::new("seal-survives-board");
    let fixture = &estate.fixture;
    if !mode_0400_denies_writes(&fixture.root) {
        // Root writes through the mode bits, so there is no refusal to retry
        // and nothing here would be evidence either way.
        eprintln!(
            "skipped: this process writes through mode 0400, so a sealed board is not sealed for it"
        );
        return;
    }
    // A reader held open for the length of the test, so the board's WAL
    // sidecars exist and stay writable while the CLI runs. That is what a live
    // board looks like, and it is what makes the retry below depend on the
    // board file's mode and nothing else: SQLite creates `-wal` and `-shm`
    // with the MAIN file's mode, so when the refused attempt is itself the
    // thing that creates them, the retry meets a 0400 `-shm` and refuses on
    // that — a board file quietly returned to 0600 would still look sealed.
    // Measured against the unconditional re-assert: without this connection
    // the retry refuses on the `-shm` and this test passes the defect.
    let live_reader = Connection::open(&estate.board).unwrap();
    live_reader
        .query_row("SELECT count(*) FROM tasks", [], |row| row.get::<_, i64>(0))
        .unwrap();

    // The BOARD only. Sealing the whole estate seals the registry too, and the
    // registry is opened first, so its refusal would arrive before `db::open`
    // ever reached the board — measured: with the whole estate sealed, this
    // test also passes the defect it exists to catch.
    SealedEstate::set_mode(&estate.board, 0o400);

    let attempt = |id: &str| -> Output {
        fixture.run(
            &fixture.main,
            &["task", "add", "must not land", "--id", id, "--json"],
        )
    };

    let first = attempt("t-sealed-1");
    let first_message = refusal_object(&first);
    assert!(
        first_message.contains("readonly database") || first_message.contains("read-only"),
        "the first write did not refuse on the permission: {first_message}"
    );
    let mode_after_first = SealedEstate::mode(&estate.board);

    // Asserted before the modes, because this is the property and it fails
    // more legibly: `refusal_object` reports a second write that LANDED as the
    // exit and the row it created, where a mode assertion reports a number.
    let second = attempt("t-sealed-2");
    let second_message = refusal_object(&second);
    assert!(
        second_message.contains("readonly database") || second_message.contains("read-only"),
        "the retry refused for some reason other than the permission: {second_message}"
    );

    assert_eq!(
        mode_after_first, 0o400,
        "a refused write re-permissioned the board it had just failed to write"
    );
    assert_eq!(
        SealedEstate::mode(&estate.board),
        0o400,
        "the second refused write re-permissioned the board"
    );

    drop(live_reader);
    estate.unseal();
    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    for id in ["t-sealed-1", "t-sealed-2"] {
        assert!(
            !listed
                .as_array()
                .unwrap()
                .iter()
                .any(|task| task["id"] == id),
            "a refused write stored {id} anyway: {listed}"
        );
    }
}

/// The same rule for the registry, opened through the same `db::open`.
///
/// `rule add` writes the registry and nothing else, and only the registry file
/// is sealed here: the board stays writable, so the refusal is the registry's
/// and so is any `chmod` that shows up on it.
#[test]
fn a_refused_registry_write_leaves_the_registry_sealed_and_refuses_the_retry() {
    let estate = SealedEstate::new("seal-survives-registry");
    let fixture = &estate.fixture;
    if !mode_0400_denies_writes(&fixture.root) {
        eprintln!(
            "skipped: this process writes through mode 0400, so a sealed registry is not sealed \
             for it"
        );
        return;
    }
    let rules = |fixture: &Fixture| -> usize {
        fixture
            .ok_json(&fixture.main, &["rule", "list", "--json"])
            .as_array()
            .unwrap()
            .len()
    };
    let before = rules(fixture);
    SealedEstate::set_mode(&estate.registry, 0o400);

    let attempt = |body: &str| -> Output {
        fixture.run(
            &fixture.main,
            &["rule", "add", body, "--as", "geoyws", "--json"],
        )
    };

    let first = attempt("Must not land in a sealed registry.");
    let first_message = refusal_object(&first);
    assert!(
        first_message.contains("readonly database") || first_message.contains("read-only"),
        "the first registry write did not refuse on the permission: {first_message}"
    );
    let mode_after_first = SealedEstate::mode(&estate.registry);

    let second = attempt("Must not land on the retry either.");
    let second_message = refusal_object(&second);
    assert!(
        second_message.contains("readonly database") || second_message.contains("read-only"),
        "the registry retry refused for some reason other than the permission: {second_message}"
    );

    assert_eq!(
        mode_after_first, 0o400,
        "a refused registry write re-permissioned the registry it had just failed to write"
    );
    assert_eq!(
        SealedEstate::mode(&estate.registry),
        0o400,
        "the second refused registry write re-permissioned the registry"
    );

    SealedEstate::set_mode(&estate.registry, 0o600);
    assert_eq!(
        rules(fixture),
        before,
        "a refused registry write stored its rule anyway"
    );
}

/// A board reachable by anyone else is still narrowed to 0600 on a writable
/// open. The other half of the same rule, and the reason it is tighten-only
/// rather than deleted.
///
/// 0644 is what `Connection::open` produced before Kanban created its files
/// itself, and 0640 is what a group-sharing umask produces, so both are boards
/// that exist on disk rather than invented modes. The write has to succeed
/// too: a `chmod` observed after a refusal would prove the opposite of what
/// this test is for.
///
/// No `mode_0400_denies_writes` skip here, unlike the two seal tests: nothing
/// is being denied, and `chmod` is a write root performs too, so this holds
/// whatever the caller's uid.
#[test]
fn a_writable_open_narrows_a_group_reachable_board_to_0600() {
    let estate = SealedEstate::new("tighten-loose-board");
    let fixture = &estate.fixture;

    for (index, loose) in [0o644u32, 0o640].into_iter().enumerate() {
        SealedEstate::set_mode(&estate.board, loose);
        assert_eq!(
            SealedEstate::mode(&estate.board),
            loose,
            "the fixture could not put the board at 0{loose:o}"
        );
        let id = format!("t-loose-{index}");
        let added = fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "lands on a loose board",
                "--id",
                &id,
                "--json",
            ],
        );
        assert_eq!(added["id"], id, "the write did not land: {added}");
        assert_eq!(
            SealedEstate::mode(&estate.board),
            0o600,
            "a writable open left the board at 0{loose:o}, reachable by more than its owner"
        );
    }
}

/// The sweep still runs on a writable open, and never on a read.
///
/// Both directions in one test on purpose. Deleting
/// [`Store::sweep_expired_claims`] would make the read-only half pass and this
/// half fail; running it on the read path — which is the defect this slice
/// removed — makes this half pass and
/// `read_only_commands_answer_from_a_board_and_registry_at_mode_0400` fail.
///
/// And the answer is the same either way, which is what makes skipping safe: a
/// lapsed lease reads as unclaimed whichever open served it, because every read
/// derives that from `expires_at` rather than from the sweep having run.
#[test]
fn the_claim_sweep_runs_on_a_writable_open_and_never_on_a_read() {
    // The lapsed lease on `t-read-2` comes from the shared setup, so the same
    // board state that every other sealed-estate test starts from is the one
    // asserted about here.
    let estate = SealedEstate::new("sealed-sweep");
    let fixture = &estate.fixture;

    let claim_rows = || -> i64 {
        Connection::open(&estate.board)
            .unwrap()
            .query_row("SELECT count(*) FROM task_claims", [], |row| row.get(0))
            .unwrap()
    };
    let expiry_events = |fixture: &Fixture| -> usize {
        fixture
            .ok_json(
                &fixture.main,
                &["events", "--kind", "claim_expired", "--json"],
            )
            .as_array()
            .unwrap()
            .len()
    };

    estate.seal();
    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let lapsed = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "t-read-2")
        .unwrap();
    assert_eq!(
        lapsed["status"], "todo",
        "a lapsed lease read as owned on the read-only open: {lapsed}"
    );
    assert!(
        lapsed["assignee"].is_null(),
        "a lapsed lease kept its holder on the read-only open: {lapsed}"
    );
    assert_eq!(
        claim_rows(),
        1,
        "a read retired the claim row, so the read path is writing again"
    );
    assert_eq!(
        expiry_events(fixture),
        0,
        "a read wrote the retirement into the ledger"
    );

    // The same board, opened writably: the sweep tidies the row and records it.
    estate.unseal();
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "a write of any kind",
            "--id",
            "t-write",
            "--json",
        ],
    );
    assert_eq!(
        claim_rows(),
        0,
        "the sweep no longer retires a lapsed lease on a writable open"
    );
    assert_eq!(
        expiry_events(fixture),
        1,
        "the sweep no longer records the retirement it performed"
    );
}

/// A board behind the schema that cannot be migrated says BOTH halves.
///
/// The read-only open refuses a board whose schema is not current, so a read
/// routes such a board to the writable open that migrates it — which is what
/// keeps `compiled_binary_still_migrates_a_board_that_is_behind` true. When
/// that open cannot happen either, the refusal has to name the schema and the
/// permission together. SQLite's own `attempt to write a readonly database`
/// names only the second, and sends an operator looking for the write that a
/// `task list` supposedly attempted.
#[test]
fn a_board_behind_the_schema_that_cannot_be_migrated_says_both_halves() {
    let estate = SealedEstate::new("sealed-behind");
    let fixture = &estate.fixture;
    if !mode_0400_denies_writes(&fixture.root) {
        eprintln!(
            "skipped: this process writes through mode 0400, so a sealed board is not sealed for it"
        );
        return;
    }
    let connection = Connection::open(&estate.board).unwrap();
    remove_v27_sprint_schema(&connection);
    connection.execute_batch("PRAGMA user_version=26;").unwrap();
    drop(connection);
    estate.seal();

    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "list",
            "--db",
            estate.board.to_str().unwrap(),
            "--json",
        ],
    );
    let message = refusal_object(&refused);
    assert!(
        message.contains("schema 26"),
        "the refusal does not name the schema the board is at: {message}"
    );
    assert!(
        message.contains("cannot write the file"),
        "the refusal does not name the permission: {message}"
    );
    assert!(
        !message.contains("attempt to write a readonly database"),
        "the refusal leaked SQLite's message instead of explaining itself: {message}"
    );

    estate.unseal();
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
    let lines: Vec<&str> = version.lines().collect();
    assert!(lines[0].contains("kanban"), "version output: {version}");
    let supported =
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["supportedBoardSchemaVersion"]
            .as_i64()
            .unwrap();
    assert!(
        lines[0].contains(&format!("board schema {supported}")),
        "version output: {version}"
    );
    assert!(
        lines[0].contains("registry schema 16"),
        "version output: {version}"
    );
    // The banner is one line now that the embedded operator UI is gone:
    // the program, its version and the two schema versions it speaks.
    assert_eq!(lines.len(), 1, "version output: {version}");
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
    // task id can never collide with one: ids carry their kind prefix
    // (`t-` here), so the second positional of `n` still reads as the row.
    kb_json(
        &fixture.main,
        &["t", "new", "edge case", "--id", "t-rm", "--json"],
    );
    kb_json(
        &fixture.main,
        &["n", "t-rm", "note on task t-rm", "--as", "geoyws", "--json"],
    );
    assert_eq!(
        kb_json(&fixture.main, &["t", "cat", "t-rm", "--json"])["id"],
        "t-rm"
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
fn compiled_binary_never_reads_a_dead_lease_as_owned_and_records_it_on_the_next_write() {
    let fixture = Fixture::new("sweep");
    fixture.ok_json(&fixture.main, &["init", "--name", "Sweep", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "abandoned", "--id", "t-1", "--json"],
    );
    let ghost = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-1",
            "--as",
            "ghost",
            "--session",
            "ghost-session",
            "--json",
        ],
    );
    // A checkpoint written while the lease is live is the freshest work the
    // dead holder left behind, and a successor must see exactly when.
    let checkpoint = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            ghost["leaseToken"].as_str().unwrap(),
            "--as",
            "ghost",
            "--session",
            "ghost-session",
            "--summary",
            "half done",
            "--intent",
            "finish it",
            "--next-action",
            "run the suite",
            "--json",
        ],
    );

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

    // A read is read-only, so the retirement is not history YET: the
    // `claim_expired` event is a write and lands on the next writable open.
    // Asserted, not glossed over — if a read ever starts writing again, this
    // is where it shows up.
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["events", "--kind", "claim_expired", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a read must not write the retirement into the ledger"
    );

    // K2: a second agent's claim receipt names who died, when, and how stale.
    // This is also the first WRITABLE open since the lease lapsed, so it is
    // the one that sweeps the row and records the retirement.
    let successor = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-1",
            "--as",
            "successor",
            "--session",
            "successor-session",
            "--json",
        ],
    );

    // The sweep is itself durable history, not a silent correction.
    let expired = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "claim_expired", "--json"],
    );
    assert_eq!(expired.as_array().unwrap().len(), 1);
    assert_eq!(expired[0]["actor"], "ghost");

    // K1: the payload names the dead holder, where they stood, and how stale
    // their last checkpoint was — read before the DELETE, not after.
    let payload = &expired[0]["payload"];
    assert_eq!(payload["sessionId"], "ghost-session");
    assert_eq!(payload["worktree"], ghost["worktree"]);
    assert_eq!(payload["worktreeKind"], ghost["worktreeKind"]);
    assert_eq!(payload["branch"], ghost["branch"]);
    assert_eq!(payload["headSha"], ghost["headSha"]);
    assert_eq!(payload["rootHead"], ghost["rootHead"]);
    assert_eq!(payload["claimedAt"], ghost["claimedAt"]);
    assert_eq!(payload["heartbeatAt"], ghost["heartbeatAt"]);
    assert_eq!(payload["lastCheckpointSeq"], checkpoint["seq"]);
    assert_eq!(payload["lastCheckpointAt"], checkpoint["createdAt"]);

    // K2, continued: the receipt the successor already got.
    let orphaned = &successor["orphanedFrom"];
    assert_eq!(orphaned["agent"], "ghost");
    assert_eq!(orphaned["sessionId"], "ghost-session");
    assert_eq!(orphaned["expiredAt"], expired[0]["createdAt"]);
    assert_eq!(orphaned["lastCheckpointAt"], checkpoint["createdAt"]);
    assert_eq!(orphaned["worktree"], ghost["worktree"]);
    assert_eq!(orphaned["branch"], ghost["branch"]);
    assert_eq!(orphaned["headSha"], ghost["headSha"]);

    // K2/K3: the same orphan and the same checkpoint are what `kb ctx` hands
    // the successor reading the packet cold.
    let ctx = fixture.ok_json(&fixture.main, &["ctx", "t-1", "--json"]);
    assert_eq!(ctx["orphanedFrom"], successor["orphanedFrom"]);
    assert_eq!(
        ctx["checkpoints"].as_array().unwrap().last().unwrap()["createdAt"],
        checkpoint["createdAt"]
    );
}

#[test]
fn compiled_binary_reports_null_last_checkpoint_when_the_dead_holder_wrote_none() {
    let fixture = Fixture::new("sweep-null");
    fixture.ok_json(&fixture.main, &["init", "--name", "SweepNull", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "abandoned", "--id", "t-1", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-1",
            "--as",
            "ghost",
            "--session",
            "ghost-session",
            "--json",
        ],
    );

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute("UPDATE task_claims SET expires_at=1", [])
        .unwrap();
    // The successor's claim is the first WRITABLE open since the lease lapsed,
    // so it is what sweeps the row and records the retirement; a read would
    // report the lapse correctly and write nothing.
    //
    // The null survives into the successor's orphanedFrom rather than being
    // reported as a fresh checkpoint that never happened.
    let successor = fixture.ok_json(
        &fixture.main,
        &["claim", "t-1", "--as", "successor", "--json"],
    );
    assert_eq!(successor["orphanedFrom"]["agent"], "ghost");
    assert_eq!(successor["orphanedFrom"]["sessionId"], "ghost-session");
    assert!(successor["orphanedFrom"]["lastCheckpointAt"].is_null());

    // The holder wrote no checkpoint since claiming, so the enriched payload
    // says exactly that with nulls, never an empty string or a stale value.
    let expired = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "claim_expired", "--json"],
    );
    assert_eq!(expired.as_array().unwrap().len(), 1);
    assert!(expired[0]["payload"]["lastCheckpointSeq"].is_null());
    assert!(expired[0]["payload"]["lastCheckpointAt"].is_null());
}

#[test]
fn compiled_binary_reports_only_the_latest_orphan_and_none_after_a_completed_cycle() {
    let fixture = Fixture::new("sweep-latest");
    fixture.ok_json(&fixture.main, &["init", "--name", "SweepLatest", "--json"]);
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();

    // Orphaned twice: the second holder, not the first, is the one a successor
    // must be told about.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "re-orphaned", "--id", "t-latest", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-latest", "--as", "first", "--json"],
    );
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE task_claims SET expires_at=1 WHERE task_id='t-latest'",
            [],
        )
        .unwrap();
    fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-latest", "--as", "second", "--json"],
    );
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE task_claims SET expires_at=1 WHERE task_id='t-latest'",
            [],
        )
        .unwrap();
    fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let third = fixture.ok_json(
        &fixture.main,
        &["claim", "t-latest", "--as", "third", "--json"],
    );
    assert_eq!(
        third["orphanedFrom"]["agent"], "second",
        "a twice-orphaned task must report the latest orphan, not the first"
    );

    // Claimed, orphaned, reclaimed and completed: after the task finishes and
    // is moved back to todo, the stale orphan must not be reported to a later
    // holder.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "completed", "--id", "t-done", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-done", "--as", "worker", "--json"],
    );
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE task_claims SET expires_at=1 WHERE task_id='t-done'",
            [],
        )
        .unwrap();
    fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-done", "--as", "successor", "--json"],
    );
    // The successor finishes, then the task is moved back into the queue.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-done", "done", "--as", "operator", "--force", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-done", "todo", "--as", "operator", "--json",
        ],
    );
    // The completed task still names its assignee, so the fresh holder takes
    // it over explicitly rather than the claim being refused as reassignment.
    let later = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-done",
            "--as",
            "later",
            "--allow-reassign",
            "--json",
        ],
    );
    assert!(
        later.get("orphanedFrom").is_none(),
        "a completed cycle must not report a stale orphan to a later holder: {later}"
    );
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
