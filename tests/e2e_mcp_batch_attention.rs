//! Compiled-binary E2E: the MCP server, `batch` and `transact`, handoffs and
//! attention cards.
//!
//! One of the `e2e_*` area targets (t-2aeec40c). Each is serial inside
//! (`--test-threads=1`); `scripts/release-gate.sh` runs the areas side by
//! side because every case owns its fixture under a pid-unique temp root.
//! Helpers more than one area uses live in `tests/e2e_support/`.

// Each area uses only some of the shared helpers and imports.
#[allow(dead_code, unused_imports)]
mod e2e_support;
use e2e_support::*;

/// What resolving through the default pair composes (ADR-042 §2 and §3): the
/// synthesized choice's label, then its consequence. Written out rather than
/// imported, so a change to either is visible in a diff of this file.
const APPROVE: &str =
    "Decision: Approve - proceed. The work the body describes goes ahead as written.";

const REJECT: &str = "Decision: Reject - do not proceed. The work the body describes does not \
                      happen; whoever raised it needs a new plan.";

fn copy_executable(source: &Path, target: &Path) {
    // Stage the executable beside the target, then rename it into place so
    // initial publication has the same atomic boundary as later replacements.
    let staging = target.with_extension("staging");
    fs::copy(source, &staging).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&staging, target).unwrap();
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
    assert_eq!(
        rule_add["inputSchema"]["properties"]["sprint"]["type"],
        "string"
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
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "new",
            "MCP release",
            "--id",
            "sp-mcp",
            "--target-version",
            "1.0.0",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "update", "t-wire", "--sprint", "sp-mcp", "--as", "geoyws", "--json",
        ],
    );
    let sprint_rule = session.ask(json!({
        "jsonrpc": "2.0", "id": 32, "method": "tools/call",
        "params": { "name": "rule_add", "arguments": {
            "body": "MCP sprint rule.", "board": ["MCP"], "sprint": "sp-mcp", "as": "geoyws"
        } }
    }));
    assert_eq!(sprint_rule["result"]["isError"], false, "{sprint_rule}");
    let sprint_context = fixture.ok_json(&fixture.main, &["context", "t-wire", "--json"]);
    assert!(
        sprint_context["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["headline"] == "MCP sprint rule."
                    && item["tags"] == json!(["ONLY:MCP", "SPRINT:sp-mcp"])
            })
    );
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

/// The refusal text of a batch that was rejected whole.
fn batch_refusal(session: &mut Session, id: i64, calls: Vec<Value>) -> String {
    let answered = session.ask(json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "batch", "arguments": { "calls": calls } }
    }));
    assert_eq!(
        answered["result"]["isError"], true,
        "a batch that must be refused was answered: {answered}"
    );
    tool_text(&answered["result"])
}

/// A response body with the reads' declared volatile keys removed and its keys
/// sorted -- the benchmark's `canonical_digest`, in Rust.
///
/// Dropped at the top level and from every element of a top-level array, for
/// the same reason the driver does it: selecting a board stamps the workspace
/// row's `lastUsedAt`, so reading twice moves it, and `context` stamps
/// `generatedAt` on every answer.
fn normalized_body(label: &str, body: &str, drop_keys: &[String]) -> String {
    let mut value: Value = serde_json::from_str(body)
        .unwrap_or_else(|error| panic!("{label} answered with non-JSON ({error}): {body}"));
    match &mut value {
        Value::Object(map) => {
            for key in drop_keys {
                map.remove(key);
            }
        }
        Value::Array(elements) => {
            for element in elements {
                if let Value::Object(map) = element {
                    for key in drop_keys {
                        map.remove(key);
                    }
                }
            }
        }
        _ => {}
    }
    value.to_string()
}

/// Whether a body actually carries a key the fixture calls volatile, where
/// `normalized_body` would drop it: at the top level, or in every element of a
/// top-level array.
fn volatile_key_present(body: &str, key: &str) -> bool {
    match serde_json::from_str::<Value>(body).unwrap_or(Value::Null) {
        Value::Object(map) => map.contains_key(key),
        Value::Array(elements) => {
            !elements.is_empty()
                && elements
                    .iter()
                    .all(|element| element.as_object().is_some_and(|map| map.contains_key(key)))
        }
        _ => false,
    }
}

fn body_digest(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[test]
fn a_batch_naming_a_writing_tool_is_refused_whole_and_runs_nothing() {
    let fixture = Fixture::new("mcp-batch-refuses-a-write");
    fixture.ok_json(&fixture.main, &["init", "--name", "BATCHWRITE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "already here", "--id", "t-before", "--json"],
    );
    let before = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));

    // The write sits last, behind two reads that would otherwise have answered.
    // A batch validated entry by entry as it ran would have executed both and
    // left the caller to work out how far it got.
    let refusal = batch_refusal(
        &mut session,
        2,
        vec![
            json!({ "name": "stale", "arguments": {} }),
            json!({ "name": "task_list", "arguments": {} }),
            json!({ "name": "task_add", "arguments": { "title": "batched write", "id": "t-batched" } }),
        ],
    );
    assert!(refusal.contains("call 2"), "{refusal}");
    assert!(refusal.contains("task_add"), "{refusal}");
    assert!(refusal.contains("writes"), "{refusal}");
    assert!(refusal.contains("nothing in it ran"), "{refusal}");

    // A tool that is not listed at all -- a long-running one, which
    // `tools/list` withholds -- is refused the same way.
    let unlisted = batch_refusal(
        &mut session,
        3,
        vec![json!({ "name": "watch", "arguments": { "limit": "1" } })],
    );
    assert!(unlisted.contains("call 0"), "{unlisted}");
    assert!(unlisted.contains("no such tool watch"), "{unlisted}");

    // And a batch may not carry a batch: the name resolves to no operation.
    let nested = batch_refusal(
        &mut session,
        4,
        vec![json!({ "name": "batch", "arguments": { "calls": [] } })],
    );
    assert!(nested.contains("no such tool batch"), "{nested}");

    session.finish();

    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-batched", "--json"])
            .status
            .success(),
        "the refused batch still created the task its write named"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        before,
        "the board changed under a batch that was refused whole"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]),
        events_before,
        "a refused batch wrote to the ledger"
    );
}

#[test]
fn a_batch_over_the_bound_is_refused_naming_the_bound() {
    let fixture = Fixture::new("mcp-batch-bound");
    fixture.ok_json(&fixture.main, &["init", "--name", "BATCHBOUND", "--json"]);
    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));

    let stale_calls = |count: usize| vec![json!({ "name": "stale", "arguments": {} }); count];

    let refusal = batch_refusal(&mut session, 2, stale_calls(33));
    assert!(refusal.contains("at most 32"), "{refusal}");
    assert!(
        refusal.contains("33"),
        "the refusal did not say how many were sent: {refusal}"
    );
    assert!(refusal.contains("nothing in it ran"), "{refusal}");

    // The bound is the bound, not one short of it: 32 runs.
    let accepted = batch_results(&mut session, 3, stale_calls(32));
    let accepted = accepted.as_array().unwrap();
    assert_eq!(accepted.len(), 32);
    assert!(
        accepted.iter().all(|entry| entry["ok"] == true),
        "a batch at the bound did not run: {accepted:?}"
    );

    session.finish();
}

#[test]
fn a_batch_entry_that_fails_carries_its_error_beside_the_others_results() {
    let fixture = Fixture::new("mcp-batch-partial-failure");
    fixture.ok_json(&fixture.main, &["init", "--name", "BATCHPART", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "answers", "--id", "t-answers", "--json"],
    );
    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));

    // The failure is second of four, so the entries after it prove a refusal
    // stops nothing, and the entry before it proves nothing was rolled back.
    let results = batch_results(
        &mut session,
        2,
        vec![
            json!({ "name": "task_show", "arguments": { "id": "t-answers" } }),
            json!({ "name": "context", "arguments": { "id": "t-absent" } }),
            json!({ "name": "stale", "arguments": {} }),
            json!({ "name": "task_list", "arguments": { "fields": "id,title" } }),
        ],
    );
    let results = results.as_array().unwrap();
    assert_eq!(results.len(), 4);

    assert_eq!(results[0]["ok"], true);
    let shown: Value = serde_json::from_str(&tool_text(&results[0]["result"])).unwrap();
    assert_eq!(shown["title"], "answers");

    assert_eq!(results[1]["ok"], false, "an unknown task id answered");
    assert!(results[1]["result"].is_null(), "a failure carried a result");
    assert_eq!(results[1]["error"]["isError"], true);
    let failed = tool_text(&results[1]["error"]);
    assert!(failed.contains("t-absent"), "{failed}");
    assert!(failed.contains("not found"), "{failed}");

    assert_eq!(results[2]["ok"], true, "a read after a failure was skipped");
    assert_eq!(tool_text(&results[2]["result"]).trim(), "[]");

    assert_eq!(results[3]["ok"], true);
    let listed: Value = serde_json::from_str(&tool_text(&results[3]["result"])).unwrap();
    assert_eq!(listed[0]["id"], "t-answers");

    // The batch itself succeeded: a per-entry refusal is that entry's answer,
    // not a verdict on the eleven others. `batch_results` asserted
    // `isError: false` on the way in.
    session.finish();
}

#[test]
fn a_batched_read_is_byte_identical_to_the_same_read_on_its_own() {
    // The twelve reads of the benchmark's v2 fixture, read from the fixture
    // itself so the property is pinned to the loop that is actually measured
    // rather than to a copy of it that can drift.
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/testing/bench/fixture-v2.json");
    let bench: Value = serde_json::from_str(&fs::read_to_string(&fixture_path).unwrap()).unwrap();
    let reads = bench["reads"].as_array().unwrap().clone();
    assert_eq!(reads.len(), 12, "the v2 fixture is no longer twelve reads");

    let fixture = Fixture::new("mcp-batch-identity");
    fixture.ok_json(&fixture.main, &["init", "--name", "BATCHID", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/driver", "--as", "geoyws", "--json"],
    );
    let mut task_ids = Vec::new();
    for index in 0..3 {
        let id = format!("t-ident{index}");
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                &format!("identity fixture task {index}"),
                "--id",
                &id,
                "--tag",
                "geoyws/driver",
                "--lane",
                "driver",
                "--json",
            ],
        );
        task_ids.push(id);
    }
    fixture.ok_json(
        &fixture.main,
        &["attention", "raise", "open row", "--as", "geoyws", "--json"],
    );
    let resolved = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "resolved row",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            resolved["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--note",
            "resolved for the identity fixture",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "identity fixture sitrep",
            "--as",
            "geoyws",
            "--lane",
            "driver",
            "--json",
        ],
    );

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap().clone();
    let read_only_hint = |name: &str| -> Option<bool> {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .map(|tool| tool["annotations"]["readOnlyHint"] == true)
    };

    // Each fixture read as this session must ask it: the fixture's own tool
    // and args, its pinned task ids swapped for this board's, and `project`
    // added exactly where the driver adds it.
    let mut planned = Vec::new();
    let mut next_task = 0;
    for read in &reads {
        let mut arguments = read["args"].as_object().unwrap().clone();
        if arguments.contains_key("id") {
            arguments.insert("id".into(), json!(task_ids[next_task % task_ids.len()]));
            next_task += 1;
        }
        if read["board_scoped"] != false {
            arguments.insert("project".into(), json!("BATCHID"));
        }
        let drop_keys = read["normalize_drop_keys"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .map(|key| key.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        planned.push((
            read["id"].as_str().unwrap().to_owned(),
            read["tool"].as_str().unwrap().to_owned(),
            Value::Object(arguments),
            drop_keys,
        ));
    }
    assert_eq!(next_task, 3, "the v2 fixture no longer pins three task ids");

    // Eleven of the twelve are batchable. `claim --candidates` is read-only in
    // the CLI -- it opens the board read-only and refuses the lease flags --
    // but its `COMMANDS` row covers every `claim` invocation, and plain `claim`
    // writes a lease. The row is the only signal this layer has, so the batch
    // refuses `claim` and the benchmark's batch arm issues that one read on its
    // own. Asserted here because the arm's round-trip count depends on it.
    let (batchable, standalone_only): (Vec<_>, Vec<_>) = planned
        .iter()
        .partition(|(_, tool, ..)| read_only_hint(tool) == Some(true));
    assert_eq!(batchable.len(), 11);
    assert_eq!(standalone_only.len(), 1);
    assert_eq!(standalone_only[0].0, "claim_candidates");
    assert_eq!(read_only_hint("claim"), Some(false));

    // Every read on its own first, so the comparison is against a real answer
    // rather than against two refusals that happen to match.
    let mut alone = BTreeMap::new();
    for (index, (id, tool, arguments, _)) in planned.iter().enumerate() {
        let answered = session.ask(json!({
            "jsonrpc": "2.0", "id": 100 + index as i64, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }));
        assert_eq!(
            answered["result"]["isError"],
            false,
            "{id} did not answer on its own: {}",
            tool_text(&answered["result"])
        );
        alone.insert(id.clone(), tool_text(&answered["result"]));
    }

    let calls = batchable
        .iter()
        .map(|(_, tool, arguments, _)| json!({ "name": tool, "arguments": arguments }))
        .collect::<Vec<_>>();
    let batched = batch_results(&mut session, 200, calls);
    let batched = batched.as_array().unwrap();
    assert_eq!(batched.len(), batchable.len());

    for (entry, (id, .., drop_keys)) in batched.iter().zip(batchable.iter()) {
        assert_eq!(entry["ok"], true, "{id} failed inside the batch: {entry}");
        let inside = tool_text(&entry["result"]);
        let outside = &alone[id];
        // The property the benchmark's equivalence check rests on, read for
        // read: the same call inside a batch and on its own answers with the
        // same bytes, apart from keys the fixture declares volatile.
        let inside_canonical =
            normalized_body(&format!("{id} inside the batch"), &inside, drop_keys);
        let outside_canonical = normalized_body(&format!("{id} on its own"), outside, drop_keys);
        assert_eq!(
            inside_canonical, outside_canonical,
            "{id} differed between a batch and a standalone call"
        );
        assert_eq!(
            body_digest(&inside_canonical),
            body_digest(&outside_canonical),
            "{id} normalized digests differ"
        );
        if drop_keys.is_empty() {
            // Nothing was declared volatile, so nothing was dropped and the
            // raw bytes must match too.
            assert_eq!(inside, *outside, "{id} raw bodies differ");
        } else {
            // A declared volatile key that is not in the response would make
            // the normalization a no-op and the comparison above vacuous, so
            // the fixture's claim about this read is checked, not trusted.
            for key in drop_keys {
                assert!(
                    volatile_key_present(&inside, key),
                    "{id} declares {key} volatile but the batched response has no \
                     such key; normalize_drop_keys asserts nothing for this read"
                );
                assert!(
                    volatile_key_present(outside, key),
                    "{id} declares {key} volatile but the standalone response has \
                     no such key; normalize_drop_keys asserts nothing for this read"
                );
            }
        }
    }

    session.finish();
}

/// The refusal text of a transact rejected before any of it ran.
fn transact_refusal(fixture: &Fixture, cwd: &Path, items: &[Value]) -> String {
    let envelope = transact_results(fixture, cwd, items);
    assert_eq!(
        envelope["ok"], false,
        "a transact that must be refused ran: {envelope}"
    );
    // The one failure that reports no rollback: nothing was attempted.
    assert_eq!(
        envelope["rolledBack"], false,
        "a pre-flight refusal reported a rollback: {envelope}"
    );
    assert_eq!(
        envelope["results"],
        json!([]),
        "a pre-flight refusal carried results: {envelope}"
    );
    let text = envelope["error"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal names itself: {envelope}"))
        .to_owned();
    assert!(text.ends_with("nothing in it ran"), "{text}");
    text
}

/// A checkpoint item's arguments, which every case spells the same way.
fn checkpoint_item(id: &str, lease: Value, actor: &str) -> Value {
    json!({ "name": "checkpoint", "arguments": {
        "id": id,
        "lease": lease,
        "as": actor,
        "summary": "did the thing",
        "intent": "do the thing",
        "next-action": "do the next thing",
        "state": "continue",
    }})
}

#[test]
fn transact_runs_its_items_in_order_and_each_result_matches_the_same_command_alone() {
    let fixture = Fixture::new("transact-order");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXALONE", "--json"]);
    fixture.ok_json(&fixture.worktree, &["init", "--name", "TXBATCH", "--json"]);
    for cwd in [&fixture.main, &fixture.worktree] {
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
        fixture.ok_json(cwd, &["task", "add", "subject", "--id", "t-1", "--json"]);
    }

    // The same write, once on its own and once as an item. Only `createdAt`
    // is dropped, because two writes cannot share a millisecond by
    // construction; everything else must be equal or the batch is a second
    // way to write the board.
    let alone = fixture.ok_json(
        &fixture.main,
        &["note", "t-1", "one", "--as", "agent-a", "--json"],
    );
    let envelope = transact_results(
        &fixture,
        &fixture.worktree,
        &[
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "one", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "two", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "three", "as": "agent-a" } }),
        ],
    );
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["failedIndex"], Value::Null, "{envelope}");
    assert_eq!(envelope["rolledBack"], false, "{envelope}");

    let mut batched = envelope["results"][0]["result"].clone();
    let mut alone = alone;
    for shape in [&mut batched, &mut alone] {
        shape.as_object_mut().unwrap().remove("createdAt");
    }
    assert_eq!(
        batched, alone,
        "a transacted write answered differently from the same command alone"
    );

    // In order, and the board agrees: the notes come back oldest first.
    let indices = envelope["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["index"].clone())
        .collect::<Vec<_>>();
    assert_eq!(indices, vec![json!(0), json!(1), json!(2)]);
    let bodies = fixture.ok_json(&fixture.worktree, &["task", "show", "t-1", "--json"])["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["body"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(bodies, ["one", "two", "three"]);
}

/// The coherent selector snapshot behind `context` and `claim` must nest
/// inside a batch: a nested BEGIN would fail every item after the first read.
#[test]
fn a_transact_can_read_context_and_claim_inside_one_batch() {
    let fixture = Fixture::new("transact-context");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXCTX", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "context", "arguments": { "id": "t-1" } }),
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }),
            json!({ "name": "context", "arguments": { "id": "t-1" } }),
        ],
    );
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(
        envelope["results"][0]["result"]["task"]["id"], "t-1",
        "{envelope}"
    );
    assert_eq!(
        envelope["results"][1]["result"]["agentID"], "agent-a",
        "{envelope}"
    );
    assert_eq!(
        envelope["results"][2]["result"]["claim"]["agentID"], "agent-a",
        "{envelope}"
    );
}

#[test]
fn a_transact_that_fails_midway_rolls_back_every_item_before_it() {
    let fixture = Fixture::new("transact-rollback");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXROLL", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let tasks_before = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);

    // A claim, a note and a checkpoint that all succeed, then an item that
    // names a task that is not there. Today's per-command commit would leave
    // the first three landed with nothing to say so.
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "in the batch", "as": "agent-a" } }),
            checkpoint_item(
                "t-1",
                json!({ "$ref": { "item": 0, "path": "/leaseToken" } }),
                "agent-a",
            ),
            json!({ "name": "note", "arguments": { "id": "t-absent", "text": "boom", "as": "agent-a" } }),
        ],
    );
    assert_eq!(envelope["ok"], false, "{envelope}");
    assert_eq!(envelope["failedIndex"], 3, "{envelope}");
    assert_eq!(envelope["rolledBack"], true, "{envelope}");
    assert!(
        envelope["results"][3]["error"]
            .as_str()
            .is_some_and(|error| error.contains("t-absent")),
        "{envelope}"
    );

    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(task["status"], "todo", "the task did not go back: {task}");
    assert_eq!(
        task["claim"],
        Value::Null,
        "a rolled-back batch left a claim"
    );
    assert_eq!(
        task["notes"],
        json!([]),
        "a rolled-back batch left a note: {task}"
    );
    assert_eq!(
        task["checkpoints"],
        json!([]),
        "a rolled-back batch left a checkpoint: {task}"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        tasks_before,
        "the board changed under a batch that rolled back"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]),
        events_before,
        "a rolled-back batch left a ledger event"
    );
}

#[test]
fn a_failed_transact_leaves_the_audit_chain_healthy_and_its_sequence_unbroken() {
    let fixture = Fixture::new("transact-chain");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXCHAIN", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let before = board_audit(&fixture);

    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "one", "as": "agent-a" } }),
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }),
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-b" } }),
        ],
    );
    assert_eq!(envelope["failedIndex"], 2, "{envelope}");
    assert_eq!(envelope["rolledBack"], true, "{envelope}");

    // Rolling back frees the sequence numbers rather than skipping them, so
    // the chain is intact rather than merely unbroken-looking (ADR-029).
    let after = board_audit(&fixture);
    assert_eq!(after["lastSeq"], before["lastSeq"], "{after}");
    assert_eq!(after["entries"], before["entries"], "{after}");
    assert_eq!(after["head"], before["head"], "{after}");
    assert_eq!(after["errors"], json!([]), "{after}");
}

#[test]
fn a_transact_reports_failed_index_rolled_back_and_skips_every_later_item() {
    let fixture = Fixture::new("transact-skips");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXSKIP", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );

    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "first", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "third", "as": "agent-a" } }),
            json!({ "name": "task_list", "arguments": {} }),
            json!({ "name": "stale", "arguments": {} }),
        ],
    );
    assert_eq!(envelope["failedIndex"], 1, "{envelope}");
    assert_eq!(envelope["rolledBack"], true, "{envelope}");
    let results = envelope["results"].as_array().unwrap();
    assert_eq!(results.len(), 5, "{envelope}");
    assert_eq!(results[0]["ok"], true, "{envelope}");
    assert_eq!(results[1]["ok"], false, "{envelope}");
    assert_eq!(
        results[1]["skipped"],
        Value::Null,
        "an item that ran was reported as never tried"
    );
    // `skipped` is its own field, so an agent tells "refused" from "never
    // tried" without parsing prose.
    for (index, result) in results.iter().enumerate().take(5).skip(2) {
        assert_eq!(result["index"], index, "{envelope}");
        assert_eq!(result["skipped"], true, "{envelope}");
        assert_eq!(result["error"], Value::Null, "{envelope}");
    }
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["notes"],
        json!([]),
        "the item before the failure landed"
    );
}

#[test]
fn a_transact_resolves_a_back_reference_to_an_earlier_items_result() {
    let fixture = Fixture::new("transact-ref");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXREF", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );

    // The lease token a claim at index 0 returns reaches a checkpoint at
    // index 2 without the agent ever seeing it.
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "working", "as": "agent-a" } }),
            checkpoint_item(
                "t-1",
                json!({ "$ref": { "item": 0, "path": "/leaseToken" } }),
                "agent-a",
            ),
        ],
    );
    assert_eq!(envelope["ok"], true, "{envelope}");
    let token = envelope["results"][0]["result"]["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(token.len(), 36, "a claim answers with a lease token");

    // `task show` withholds the token, so the proof that the reference
    // resolved to the RIGHT value is the checkpoint: `require_lease` refuses
    // any token but the live one, and a wrong token would have failed the
    // item and rolled the claim back with it.
    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(task["claim"]["agentID"], "agent-a", "{task}");
    assert_eq!(task["status"], "in_progress", "{task}");
    let checkpoints = task["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 1, "{task}");
    assert_eq!(checkpoints[0]["summary"], "did the thing", "{task}");
    assert_eq!(task["notes"].as_array().unwrap().len(), 1, "{task}");

    // And a token that is not the one item 0 returned is refused, so the
    // reference above cannot have been resolving to something inert.
    let wrong = transact_results(
        &fixture,
        &fixture.main,
        &[checkpoint_item(
            "t-1",
            json!(format!("wrong-{token}")),
            "agent-a",
        )],
    );
    assert_eq!(wrong["failedIndex"], 0, "{wrong}");
}

#[test]
fn a_transact_with_an_unresolvable_reference_is_refused_whole_and_runs_nothing() {
    let fixture = Fixture::new("transact-bad-ref");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXBADREF", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let tasks_before = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);

    // Each list puts a real write first, so a reference validated as it ran
    // would have landed one.
    let write = json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } });
    for (label, reference, expected) in [
        (
            "forward",
            json!({ "item": 2, "path": "/leaseToken" }),
            "item 2",
        ),
        (
            "self",
            json!({ "item": 1, "path": "/leaseToken" }),
            "item 1",
        ),
        (
            "out of range",
            json!({ "item": 9, "path": "/leaseToken" }),
            "item 9",
        ),
        (
            "malformed pointer",
            json!({ "item": 0, "path": "leaseToken" }),
            "JSON Pointer",
        ),
    ] {
        let refusal = transact_refusal(
            &fixture,
            &fixture.main,
            &[
                write.clone(),
                checkpoint_item("t-1", json!({ "$ref": reference }), "agent-a"),
            ],
        );
        // The offending item is item 1 in every list, and the refusal names it.
        assert!(refusal.contains("transact item 1"), "{label}: {refusal}");
        assert!(refusal.contains(expected), "{label}: {refusal}");

        assert_eq!(
            fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
            tasks_before,
            "{label}: the board changed under a refused batch"
        );
        assert_eq!(
            fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]),
            events_before,
            "{label}: a refused batch wrote to the ledger"
        );
    }
}

#[test]
fn a_read_inside_a_transact_observes_the_earlier_writes() {
    let fixture = Fixture::new("transact-read");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXREAD", "--json"]);

    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "task_add", "arguments": { "title": "made here", "id": "t-new", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-new", "text": "written here", "as": "agent-a" } }),
            json!({ "name": "task_show", "arguments": { "id": "t-new" } }),
            json!({ "name": "events", "arguments": { "task": "t-new", "limit": "10" } }),
        ],
    );
    assert_eq!(envelope["ok"], true, "{envelope}");

    // A read item sees the uncommitted writes of the items before it: within
    // one transaction on one connection, a statement sees that transaction's
    // own writes. A second, read-only connection would answer from the
    // pre-batch snapshot and say nothing about it.
    let shown = &envelope["results"][2]["result"];
    assert_eq!(shown["id"], "t-new", "{envelope}");
    assert_eq!(shown["notes"][0]["body"], "written here", "{envelope}");
    let kinds = envelope["results"][3]["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["note_added", "task_added"], "{envelope}");
}

#[test]
fn a_transact_naming_batch_or_transact_is_refused_whole() {
    let fixture = Fixture::new("transact-nested");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXNEST", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);

    let write =
        json!({ "name": "note", "arguments": { "id": "t-1", "text": "before", "as": "agent-a" } });
    // Its own name is refused by name, because `transact` has a COMMANDS row.
    let itself = transact_refusal(
        &fixture,
        &fixture.main,
        &[
            write.clone(),
            json!({ "name": "transact", "arguments": { "items": "[]" } }),
        ],
    );
    assert!(itself.contains("transact item 1"), "{itself}");
    assert!(itself.contains("may not carry a batch"), "{itself}");

    // The read-only batch is refused by name too: its COMMANDS row publishes
    // `kanban batch` as a read, which would otherwise admit it as an item
    // (docs/specs/batch.md BA-05).
    let read_batch = transact_refusal(
        &fixture,
        &fixture.main,
        &[
            write.clone(),
            json!({ "name": "batch", "arguments": { "calls": [] } }),
        ],
    );
    assert!(read_batch.contains("transact item 1"), "{read_batch}");
    assert!(
        read_batch.contains("names batch, and a batch may not carry a batch"),
        "{read_batch}"
    );

    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["notes"],
        json!([]),
        "the write before the nested item landed"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]),
        events_before,
        "a refused batch wrote to the ledger"
    );
}

#[test]
fn a_transact_over_the_bound_is_refused_naming_the_bound() {
    let fixture = Fixture::new("transact-bound");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXBOUND", "--json"]);

    let stale_items = |count: usize| vec![json!({ "name": "stale", "arguments": {} }); count];

    let refusal = transact_refusal(&fixture, &fixture.main, &stale_items(33));
    assert!(refusal.contains("at most 32"), "{refusal}");
    assert!(
        refusal.contains("33"),
        "the refusal did not say how many were sent: {refusal}"
    );

    // The bound is the bound, not one short of it: 32 runs.
    let envelope = transact_results(&fixture, &fixture.main, &stale_items(32));
    assert_eq!(envelope["ok"], true, "{envelope}");
    let results = envelope["results"].as_array().unwrap();
    assert_eq!(results.len(), 32);
    assert!(
        results.iter().all(|entry| entry["ok"] == true),
        "a batch at the bound did not run: {envelope}"
    );
}

#[test]
fn every_item_of_a_transact_is_authorized_as_if_it_arrived_alone() {
    let fixture = Fixture::new("transact-authz");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXAUTHZ", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );

    // The actor is the item's own, and a batch confers nothing: agent-b
    // holding agent-a's lease token is refused inside a batch exactly as it
    // is on its own, and the refusal rolls agent-a's claim back with it.
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }),
            checkpoint_item(
                "t-1",
                json!({ "$ref": { "item": 0, "path": "/leaseToken" } }),
                "agent-b",
            ),
        ],
    );
    assert_eq!(envelope["failedIndex"], 1, "{envelope}");
    assert_eq!(envelope["rolledBack"], true, "{envelope}");
    let batched = envelope["results"][1]["error"].as_str().unwrap().to_owned();
    assert!(batched.contains("lease belongs to agent-a"), "{batched}");

    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(
        task["claim"],
        Value::Null,
        "the batch kept the claim its own failure rolled back: {task}"
    );

    // The same item alone refuses in the same words, which is the property
    // "authorized as if it arrived alone" means.
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-1", "--as", "agent-a", "--json"],
    );
    let alone = fixture.run(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "agent-b",
            "--summary",
            "did the thing",
            "--intent",
            "do the thing",
            "--next-action",
            "do the next thing",
            "--state",
            "continue",
            "--json",
        ],
    );
    assert!(!alone.status.success(), "the same item alone was accepted");
    assert!(
        String::from_utf8_lossy(&alone.stderr).contains("lease belongs to agent-a"),
        "stderr: {}",
        String::from_utf8_lossy(&alone.stderr)
    );
}

#[test]
fn a_landed_transact_stamps_batch_id_and_batch_index_on_every_event_and_a_rolled_back_one_stamps_none()
 {
    let fixture = Fixture::new("transact-stamps");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXSTAMP", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );

    let landed = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "stamped", "as": "agent-a" } }),
        ],
    );
    assert_eq!(landed["ok"], true, "{landed}");
    let batch_id = landed["batchId"].as_str().unwrap().to_owned();

    let events = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);
    let stamped = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["payload"]["batchId"] == json!(batch_id))
        .map(|event| {
            (
                event["kind"].as_str().unwrap().to_owned(),
                event["payload"]["batchIndex"].clone(),
            )
        })
        .collect::<Vec<_>>();
    // Newest first, so the batch reads backwards: the index is what puts the
    // order back, from the ledger alone.
    assert_eq!(
        stamped,
        vec![
            ("note_added".to_owned(), json!(1)),
            ("task_claimed".to_owned(), json!(0)),
        ],
        "{events}"
    );
    // The events the batch did not append carry no stamp -- including the
    // ones the board open wrote on its way in.
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["payload"]["batchId"] == Value::Null)
            .count()
            >= 2,
        "{events}"
    );

    let rolled_back = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "note", "arguments": { "id": "t-1", "text": "never", "as": "agent-a" } }),
            json!({ "name": "note", "arguments": { "id": "t-absent", "text": "boom", "as": "agent-a" } }),
        ],
    );
    assert_eq!(rolled_back["rolledBack"], true, "{rolled_back}");
    let rolled_back_id = rolled_back["batchId"].as_str().unwrap().to_owned();
    // A batchId in the ledger always means a batch that landed whole.
    assert!(
        fixture
            .ok_json(&fixture.main, &["events", "--limit", "100", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["payload"]["batchId"] != json!(rolled_back_id)),
        "a rolled-back batch stamped the ledger"
    );
}

#[test]
fn the_schema_lists_transact_as_a_writing_operation() {
    let fixture = Fixture::new("transact-schema");
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operation = schema["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|operation| operation["name"] == "transact")
        .unwrap_or_else(|| panic!("the manifest does not publish transact: {schema}"));
    assert_eq!(operation["readOnly"], false);
    assert_eq!(operation["longRunning"], false);
    assert_eq!(operation["positionals"], json!([]));
    assert_eq!(operation["createsBoard"], false);
    // It addresses one board like every other board command, so an adapter
    // may offer all three selectors on it.
    assert_eq!(operation["ignoredSelectors"], json!([]));
    let flags = operation["flags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|flag| (flag["name"].clone(), flag["kind"].clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        flags,
        vec![
            (json!("items"), json!("value")),
            (json!("items-file"), json!("value")),
        ]
    );
}

/// One `kanban batch`, answered as its `results` (docs/specs/batch.md BA-03).
///
/// A batch whose items failed is still a batch that answered, so the exit
/// status is zero whatever the items said, and the envelope carries nothing
/// but `results`.
fn cli_batch_results(fixture: &Fixture, cwd: &Path, items: &[Value]) -> Vec<Value> {
    let list = serde_json::to_string(items).unwrap();
    let output = fixture.run(cwd, &["batch", "--items", &list, "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "kanban batch exited non-zero\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("kanban batch stdout is not JSON: {error}\n{stdout}"));
    let keys = envelope.as_object().unwrap().keys().collect::<Vec<_>>();
    assert_eq!(keys, ["results"], "the envelope grew a field: {envelope}");
    let results = envelope["results"].as_array().unwrap().clone();
    let indices = results
        .iter()
        .map(|entry| entry["index"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        indices,
        (0..items.len())
            .map(|index| json!(index))
            .collect::<Vec<_>>(),
        "results are not one per item, in order: {envelope}"
    );
    results
}

/// The refusal of a `kanban batch` rejected before any of it ran.
fn cli_batch_refusal(fixture: &Fixture, cwd: &Path, items: &[Value]) -> String {
    let list = serde_json::to_string(items).unwrap();
    let output = fixture.run(cwd, &["batch", "--items", &list, "--json"]);
    assert!(
        !output.status.success(),
        "a kanban batch that must be refused was answered: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let answered: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "a refused kanban batch printed no JSON error: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let text = answered["error"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal names itself: {answered}"))
        .to_owned();
    assert!(text.ends_with("nothing in it ran"), "{text}");
    text
}

/// A started MCP session on `cwd`, past `initialize`.
fn mcp_session(fixture: &Fixture, cwd: &Path) -> Session {
    let mut session = Session::start(Path::new(env!("CARGO_BIN_EXE_kanban")), cwd, &fixture.data);
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    session
}

/// BA-01, BA-03, BA-11: the benchmark's read loop, plus one more read to make
/// twelve, through ONE `kanban batch --items-file`, answers read for read what
/// the same twelve calls answer one process each.
#[test]
fn kanban_batch_of_twelve_reads_is_byte_identical_to_twelve_single_calls() {
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/testing/bench/fixture-v2.json");
    let bench: Value = serde_json::from_str(&fs::read_to_string(&fixture_path).unwrap()).unwrap();
    let reads = bench["reads"].as_array().unwrap().clone();
    assert_eq!(reads.len(), 12, "the v2 fixture is no longer twelve reads");

    let fixture = Fixture::new("cli-batch-identity");
    fixture.ok_json(&fixture.main, &["init", "--name", "CLIBATCHID", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/driver", "--as", "geoyws", "--json"],
    );
    let mut task_ids = Vec::new();
    for index in 0..3 {
        let id = format!("t-ident{index}");
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                &format!("identity fixture task {index}"),
                "--id",
                &id,
                "--tag",
                "geoyws/driver",
                "--lane",
                "driver",
                "--json",
            ],
        );
        task_ids.push(id);
    }
    fixture.ok_json(
        &fixture.main,
        &["attention", "raise", "open row", "--as", "geoyws", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "identity fixture sitrep",
            "--as",
            "geoyws",
            "--lane",
            "driver",
            "--json",
        ],
    );

    // The fixture's reads as a lane on this board asks them. `claim
    // --candidates` is the one the batch refuses (its row covers every
    // `claim`, and plain `claim` writes), so `tag_list` stands in as the
    // twelfth. No item names `project`: the batch addresses one board and an
    // item may not name another (docs/specs/batch.md §6), so the board is the
    // working directory's for the batch and for every single call alike.
    let mut planned = Vec::new();
    let mut next_task = 0;
    for read in &reads {
        if read["tool"] == "claim" {
            continue;
        }
        let mut arguments = read["args"].as_object().unwrap().clone();
        if arguments.contains_key("id") {
            arguments.insert("id".into(), json!(task_ids[next_task % task_ids.len()]));
            next_task += 1;
        }
        let drop_keys = read["normalize_drop_keys"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .map(|key| key.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        planned.push((
            read["id"].as_str().unwrap().to_owned(),
            read["tool"].as_str().unwrap().to_owned(),
            Value::Object(arguments),
            drop_keys,
        ));
    }
    planned.push((
        "tag_list".to_owned(),
        "tag_list".to_owned(),
        json!({}),
        Vec::new(),
    ));
    assert_eq!(planned.len(), 12);

    // Every read alone first, each its own `kanban` process: MCP `tools/call`
    // runs the binary once per call with the argument list the batch builds.
    let mut session = mcp_session(&fixture, &fixture.main);
    let mut alone = Vec::new();
    for (index, (id, tool, arguments, _)) in planned.iter().enumerate() {
        let answered = session.ask(json!({
            "jsonrpc": "2.0", "id": 100 + index as i64, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }));
        assert_eq!(
            answered["result"]["isError"],
            false,
            "{id} did not answer on its own: {}",
            tool_text(&answered["result"])
        );
        alone.push(tool_text(&answered["result"]));
    }
    session.finish();

    // The same twelve through one process, the list delivered as a file: the
    // form a lane uses once a list outgrows one argv string (BA-01).
    let items = planned
        .iter()
        .map(|(_, tool, arguments, _)| json!({ "name": tool, "arguments": arguments }))
        .collect::<Vec<_>>();
    let list_path = fixture.main.join("cli-batch-items.json");
    fs::write(&list_path, serde_json::to_string(&items).unwrap()).unwrap();
    let output = fixture.run(
        &fixture.main,
        &[
            "batch",
            "--items-file",
            list_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "kanban batch --items-file failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    let batched = envelope["results"].as_array().unwrap();
    assert_eq!(batched.len(), 12, "{envelope}");

    for (index, (entry, ((id, .., drop_keys), outside))) in batched
        .iter()
        .zip(planned.iter().zip(alone.iter()))
        .enumerate()
    {
        assert_eq!(entry["index"], index, "{id} out of order: {entry}");
        assert_eq!(entry["ok"], true, "{id} failed inside the batch: {entry}");
        let inside = entry["result"].to_string();
        assert_eq!(
            normalized_body(&format!("{id} inside kanban batch"), &inside, drop_keys),
            normalized_body(&format!("{id} on its own"), outside, drop_keys),
            "{id} differed between kanban batch and the same call alone"
        );
        for key in drop_keys {
            assert!(
                volatile_key_present(&inside, key),
                "{id} declares {key} volatile but the batched answer has no such key"
            );
        }
    }

    // Published as a read-only operation, which is what makes adapters and the
    // manifest carry it.
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operation = schema["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|operation| operation["name"] == "batch")
        .unwrap_or_else(|| panic!("the manifest does not publish batch: {schema}"));
    assert_eq!(operation["readOnly"], true, "{operation}");
}

/// BA-03: every item is attempted and answers for itself; one refusal hides
/// no other answer and is not a batch failure.
#[test]
fn kanban_batch_attempts_every_item_and_reports_each_independently() {
    let fixture = Fixture::new("cli-batch-partial");
    fixture.ok_json(&fixture.main, &["init", "--name", "CLIBATCHPART", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "answers", "--id", "t-answers", "--json"],
    );
    let results = cli_batch_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "task_show", "arguments": { "id": "t-answers" } }),
            json!({ "name": "context", "arguments": { "id": "t-absent" } }),
            json!({ "name": "task_show", "arguments": { "id": "t-answers", "bogus": "1" } }),
            json!({ "name": "task_show", "arguments": { "id": "t-answers" } }),
        ],
    );
    assert_eq!(results[0]["ok"], true, "{results:?}");
    assert_eq!(results[0]["result"]["title"], "answers");
    assert_eq!(results[1]["ok"], false, "an unknown task id answered");
    assert!(
        results[1].get("result").is_none(),
        "a failure carried a result"
    );
    let failed = results[1]["error"].as_str().unwrap();
    assert!(failed.contains("t-absent"), "{failed}");
    // An argument the command does not take is that item's refusal, in the
    // words the MCP layer uses for it, not a batch refusal.
    assert_eq!(results[2]["ok"], false);
    assert!(
        results[2]["error"]
            .as_str()
            .unwrap()
            .contains("task_show has no argument bogus"),
        "{results:?}"
    );
    assert_eq!(results[3]["ok"], true, "a read after a failure was skipped");
    assert_eq!(results[3]["result"], results[0]["result"]);
}

/// BA-02: the MCP batch's bound, not a second number. 33 is refused whole,
/// naming both numbers; 32 runs.
#[test]
fn kanban_batch_over_the_bound_is_refused_naming_the_bound() {
    let fixture = Fixture::new("cli-batch-bound");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "CLIBATCHBOUND", "--json"],
    );
    let reads = |count: usize| vec![json!({ "name": "tag_list", "arguments": {} }); count];

    let refusal = cli_batch_refusal(&fixture, &fixture.main, &reads(33));
    assert!(refusal.contains("at most 32"), "{refusal}");
    assert!(refusal.contains("given 33"), "{refusal}");

    let results = cli_batch_results(&fixture, &fixture.main, &reads(32));
    assert_eq!(results.len(), 32);
    assert!(
        results.iter().all(|entry| entry["ok"] == true),
        "a batch at the bound did not run: {results:?}"
    );
}

/// BA-04, BA-05: a write, a nested batcher or an unknown name refuses the
/// whole list before anything runs, naming the index; a write names the fix.
#[test]
fn kanban_batch_refuses_writes_and_nested_batches_and_runs_nothing() {
    let fixture = Fixture::new("cli-batch-refusals");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "CLIBATCHREFUSE", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "already here", "--id", "t-1", "--json"],
    );
    let tasks_before = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);
    let read = json!({ "name": "task_show", "arguments": { "id": "t-1" } });

    // Plain `claim`: its row covers every `claim`, and plain `claim` writes a
    // lease. The read before it would have answered had the batch validated
    // item by item as it ran.
    let claim = cli_batch_refusal(
        &fixture,
        &fixture.main,
        &[
            read.clone(),
            json!({ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }),
        ],
    );
    assert_eq!(
        claim,
        "kanban batch call 1 names claim, which writes, and a batch carries read-only tools \
         only: run writes through kanban transact; nothing in it ran"
    );

    // `transact` is a batcher and a writer: refused, with the same fix.
    let transact = cli_batch_refusal(
        &fixture,
        &fixture.main,
        &[
            read.clone(),
            json!({ "name": "transact", "arguments": { "items": "[]" } }),
        ],
    );
    assert!(
        transact.starts_with("kanban batch call 1 names transact, which writes"),
        "{transact}"
    );
    assert!(
        transact.contains("run writes through kanban transact"),
        "{transact}"
    );

    // A batch may not carry a batch, although `batch` now has a row.
    let nested = cli_batch_refusal(
        &fixture,
        &fixture.main,
        &[
            read.clone(),
            json!({ "name": "batch", "arguments": { "items": "[]" } }),
        ],
    );
    assert_eq!(
        nested,
        "kanban batch call 1 names no such tool batch; nothing in it ran"
    );

    let unknown = cli_batch_refusal(
        &fixture,
        &fixture.main,
        &[read.clone(), json!({ "name": "frobnicate" })],
    );
    assert_eq!(
        unknown,
        "kanban batch call 1 names no such tool frobnicate; nothing in it ran"
    );

    // An item naming a second board: the selector belongs to the batch.
    let selector = cli_batch_refusal(
        &fixture,
        &fixture.main,
        &[json!({ "name": "task_show", "arguments": { "id": "t-1", "project": "ELSEWHERE" } })],
    );
    assert!(
        selector.starts_with("kanban batch call 0 names --project"),
        "{selector}"
    );
    // `--all-boards` too: a batched search would otherwise answer about every
    // registered board, outside the one the batch addresses.
    let all_boards = cli_batch_refusal(
        &fixture,
        &fixture.main,
        &[
            read.clone(),
            json!({ "name": "search", "arguments": { "query": "row", "all-boards": true } }),
        ],
    );
    assert!(
        all_boards.starts_with("kanban batch call 1 names --all-boards"),
        "{all_boards}"
    );

    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]),
        tasks_before,
        "a refused batch changed the row it named"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]),
        events_before,
        "a refused batch wrote to the ledger"
    );
}

/// BA-06 (acceptance A4): a lane's claim-to-release loop is one `transact`,
/// and its `context` read sees the claim the batch has not committed yet; a
/// wrong lease part way rolls the whole loop back.
#[test]
fn a_transact_carries_a_whole_claim_to_release_loop_and_its_read_sees_the_claim() {
    let fixture = Fixture::new("transact-loop");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXLOOP", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-loop", "--json"],
    );
    let lease = || json!({ "$ref": { "item": 0, "path": "/leaseToken" } });
    let loop_items = |checkpoint_lease: Value| {
        vec![
            json!({ "name": "claim", "arguments": { "id": "t-loop", "as": "agent-a" } }),
            json!({ "name": "context", "arguments": { "id": "t-loop" } }),
            checkpoint_item("t-loop", checkpoint_lease, "agent-a"),
            json!({ "name": "note", "arguments": { "id": "t-loop", "text": "loop note", "as": "agent-a" } }),
            json!({ "name": "release", "arguments": { "id": "t-loop", "lease": lease() } }),
        ]
    };

    // A wrong lease at index 2: nothing lands, later items are skipped.
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);
    let failed = transact_results(&fixture, &fixture.main, &loop_items(json!("not-the-lease")));
    assert_eq!(failed["failedIndex"], 2, "{failed}");
    assert_eq!(failed["rolledBack"], true, "{failed}");
    assert_eq!(failed["results"][3]["skipped"], true, "{failed}");
    assert_eq!(failed["results"][4]["skipped"], true, "{failed}");
    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-loop", "--json"]);
    assert_eq!(task["claim"], Value::Null, "{task}");
    assert_eq!(task["notes"], json!([]), "{task}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]),
        events_before,
        "a rolled-back loop wrote to the ledger"
    );

    // The right lease: every item lands, and the read at index 1 carries the
    // claim index 0 wrote before anything committed.
    let landed = transact_results(&fixture, &fixture.main, &loop_items(lease()));
    assert_eq!(landed["ok"], true, "{landed}");
    let claim = &landed["results"][0]["result"];
    let seen = &landed["results"][1]["result"]["claim"];
    assert_eq!(seen["agentID"], "agent-a", "{landed}");
    assert_eq!(seen["claimedAt"], claim["claimedAt"], "{landed}");
    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-loop", "--json"]);
    assert_eq!(
        task["claim"],
        Value::Null,
        "the loop's release did not land: {task}"
    );
    assert_eq!(task["notes"][0]["body"], "loop note", "{task}");
}

/// BA-10: each item keeps its own bands, and a read batch writes nothing.
#[test]
fn kanban_batch_items_keep_their_own_bands_and_the_batch_writes_nothing() {
    let fixture = Fixture::new("cli-batch-bands");
    fixture.ok_json(&fixture.main, &["init", "--name", "CLIBATCHBAND", "--json"]);
    // One more sitrep than `sitrep list --all` answers without `--limit`.
    // `--all`, because posting archives a lane's older sitreps and the
    // current view alone never grows past the band.
    for index in 0..21 {
        fixture.ok_json(
            &fixture.main,
            &[
                "sitrep",
                "post",
                &format!("band sitrep {index}"),
                "--as",
                "geoyws",
                "--lane",
                "driver",
                "--json",
            ],
        );
    }
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "500", "--json"]);
    let audit_before = board_audit(&fixture);

    let results = cli_batch_results(
        &fixture,
        &fixture.main,
        &[
            json!({ "name": "sitrep_list", "arguments": { "all": true } }),
            json!({ "name": "sitrep_list", "arguments": { "all": true, "limit": "21" } }),
            json!({ "name": "tag_list", "arguments": {} }),
        ],
    );
    // The listing over its default band refuses naming `--limit`, inside a
    // batch exactly as alone, rather than passing a first page off as whole.
    assert_eq!(results[0]["ok"], false, "{results:?}");
    let refused = results[0]["error"].as_str().unwrap();
    assert!(refused.contains("--limit"), "{refused}");
    let alone = fixture.run(&fixture.main, &["sitrep", "list", "--all", "--json"]);
    assert!(!alone.status.success());
    assert!(
        String::from_utf8_lossy(&alone.stderr).contains(refused),
        "the batched refusal differs from the same call alone: {refused}"
    );
    // Its siblings answer.
    assert_eq!(results[1]["ok"], true, "{results:?}");
    assert_eq!(results[1]["result"].as_array().unwrap().len(), 21);
    assert_eq!(results[2]["ok"], true, "{results:?}");

    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--limit", "500", "--json"]),
        events_before,
        "a read batch appended to the ledger"
    );
    assert_eq!(
        board_audit(&fixture),
        audit_before,
        "a read batch moved the audit chain"
    );
}

/// BA-12: one pre-flight, so the CLI batch and the MCP batch refuse the same
/// list in the same words, differing only in naming the batcher and where a
/// write goes; and `tools/list` still offers `batch` exactly once.
#[test]
fn kanban_batch_and_mcp_batch_refuse_the_same_list_in_the_same_words() {
    let fixture = Fixture::new("cli-batch-one-parser");
    fixture.ok_json(&fixture.main, &["init", "--name", "CLIBATCHONE", "--json"]);
    let mut session = mcp_session(&fixture, &fixture.main);

    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let offered = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| tool["name"] == "batch")
        .count();
    assert_eq!(offered, 1, "tools/list offers batch {offered} times");

    let lists = [
        vec![json!({ "name": "task_add", "arguments": { "title": "x" } })],
        vec![json!({ "name": "transact", "arguments": {} })],
        vec![json!({ "name": "tag_list" }), json!({ "name": "batch" })],
        vec![json!({ "name": "watch" })],
        vec![json!({ "arguments": {} })],
        vec![json!({ "name": "tag_list", "extra": 1 })],
        vec![json!("tag_list")],
        vec![json!({ "name": "tag_list" }); 33],
    ];
    for (id, list) in lists.iter().enumerate() {
        let mcp = batch_refusal(&mut session, 10 + id as i64, list.clone());
        let cli = cli_batch_refusal(&fixture, &fixture.main, list);
        let expected = format!("kanban {mcp}").replace(
            "run writes through transact;",
            "run writes through kanban transact;",
        );
        assert_eq!(cli, expected, "list {id} is refused in different words");
    }
    session.finish();
}

#[test]
fn a_transact_whose_items_exceed_one_argv_string_still_runs() {
    let fixture = Fixture::new("transact-argv");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXARGV", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );

    // Bodies large enough that the list cannot travel as arguments at all:
    // Linux caps one argv string at 128 KiB (`MAX_ARG_STRLEN`) and macOS caps
    // the whole block at `ARG_MAX`, so 2.5 MB is over both.
    let body = "x".repeat(80_000);
    let items = (0..32)
        .map(|index| {
            json!({ "name": "note", "arguments": {
                "id": "t-1",
                "text": format!("{index}:{body}"),
                "as": "agent-a",
            }})
        })
        .collect::<Vec<_>>();
    let list = serde_json::to_string(&items).unwrap();
    assert!(
        list.len() > 256 * 1024,
        "the list is only {} bytes",
        list.len()
    );

    // `--items` cannot carry it: either the kernel refuses the exec or the
    // binary never sees the argument. Both are the same answer, and the
    // spawn error is not an assertion failure -- it is the ceiling.
    let attempted = fixture
        .command(&fixture.main)
        .args(["transact", "--items", &list, "--json"])
        .output();
    let refused = match &attempted {
        Err(_) => true,
        Ok(output) => !output.status.success(),
    };
    assert!(
        refused,
        "a {} byte item list travelled as one argv string",
        list.len()
    );

    // `--items-file` carries the same list, and every item lands.
    let path = fixture.root.join("items.json");
    fs::write(&path, &list).unwrap();
    let output = fixture.run(
        &fixture.main,
        &["transact", "--items-file", path.to_str().unwrap(), "--json"],
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["ok"], true, "{}", envelope["error"]);
    assert!(output.status.success());
    assert_eq!(envelope["results"].as_array().unwrap().len(), 32);
    let notes = fixture.ok_json(
        &fixture.main,
        &["task", "show", "t-1", "--limit", "40", "--json"],
    );
    assert_eq!(
        notes["notes"].as_array().unwrap().len(),
        32,
        "{}",
        notes["notes"]
    );

    // And the two selectors are two answers to one question.
    let both = fixture.run(
        &fixture.main,
        &[
            "transact",
            "--items",
            "[]",
            "--items-file",
            path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!both.status.success());
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("pass one"),
        "stderr: {}",
        String::from_utf8_lossy(&both.stderr)
    );
}

#[test]
fn add_note_is_atomic_with_its_ledger_event() {
    let fixture = Fixture::new("transact-note-atomic");
    let record = fixture.ok_json(&fixture.main, &["init", "--name", "TXNOTE", "--json"]);
    let board = PathBuf::from(record["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-1",
            "the one that landed",
            "--as",
            "agent-a",
            "--json",
        ],
    );

    // Break the chain head so the ledger append is the statement that fails,
    // AFTER the note row has been written. Until ADR-041 §3.3 `add_note` took
    // no transaction at all: the note committed on its own and the refusal
    // left a note the ledger has no record of -- the one write path on the
    // board that was not atomic even with itself.
    let connection = Connection::open(&board).unwrap();
    let head: (i64, String) = connection
        .query_row(
            "SELECT seq,event_hash FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    connection
        .execute("UPDATE events SET event_hash=NULL WHERE seq=?", [head.0])
        .unwrap();

    let refused = fixture.run(
        &fixture.main,
        &[
            "note",
            "t-1",
            "the one that must not land",
            "--as",
            "agent-a",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "the note was accepted with the ledger unable to record it: {}",
        String::from_utf8_lossy(&refused.stdout)
    );

    // Put the chain back, so the assertions below are about the note and not
    // about the tamper.
    connection
        .execute(
            "UPDATE events SET event_hash=? WHERE seq=?",
            params![head.1, head.0],
        )
        .unwrap();
    drop(connection);

    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    let bodies = task["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["body"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        bodies,
        ["the one that landed"],
        "a note landed without its ledger event"
    );
    let notes_logged = fixture
        .ok_json(
            &fixture.main,
            &["events", "--kind", "note_added", "--limit", "100", "--json"],
        )
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        notes_logged,
        bodies.len(),
        "the notes on the board and the note events in the ledger disagree"
    );
    assert_eq!(board_audit(&fixture)["errors"], json!([]));
}

/// One `tools/call transact` on an open session, answered as the `isError`
/// flag and the envelope its text content carries.
///
/// The two are checked against each other here, so no case has to remember
/// that they are one verdict: a client that reads the flag and a client that
/// reads the envelope must never act differently.
fn mcp_transact(session: &mut Session, id: i64, arguments: Value) -> (bool, Value) {
    let answered = session.ask(json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "transact", "arguments": arguments }
    }));
    let text = tool_text(&answered["result"]);
    let is_error = answered["result"]["isError"]
        .as_bool()
        .unwrap_or_else(|| panic!("a tool result says whether it failed: {answered}"));
    let envelope: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("a transact answers with its envelope ({error}): {text}"));
    assert_eq!(
        is_error,
        envelope["ok"] == json!(false),
        "isError and the envelope's own verdict disagree: {answered}"
    );
    assert!(
        envelope["batchId"]
            .as_str()
            .is_some_and(|id| id.len() == 36),
        "the envelope carries no batch id: {envelope}"
    );
    (is_error, envelope)
}

/// The refusal text of a `tools/call transact` the server answered without
/// ever reaching an envelope.
fn mcp_transact_refusal(session: &mut Session, id: i64, arguments: Value) -> String {
    let answered = session.ask(json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "transact", "arguments": arguments }
    }));
    assert_eq!(
        answered["result"]["isError"], true,
        "a transact that must be refused was answered: {answered}"
    );
    tool_text(&answered["result"])
}

/// A `git` on PATH that records which process invoked it and what that process
/// was running, then hands the call to the real git.
///
/// This is how a spawn is counted. Every provenance-bearing write asks git
/// where it is (`rust/gitctx.rs:65`), and git is a child of whichever kanban
/// process ran that item, so `$PPID` in the stub *is* that process: the
/// distinct pids in the log are the kanban processes that ran items, and `ps`
/// names what each of them was running. One pid whose command line is
/// `transact --items-file …` is one invocation carrying the whole list; five
/// pids would be the per-item spawn ADR-041 §3.4 refuses, which cannot share
/// a transaction.
///
/// Returns the log path and the PATH the session must run with.
fn git_caller_stub(fixture: &Fixture) -> (PathBuf, String) {
    let found = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("look for git");
    assert!(found.status.success(), "no git on PATH to wrap");
    let real = String::from_utf8_lossy(&found.stdout).trim().to_owned();
    let stubs = fixture.root.join("stubs");
    fs::create_dir_all(&stubs).unwrap();
    let log = fixture.root.join("git-callers.log");
    write_executable(
        &stubs.join("git"),
        &format!(
            r#"#!/bin/sh
printf '%s\t%s\n' "$PPID" "$(ps -ww -p "$PPID" -o command= 2>/dev/null)" >> '{log}'
exec '{real}' "$@"
"#,
            log = log.display(),
        ),
    );
    let path = format!(
        "{}:{}",
        stubs.display(),
        env::var("PATH").unwrap_or_default()
    );
    (log, path)
}

/// One line per git invocation, as `(caller pid, caller command line)`.
fn git_callers(log: &Path) -> Vec<(String, String)> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.splitn(2, '\t');
            (
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_tool_list_offers_transact_once_with_read_only_hint_false() {
    let fixture = Fixture::new("mcp-transact-listed");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXLIST", "--json"]);
    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap().clone();

    // Once. `transact` has a `COMMANDS` row, so the generated map would emit
    // one entry and the hand-written schema another -- two descriptions of one
    // tool, and a client keeping the last one it read would send `--items` as
    // a string it serialized itself.
    let named = tools
        .iter()
        .filter(|tool| tool["name"] == "transact")
        .collect::<Vec<_>>();
    assert_eq!(
        named.len(),
        1,
        "transact is advertised {} times: {listed}",
        named.len()
    );
    let transact = named[0];
    assert_eq!(transact["annotations"]["readOnlyHint"], false);
    let properties = &transact["inputSchema"]["properties"];
    assert_eq!(properties["items"]["type"], "array");
    assert_eq!(properties["items"]["maxItems"], 32);
    assert_eq!(transact["inputSchema"]["required"], json!(["items"]));
    assert_eq!(properties["items"]["items"]["required"], json!(["name"]));
    assert_eq!(properties["items"]["items"]["additionalProperties"], false);
    // The entry the row would have generated offers both flags as strings;
    // this one offers the list and keeps the file to itself.
    assert!(
        properties.get("items-file").is_none(),
        "the tool offers the temporary file only the server may name: {transact}"
    );
    // An item may not name a board of its own, so `transact` has to take the
    // selector -- its `ignoredSelectors` is empty for exactly this reason.
    for flag in ["db", "project", "workspace"] {
        assert_eq!(properties[flag]["type"], "string", "--{flag}");
    }

    // The read-only batch is frozen by ADR-041, not extended.
    let batch = tools.iter().find(|tool| tool["name"] == "batch").unwrap();
    assert_eq!(batch["annotations"]["readOnlyHint"], true);

    // And no other name is doubled either, which is what makes `tools/list`
    // and `tools/call` the same set.
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        deduped(names.clone()).len(),
        names.len(),
        "tools/list repeats a name"
    );

    session.finish();
}

#[test]
fn a_transacted_write_is_identical_to_the_same_write_on_its_own() {
    let fixture = Fixture::new("mcp-transact-identity");
    // Two boards holding the same row, so the two writes cannot see each
    // other: the standalone one lands on TXMCPA, the transacted one on TXMCPB.
    fixture.ok_json(&fixture.main, &["init", "--name", "TXMCPA", "--json"]);
    fixture.ok_json(&fixture.worktree, &["init", "--name", "TXMCPB", "--json"]);
    for cwd in [&fixture.main, &fixture.worktree] {
        fixture.ok_json(cwd, &["task", "add", "subject", "--id", "t-1", "--json"]);
    }
    let alone = fixture.ok_json(
        &fixture.main,
        &["note", "t-1", "one", "--as", "agent-a", "--json"],
    );

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    // `project` names the other board, which is also the proof that a
    // selector reaches the one process: no item may carry one.
    let (is_error, envelope) = mcp_transact(
        &mut session,
        2,
        json!({
            "project": "TXMCPB",
            "items": [
                { "name": "note", "arguments": { "id": "t-1", "text": "one", "as": "agent-a" } },
                { "name": "note", "arguments": { "id": "t-1", "text": "two", "as": "agent-a" } },
            ],
        }),
    );
    assert!(!is_error, "{envelope}");
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["failedIndex"], Value::Null, "{envelope}");
    assert_eq!(envelope["rolledBack"], false, "{envelope}");
    assert_eq!(envelope["results"][0]["index"], 0, "{envelope}");
    session.finish();

    // The `tests/e2e.rs:14010` property, for a write: the same command through
    // a transacted tool call and on its own answers with the same bytes.
    // `createdAt` is the only field dropped, and only because two writes
    // cannot share a millisecond by construction -- nothing else in a note is
    // per-write, no id and no sequence number, so everything else must match.
    let volatile = vec!["createdAt".to_owned()];
    let inside = envelope["results"][0]["result"].to_string();
    let outside = alone.to_string();
    for (label, body) in [
        ("the transacted note", &inside),
        ("the note alone", &outside),
    ] {
        assert!(
            volatile_key_present(body, "createdAt"),
            "{label} carries no createdAt, so dropping it asserts nothing: {body}"
        );
    }
    let inside_canonical = normalized_body("the transacted note", &inside, &volatile);
    let outside_canonical = normalized_body("the note alone", &outside, &volatile);
    assert_eq!(
        inside_canonical, outside_canonical,
        "a transacted write answered differently from the same write alone"
    );
    assert_eq!(
        body_digest(&inside_canonical),
        body_digest(&outside_canonical)
    );

    // And the write landed on the board the selector named, both items of it.
    let bodies = |cwd: &Path| {
        fixture.ok_json(cwd, &["task", "show", "t-1", "--json"])["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["body"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(bodies(&fixture.worktree), ["one", "two"]);
    assert_eq!(bodies(&fixture.main), ["one"]);
}

#[test]
fn a_failed_transact_answers_is_error_true_and_carries_the_envelope() {
    let fixture = Fixture::new("mcp-transact-failed");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXMCPFAIL", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let tasks_before = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    // A claim and a note that both succeed, then an item naming a task that is
    // not there: the failure has to arrive as a tool error that still carries
    // how far the batch got, because "it failed" without `failedIndex` and
    // `rolledBack` leaves an agent to guess what is on the board.
    let (is_error, envelope) = mcp_transact(
        &mut session,
        2,
        json!({
            "items": [
                { "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } },
                { "name": "note", "arguments": { "id": "t-1", "text": "in the batch", "as": "agent-a" } },
                { "name": "note", "arguments": { "id": "t-absent", "text": "boom", "as": "agent-a" } },
            ],
        }),
    );
    assert!(
        is_error,
        "a rolled-back transact reported success: {envelope}"
    );
    assert_eq!(envelope["ok"], false, "{envelope}");
    assert_eq!(envelope["failedIndex"], 2, "{envelope}");
    assert_eq!(envelope["rolledBack"], true, "{envelope}");
    // The items before the failure ran and were undone, which is why they
    // report `ok` and the board below reports nothing.
    assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
    assert_eq!(envelope["results"][1]["ok"], true, "{envelope}");
    assert!(
        envelope["results"][2]["error"]
            .as_str()
            .is_some_and(|error| error.contains("t-absent")),
        "{envelope}"
    );
    let batch_id = envelope["batchId"].as_str().unwrap().to_owned();
    session.finish();

    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(task["status"], "todo", "the task did not go back: {task}");
    assert_eq!(task["claim"], Value::Null, "{task}");
    assert_eq!(task["notes"], json!([]), "{task}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        tasks_before,
        "the board changed under a transact that rolled back"
    );
    let events_after = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);
    assert_eq!(
        events_after, events_before,
        "a rolled-back transact left a ledger event"
    );
    assert!(
        events_after
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["payload"]["batchId"] != json!(batch_id)),
        "a rolled-back transact stamped the ledger: {events_after}"
    );
}

#[test]
fn a_transact_runs_the_binary_once_for_the_whole_list() {
    let fixture = Fixture::new("mcp-transact-one-process");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXMCPONE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let (log, path) = git_caller_stub(&fixture);

    let mut session = Session::start_with_env(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
        &[("PATH", path.as_str())],
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let server = session.pid();
    assert!(
        git_callers(&log).is_empty(),
        "the server asked git something before any call, so the count below is not the call's"
    );

    // Five items, four of which capture provenance and therefore run git:
    // claim, checkpoint and heartbeat all ask where they are.
    let (is_error, envelope) = mcp_transact(
        &mut session,
        2,
        json!({
            "items": [
                { "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } },
                { "name": "note", "arguments": { "id": "t-1", "text": "first", "as": "agent-a" } },
                checkpoint_item("t-1", json!({ "$ref": { "item": 0, "path": "/leaseToken" } }), "agent-a"),
                { "name": "heartbeat", "arguments": { "id": "t-1", "lease": { "$ref": { "item": 0, "path": "/leaseToken" } } } },
                { "name": "note", "arguments": { "id": "t-1", "text": "last", "as": "agent-a" } },
            ],
        }),
    );
    assert!(!is_error, "{envelope}");
    assert_eq!(
        envelope["results"].as_array().unwrap().len(),
        5,
        "{envelope}"
    );
    let batch_id = envelope["batchId"].as_str().unwrap().to_owned();
    session.finish();

    // One process ran all five items.
    let callers = git_callers(&log);
    assert!(
        callers.len() >= 3,
        "git never ran, so this counts nothing: {callers:?}"
    );
    let pids = deduped(callers.iter().map(|(pid, _)| pid.clone()).collect());
    assert_eq!(
        pids.len(),
        1,
        "the five items ran in {} processes: {callers:?}",
        pids.len()
    );
    assert_ne!(
        pids[0],
        server.to_string(),
        "the server answered in its own process instead of running the binary"
    );
    let commands = deduped(callers.iter().map(|(_, command)| command.clone()).collect());
    assert_eq!(commands.len(), 1, "{callers:?}");
    let command = &commands[0];
    assert!(command.contains(" transact "), "{command}");
    assert!(command.contains("--items-file"), "{command}");
    assert!(
        !command.contains("--items "),
        "the list travelled as an argv string, which has a ceiling: {command}"
    );

    // And the ledger agrees, from the other side: `batchId` is minted once per
    // `transact` process, so one id across every stamped event is one process,
    // and one transaction is what makes the batch atomic at all.
    let events = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);
    let stamped = deduped(
        events
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["payload"]["batchId"].as_str())
            .map(str::to_owned)
            .collect(),
    );
    assert_eq!(stamped, vec![batch_id], "{events}");

    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(task["claim"]["agentID"], "agent-a", "{task}");
    assert_eq!(task["checkpoints"].as_array().unwrap().len(), 1, "{task}");
    assert_eq!(task["notes"].as_array().unwrap().len(), 2, "{task}");
}

#[test]
fn a_malformed_transact_is_refused_without_running_the_binary() {
    let fixture = Fixture::new("mcp-transact-malformed");
    fixture.ok_json(&fixture.main, &["init", "--name", "TXMCPBAD", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "subject", "--id", "t-1", "--json"],
    );
    let (log, path) = git_caller_stub(&fixture);

    let mut session = Session::start_with_env(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
        &[("PATH", path.as_str())],
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));

    // The binary would refuse each of these in the same words. Refusing them
    // here is the point: a call that is not a list of calls at all has nothing
    // to gain from a process start, which is the cost this tool exists to
    // remove.
    for (index, (arguments, expected)) in [
        (json!({}), "transact needs an items array"),
        (json!({ "items": "[]" }), "items must be an array"),
        (
            json!({ "items": [{ "arguments": { "id": "t-1" } }] }),
            "item 0 needs a name",
        ),
        (
            json!({ "items": [{ "name": "note", "arguments": "id=t-1" }] }),
            "item 0 has arguments that are not an object",
        ),
        (
            json!({ "items": [], "items-file": "/tmp/elsewhere.json" }),
            "transact has no argument items-file",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let refusal = mcp_transact_refusal(&mut session, 10 + index as i64, arguments.clone());
        assert!(refusal.contains(expected), "{arguments}: {refusal}");
        assert!(
            git_callers(&log).is_empty(),
            "{arguments} cost a process start to be refused: {:?}",
            git_callers(&log)
        );
    }

    // A legal list does reach the binary, so the emptiness above is a refusal
    // rather than a stub that never worked.
    let (is_error, envelope) = mcp_transact(
        &mut session,
        20,
        json!({ "items": [{ "name": "claim", "arguments": { "id": "t-1", "as": "agent-a" } }] }),
    );
    assert!(!is_error, "{envelope}");
    session.finish();
    assert!(
        !git_callers(&log).is_empty(),
        "the counting git stub never ran even for a legal transact"
    );

    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(task["claim"]["agentID"], "agent-a", "{task}");
    assert_eq!(task["notes"], json!([]), "a refused transact wrote: {task}");
}

#[test]
fn the_read_only_batch_still_refuses_transact_and_every_other_writing_tool() {
    let fixture = Fixture::new("mcp-batch-refuses-transact");
    fixture.ok_json(&fixture.main, &["init", "--name", "BATCHFROZEN", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "already here", "--id", "t-before", "--json"],
    );
    let before = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let events_before = fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]);

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap().clone();

    // By name, because a batch carrying `transact` would be the whole
    // read-only argument undone in one entry: the batch is safe *because* it
    // refuses a write, and `transact` is the write worth smuggling.
    let refusal = batch_refusal(
        &mut session,
        3,
        vec![
            json!({ "name": "task_list", "arguments": {} }),
            json!({ "name": "transact", "arguments": { "items": [] } }),
        ],
    );
    assert!(refusal.contains("call 1"), "{refusal}");
    assert!(refusal.contains("transact"), "{refusal}");
    assert!(refusal.contains("writes"), "{refusal}");
    assert!(refusal.contains("nothing in it ran"), "{refusal}");

    // And every other writing tool the list advertises, so the refusal is the
    // `COMMANDS` row it reads rather than a list of names kept by hand.
    let writing = tools
        .iter()
        .filter(|tool| tool["annotations"]["readOnlyHint"] == json!(false))
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        writing.contains(&"transact".to_owned()),
        "transact is not advertised as writing, so nothing above was refused for writing"
    );
    assert!(
        writing.len() > 40,
        "only {} writing tools were listed: {writing:?}",
        writing.len()
    );
    for (index, name) in writing.iter().enumerate() {
        let refusal = batch_refusal(
            &mut session,
            100 + index as i64,
            vec![
                json!({ "name": "stale", "arguments": {} }),
                json!({ "name": name, "arguments": {} }),
            ],
        );
        assert!(refusal.contains("call 1"), "{name}: {refusal}");
        assert!(refusal.contains(name.as_str()), "{name}: {refusal}");
        assert!(refusal.contains("writes"), "{name}: {refusal}");
        assert!(refusal.contains("nothing in it ran"), "{name}: {refusal}");
    }

    session.finish();

    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        before,
        "the board changed under batches that were refused whole"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--limit", "100", "--json"]),
        events_before,
        "a refused batch wrote to the ledger"
    );
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
fn handoff_create_refuses_bare_driver_lanes_but_not_non_lane_identities() {
    let fixture = Fixture::new("handoff-lane-target-create");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "HANDOFF-LANES", "--json"],
    );

    for lane in ["driver", "driver-2", "driver-3"] {
        let refused = fixture.run(
            &fixture.main,
            &[
                "handoff",
                "create",
                "--as",
                "outgoing",
                "--to",
                lane,
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
        assert_eq!(
            message,
            format!(
                "handoff target {lane} is a bare lane name; pass the full typed actor as --to @:team/project/{lane}"
            )
        );
        assert!(
            fixture
                .ok_json(&fixture.main, &["handoff", "list", "--json"])
                .as_array()
                .unwrap()
                .is_empty(),
            "a refused bare target wrote a handoff row"
        );
    }

    // These are outside /^driver(?:-[1-9][0-9]*)?$/ and therefore retain the
    // ordinary identity behavior. The fourth value proves the rule is not a
    // blanket refusal of every untyped addressee.
    for target in ["driverless", "driver-two", "driver-0", "incoming"] {
        let created = fixture.ok_json(
            &fixture.main,
            &[
                "handoff",
                "create",
                "--as",
                "outgoing",
                "--to",
                target,
                "--summary",
                "s",
                "--intent",
                "i",
                "--next-action",
                "n",
                "--json",
            ],
        );
        assert_eq!(created["toAgent"], target);
    }
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["handoff", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn typed_lane_actor_accepts_only_its_matching_legacy_bare_lane_target() {
    let fixture = Fixture::new("handoff-lane-target-legacy");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "HANDOFF-LEGACY", "--json"],
    );
    let board = board_path_for_project(&fixture, &fixture.main, "HANDOFF-LEGACY");

    let seed_legacy = |label: &str, target: &str| {
        let created = fixture.ok_json(
            &fixture.main,
            &[
                "handoff",
                "create",
                "--as",
                "outgoing",
                "--to",
                label,
                "--summary",
                "legacy row",
                "--intent",
                "resume it",
                "--next-action",
                "accept it",
                "--json",
            ],
        );
        let id = created["id"].as_str().unwrap().to_owned();
        Connection::open(&board)
            .unwrap()
            .execute(
                "UPDATE handoffs SET to_agent=? WHERE id=?",
                params![target, id],
            )
            .unwrap();
        id
    };

    let matching = seed_legacy("legacy-matching", "driver-2");
    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &matching,
            "--as",
            "@:team/project/driver-2",
            "--json",
        ],
    );
    assert_eq!(accepted["handoff"]["toAgent"], "driver-2");
    assert_eq!(accepted["handoff"]["acceptedBy"], "@:team/project/driver-2");
    assert!(accepted["claim"].is_null());

    let wrong_lane = seed_legacy("legacy-wrong-lane", "driver-2");
    for actor in ["@:team/project/driver-3", "driver-3"] {
        let refused = fixture.run(
            &fixture.main,
            &["handoff", "accept", &wrong_lane, "--as", actor, "--json"],
        );
        assert_eq!(
            refusal_object(&refused),
            format!("handoff {wrong_lane} targets driver-2, not {actor}")
        );
    }

    let typed = seed_legacy("legacy-typed-target", "@:team/project/driver-2");
    let same_lane_other_actor = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &typed,
            "--as",
            "@:other/project/driver-2",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&same_lane_other_actor),
        format!("handoff {typed} targets @:team/project/driver-2, not @:other/project/driver-2")
    );
    let exact = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &typed,
            "--as",
            "@:team/project/driver-2",
            "--json",
        ],
    );
    assert_eq!(exact["handoff"]["toAgent"], "@:team/project/driver-2");
    assert_eq!(exact["handoff"]["acceptedBy"], "@:team/project/driver-2");

    let pending = fixture.ok_json(
        &fixture.main,
        &["handoff", "list", "--status", "pending", "--json"],
    );
    assert_eq!(pending.as_array().unwrap().len(), 1);
    assert_eq!(pending[0]["id"], wrong_lane);
    assert_eq!(pending[0]["toAgent"], "driver-2");
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
            "@:team/project/driver-2",
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
            "handoff",
            "list",
            "--status",
            "pending",
            "--to",
            "@:team/project/driver-2",
            "--json",
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
                &[
                    "handoff",
                    "list",
                    "--to",
                    "@:team/project/driver-1",
                    "--json"
                ]
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
            "@:team/project/driver-2",
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
            "@:team/project/driver-2",
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
    // Since BOARD_V36 the link is plain TEXT with no `ON DELETE SET NULL`:
    // the account keeps the removed task's id — which the read paths gate
    // on — instead of dropping it into board scope.
    assert_eq!(kept["taskID"], "t-1");

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
            "@:team/project/driver-2",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    assert_eq!(created["toAgent"], "@:team/project/driver-2");
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
            "@:team/project/driver-2",
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
    for tag in ["geoyws/infra", "geoyws/ui"] {
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
            "geoyws/infra",
            "--json",
        ],
    );
    assert_eq!(blocking["status"], "open");
    assert_eq!(blocking["taskID"], "t-1");
    assert_eq!(blocking["tags"], json!(["geoyws/infra"]));
    let approval = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "2 commits ready for a production push, which needs George approval",
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
            "geoyws/ui",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    assert_eq!(retagged["tags"], json!(["geoyws/infra", "geoyws/ui"]));
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
    assert_eq!(corrected["tags"], json!(["geoyws/infra", "geoyws/ui"]));
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
        json!(["geoyws/infra", "geoyws/ui"])
    );
    assert_eq!(updates[1]["payload"]["changed"], json!(["tags"]));
    assert_eq!(
        updates[1]["payload"]["previousTags"],
        json!(["geoyws/infra"])
    );
    let infra = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--tag", "geoyws/infra", "--json"],
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
            "--choice",
            "reject",
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
    // The resolution is composed from the choice, not passed in: the label
    // and consequence of the default pair, then the note.
    assert_eq!(
        self_reopened["resolution"],
        "Decision: Reject - do not proceed. The work the body describes does not happen; \
         whoever raised it needs a new plan.\nNote: The raiser withdrew its own item after \
         verification."
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
            "--choice",
            "approve",
            "--note",
            "Probe",
            "--json",
        ],
    );
    assert!(!unauthorized.status.success());
    assert!(String::from_utf8_lossy(&unauthorized.stderr).contains("only geoyws"));
    // A note is optional on an authored choice; a CHOICE never is, because a
    // resolve with no verdict is what ADR-042 removed.
    let missing_choice = fixture.run(
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
    assert!(!missing_choice.status.success());
    assert!(
        String::from_utf8_lossy(&missing_choice.stderr)
            .contains("attention resolve requires --choice KEY")
    );

    // Settling one keeps it: the trail is the feature.
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            approval["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--note",
            "approved and pushed",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["resolvedBy"], "geoyws");
    assert_eq!(
        settled["resolution"],
        "Decision: Approve - proceed. The work the body describes goes ahead as written.\n\
         Note: approved and pushed"
    );
    assert_eq!(settled["decision"]["choice"], "approve");
    assert_eq!(settled["decision"]["outcome"], "approve");
    assert_eq!(settled["decision"]["by"], "geoyws");
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
    assert!(
        reopened["resolution"]
            .as_str()
            .unwrap()
            .starts_with("Decision: Approve - proceed."),
        "the reopened row keeps the resolution it undid: {}",
        reopened["resolution"]
    );
    // The decision left the row and stayed in the ledger.
    assert!(reopened["decision"].is_null());
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
            "--choice",
            "approve",
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
            "--choice",
            "approve",
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
    // Since BOARD_V36 the link is plain TEXT with no `ON DELETE SET NULL`:
    // the orphaned row keeps the removed task's id (which the read paths
    // gate on) instead of reading as board scope.
    assert_eq!(survivor["taskID"], "t-1");
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
            "--choice",
            "approve",
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
    remove_v27_sprint_schema(&connection);
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
    assert_eq!(survivor["tags"], json!(["geoyws/infra", "geoyws/ui"]));
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["projects"][0]["schemaVersion"],
        38
    );
}

/// The card ADR-042 §7 works through, converted from a-347ff24c, as argv.
///
/// One place, because several cases raise the same item and a card is a piece
/// of writing rather than a fixture field. Bounds: question 133 of 160,
/// context 452 of 800, labels 37, 35 and 44 of 60, consequences 130, 108 and
/// 104 of 200, three choices within 2-4, exactly one recommended.
const CARD: [&str; 18] = [
    "--question",
    "hax has no logged-in Claude account, so the pubsub adapter cannot record one real Claude reply - assign a seat, or drop that receipt?",
    "--context",
    "Claude Code 2.1.236 is installed on hax and its dispatcher config loads, but the saved login is revoked and a real turn answers HTTP 401. Only you can finish it: it needs a paid seat and a browser login nobody else can complete. One task is waiting, and nothing is waiting on that task. References: t-8c656910.",
    "--choice",
    "assign-and-login=Assign a Claude seat to hax and log in|approve",
    "--consequence",
    "assign-and-login=You buy or free one Claude seat and finish the browser login: ten minutes of your time plus the seat's monthly cost.",
    "--choice",
    "keep-parked=Keep it parked until a seat frees up|defer",
    "--consequence",
    "keep-parked=Nothing changes and nobody waits on you; a task is filed to re-raise this the day a seat frees up.",
    "--choice",
    "drop-receipt=Drop the live-Claude receipt from the adapter|reject",
    "--consequence",
    "drop-receipt=The adapter is proven against the other providers only and the install task closes as cancelled.",
    "--recommend",
    "assign-and-login",
];

/// What `--choice keep-parked` composes from [`CARD`]: the label, then the
/// consequence.
const KEEP_PARKED: &str = "Decision: Keep it parked until a seat frees up. Nothing changes and \
                           nobody waits on you; a task is filed to re-raise this the day a seat \
                           frees up.";

/// The three choices [`CARD`] declares, exactly as a caller reads them back.
fn card_choices() -> Value {
    json!([
        {
            "key": "assign-and-login",
            "label": "Assign a Claude seat to hax and log in",
            "consequence": "You buy or free one Claude seat and finish the browser login: ten minutes of your time plus the seat's monthly cost.",
            "outcome": "approve",
            "recommended": true,
        },
        {
            "key": "keep-parked",
            "label": "Keep it parked until a seat frees up",
            "consequence": "Nothing changes and nobody waits on you; a task is filed to re-raise this the day a seat frees up.",
            "outcome": "defer",
            "recommended": false,
        },
        {
            "key": "drop-receipt",
            "label": "Drop the live-Claude receipt from the adapter",
            "consequence": "The adapter is proven against the other providers only and the install task closes as cancelled.",
            "outcome": "reject",
            "recommended": false,
        },
    ])
}

/// The pair a row with no authored choices is served as.
fn default_pair() -> Value {
    json!([
        {
            "key": "approve",
            "label": "Approve - proceed",
            "consequence": "The work the body describes goes ahead as written.",
            "outcome": "approve",
            "recommended": false,
        },
        {
            "key": "reject",
            "label": "Reject - do not proceed",
            "consequence": "The work the body describes does not happen; whoever raised it needs a new plan.",
            "outcome": "reject",
            "recommended": false,
        },
    ])
}

/// `attention raise BODY --as ACTOR <extra> --json`.
fn raise_carded(fixture: &Fixture, body: &str, actor: &str, extra: &[&str]) -> Value {
    let mut args = vec!["attention", "raise", body, "--as", actor];
    args.extend_from_slice(extra);
    args.push("--json");
    fixture.ok_json(&fixture.main, &args)
}

/// Return a current fixture to the exact pre-card schema shape before a
/// historical migration test lowers `user_version` below V25.
///
/// The v24 search view indexed the raiser, the body and the resolution and
/// nothing from the card, so its attention arm is put back before the columns
/// go: SQLite re-prepares every trigger after a `DROP COLUMN`, and the three
/// `search_attention_*` triggers read the view by name, so a view still
/// naming `a.question` refuses the drop. The arm is rewritten in place rather
/// than restated, so this fixture cannot drift from the view the ladder
/// actually ships.
fn remove_v25_attention_card_schema(connection: &Connection) {
    let shipped: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='view' AND name='search_source_rows'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let head = "'attention: ' || a.kind,\n";
    let tail = "\n a.status,";
    let start = shipped.find(head).expect("the attention arm's title") + head.len();
    let end = shipped[start..]
        .find(tail)
        .expect("the attention arm's status")
        + start;
    let v24 = format!(
        "{}{}{}",
        &shipped[..start],
        " a.raised_by || char(10) || a.body || char(10) || COALESCE(a.resolution,''),",
        &shipped[end..]
    );
    assert!(
        !v24.contains("a.question") && !v24.contains("a.choices"),
        "the v24 view still names the card: {v24}"
    );
    connection
        .execute_batch(&format!(
            "DROP VIEW search_source_rows;\n{v24};\n\
             ALTER TABLE attention DROP COLUMN question;\
             ALTER TABLE attention DROP COLUMN context;\
             ALTER TABLE attention DROP COLUMN choices;\
             ALTER TABLE attention DROP COLUMN decision;"
        ))
        .unwrap();
}

#[test]
fn an_attention_raised_with_choices_round_trips_every_field_through_list_and_json() {
    let fixture = Fixture::new("card-round-trip");
    fixture.ok_json(&fixture.main, &["init", "--name", "CARD", "--json"]);

    let raised = raise_carded(
        &fixture,
        "PARKED - until an account is assigned to hax. Not engineering.",
        "claude@driver",
        &CARD,
    );
    assert_eq!(raised["question"], CARD[1]);
    assert_eq!(raised["context"], CARD[3]);
    assert_eq!(raised["choices"], card_choices());
    assert!(raised["decision"].is_null(), "an open row has no decision");

    // The listing is the same row, not a second rendering of it.
    let listed = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0], raised);

    // And the four keys are addressable by `--fields`, which validates
    // against the published field list rather than against the row.
    let projected = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--fields",
            "id,question,context,choices,decision",
            "--json",
        ],
    );
    let keys = projected[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(keys, ["choices", "context", "decision", "id", "question"]);
    assert_eq!(projected[0]["choices"], card_choices());

    // The card survives an update that rewrites only the body, and an update
    // may replace the choices without touching the question.
    let rebodied = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            raised["id"].as_str().unwrap(),
            "--as",
            "claude@driver",
            "--body",
            "PARKED - still waiting on a seat.",
            "--json",
        ],
    );
    assert_eq!(rebodied["question"], CARD[1]);
    assert_eq!(rebodied["choices"], card_choices());

    let recarded = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            raised["id"].as_str().unwrap(),
            "--as",
            "claude@driver",
            "--choice",
            "assign-and-login=Assign a Claude seat to hax and log in|approve",
            "--consequence",
            "assign-and-login=One seat is bought and the login is finished today.",
            "--choice",
            "wait=Wait for the 2026-10-01 renewal|defer",
            "--consequence",
            "wait=Nothing changes until 2026-10-01, when this is re-raised.",
            "--recommend",
            "assign-and-login",
            "--json",
        ],
    );
    assert_eq!(
        recarded["question"], CARD[1],
        "replacing the choices must not clear the question"
    );
    assert_eq!(recarded["choices"].as_array().unwrap().len(), 2);
    assert_eq!(recarded["choices"][1]["key"], "wait");

    // `--clear-card` is the one way back to the default pair.
    let cleared = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            raised["id"].as_str().unwrap(),
            "--as",
            "claude@driver",
            "--clear-card",
            "--json",
        ],
    );
    assert!(cleared["question"].is_null());
    assert!(cleared["context"].is_null());
    assert_eq!(cleared["choices"], default_pair());

    // The ledger kept every superseded card.
    let updates = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_updated", "--json"],
    );
    let cleared_event = updates
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["payload"]["previousChoices"][1]["key"] == "wait")
        .expect("the clearing update kept the card it removed");
    assert_eq!(cleared_event["payload"]["changed"], json!(["card"]));
    assert_eq!(cleared_event["payload"]["previousQuestion"], CARD[1]);
}

#[test]
fn native_attention_check_round_trips_rewrites_redacts_and_refuses_atomically() {
    let fixture = Fixture::new("native-acc-definition");
    fixture.ok_json(&fixture.main, &["init", "--name", "ACC", "--json"]);
    let live_board = board_path_for_project(&fixture, &fixture.main, "ACC");
    let raised = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Choose after demonstrating the Store boundary.",
            "--as",
            "claude@driver",
            "--check",
            "Where is the attention check definition validated?",
            "--check-choice",
            "store=The Store validates it",
            "--check-choice",
            "client=The client validates it",
            "--check-answer",
            "store",
            "--check-explain",
            "SECRET_EXPLANATION rust/store.rs validates the complete definition before writing.",
            "--check-about",
            "rust/store.rs",
            "--json",
        ],
    );
    let id = raised["id"].as_str().unwrap();
    assert_eq!(
        raised["check"]["question"],
        "Where is the attention check definition validated?"
    );
    assert_eq!(raised["check"]["answer"], "store");
    assert_eq!(raised["check"]["choices"].as_array().unwrap().len(), 2);
    assert!(raised["check"]["choices"][0].get("outcome").is_none());
    assert!(raised["check"]["choices"][0].get("recommended").is_none());

    let shown = fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]);
    assert!(shown["check"].get("answer").is_none());
    assert!(shown["check"].get("explanation").is_none());
    let listed = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert!(listed[0]["check"].get("answer").is_none());
    assert!(listed[0]["check"].get("explanation").is_none());
    let other = fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]);
    assert_eq!(other, shown);
    let events = fixture.ok_json(&fixture.main, &["events", "--json"]);
    assert!(!events.to_string().contains("SECRET_EXPLANATION"));

    let before = shown.clone();
    let events_before = events.as_array().unwrap().len();
    let refused = fixture.run(
        &fixture.main,
        &[
            "attention",
            "update",
            id,
            "--as",
            "claude@driver",
            "--check",
            "Which layer owns the definition?",
            "--check-choice",
            "store=The Store owns it",
            "--check-choice",
            "client=The client owns it",
            "--check-answer",
            "store",
            "--check-explain",
            "rust/store.rs owns the write invariant.",
            "--check-about",
            "not a subject",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert!(refusal_object(&refused).contains("`about` not matching"));
    assert_eq!(
        fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]),
        before
    );
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["events", "--json"])
            .as_array()
            .unwrap()
            .len(),
        events_before
    );

    let non_raiser = fixture.run(
        &fixture.main,
        &[
            "attention",
            "update",
            id,
            "--as",
            "geoyws",
            "--check",
            "malformed and incomplete",
            "--json",
        ],
    );
    assert!(!non_raiser.status.success());
    assert!(refusal_object(&non_raiser).contains("raiser claude@driver"));
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["events", "--json"])
            .as_array()
            .unwrap()
            .len(),
        events_before,
        "malformed non-raiser input wrote an event"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]),
        before,
        "malformed non-raiser input changed the row"
    );

    // ACC-05: another lane is refused the same way, naming the raiser, and
    // writes nothing either.
    let other_lane = fixture.run(
        &fixture.main,
        &[
            "attention",
            "update",
            id,
            "--as",
            "somebody@driver-2",
            "--check",
            "malformed and incomplete",
            "--json",
        ],
    );
    assert!(!other_lane.status.success());
    assert!(refusal_object(&other_lane).contains("raiser claude@driver"));
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["events", "--json"])
            .as_array()
            .unwrap()
            .len(),
        events_before,
        "another lane's check input wrote an event"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]),
        before,
        "another lane's check input changed the row"
    );

    // ACC-05: resolve takes only --check-answered, never a check definition.
    // A definition flag on resolve is refused before any write.
    let resolve_definition = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--check",
            "Which layer owns the definition?",
            "--json",
        ],
    );
    assert!(!resolve_definition.status.success());
    assert!(
        refusal_object(&resolve_definition).contains("unknown flag --check"),
        "resolve must not accept a check definition"
    );
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["events", "--json"])
            .as_array()
            .unwrap()
            .len(),
        events_before,
        "a resolve carrying a definition wrote an event"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]),
        before,
        "a resolve carrying a definition changed the row"
    );

    let rewritten = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            id,
            "--as",
            "claude@driver",
            "--check",
            "Which layer owns the definition?",
            "--check-choice",
            "store=The Store owns it",
            "--check-choice",
            "client=The client owns it",
            "--check-answer",
            "store",
            "--check-explain",
            "SECRET_RECEIPT rust/store.rs owns the write invariant.",
            "--check-about",
            "rust/store.rs",
            "--json",
        ],
    );
    assert_eq!(
        rewritten["check"]["question"],
        "Which layer owns the definition?"
    );
    assert!(rewritten["check"].get("answer").is_none());
    assert!(rewritten["check"].get("explanation").is_none());
    let rewritten_shown = fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]);
    assert_eq!(rewritten_shown["check"], rewritten["check"]);
    let stored_definition: (String, String) = Connection::open(&live_board)
        .unwrap()
        .query_row(
            "SELECT check_answer,check_explanation FROM attention WHERE id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_definition.0, "store");
    assert!(stored_definition.1.contains("SECRET_RECEIPT"));
    assert!(
        !fixture
            .ok_json(&fixture.main, &["events", "--json"])
            .to_string()
            .contains("SECRET_RECEIPT")
    );

    let backup = fixture.root.join("acc-backup");
    let receipt = fixture.ok_json(
        &fixture.main,
        &["backup", "--output", backup.to_str().unwrap(), "--json"],
    );
    let board = receipt["boards"].as_array().unwrap()[0].as_str().unwrap();
    let backed_up = fixture.ok_json(
        &fixture.main,
        &["attention", "show", id, "--db", board, "--json"],
    );
    assert_eq!(backed_up["check"], rewritten_shown["check"]);
    let backed_definition: (String, String) = Connection::open(board)
        .unwrap()
        .query_row(
            "SELECT check_answer,check_explanation FROM attention WHERE id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(backed_definition, stored_definition);
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            id,
            "--as",
            "claude@driver",
            "--body",
            "Mutation after the backup.",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "restore",
            "--from",
            backup.to_str().unwrap(),
            "--force",
            "--as",
            "tester",
            "--json",
        ],
    );
    let restored = fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]);
    assert_eq!(restored["check"], rewritten_shown["check"]);
    assert_eq!(restored["body"], rewritten_shown["body"]);
    let restored_definition: (String, String) = Connection::open(&live_board)
        .unwrap()
        .query_row(
            "SELECT check_answer,check_explanation FROM attention WHERE id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(restored_definition, stored_definition);
    // The answer is `store`, so this resolve both records it and reveals the
    // definition the moment the answer stands.
    let resolved = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--check-answered",
            "store",
            "--json",
        ],
    );
    assert_eq!(resolved["check"]["answered"], "store");
    assert_eq!(resolved["check"]["correct"], true);
    assert!(resolved["check"]["answeredAt"].as_i64().is_some());
    assert_eq!(resolved["check"]["answer"], "store");
    assert!(
        resolved["check"]["explanation"]
            .as_str()
            .unwrap()
            .contains("SECRET_RECEIPT")
    );
    assert!(
        resolved["resolution"]
            .as_str()
            .unwrap()
            .contains("ACC: pass")
    );
    // The reveal is a read law, not a receipt fluke.
    let shown_after = fixture.ok_json(&fixture.main, &["attention", "show", id, "--json"]);
    assert_eq!(shown_after["check"]["answer"], "store");
    assert!(
        shown_after["check"]["explanation"]
            .as_str()
            .unwrap()
            .contains("SECRET_RECEIPT")
    );
    // ACC-05: reopen clears the recorded result with the decision, so the
    // read redacts again and the next resolution answers again.
    let reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            id,
            "--as",
            "geoyws",
            "--note",
            "Undo for redaction proof.",
            "--json",
        ],
    );
    assert!(reopened["check"].get("answer").is_none());
    assert!(reopened["check"].get("explanation").is_none());
    assert!(reopened["check"].get("answered").is_none());
    assert!(!reopened.to_string().contains("SECRET_RECEIPT"));
}

#[test]
fn native_check_refuses_a_diagnosis_shaped_raise_then_accepts_the_rewrite() {
    let fixture = Fixture::new("native-acc-diagnosis");
    fixture.ok_json(&fixture.main, &["init", "--name", "ACC-DIAG", "--json"]);
    // A typed row ID in the question is a finding read back, not teaching.
    let refused = fixture.run(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Choose after demonstrating the Store boundary.",
            "--as",
            "claude@driver",
            "--check",
            "How does t-1234abcd behave?",
            "--check-choice",
            "store=The Store validates it",
            "--check-choice",
            "client=The client validates it",
            "--check-answer",
            "store",
            "--check-explain",
            "The Store component enforces it.",
            "--check-about",
            "rust/store.rs",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    let refusal = refusal_object(&refused);
    assert!(refusal.contains("t-1234abcd"), "{refusal}");
    assert!(
        refusal.contains(
            "a check teaches how the system works; it never reads this row's finding back."
        ),
        "{refusal}"
    );
    let listed = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert!(
        listed.as_array().unwrap().is_empty(),
        "a refused diagnosis-shaped raise still wrote: {listed}"
    );
    // The same raise rewritten to teach the system rule is accepted.
    let raised = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Choose after demonstrating the Store boundary.",
            "--as",
            "claude@driver",
            "--check",
            "Where is the attention check definition validated?",
            "--check-choice",
            "store=The Store validates it",
            "--check-choice",
            "client=The client validates it",
            "--check-answer",
            "store",
            "--check-explain",
            "The Store component enforces it.",
            "--check-about",
            "rust/store.rs",
            "--json",
        ],
    );
    assert_eq!(
        raised["check"]["question"],
        "Where is the attention check definition validated?"
    );
    assert_eq!(raised["check"]["about"], "rust/store.rs");
}

#[test]
fn schema_30_migrates_once_to_native_check_columns_without_inventing_a_check() {
    let fixture = Fixture::new("native-acc-migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "ACC-MIGRATE", "--json"]);
    let old = raise_carded(&fixture, "An existing no-check row.", "claude@driver", &[]);
    let board = board_path_for_project(&fixture, &fixture.main, "ACC-MIGRATE");
    {
        let connection = Connection::open(&board).unwrap();
        let mut sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='attention'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        for declaration in [
            " check_question TEXT CHECK(check_question IS NULL OR length(check_question) BETWEEN 1 AND 160),\n",
            " check_choices TEXT CHECK(check_choices IS NULL OR (json_valid(check_choices) AND json_type(check_choices)='array'\n   AND json_array_length(check_choices) BETWEEN 2 AND 4)),\n",
            " check_answer TEXT,\n",
            " check_explanation TEXT CHECK(check_explanation IS NULL OR length(check_explanation) BETWEEN 1 AND 400),\n",
            // v33 rebuilds the table, so definition and result columns stand
            // on their own lines now (v32's ALTERs had landed them on the
            // check_about line); a simulated v30 board strips all eight.
            // v34's and v35's ALTERs append lane and return_trigger to the
            // last column definition line, so the answered_at strip carries
            // both: the simulated v30 board must not contain either column,
            // or the v34/v35 reruns would meet a duplicate column instead of
            // an empty slot.
            " check_about TEXT,\n",
            " check_answered TEXT,\n",
            " check_correct INTEGER CHECK(check_correct IS NULL OR check_correct IN (0,1)),\n",
            " check_answered_at INTEGER, lane TEXT, return_trigger TEXT,\n",
            " CHECK(\n   (check_question IS NULL AND check_choices IS NULL AND check_answer IS NULL\n    AND check_explanation IS NULL AND check_about IS NULL)\n   OR\n   (check_question IS NOT NULL AND check_choices IS NOT NULL AND check_answer IS NOT NULL\n    AND check_explanation IS NOT NULL AND check_about IS NOT NULL)\n ),\n",
        ] {
            let without = sql.replace(declaration, "");
            assert_ne!(without, sql, "missing v31 declaration: {declaration}");
            sql = without;
        }
        connection
            .pragma_update(None, "writable_schema", true)
            .unwrap();
        connection
            .execute(
                "UPDATE sqlite_master SET sql=? WHERE type='table' AND name='attention'",
                [sql],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 30).unwrap();
        connection
            .pragma_update(None, "writable_schema", false)
            .unwrap();
    }
    let first = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(first[0]["id"], old["id"]);
    assert!(first[0].get("check").is_none());
    let second = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(
        second, first,
        "opening an already migrated board must be stable"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["projects"][0]["schemaVersion"],
        38
    );
    let checked = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "A check must survive a rewound version.",
            "--as",
            "claude@driver",
            "--check",
            "Where does schema migration run?",
            "--check-choice",
            "db=The database module",
            "--check-choice",
            "ui=The browser module",
            "--check-answer",
            "db",
            "--check-explain",
            "SECRET_RERUN rust/db.rs owns the migration ladder.",
            "--check-about",
            "rust/db.rs",
            "--json",
        ],
    );
    let checked_id = checked["id"].as_str().unwrap();
    {
        let connection = Connection::open(&board).unwrap();
        connection
            .execute_batch(
                "PRAGMA writable_schema=ON;\n                 UPDATE sqlite_master SET sql=replace(sql,\n                   'length(check_question) BETWEEN 1 AND 160',\n                   'length(check_question) BETWEEN 1 AND 159')\n                 WHERE type='table' AND name='attention';\n                 PRAGMA user_version=30;\n                 PRAGMA writable_schema=OFF;",
            )
            .unwrap();
    }
    let rerun = fixture.ok_json(&fixture.main, &["attention", "show", checked_id, "--json"]);
    assert!(rerun["check"].get("answer").is_none());
    assert!(rerun["check"].get("explanation").is_none());
    let rerun_definition: (String, String) = Connection::open(&board)
        .unwrap()
        .query_row(
            "SELECT check_answer,check_explanation FROM attention WHERE id=?",
            [checked_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(rerun_definition.0, "db");
    assert!(rerun_definition.1.contains("SECRET_RERUN"));
    let attention_sql: String = Connection::open(&board)
        .unwrap()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='attention'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(attention_sql.contains("length(check_question) BETWEEN 1 AND 160"));
    assert!(attention_sql.contains("json_array_length(check_choices) BETWEEN 2 AND 4"));
    assert!(attention_sql.contains("length(check_explanation) BETWEEN 1 AND 400"));
    assert!(
        attention_sql
            .contains("check_question IS NULL AND check_choices IS NULL AND check_answer IS NULL")
    );
    assert!(attention_sql.contains(
        "check_question IS NOT NULL AND check_choices IS NOT NULL AND check_answer IS NOT NULL"
    ));
}

#[test]
fn resolve_records_the_native_check_answer_as_data_across_the_three_paths() {
    let fixture = Fixture::new("native-acc-answer-cli");
    fixture.ok_json(&fixture.main, &["init", "--name", "ACC-ANSWER", "--json"]);
    let board = board_path_for_project(&fixture, &fixture.main, "ACC-ANSWER");
    let checked = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Three resolve paths, one recorded answer each.",
            "--as",
            "claude@driver",
            "--check",
            "Where does the one check answer live after a resolve?",
            "--check-choice",
            "fields=Stored fields on the attention row",
            "--check-choice",
            "notes=Note text on the resolution",
            "--check-answer",
            "fields",
            "--check-explain",
            "SECRET_JOURNEY rust/store.rs records answered, correct and answeredAt as columns.",
            "--check-about",
            "rust/store.rs",
            "--json",
        ],
    );
    let checked_id = checked["id"].as_str().unwrap();
    let events_before = fixture
        .ok_json(&fixture.main, &["events", "--json"])
        .as_array()
        .unwrap()
        .len();

    // An undeclared key is refused before any result or resolution write.
    let undeclared = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            checked_id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--check-answered",
            "nope",
            "--json",
        ],
    );
    assert!(!undeclared.status.success());
    assert!(
        refusal_object(&undeclared).contains("names no --check-choice"),
        "undeclared key refused by name"
    );
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["events", "--json"])
            .as_array()
            .unwrap()
            .len(),
        events_before
    );

    // Path two: a wrong declared key resolves and records the miss.
    let miss = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            checked_id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--check-answered",
            "notes",
            "--json",
        ],
    );
    assert_eq!(miss["status"], "resolved");
    assert_eq!(miss["check"]["answered"], "notes");
    assert_eq!(miss["check"]["correct"], false);
    assert!(miss["check"]["answeredAt"].as_i64().is_some());
    assert_eq!(miss["check"]["answer"], "fields");
    assert!(
        miss["check"]["explanation"]
            .as_str()
            .unwrap()
            .contains("SECRET_JOURNEY")
    );
    assert!(
        miss["resolution"]
            .as_str()
            .unwrap()
            .contains("ACC: miss on notes"),
        "the note echo agrees with the stored fields"
    );
    let resolved_events = fixture.ok_json(&fixture.main, &["events", "--json"]);
    let resolved_event = resolved_events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "attention_resolved")
        .unwrap();
    assert_eq!(
        resolved_event["payload"]["checkResult"]["answered"],
        "notes"
    );
    assert_eq!(resolved_event["payload"]["checkResult"]["correct"], false);
    assert!(
        !resolved_events.to_string().contains("SECRET_JOURNEY"),
        "the event ledger never carries the explanation"
    );
    // The miss survives a reopen: the answer is recorded once, never retried,
    // and the reopen clears it with the decision.
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            checked_id,
            "--as",
            "geoyws",
            "--note",
            "Journey continues.",
            "--json",
        ],
    );

    // Path three: the right key records the pass.
    let pass = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            checked_id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--check-answered",
            "fields",
            "--json",
        ],
    );
    assert_eq!(pass["check"]["answered"], "fields");
    assert_eq!(pass["check"]["correct"], true);
    assert!(
        pass["resolution"].as_str().unwrap().contains("ACC: pass"),
        "the note echo agrees with the stored fields"
    );
    // The stored columns agree with the served projection.
    let stored: (Option<String>, Option<i64>) = Connection::open(&board)
        .unwrap()
        .query_row(
            "SELECT check_answered,check_correct FROM attention WHERE id=?",
            [checked_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, (Some("fields".into()), Some(1)));

    // A row with no check refuses the flag by name and resolves as before.
    let plain = raise_carded(&fixture, "An ordinary card.", "claude@driver", &[]);
    let plain_id = plain["id"].as_str().unwrap();
    let flagged = fixture.run(
        &fixture.main,
        &[
            "attention",
            "resolve",
            plain_id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--check-answered",
            "fields",
            "--json",
        ],
    );
    assert!(!flagged.status.success());
    let flagged_refusal = refusal_object(&flagged);
    assert!(
        flagged_refusal.contains("carries no comprehension check"),
        "{flagged_refusal}"
    );
    let plain_resolved = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            plain_id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--json",
        ],
    );
    assert_eq!(plain_resolved["status"], "resolved");
    assert!(plain_resolved.get("check").is_none());
}

/// ACC-06 as amended, ACC-20 and ACC-21 (t-1aa9f553): a bare resolve settles
/// a checked row and leaves the check pending; `attention check` answers it
/// once whether the row is open or resolved, through the store law the web
/// route shares, and its receipt teaches — the verdict, the correct key with
/// its label and the explanation, for a pass as much as a miss.
#[test]
fn attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits() {
    let fixture = Fixture::new("native-acc-check-verb");
    fixture.ok_json(&fixture.main, &["init", "--name", "ACC-CHECK", "--json"]);
    let raise_checked = |body: &str| -> String {
        raise_carded(
            &fixture,
            body,
            "claude@driver",
            &[
                "--check",
                "Where does the one check answer live?",
                "--check-choice",
                "fields=Stored fields on the attention row",
                "--check-choice",
                "notes=Note text on the resolution",
                "--check-answer",
                "fields",
                "--check-explain",
                "SECRET_TEACH rust/store.rs records answered, correct and answeredAt as columns.",
                "--check-about",
                "rust/store.rs",
            ],
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let assert_pending = |check: &Value| {
        assert_eq!(check["question"], "Where does the one check answer live?");
        assert_eq!(check["about"], "rust/store.rs");
        for hidden in ["answer", "explanation", "answered", "correct", "answeredAt"] {
            assert!(check.get(hidden).is_none(), "{hidden} leaked: {check}");
        }
    };

    // (a) A bare resolve settles the row; the check stays pending, redacted
    // on the receipt, on show and on the resolved listing, and no echo is
    // written because nothing was answered.
    let later = raise_checked("Settled now, answered later.");
    let resolved = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &later,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--json",
        ],
    );
    assert_eq!(resolved["status"], "resolved");
    assert_pending(&resolved["check"]);
    assert!(!resolved["resolution"].as_str().unwrap().contains("ACC:"));
    assert_pending(
        &fixture.ok_json(&fixture.main, &["attention", "show", &later, "--json"])["check"],
    );
    let listed = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--status",
            "resolved",
            "--limit",
            "500",
            "--json",
        ],
    );
    let listed_row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == later.as_str())
        .unwrap();
    assert_pending(&listed_row["check"]);
    assert!(!listed.to_string().contains("SECRET_TEACH"));

    // (b) The resolved row accepts its one answer; the receipt carries the
    // result beside the correct key and the explanation, the row stays
    // resolved, and the ledger records the answer without the explanation.
    let miss = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "check",
            &later,
            "--as",
            "geoyws",
            "--key",
            "notes",
            "--json",
        ],
    );
    assert_eq!(miss["status"], "resolved");
    assert_eq!(miss["check"]["answered"], "notes");
    assert_eq!(miss["check"]["correct"], false);
    assert_eq!(miss["check"]["answer"], "fields");
    assert!(miss["check"]["answeredAt"].as_i64().is_some());
    assert!(
        miss["check"]["explanation"]
            .as_str()
            .unwrap()
            .contains("SECRET_TEACH")
    );
    assert_eq!(miss["resolution"], resolved["resolution"]);
    let events = fixture.ok_json(&fixture.main, &["events", "--json"]);
    let answered_event = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "attention_check_answered")
        .unwrap();
    assert_eq!(answered_event["payload"]["answered"], "notes");
    assert_eq!(answered_event["payload"]["correct"], false);
    assert!(!events.to_string().contains("SECRET_TEACH"));
    // A second answer — the same key or another — is refused unchanged.
    let before = attention_and_chain(&fixture);
    for key in ["fields", "notes"] {
        let again = attention_refusal(&fixture, &["check", &later, "--as", "geoyws", "--key", key]);
        assert!(again.contains("exactly one answer"), "{again}");
    }
    assert_eq!(attention_and_chain(&fixture), before);

    // (e) Reopen clears the result; the check is answerable again, and the
    // human receipt teaches on a pass too.
    let reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            &later,
            "--as",
            "geoyws",
            "--note",
            "Ask again.",
            "--json",
        ],
    );
    assert_pending(&reopened["check"]);
    let text = fixture.run(
        &fixture.main,
        &[
            "attention",
            "check",
            &later,
            "--as",
            "geoyws",
            "--key",
            "fields",
        ],
    );
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        "ACC: pass\nanswer: fields — Stored fields on the attention row\nwhy: SECRET_TEACH \
         rust/store.rs records answered, correct and answeredAt as columns.\n"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["attention", "show", &later, "--json"])["status"],
        "open"
    );

    // (c) On an open row the raiser's answer leaves it open; a later bare
    // resolve reuses the recorded result and echoes nothing.
    let open = raise_checked("Answered first, settled after.");
    let answered = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "check",
            &open,
            "--as",
            "claude@driver",
            "--key",
            "notes",
            "--json",
        ],
    );
    assert_eq!(answered["status"], "open");
    assert_eq!(answered["check"]["correct"], false);
    let miss_text = fixture.run(
        &fixture.main,
        &[
            "attention",
            "check",
            &open,
            "--as",
            "geoyws",
            "--key",
            "fields",
        ],
    );
    assert!(
        !miss_text.status.success(),
        "the text path refuses a second answer too"
    );
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &open,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["check"]["answered"], "notes");
    assert!(!settled["resolution"].as_str().unwrap().contains("ACC:"));

    // (d) Refusals write nothing: no check, an undeclared key, and an actor
    // who is neither the operator nor the raiser.
    let plain = raise_carded(&fixture, "No check here.", "claude@driver", &[]);
    let plain = plain["id"].as_str().unwrap();
    let pending = raise_checked("Still pending.");
    let before = attention_and_chain(&fixture);
    let no_check = attention_refusal(
        &fixture,
        &["check", plain, "--as", "geoyws", "--key", "fields"],
    );
    assert!(
        no_check.contains("carries no comprehension check"),
        "{no_check}"
    );
    let undeclared = attention_refusal(
        &fixture,
        &["check", &pending, "--as", "geoyws", "--key", "nope"],
    );
    assert!(
        undeclared.contains("names no --check-choice"),
        "{undeclared}"
    );
    let stranger = attention_refusal(
        &fixture,
        &[
            "check",
            &pending,
            "--as",
            "codex@driver-2",
            "--key",
            "fields",
        ],
    );
    assert!(
        stranger.contains("only geoyws or that same raiser may answer its check"),
        "{stranger}"
    );
    assert!(!stranger.contains("SECRET_TEACH"), "{stranger}");
    assert_eq!(attention_and_chain(&fixture), before);
}

#[test]
fn an_attention_raised_without_choices_reads_as_the_default_approve_reject_pair_with_no_recommendation()
 {
    let fixture = Fixture::new("card-default-pair");
    fixture.ok_json(&fixture.main, &["init", "--name", "PAIR", "--json"]);

    let raised = raise_carded(
        &fixture,
        "Two commits are ready for staging.",
        "geoyws",
        &[],
    );
    assert!(raised["question"].is_null(), "the body serves as both");
    assert!(raised["context"].is_null());
    assert_eq!(raised["choices"], default_pair());
    assert_eq!(
        raised["choices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|choice| choice["recommended"] == json!(true))
            .count(),
        0,
        "nobody authored this pair, so nothing may be marked"
    );

    // NULL stays NULL in storage, so a backfilled row is distinguishable
    // from a never-authored one for as long as any remain.
    let board = board_path_for_project(&fixture, &fixture.main, "PAIR");
    let stored: Option<String> = Connection::open(&board)
        .unwrap()
        .query_row("SELECT choices FROM attention", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stored, None, "the default pair must not be written down");

    // And the pair is answerable: both keys resolve.
    let resolved = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            raised["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "reject",
            "--json",
        ],
    );
    assert_eq!(resolved["decision"]["choice"], "reject");
    assert_eq!(resolved["decision"]["outcome"], "reject");
    assert_eq!(resolved["resolution"], REJECT);
}

#[test]
fn resolving_with_a_choice_records_the_decision_object_and_composes_the_resolution_text() {
    let fixture = Fixture::new("card-decision");
    fixture.ok_json(&fixture.main, &["init", "--name", "DECIDE", "--json"]);
    let first = raise_carded(&fixture, "The P0 body.", "claude@driver", &CARD);
    let second = raise_carded(&fixture, "The second P0 body.", "claude@driver", &CARD);

    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            first["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "keep-parked",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["decision"]["choice"], "keep-parked");
    assert_eq!(settled["decision"]["outcome"], "defer");
    assert!(
        settled["decision"]["note"].is_null(),
        "a note is optional on an authored choice"
    );
    assert_eq!(settled["decision"]["by"], "geoyws");
    assert_eq!(settled["decision"]["at"], settled["resolvedAt"]);
    assert_eq!(settled["resolution"], KEEP_PARKED);

    // A note rides on a second line, and only when one was given.
    let noted = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            second["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "assign-and-login",
            "--note",
            "Buying the seat this afternoon.",
            "--json",
        ],
    );
    assert_eq!(noted["decision"]["outcome"], "approve");
    assert_eq!(noted["decision"]["note"], "Buying the seat this afternoon.");
    assert_eq!(
        noted["resolution"],
        "Decision: Assign a Claude seat to hax and log in. You buy or free one Claude seat and \
         finish the browser login: ten minutes of your time plus the seat's monthly cost.\n\
         Note: Buying the seat this afternoon."
    );

    // The same object is on the ledger, where a lane reads it.
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_resolved", "--json"],
    );
    let recorded = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["payload"]["attentionID"] == first["id"])
        .expect("the resolve event");
    assert_eq!(recorded["payload"]["decision"], settled["decision"]);
    assert!(recorded["payload"]["previousDecision"].is_null());
}

#[test]
fn resolving_with_custom_requires_an_outcome_and_a_note_and_records_both() {
    let fixture = Fixture::new("card-custom");
    fixture.ok_json(&fixture.main, &["init", "--name", "CUSTOM", "--json"]);
    let item = raise_carded(&fixture, "The P0 body.", "claude@driver", &CARD);
    let id = item["id"].as_str().unwrap();
    let needs_both =
        "attention: a custom answer needs --outcome (approve, reject, defer, other) and --note";

    assert_eq!(
        attention_refusal(
            &fixture,
            &["resolve", id, "--as", "geoyws", "--choice", "custom"]
        ),
        needs_both
    );
    assert_eq!(
        attention_refusal(
            &fixture,
            &[
                "resolve",
                id,
                "--as",
                "geoyws",
                "--choice",
                "custom",
                "--outcome",
                "defer",
            ]
        ),
        needs_both
    );
    assert_eq!(
        attention_refusal(
            &fixture,
            &[
                "resolve", id, "--as", "geoyws", "--choice", "custom", "--note", "later",
            ]
        ),
        needs_both
    );
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["attention", "list", "--status", "open", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        1,
        "a refused custom answer must leave the item open"
    );

    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            id,
            "--as",
            "geoyws",
            "--choice",
            "custom",
            "--outcome",
            "defer",
            "--note",
            "Do it after the aix pin lands.",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["decision"]["choice"], "custom");
    assert_eq!(settled["decision"]["outcome"], "defer");
    assert_eq!(
        settled["decision"]["note"],
        "Do it after the aix pin lands."
    );
    assert_eq!(settled["decision"]["by"], "geoyws");
    assert_eq!(
        settled["resolution"],
        "Decision: Custom answer, recorded as defer.\nNote: Do it after the aix pin lands."
    );
    // The reserved key is not one of the row's choices and never became one.
    assert_eq!(settled["choices"], card_choices());
}

#[test]
fn resolve_without_a_choice_is_refused_and_the_item_stays_open() {
    let fixture = Fixture::new("card-no-choice");
    fixture.ok_json(&fixture.main, &["init", "--name", "NOCHOICE", "--json"]);
    let item = raise_carded(&fixture, "Needs a verdict.", "geoyws", &[]);
    let id = item["id"].as_str().unwrap();
    let before = attention_and_chain(&fixture);

    for args in [
        vec!["resolve", id, "--as", "geoyws"],
        vec!["resolve", id, "--as", "geoyws", "--note", "looks fine"],
    ] {
        assert_eq!(
            attention_refusal(&fixture, &args),
            "attention resolve requires --choice KEY or \
             --choice custom --outcome X --note TEXT"
        );
    }
    assert_eq!(
        attention_and_chain(&fixture),
        before,
        "a refused resolve wrote something"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["attention", "list", "--json"])[0]["status"],
        "open"
    );
}

#[test]
fn every_card_refusal_names_its_fix() {
    let fixture = Fixture::new("card-refusals");
    fixture.ok_json(&fixture.main, &["init", "--name", "REFUSE", "--json"]);
    let open = raise_carded(&fixture, "An open item with no card.", "geoyws", &[]);
    let open_id = open["id"].as_str().unwrap().to_owned();
    let settled = raise_carded(&fixture, "A settled item.", "geoyws", &[]);
    let settled_id = settled["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &settled_id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--json",
        ],
    );
    let before = attention_and_chain(&fixture);

    // Two well-formed choices, which every raise case below breaks in exactly
    // one way.
    let pair: [&str; 8] = [
        "--choice",
        "assign=Assign a seat|approve",
        "--consequence",
        "assign=One seat is bought and the login is finished today.",
        "--choice",
        "wait=Wait for the renewal|defer",
        "--consequence",
        "wait=Nothing changes until 2026-10-01, when this is re-raised.",
    ];
    fn raise<'a>(extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec!["raise", "a body", "--as", "claude@driver"];
        args.extend_from_slice(extra);
        args
    }
    let long_question = "q".repeat(161);

    // 1. `--consequence` names a key no `--choice` declared.
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                "assign=Assign a seat|approve",
                "--choice",
                "wait=Wait for the renewal|defer",
                "--consequence",
                "typo=Something happens.",
            ])
        ),
        "attention: no choice named typo; declared keys are assign, wait"
    );
    // 2. Two `--choice` share a key.
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                "assign=Assign a seat|approve",
                "--choice",
                "assign=Assign two seats|approve",
            ])
        ),
        "attention: choice key assign is given twice; keys must be unique within an item"
    );
    // 3. Choices given and the count is not 2-4.
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                "assign=Assign a seat|approve",
                "--consequence",
                "assign=One seat is bought and the login is finished today.",
                "--recommend",
                "assign",
            ])
        ),
        "attention: an item carries 2 to 4 choices; 1 were given"
    );
    // 4. Choices given and nothing is recommended.
    assert_eq!(
        attention_refusal(&fixture, &raise(&pair)),
        "attention: exactly one choice is recommended; 0 were"
    );
    // 5. A declared choice has no `--consequence`.
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                "assign=Assign a seat|approve",
                "--consequence",
                "assign=One seat is bought and the login is finished today.",
                "--choice",
                "wait=Wait for the renewal|defer",
                "--recommend",
                "assign",
            ])
        ),
        "attention: choice wait has no --consequence; every choice must say what happens if \
         it is picked"
    );
    // 6. `--question` without `--context`, and the reverse.
    let paired = "attention: --question and --context are one card; give both or neither";
    assert_eq!(
        attention_refusal(&fixture, &raise(&["--question", "Assign a seat?"])),
        paired
    );
    assert_eq!(
        attention_refusal(&fixture, &raise(&["--context", "A turn answers HTTP 401."])),
        paired
    );
    // 7. `--choice custom` on resolve without `--outcome` or without `--note`.
    assert_eq!(
        attention_refusal(
            &fixture,
            &["resolve", &open_id, "--as", "geoyws", "--choice", "custom"]
        ),
        "attention: a custom answer needs --outcome (approve, reject, defer, other) and --note"
    );
    // 8. Any card flag on a resolved item.
    assert_eq!(
        attention_refusal(
            &fixture,
            &[
                "update",
                &settled_id,
                "--as",
                "geoyws",
                "--question",
                "Too late?",
                "--context",
                "The item is already history.",
            ]
        ),
        format!("attention {settled_id} is resolved history; its card cannot be rewritten")
    );
    // 9. `--choice custom=…` on raise or update.
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                "custom=Write your own|other",
                "--consequence",
                "custom=Whatever you type is the verdict.",
                "--choice",
                "wait=Wait for the renewal|defer",
                "--consequence",
                "wait=Nothing changes until 2026-10-01, when this is re-raised.",
                "--recommend",
                "wait",
            ])
        ),
        "attention: custom is reserved for the free-text answer and cannot be a choice key"
    );
    // 10. `--outcome` on resolve without `--choice custom`.
    assert_eq!(
        attention_refusal(
            &fixture,
            &[
                "resolve",
                &open_id,
                "--as",
                "geoyws",
                "--choice",
                "approve",
                "--outcome",
                "reject",
            ]
        ),
        "attention: --outcome applies only to --choice custom; an authored choice carries its \
         own outcome"
    );
    // 11. `--recommend` with no `--choice`.
    assert_eq!(
        attention_refusal(&fixture, &raise(&["--recommend", "assign"])),
        "attention: --recommend needs choices; give --choice or drop it"
    );
    // 12. Every bound names the field, the bound and what it got.
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--question",
                &long_question,
                "--context",
                "A turn answers HTTP 401.",
            ])
        ),
        "attention: --question is 161 characters; the bound is 160"
    );
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--question",
                "Assign a seat?",
                "--context",
                &"c".repeat(801),
            ])
        ),
        "attention: --context is 801 characters; the bound is 800"
    );
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                &format!("assign={}|approve", "L".repeat(61)),
                "--consequence",
                "assign=One seat is bought and the login is finished today.",
                "--choice",
                "wait=Wait for the renewal|defer",
                "--consequence",
                "wait=Nothing changes until 2026-10-01, when this is re-raised.",
                "--recommend",
                "assign",
            ])
        ),
        "attention: choice assign label is 61 characters; the bound is 60"
    );
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                "assign=Assign a seat|approve",
                "--consequence",
                &format!("assign={}", "C".repeat(201)),
                "--choice",
                "wait=Wait for the renewal|defer",
                "--consequence",
                "wait=Nothing changes until 2026-10-01, when this is re-raised.",
                "--recommend",
                "assign",
            ])
        ),
        "attention: choice assign consequence is 201 characters; the bound is 200"
    );
    assert_eq!(
        attention_refusal(
            &fixture,
            &raise(&[
                "--choice",
                "assign=Assign a seat|and log in|approve",
                "--consequence",
                "assign=One seat is bought and the login is finished today.",
                "--choice",
                "wait=Wait for the renewal|defer",
                "--consequence",
                "wait=Nothing changes until 2026-10-01, when this is re-raised.",
                "--recommend",
                "assign",
            ])
        ),
        "attention: choice assign label contains '|', which separates the label from the \
         outcome in --choice KEY=LABEL|OUTCOME; write the label without it"
    );
    let bad_key = attention_refusal(
        &fixture,
        &raise(&[
            "--choice",
            "Assign Seat=Assign a seat|approve",
            "--consequence",
            "Assign Seat=One seat is bought and the login is finished today.",
            "--choice",
            "wait=Wait for the renewal|defer",
            "--consequence",
            "wait=Nothing changes until 2026-10-01, when this is re-raised.",
            "--recommend",
            "Assign Seat",
        ]),
    );
    assert!(
        bad_key.starts_with("attention: choice key \"Assign Seat\" is not a slug;"),
        "{bad_key}"
    );
    assert!(bad_key.contains("[a-z0-9][a-z0-9-]{0,31}"), "{bad_key}");
    // 13. Resolve naming a key the row does not carry -- what makes a stale
    // card safe: the click names a key that no longer exists and is refused
    // rather than mapped to whatever now sits in that position.
    assert_eq!(
        attention_refusal(
            &fixture,
            &[
                "resolve",
                &open_id,
                "--as",
                "geoyws",
                "--choice",
                "keep-parked",
            ]
        ),
        format!("attention {open_id} has no choice keep-parked; its choices are approve, reject")
    );
    // 14. Resolve with no `--choice`.
    assert_eq!(
        attention_refusal(&fixture, &["resolve", &open_id, "--as", "geoyws"]),
        "attention resolve requires --choice KEY or --choice custom --outcome X --note TEXT"
    );

    assert_eq!(
        attention_and_chain(&fixture),
        before,
        "a refusal wrote to the board or to the ledger"
    );
}

#[test]
fn a_resolved_item_refuses_every_card_flag() {
    let fixture = Fixture::new("card-resolved-history");
    fixture.ok_json(&fixture.main, &["init", "--name", "HISTORY", "--json"]);
    let item = raise_carded(&fixture, "A settled item.", "claude@driver", &CARD);
    let id = item["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "geoyws",
            "--choice",
            "drop-receipt",
            "--json",
        ],
    );
    let before = attention_and_chain(&fixture);
    let expected = format!("attention {id} is resolved history; its card cannot be rewritten");

    for extra in [
        vec![
            "--question",
            "A new question?",
            "--context",
            "New context for it.",
        ],
        vec![
            "--choice",
            "assign=Assign a seat|approve",
            "--consequence",
            "assign=One seat is bought today.",
            "--choice",
            "wait=Wait for the renewal|defer",
            "--consequence",
            "wait=Nothing changes until 2026-10-01.",
            "--recommend",
            "assign",
        ],
        vec![
            "--check",
            "Where is the definition validated?",
            "--check-choice",
            "store=The Store validates it",
            "--check-choice",
            "client=The client validates it",
            "--check-answer",
            "store",
            "--check-explain",
            "rust/store.rs validates it.",
            "--check-about",
            "rust/store.rs",
        ],
        vec!["--clear-card"],
        vec!["--body", "A rewritten body."],
    ] {
        let mut args = vec!["update", id.as_str(), "--as", "geoyws"];
        args.extend_from_slice(&extra);
        assert_eq!(attention_refusal(&fixture, &args), expected, "{extra:?}");
    }
    assert_eq!(
        attention_and_chain(&fixture),
        before,
        "a refused rewrite touched resolved history"
    );
}

#[test]
fn reopening_keeps_the_previous_decision_in_the_ledger_and_clears_it_from_the_row() {
    let fixture = Fixture::new("card-reopen");
    fixture.ok_json(&fixture.main, &["init", "--name", "REOPEN", "--json"]);
    let item = raise_carded(&fixture, "The P0 body.", "claude@driver", &CARD);
    let id = item["id"].as_str().unwrap().to_owned();
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "geoyws",
            "--choice",
            "keep-parked",
            "--note",
            "Not this week.",
            "--json",
        ],
    );
    let decision = settled["decision"].clone();
    assert_eq!(decision["choice"], "keep-parked");

    let reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            &id,
            "--as",
            "geoyws",
            "--note",
            "A seat freed up sooner than expected.",
            "--json",
        ],
    );
    assert_eq!(reopened["status"], "open");
    assert!(
        reopened["decision"].is_null(),
        "an open row must carry no decision: {reopened}"
    );
    // The resolution it undid is still on the row, as it was before ADR-042.
    assert_eq!(
        reopened["resolution"],
        format!("{KEEP_PARKED}\nNote: Not this week.")
    );
    assert_eq!(reopened["resolvedBy"], "geoyws");

    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_reopened", "--json"],
    );
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(
        events[0]["payload"]["decision"], decision,
        "the ledger is where the decision history lives"
    );
    assert_eq!(
        events[0]["payload"]["resolution"],
        format!("{KEEP_PARKED}\nNote: Not this week.")
    );
}

#[test]
fn re_resolving_a_reopened_item_records_the_new_decision_and_the_previous_one_survives_in_the_ledger()
 {
    let fixture = Fixture::new("card-re-resolve");
    fixture.ok_json(&fixture.main, &["init", "--name", "RERESOLVE", "--json"]);
    let item = raise_carded(&fixture, "The P0 body.", "claude@driver", &CARD);
    let id = item["id"].as_str().unwrap().to_owned();
    let first = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "geoyws",
            "--choice",
            "keep-parked",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            &id,
            "--as",
            "geoyws",
            "--note",
            "A seat freed up.",
            "--json",
        ],
    );
    let second = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "geoyws",
            "--choice",
            "assign-and-login",
            "--note",
            "Seat assigned; logging in now.",
            "--json",
        ],
    );
    assert_eq!(second["decision"]["choice"], "assign-and-login");
    assert_eq!(second["decision"]["outcome"], "approve");
    assert!(
        second["reopenedAt"].is_null(),
        "a re-resolve clears the reopen marks"
    );

    // Both decisions are reachable, and only the newer one is on the row.
    let resolves = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_resolved", "--json"],
    );
    let payloads = resolves
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["payload"].clone())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2);
    let latest = payloads
        .iter()
        .find(|payload| payload["decision"]["choice"] == json!("assign-and-login"))
        .expect("the second resolve");
    // `previousDecision` mirrors the ROW, and the reopen cleared it from
    // there; the decision it undid is in the reopen event below. The row's
    // resolution survived the reopen, so that one is carried.
    assert!(
        latest["previousDecision"].is_null(),
        "the row carried no decision to supersede: {latest}"
    );
    assert_eq!(latest["previousResolution"], KEEP_PARKED);
    let earliest = payloads
        .iter()
        .find(|payload| payload["decision"]["choice"] == json!("keep-parked"))
        .expect("the first resolve");
    assert_eq!(earliest["decision"], first["decision"]);
    let reopen = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_reopened", "--json"],
    );
    assert_eq!(reopen[0]["payload"]["decision"], first["decision"]);
    assert_eq!(board_audit(&fixture)["errors"], json!([]));
}

#[test]
fn a_board_migrates_from_schema_24_to_25_and_its_existing_attention_rows_read_as_the_default_pair()
{
    let fixture = Fixture::new("card-migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "MIGRATE", "--json"]);
    let open = raise_carded(&fixture, "An open v24 item.", "claude@driver", &[]);
    let closed = raise_carded(&fixture, "A settled v24 item.", "claude@driver", &[]);
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            closed["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--note",
            "Historical bytes, preserved.",
            "--json",
        ],
    );
    let historical_resolution = settled["resolution"].as_str().unwrap().to_owned();

    let board = board_path_for_project(&fixture, &fixture.main, "MIGRATE");
    {
        let connection = Connection::open(&board).unwrap();
        remove_v27_sprint_schema(&connection);
        remove_v25_attention_card_schema(&connection);
        connection.execute_batch("PRAGMA user_version=24;").unwrap();
        let columns: Vec<String> = connection
            .prepare("SELECT name FROM pragma_table_info('attention')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for gone in ["question", "context", "choices", "decision"] {
            assert!(
                !columns.contains(&gone.to_owned()),
                "the v24 fixture still has {gone}"
            );
        }
    }

    // Any ordinary command migrates it.
    let migrated = fixture.ok_json(&fixture.main, &["attention", "list", "--all", "--json"]);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["projects"][0]["schemaVersion"],
        38
    );
    for row in migrated.as_array().unwrap() {
        assert!(row["question"].is_null());
        assert!(row["context"].is_null());
        assert_eq!(
            row["choices"],
            default_pair(),
            "a migrated row must read as the default pair"
        );
        assert!(
            row["decision"].is_null(),
            "the migration must not invent a decision for a row settled before it"
        );
    }
    let survivor = migrated
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == closed["id"])
        .expect("the settled row survived");
    assert_eq!(
        survivor["resolution"], historical_resolution,
        "no historical resolution is rewritten"
    );
    assert_eq!(survivor["status"], "resolved");
    assert_eq!(survivor["resolvedBy"], "geoyws");

    // And the migrated open row is answerable through the pair it now reads as.
    let resolved = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            open["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--json",
        ],
    );
    assert_eq!(resolved["decision"]["outcome"], "approve");
    assert_eq!(resolved["resolution"], APPROVE);

    // The search projection the migration rebuilt is usable, and a rebuild
    // over a migrated board reports every document embedded.
    let rebuilt = fixture.ok_json(
        &fixture.main,
        &["search-rebuild", "--as", "tester", "--json"],
    );
    assert_eq!(rebuilt["documents"], rebuilt["embedded"]);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["projects"][0]["searchIndex"]["healthy"],
        true
    );
}

#[test]
fn the_audit_chain_stays_healthy_across_the_card_migration() {
    let fixture = Fixture::new("card-migration-chain");
    fixture.ok_json(&fixture.main, &["init", "--name", "CHAIN", "--json"]);
    raise_carded(&fixture, "An open v24 item.", "claude@driver", &[]);
    let before = board_audit(&fixture);

    let board = board_path_for_project(&fixture, &fixture.main, "CHAIN");
    {
        let connection = Connection::open(&board).unwrap();
        remove_v27_sprint_schema(&connection);
        remove_v25_attention_card_schema(&connection);
        connection.execute_batch("PRAGMA user_version=24;").unwrap();
    }
    fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);

    // The migration is a schema change and nothing else: it appends no event,
    // so the chain's head and length are exactly what they were (ADR-029).
    let after = board_audit(&fixture);
    assert_eq!(after["lastSeq"], before["lastSeq"], "{after}");
    assert_eq!(after["entries"], before["entries"], "{after}");
    assert_eq!(after["head"], before["head"], "{after}");
    assert_eq!(after["errors"], json!([]), "{after}");
    let report = fixture.ok_json(&fixture.main, &["audit", "verify", "--json"]);
    assert_eq!(report["healthy"], true, "{report}");
}

#[test]
fn search_finds_an_item_by_its_question_and_by_a_choice_consequence() {
    let fixture = Fixture::new("card-search");
    fixture.ok_json(&fixture.main, &["init", "--name", "CARDSEARCH", "--json"]);
    let item = raise_carded(
        &fixture,
        "PARKED - waiting on a seat.",
        "claude@driver",
        &CARD,
    );
    let id = item["id"].as_str().unwrap().to_owned();

    // A freshly raised row needs no rebuild: its own trigger indexed the card.
    let found = |query: &str| -> bool {
        fixture.ok_json(&fixture.main, &["search", query, "--json"])["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["sourceId"] == json!(id) && hit["sourceKind"] == json!("attention"))
    };
    assert!(found("logged-in Claude account"), "the question is indexed");
    assert!(
        found("seat's monthly cost"),
        "a choice's consequence is indexed"
    );
    assert!(
        found("Keep it parked until a seat frees up"),
        "a choice's label is indexed"
    );
    assert!(found("PARKED waiting seat"), "the body is still indexed");

    // A row whose document predates the card -- the state every board is in
    // immediately after the migration -- is found again once rebuilt.
    let board = board_path_for_project(&fixture, &fixture.main, "CARDSEARCH");
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE search_documents SET body='stale' WHERE source_kind='attention'",
            [],
        )
        .unwrap();
    assert!(
        !found("seat's monthly cost"),
        "the fixture did not go stale"
    );
    let rebuilt = fixture.ok_json(
        &fixture.main,
        &["search-rebuild", "--as", "tester", "--json"],
    );
    assert_eq!(rebuilt["documents"], rebuilt["embedded"]);
    assert!(
        found("seat's monthly cost"),
        "search-rebuild must index the card"
    );
    assert!(found("logged-in Claude account"));
}

#[test]
fn the_schema_publishes_the_card_flags_with_the_right_kinds() {
    let fixture = Fixture::new("card-schema");
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operation = |name: &str| -> Value {
        schema["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|operation| operation["name"] == json!(name))
            .unwrap_or_else(|| panic!("no {name} operation"))
            .clone()
    };
    let flag = |operation: &Value, name: &str| -> Option<Value> {
        operation["flags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|flag| flag["name"] == json!(name))
            .cloned()
    };

    for name in ["attention raise", "attention update"] {
        let operation = operation(name);
        for listed in ["choice", "consequence", "check-choice"] {
            assert_eq!(
                flag(&operation, listed).unwrap_or_else(|| panic!("{name} has no --{listed}"))["kind"],
                "list",
                "{name} --{listed}"
            );
        }
        for valued in [
            "question",
            "context",
            "recommend",
            "check",
            "check-answer",
            "check-explain",
            "check-about",
        ] {
            assert_eq!(flag(&operation, valued).unwrap()["kind"], "value");
        }
        assert!(
            flag(&operation, "outcome").is_none(),
            "{name} must not offer --outcome"
        );
        assert!(flag(&operation, "check-outcome").is_none());
        assert!(flag(&operation, "check-recommend").is_none());
    }
    let update = operation("attention update");
    assert_eq!(flag(&update, "clear-card").unwrap()["kind"], "boolean");
    let show = operation("attention show");
    assert!(
        flag(&show, "as").is_none(),
        "show cannot authenticate a caller-supplied actor"
    );

    let resolve = operation("attention resolve");
    assert_eq!(
        flag(&resolve, "choice").unwrap()["kind"],
        "value",
        "a resolve takes exactly one answer"
    );
    assert!(
        flag(&resolve, "consequence").is_none(),
        "a resolve authors nothing"
    );
    assert!(flag(&resolve, "recommend").is_none());
    let outcome = flag(&resolve, "outcome").expect("attention resolve publishes --outcome");
    assert_eq!(outcome["kind"], "value");
    assert_eq!(
        outcome["values"],
        json!(["approve", "reject", "defer", "other"])
    );
    assert_eq!(flag(&resolve, "note").unwrap()["kind"], "value");
}

#[test]
fn attention_resolve_refuses_a_second_choice_flag() {
    let fixture = Fixture::new("card-one-answer");
    fixture.ok_json(&fixture.main, &["init", "--name", "ONEANSWER", "--json"]);
    let item = raise_carded(&fixture, "The P0 body.", "claude@driver", &CARD);
    let id = item["id"].as_str().unwrap().to_owned();
    let before = attention_and_chain(&fixture);

    // Two answers to one question. The parser refuses it rather than keeping
    // the last, which is what makes the schema's `value` kind true.
    let refusal = attention_refusal(
        &fixture,
        &[
            "resolve",
            &id,
            "--as",
            "geoyws",
            "--choice",
            "keep-parked",
            "--choice",
            "drop-receipt",
        ],
    );
    assert!(
        refusal.contains("--choice (keep-parked, drop-receipt) given more than once"),
        "{refusal}"
    );
    assert_eq!(attention_and_chain(&fixture), before);

    // The same flag repeats on raise, where it is a list of authored options.
    let repeated = raise_carded(&fixture, "Repeats on raise.", "claude@driver", &CARD);
    assert_eq!(repeated["choices"].as_array().unwrap().len(), 3);
}

#[test]
fn the_mcp_attention_tools_mirror_every_card_flag_one_to_one() {
    let fixture = Fixture::new("card-mcp");
    fixture.ok_json(&fixture.main, &["init", "--name", "CARDMCP", "--json"]);
    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap().clone();
    let tool = |name: &str| -> Value {
        tools
            .iter()
            .find(|tool| tool["name"] == json!(name))
            .unwrap_or_else(|| panic!("no {name} tool"))
            .clone()
    };

    for name in ["attention_raise", "attention_update"] {
        let properties = tool(name)["inputSchema"]["properties"].clone();
        for listed in ["choice", "consequence", "check-choice"] {
            assert_eq!(
                properties[listed]["type"], "array",
                "{name} {listed} must accept the list the CLI repeats"
            );
            assert_eq!(properties[listed]["items"]["type"], "string");
        }
        for valued in [
            "question",
            "context",
            "recommend",
            "check",
            "check-answer",
            "check-explain",
            "check-about",
        ] {
            assert_eq!(properties[valued]["type"], "string", "{name} {valued}");
        }
        assert!(properties.get("outcome").is_none(), "{name} outcome");
        assert!(properties.get("check-outcome").is_none());
        assert!(properties.get("check-recommend").is_none());
    }
    assert_eq!(
        tool("attention_update")["inputSchema"]["properties"]["clear-card"]["type"],
        "boolean"
    );
    let show = tool("attention_show")["inputSchema"]["properties"].clone();
    assert!(
        show.get("as").is_none(),
        "MCP show must not offer spoofable actor input"
    );
    let resolve = tool("attention_resolve")["inputSchema"]["properties"].clone();
    assert_eq!(resolve["choice"]["type"], "string");
    assert_eq!(resolve["outcome"]["type"], "string");
    assert_eq!(resolve["note"]["type"], "string");
    assert!(resolve.get("consequence").is_none());
    assert!(resolve.get("recommend").is_none());

    // A call over the wire, with arrays, equals the same card raised through
    // the CLI -- everything but the id and the millisecond it was written in.
    let over_the_wire = session.ask(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "attention_raise", "arguments": {
            "text": "The P0 body.",
            "as": "claude@driver",
            "question": CARD[1],
            "context": CARD[3],
            "choice": [CARD[5], CARD[9], CARD[13]],
            "consequence": [CARD[7], CARD[11], CARD[15]],
            "recommend": "assign-and-login",
        }}
    }));
    assert_eq!(
        over_the_wire["result"]["isError"],
        false,
        "{}",
        tool_text(&over_the_wire["result"])
    );
    let mut through_mcp: Value =
        serde_json::from_str(&tool_text(&over_the_wire["result"])).expect("a tool answers JSON");
    let mut through_cli = raise_carded(&fixture, "The P0 body.", "claude@driver", &CARD);
    let mcp_id = through_mcp["id"].as_str().unwrap().to_owned();
    for row in [&mut through_mcp, &mut through_cli] {
        let object = row.as_object_mut().unwrap();
        object.remove("id");
        object.remove("createdAt");
    }
    assert_eq!(
        through_mcp, through_cli,
        "the generated tool wrote a different row from the CLI"
    );
    assert_eq!(through_mcp["choices"], card_choices());

    // And a resolve takes scalars, not arrays.
    let answered = session.ask(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "attention_resolve", "arguments": {
            "id": mcp_id, "as": "geoyws", "choice": "keep-parked",
        }}
    }));
    assert_eq!(
        answered["result"]["isError"],
        false,
        "{}",
        tool_text(&answered["result"])
    );
    let resolved: Value = serde_json::from_str(&tool_text(&answered["result"])).unwrap();
    assert_eq!(resolved["decision"]["choice"], "keep-parked");
    assert_eq!(resolved["decision"]["outcome"], "defer");
    assert_eq!(resolved["resolution"], KEEP_PARKED);

    // An array where the CLI takes one value is refused, not silently joined.
    let two_answers = session.ask(json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "attention_resolve", "arguments": {
            "id": mcp_id, "as": "geoyws", "choice": ["keep-parked", "drop-receipt"],
        }}
    }));
    assert_eq!(two_answers["result"]["isError"], true);
    session.finish();
}

#[test]
fn a_transact_of_thirty_two_attention_updates_lands_or_rolls_back_whole() {
    let fixture = Fixture::new("card-transact");
    fixture.ok_json(&fixture.main, &["init", "--name", "CARDTX", "--json"]);
    let ids = (0..32)
        .map(|index| {
            raise_carded(
                &fixture,
                &format!("A body needing a card ({index})."),
                "claude@driver",
                &[],
            )["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();

    let item = |id: &str, index: usize| -> Value {
        json!({ "name": "attention_update", "arguments": {
            "id": id,
            "as": "claude@driver",
            "question": format!("Does row {index} ship as written, or wait for the pin?"),
            "context": format!("Row {index} is measured and ready; nothing else waits on it, and waiting costs one day."),
            "choice": [
                "ship=Ship it as written|approve",
                "wait=Wait for the aix pin|defer",
            ],
            "consequence": [
                "ship=It goes out today and the receipt lands the same hour.",
                "wait=Nothing ships until the pin lands, and this is re-raised then.",
            ],
            "recommend": "ship",
        }})
    };

    // The backfill's batch shape: 32 cards, one transaction.
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &ids.iter()
            .enumerate()
            .map(|(index, id)| item(id, index))
            .collect::<Vec<_>>(),
    );
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["results"].as_array().unwrap().len(), 32);
    let carded = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--limit", "100", "--json"],
    );
    assert_eq!(carded.as_array().unwrap().len(), 32);
    for row in carded.as_array().unwrap() {
        assert!(
            row["question"].as_str().unwrap().starts_with("Does row "),
            "{row}"
        );
        assert_eq!(row["choices"].as_array().unwrap().len(), 2);
        assert_eq!(row["choices"][0]["key"], "ship");
        assert!(row["choices"][0]["recommended"].as_bool().unwrap());
    }
    let landed = carded.clone();
    let before = board_audit(&fixture);

    // And one bad card takes the whole batch with it: a card with no
    // recommendation at index 17, refused by the same validator.
    let mut items = ids
        .iter()
        .enumerate()
        .map(|(index, id)| item(id, index + 100))
        .collect::<Vec<_>>();
    items[17]["arguments"]
        .as_object_mut()
        .unwrap()
        .remove("recommend");
    let envelope = transact_results(&fixture, &fixture.main, &items);
    assert_eq!(envelope["ok"], false, "{envelope}");
    assert_eq!(envelope["failedIndex"], 17, "{envelope}");
    assert_eq!(envelope["rolledBack"], true, "{envelope}");
    assert!(
        envelope["results"][17]["error"]
            .as_str()
            .unwrap()
            .contains("exactly one choice is recommended; 0 were"),
        "{envelope}"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["attention", "list", "--limit", "100", "--json"]
        ),
        landed,
        "a rolled-back batch left one of its 32 updates behind"
    );
    let after = board_audit(&fixture);
    assert_eq!(after["lastSeq"], before["lastSeq"], "{after}");
    assert_eq!(after["head"], before["head"], "{after}");
    assert_eq!(after["errors"], json!([]), "{after}");
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
            "--choice",
            "approve",
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
            "--choice",
            "approve",
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
            "--choice",
            "reject",
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
fn attention_raise_stores_lane_and_list_matches_both_routes() {
    // SPA-62 (A18): a raised card carries its lane, and the lane queue reads
    // the stored lane and both older routes — raiser `<anything>@<lane>`,
    // or the task the row is about carrying that lane.
    let fixture = Fixture::new("attention-lane-queue");
    fixture.ok_json(&fixture.main, &["init", "--name", "LANES", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Lane work",
            "--id",
            "t-lane",
            "--lane",
            "driver-2",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Unlaned work", "--id", "t-plain", "--json"],
    );
    let raise = |args: &[&str]| fixture.ok_json(&fixture.main, args);
    // Route one: the stored lane, by an actor with no lane suffix, about no
    // task, so neither older route can claim it.
    let stored = raise(&[
        "attention",
        "raise",
        "stored route",
        "--as",
        "geoyws",
        "--lane",
        "driver-2",
        "--json",
    ]);
    assert_eq!(stored["lane"], "driver-2");
    // Route two: no stored lane, raiser `worker@driver-2`, untagged task.
    let by_raiser = raise(&[
        "attention",
        "raise",
        "raiser route",
        "--as",
        "worker@driver-2",
        "--task",
        "t-plain",
        "--json",
    ]);
    assert!(by_raiser["lane"].is_null(), "{}", by_raiser);
    // Route three: no stored lane, about the lane's task, raised by a laned
    // actor of another lane.
    let by_task = raise(&[
        "attention",
        "raise",
        "task route",
        "--as",
        "worker@driver-3",
        "--task",
        "t-lane",
        "--json",
    ]);
    assert!(by_task["lane"].is_null(), "{}", by_task);
    // A fourth card in no route to `driver-2`.
    raise(&[
        "attention",
        "raise",
        "elsewhere",
        "--as",
        "worker@driver-3",
        "--task",
        "t-plain",
        "--json",
    ]);
    let id_of = |row: &Value| row["id"].as_str().unwrap().to_owned();
    let stored_id = id_of(&stored);
    let by_raiser_id = id_of(&by_raiser);
    let by_task_id = id_of(&by_task);

    let mut ids: Vec<String> = raise(&["attention", "list", "--lane", "driver-2", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .map(id_of)
        .collect();
    ids.sort();
    let mut expected = vec![stored_id.clone(), by_raiser_id.clone(), by_task_id.clone()];
    expected.sort();
    assert_eq!(
        ids, expected,
        "the lane queue reads all three routes and no fourth"
    );
    // The stored value round-trips on show and on the listing's JSON row.
    assert_eq!(
        raise(&["attention", "show", stored_id.as_str(), "--json"])["lane"],
        "driver-2"
    );
    assert!(raise(&["attention", "show", by_raiser_id.as_str(), "--json"])["lane"].is_null());
    let listed = raise(&["attention", "list", "--lane", "driver-2", "--json"]);
    let listed_stored = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == stored_id)
        .unwrap();
    assert_eq!(listed_stored["lane"], "driver-2");
    // A lane no row uses reads empty rather than an error.
    let empty = raise(&["attention", "list", "--lane", "driver-9", "--json"]);
    assert_eq!(empty.as_array().unwrap().len(), 0);

    // Control: the same three shapes raised without any stored lane answer
    // exactly as before this change — the two older routes and nothing else.
    let control = Fixture::new("attention-lane-queue-control");
    control.ok_json(&control.main, &["init", "--name", "LANES", "--json"]);
    control.ok_json(
        &control.main,
        &[
            "task",
            "add",
            "Lane work",
            "--id",
            "t-lane",
            "--lane",
            "driver-2",
            "--json",
        ],
    );
    control.ok_json(
        &control.main,
        &["task", "add", "Unlaned work", "--id", "t-plain", "--json"],
    );
    let raise_control = |args: &[&str]| control.ok_json(&control.main, args);
    raise_control(&[
        "attention",
        "raise",
        "stored route",
        "--as",
        "geoyws",
        "--json",
    ]);
    let control_raiser = id_of(&raise_control(&[
        "attention",
        "raise",
        "raiser route",
        "--as",
        "worker@driver-2",
        "--task",
        "t-plain",
        "--json",
    ]));
    let control_task = id_of(&raise_control(&[
        "attention",
        "raise",
        "task route",
        "--as",
        "worker@driver-3",
        "--task",
        "t-lane",
        "--json",
    ]));
    raise_control(&[
        "attention",
        "raise",
        "elsewhere",
        "--as",
        "worker@driver-3",
        "--task",
        "t-plain",
        "--json",
    ]);
    let mut control_ids: Vec<String> =
        raise_control(&["attention", "list", "--lane", "driver-2", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .map(id_of)
            .collect();
    control_ids.sort();
    let mut control_expected = vec![control_raiser, control_task];
    control_expected.sort();
    assert_eq!(control_ids, control_expected);
}

/// Today and tomorrow as UTC calendar days, read off SQLite's own clock — the
/// same `date` arithmetic the store's firing predicate uses — so a test that
/// straddles midnight asks both sides of the same clock.
fn utc_today_and_tomorrow() -> (String, String) {
    Connection::open_in_memory()
        .unwrap()
        .query_row("SELECT date('now'), date('now','+1 day')", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
}

/// The ids `attention list <extra> --json` returns, sorted.
fn attention_ids(fixture: &Fixture, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["attention", "list", "--limit", "500"];
    args.extend_from_slice(extra);
    args.push("--json");
    let mut ids: Vec<String> = fixture
        .ok_json(&fixture.main, &args)
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

/// This board's `openAttention` as `dashboard --json` counts it.
fn dashboard_open_attention(fixture: &Fixture, board: &str) -> i64 {
    fixture
        .ok_json(&fixture.main, &["dashboard", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == board)
        .unwrap_or_else(|| panic!("the dashboard lists no {board}"))["openAttention"]
        .as_i64()
        .unwrap()
}

#[test]
fn a_defer_with_a_return_trigger_snoozes_the_card_until_the_trigger_fires_on_read() {
    // SPA-64 (A20), over the compiled binary, every call a fresh process
    // against the same board file: a defer naming a return trigger leaves
    // the row open with its decision, hides it from the default queue and
    // its counts while `--all` and show still name it, and each of the three
    // forms brings it back on read — no sweep runs anywhere in this test.
    let fixture = Fixture::new("stale-queue-snooze");
    fixture.ok_json(&fixture.main, &["init", "--name", "STALE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "The work a card waits on",
            "--id",
            "t-wait",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Unrelated work", "--id", "t-other", "--json"],
    );
    let raise = |body: &str| -> String {
        raise_carded(&fixture, body, "claude@driver", &CARD)["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let by_date = raise("Snoozed until tomorrow.");
    let by_today = raise("Snoozed until a day that has already begun.");
    let by_task = raise("Snoozed until t-wait is written.");
    let by_event = raise("Snoozed until a card is updated.");
    let triggerless = raise("Deferred with no trigger.");
    let never = raise("Never snoozed.");
    let (today, tomorrow) = utc_today_and_tomorrow();
    let defer = |id: &str, trigger: &str| -> Value {
        fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "resolve",
                id,
                "--as",
                "geoyws",
                "--choice",
                "keep-parked",
                "--return-trigger",
                trigger,
                "--json",
            ],
        )
    };

    // The receipt: still open, the defer decision recorded, the trigger
    // beside it, and nothing under resolved_* — the snooze is not a
    // resolution.
    let date_trigger = format!("date:{tomorrow}");
    let snoozed = defer(&by_date, &date_trigger);
    assert_eq!(snoozed["status"], "open", "{snoozed}");
    assert_eq!(snoozed["returnTrigger"], date_trigger.as_str(), "{snoozed}");
    assert_eq!(snoozed["decision"]["choice"], "keep-parked", "{snoozed}");
    assert_eq!(snoozed["decision"]["outcome"], "defer", "{snoozed}");
    assert_eq!(snoozed["decision"]["by"], "geoyws", "{snoozed}");
    assert!(snoozed["resolvedAt"].is_null(), "{snoozed}");
    assert!(snoozed["resolvedBy"].is_null(), "{snoozed}");
    assert!(snoozed["resolution"].is_null(), "{snoozed}");

    // `date:D` fires at the start of D in UTC, so a day already begun has
    // fired by the time anyone reads it.
    let fired_today = defer(&by_today, &format!("date:{today}"));
    assert_eq!(fired_today["status"], "open");
    // `task:` and `event:` fire only on a write strictly after the deferral:
    // the snooze's own `attention_updated` envelope must not fire an
    // `event:attention_updated` trigger.
    defer(&by_task, "task:t-wait");
    defer(&by_event, "event:attention_updated");

    // A defer with no trigger resolves exactly as ADR-042 §1 says.
    let resolved = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &triggerless,
            "--as",
            "geoyws",
            "--choice",
            "keep-parked",
            "--json",
        ],
    );
    assert_eq!(resolved["status"], "resolved", "{resolved}");
    assert_eq!(resolved["decision"]["outcome"], "defer", "{resolved}");
    assert!(resolved["returnTrigger"].is_null(), "{resolved}");

    // The default open queue, the unfiltered default listing and the
    // dashboard count all read the three unfired rows as absent; the fired
    // `date:today` row and the never-snoozed row are there.
    let mut visible_open = vec![by_today.clone(), never.clone()];
    visible_open.sort();
    assert_eq!(
        attention_ids(&fixture, &["--status", "open"]),
        visible_open,
        "the default open queue must hide exactly the unfired snoozes"
    );
    let mut visible_all_statuses = vec![by_today.clone(), never.clone(), triggerless.clone()];
    visible_all_statuses.sort();
    assert_eq!(attention_ids(&fixture, &[]), visible_all_statuses);
    assert_eq!(
        dashboard_open_attention(&fixture, "STALE"),
        2,
        "a snoozed row leaked into the open count"
    );
    // `--all` still names every snoozed row, each with its trigger.
    let everything = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--all", "--limit", "500", "--json"],
    );
    let trigger_of = |id: &str| -> Value {
        everything
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("--all does not name {id}: {everything}"))["returnTrigger"]
            .clone()
    };
    assert_eq!(trigger_of(&by_date), json!(date_trigger));
    assert_eq!(trigger_of(&by_task), json!("task:t-wait"));
    assert_eq!(trigger_of(&by_event), json!("event:attention_updated"));
    assert_eq!(
        trigger_of(&never),
        Value::Null,
        "a row that never snoozed carries a null trigger, not an absent key"
    );
    // So does a direct show.
    let shown = fixture.ok_json(&fixture.main, &["attention", "show", &by_date, "--json"]);
    assert_eq!(shown["returnTrigger"], date_trigger.as_str());
    assert_eq!(shown["status"], "open");

    // A write to some other task fires nothing.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "update", "t-other", "--as", "geoyws", "--title", "Renamed", "--json",
        ],
    );
    assert!(!attention_ids(&fixture, &["--status", "open"]).contains(&by_task));
    // A write to t-wait fires `task:t-wait` on the next read.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "update", "t-wait", "--as", "geoyws", "--title", "Written", "--json",
        ],
    );
    let open = attention_ids(&fixture, &["--status", "open"]);
    assert!(
        open.contains(&by_task),
        "task:t-wait did not fire: {open:?}"
    );
    assert!(
        !open.contains(&by_event),
        "a task write is not an attention_updated event: {open:?}"
    );
    // An `attention_updated` recorded after the deferral fires the event
    // trigger; `never` is corrected, which records exactly that kind.
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "update",
            &never,
            "--as",
            "claude@driver",
            "--body",
            "Corrected.",
            "--json",
        ],
    );
    let open = attention_ids(&fixture, &["--status", "open"]);
    assert!(
        open.contains(&by_event),
        "event:attention_updated did not fire: {open:?}"
    );
    assert!(
        !open.contains(&by_date),
        "tomorrow has not arrived: {open:?}"
    );
    assert_eq!(dashboard_open_attention(&fixture, "STALE"), 4);

    // The date arriving: the stored day is moved to one already begun, which
    // is what reading it tomorrow would see. It returns still open, its
    // defer decision intact — the return is not a resolution.
    let board = board_path_for_project(&fixture, &fixture.main, "STALE");
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE attention SET return_trigger=? WHERE id=?",
            params![format!("date:{today}"), by_date],
        )
        .unwrap();
    let queue = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--status", "open", "--json"],
    );
    let returned = queue
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == by_date.as_str())
        .unwrap_or_else(|| panic!("the arrived date did not bring the card back: {queue}"));
    assert_eq!(returned["status"], "open");
    assert_eq!(returned["decision"]["choice"], "keep-parked");
    assert_eq!(returned["decision"]["outcome"], "defer");
    assert!(returned["resolvedAt"].is_null());
    assert_eq!(dashboard_open_attention(&fixture, "STALE"), 5);

    // A returned card settles like any other, and settling clears the
    // trigger so the row is history rather than a snooze.
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &by_date,
            "--as",
            "geoyws",
            "--choice",
            "assign-and-login",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert!(settled["returnTrigger"].is_null(), "{settled}");
}

#[test]
fn a_malformed_or_misapplied_return_trigger_is_refused_and_writes_nothing() {
    // SPA-64 refusals (A20): the malformed trigger names the three forms; a
    // trigger on a non-defer answer, on a task not on this board, or from an
    // actor who may not answer the row is refused; each refusal leaves every
    // row and the audit chain exactly as it was, and the corrected retry
    // lands.
    let fixture = Fixture::new("stale-queue-refusals");
    fixture.ok_json(&fixture.main, &["init", "--name", "STALE-REFUSE", "--json"]);
    let id = raise_carded(&fixture, "A card to snooze.", "claude@driver", &CARD)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = attention_and_chain(&fixture);
    let resolve = |actor: &str, choice: &[&str], trigger: &str| -> String {
        let mut args = vec!["resolve", id.as_str(), "--as", actor];
        args.extend_from_slice(choice);
        args.extend_from_slice(&["--return-trigger", trigger]);
        attention_refusal(&fixture, &args)
    };
    for malformed in [
        "tomorrow",
        "date:2026-02-30",
        "date:2026-10-1",
        "week:40",
        "task:",
        "",
    ] {
        let refusal = resolve("geoyws", &["--choice", "keep-parked"], malformed);
        assert!(
            refusal.contains("date:YYYY-MM-DD, task:<t-id> or event:<kind>"),
            "{malformed:?}: {refusal}"
        );
        assert_eq!(attention_and_chain(&fixture), before, "{malformed:?} wrote");
    }
    let refusal = resolve(
        "geoyws",
        &["--choice", "assign-and-login"],
        "date:2031-01-01",
    );
    assert!(
        refusal.contains("--return-trigger snoozes a defer")
            && refusal.contains("assign-and-login records approve"),
        "{refusal}"
    );
    assert_eq!(attention_and_chain(&fixture), before);
    let refusal = resolve(
        "geoyws",
        &["--choice", "custom", "--outcome", "reject", "--note", "no"],
        "date:2031-01-01",
    );
    assert!(refusal.contains("custom records reject"), "{refusal}");
    let refusal = resolve("geoyws", &["--choice", "keep-parked"], "task:t-nowhere");
    assert!(
        refusal.contains("names task t-nowhere, which is not on this board"),
        "{refusal}"
    );
    let refusal = resolve("geoyws", &["--choice", "keep-parked"], "event:no_such_kind");
    assert!(
        refusal.contains("names event kind no_such_kind, which this board never records"),
        "{refusal}"
    );
    // Only George or the raiser may answer the row, snooze included.
    let refusal = resolve(
        "someone@lane-9",
        &["--choice", "keep-parked"],
        "date:2031-01-01",
    );
    assert!(
        refusal.contains("only geoyws or that same raiser may resolve it"),
        "{refusal}"
    );
    assert_eq!(attention_and_chain(&fixture), before);

    // The retry with a well-formed trigger, through the custom defer form,
    // by the raiser.
    let snoozed = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "claude@driver",
            "--choice",
            "custom",
            "--outcome",
            "defer",
            "--note",
            "after the pin lands",
            "--return-trigger",
            "date:2031-01-01",
            "--json",
        ],
    );
    assert_eq!(snoozed["status"], "open");
    assert_eq!(snoozed["decision"]["choice"], "custom");
    assert_eq!(snoozed["decision"]["outcome"], "defer");
    assert_eq!(snoozed["decision"]["note"], "after the pin lands");
    assert_eq!(snoozed["returnTrigger"], "date:2031-01-01");
    assert!(attention_ids(&fixture, &["--status", "open"]).is_empty());
    // The snooze is on the ledger, stamped at the decision's own instant.
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_updated", "--json"],
    );
    let envelope = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["payload"]["attentionID"] == id.as_str())
        .unwrap_or_else(|| panic!("no snooze envelope: {events}"));
    assert_eq!(envelope["payload"]["returnTrigger"], "date:2031-01-01");
    assert_eq!(envelope["payload"]["decision"]["outcome"], "defer");
    assert_eq!(
        envelope["createdAt"], snoozed["decision"]["at"],
        "the snooze envelope must carry the deferral instant"
    );
}

#[test]
fn superseded_questions_settle_in_one_transact_batch_or_not_at_all() {
    // SPA-65 (A20): five superseded rows settle as one `transact` of five
    // `attention_resolve` items; a batch naming one already-resolved row, a
    // stale key or a row its actor may not resolve settles none of them,
    // names the failed index and the row, and leaves the audit chain where
    // it was; the corrected retry lands whole.
    let fixture = Fixture::new("stale-queue-batch");
    fixture.ok_json(&fixture.main, &["init", "--name", "STALE-BATCH", "--json"]);
    let raise = |body: String| -> String {
        raise_carded(&fixture, &body, "claude@driver", &CARD)["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let superseded: Vec<String> = (0..5)
        .map(|index| raise(format!("Superseded question {index}.")))
        .collect();
    let item = |id: &str, actor: &str, choice: &str| -> Value {
        json!({ "name": "attention_resolve", "arguments": {
            "id": id, "as": actor, "choice": choice,
            "note": "Decided elsewhere; superseded.",
        }})
    };
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &superseded
            .iter()
            .map(|id| item(id, "geoyws", "drop-receipt"))
            .collect::<Vec<_>>(),
    );
    assert_eq!(envelope["ok"], true, "{envelope}");
    let batch_id = envelope["batchId"].as_str().unwrap().to_owned();
    let rows = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--all", "--limit", "500", "--json"],
    );
    for id in &superseded {
        let row = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id.as_str())
            .unwrap();
        assert_eq!(row["status"], "resolved", "{row}");
        assert_eq!(row["resolvedBy"], "geoyws", "{row}");
        assert_eq!(row["decision"]["choice"], "drop-receipt", "{row}");
        assert_eq!(row["decision"]["by"], "geoyws", "{row}");
        assert_eq!(row["decision"]["note"], "Decided elsewhere; superseded.");
    }
    let stamped = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_resolved", "--json"],
    );
    assert_eq!(
        stamped
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["payload"]["batchId"] == batch_id.as_str())
            .count(),
        5,
        "each settled row carries the one batch id: {stamped}"
    );
    assert_eq!(dashboard_open_attention(&fixture, "STALE-BATCH"), 0);

    // Three fresh rows; every refused batch below must leave all three open.
    let fresh: Vec<String> = (0..3)
        .map(|index| raise(format!("Fresh question {index}.")))
        .collect();
    let open_before = attention_ids(&fixture, &["--status", "open"]);
    assert_eq!(open_before.len(), 3);
    let chain_before = board_audit(&fixture);
    for (label, failed_index, bad_item, expected) in [
        (
            "an already-resolved row",
            1,
            item(&superseded[2], "geoyws", "drop-receipt"),
            format!("attention {} was already resolved by geoyws", superseded[2]),
        ),
        (
            "a stale key",
            2,
            item(&fresh[2], "geoyws", "no-such-key"),
            format!("attention {} has no choice no-such-key", fresh[2]),
        ),
        (
            "an actor who may not resolve the row",
            2,
            item(&fresh[2], "someone@lane-9", "drop-receipt"),
            format!("attention {} was raised by claude@driver", fresh[2]),
        ),
    ] {
        let mut items = vec![
            item(&fresh[0], "geoyws", "drop-receipt"),
            item(&fresh[1], "claude@driver", "keep-parked"),
            item(&fresh[2], "geoyws", "drop-receipt"),
        ];
        items.insert(failed_index, bad_item);
        items.truncate(3);
        let envelope = transact_results(&fixture, &fixture.main, &items);
        assert_eq!(envelope["ok"], false, "{label}: {envelope}");
        assert_eq!(envelope["failedIndex"], failed_index, "{label}: {envelope}");
        assert_eq!(envelope["rolledBack"], true, "{label}: {envelope}");
        let error = envelope["results"][failed_index]["error"].as_str().unwrap();
        assert!(error.contains(&expected), "{label}: {error}");
        assert_eq!(
            attention_ids(&fixture, &["--status", "open"]),
            open_before,
            "{label}: a refused batch settled some of its rows"
        );
        assert_eq!(
            dashboard_open_attention(&fixture, "STALE-BATCH"),
            3,
            "{label}"
        );
        let chain_after = board_audit(&fixture);
        assert_eq!(chain_after["lastSeq"], chain_before["lastSeq"], "{label}");
        assert_eq!(chain_after["head"], chain_before["head"], "{label}");
    }

    // The retry without the bad item lands whole, per-item actors intact.
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            item(&fresh[0], "geoyws", "drop-receipt"),
            item(&fresh[1], "claude@driver", "keep-parked"),
            item(&fresh[2], "geoyws", "assign-and-login"),
        ],
    );
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert!(attention_ids(&fixture, &["--status", "open"]).is_empty());
    let settled = fixture.ok_json(&fixture.main, &["attention", "show", &fresh[1], "--json"]);
    assert_eq!(settled["resolvedBy"], "claude@driver");
    assert_eq!(settled["decision"]["outcome"], "defer");
}
