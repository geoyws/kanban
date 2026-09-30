use crate::codex_app_server_messages::CLIENT_NAME;
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;

const MAX_LINE_BYTES: usize = 64 * 1024;
const EXPECTED_APPROVAL_POLICY: &str = "never";
const EXPECTED_SANDBOX_TYPE: &str = "readOnly";
const EXPECTED_MODEL_PROVIDER: &str = "openai";
const EXPECTED_THREAD_SOURCE: &str = "vscode";
const EXPECTED_THREAD_STATUS: &str = "inProgress";
const EXPECTED_TURN_COMPLETED_STATUS: &str = "completed";
const EXPECTED_TURN_ID: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitInitResponse,
    AwaitThreadStartResponse,
    AwaitTurnStartResponse,
    Streaming,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    UserMessage,
    Reasoning,
    AgentMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemState {
    Started(ItemKind),
    Completed(ItemKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transition {
    Continue,
    SendThreadStart,
    SendTurnStart { thread_id: String },
    Completed,
}

#[derive(Debug, Clone)]
pub(crate) struct StateMachine {
    canonical_cwd: String,
    canonical_codex_home: String,
    required_version: String,
    expected_idempotency_key: String,
    phase: Phase,
    thread_started_seen: bool,
    turn_started_seen: bool,
    thread_id: Option<String>,
    turn_id: Option<String>,
    started_items: HashMap<String, ItemState>,
    ack_completed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AckPayload {
    accepted: bool,
    idempotency_key: String,
}

impl StateMachine {
    pub(crate) fn new(
        canonical_cwd: impl Into<String>,
        canonical_codex_home: impl Into<String>,
        required_version: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> Result<Self> {
        let canonical_cwd = canonical_cwd.into();
        let canonical_codex_home = canonical_codex_home.into();
        let required_version = required_version.into();
        let expected_idempotency_key = idempotency_key.into();
        validate_cwd(&canonical_cwd)?;
        validate_absolute_path(&canonical_codex_home, "codex home")?;
        validate_required_version(&required_version)?;
        validate_key(&expected_idempotency_key, "idempotency key")?;
        Ok(Self {
            canonical_cwd,
            canonical_codex_home,
            required_version,
            expected_idempotency_key,
            phase: Phase::AwaitInitResponse,
            thread_started_seen: false,
            turn_started_seen: false,
            thread_id: None,
            turn_id: None,
            started_items: HashMap::new(),
            ack_completed: false,
        })
    }

    pub(crate) fn feed(&mut self, line: &[u8]) -> Result<Transition> {
        if self.phase == Phase::Completed {
            bail!("state machine is already completed");
        }
        let value = parse_line(line)?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("app server line must be a JSON object"))?;
        if object.contains_key("id") && object.contains_key("method") {
            bail!("server requests are not accepted on this input");
        }

        if object.contains_key("method") {
            return self.feed_notification(object);
        }
        if object.contains_key("id") {
            return self.feed_response(object);
        }
        bail!("app server line must be a response or notification");
    }

    fn feed_response(&mut self, object: &Map<String, Value>) -> Result<Transition> {
        match self.phase {
            Phase::AwaitInitResponse => {
                expect_response_id(object, 1)?;
                let result = expect_success_response(object)?;
                validate_initialize_response(
                    result,
                    &self.canonical_codex_home,
                    &self.required_version,
                )?;
                self.phase = Phase::AwaitThreadStartResponse;
                Ok(Transition::SendThreadStart)
            }
            Phase::AwaitThreadStartResponse => {
                expect_response_id(object, 2)?;
                let result = expect_success_response(object)?;
                let thread_id = validate_thread_start_response(
                    result,
                    &self.canonical_cwd,
                    &self.required_version,
                )?;
                self.thread_id = Some(thread_id.clone());
                self.phase = Phase::AwaitTurnStartResponse;
                Ok(Transition::SendTurnStart { thread_id })
            }
            Phase::AwaitTurnStartResponse => {
                expect_response_id(object, EXPECTED_TURN_ID)?;
                let result = expect_success_response(object)?;
                let turn_id = validate_turn_start_response(result, self.thread_id.as_deref())?;
                self.turn_id = Some(turn_id);
                self.phase = Phase::Streaming;
                Ok(Transition::Continue)
            }
            Phase::Streaming => bail!("out-of-order response id"),
            Phase::Completed => unreachable!(),
        }
    }

    fn feed_notification(&mut self, object: &Map<String, Value>) -> Result<Transition> {
        let method = string_field(object, "method", "notification")?;
        match method {
            "thread/started" => {
                if self.phase != Phase::AwaitTurnStartResponse && self.phase != Phase::Streaming {
                    bail!("thread/started is only accepted after thread/start response");
                }
                if self.thread_started_seen {
                    bail!("thread/started is only accepted before turn/started");
                }
                let params = object_params(object, "thread/started")?;
                validate_thread_started(
                    params,
                    self.thread_id.as_deref(),
                    &self.canonical_cwd,
                    &self.required_version,
                )?;
                self.thread_started_seen = true;
                Ok(Transition::Continue)
            }
            "turn/started" => {
                if self.phase != Phase::Streaming {
                    bail!("turn/started is only accepted after turn/start response");
                }
                if !self.thread_started_seen {
                    bail!("turn/started is only accepted after thread/started");
                }
                if self.turn_started_seen {
                    bail!("turn/started is only accepted after thread/started");
                }
                let params = object_params(object, "turn/started")?;
                validate_turn_started(params, self.thread_id.as_deref(), self.turn_id.as_deref())?;
                self.turn_started_seen = true;
                Ok(Transition::Continue)
            }
            "item/started" => {
                self.require_turn_started("item/started")?;
                let params = object_params(object, "item/started")?;
                let item = validate_item_started(params)?;
                let thread_id = string_field(params, "threadId", "item/started")?;
                let turn_id = string_field(params, "turnId", "item/started")?;
                validate_matching_ids(
                    thread_id,
                    turn_id,
                    self.thread_id.as_deref(),
                    self.turn_id.as_deref(),
                )?;
                if self.started_items.contains_key(&item.id) {
                    bail!("duplicate item/started for {}", item.id);
                }
                self.started_items
                    .insert(item.id, ItemState::Started(item.kind));
                Ok(Transition::Continue)
            }
            "item/completed" => {
                self.require_turn_started("item/completed")?;
                let params = object_params(object, "item/completed")?;
                let item = validate_item_completed(params)?;
                let thread_id = string_field(params, "threadId", "item/completed")?;
                let turn_id = string_field(params, "turnId", "item/completed")?;
                validate_matching_ids(
                    thread_id,
                    turn_id,
                    self.thread_id.as_deref(),
                    self.turn_id.as_deref(),
                )?;
                self.complete_item(&item.id, item.kind, item.text.as_deref())?;
                Ok(Transition::Continue)
            }
            "item/agentMessage/delta" => {
                self.require_turn_started("item/agentMessage/delta")?;
                let params = object_params(object, "item/agentMessage/delta")?;
                let thread_id = string_field(params, "threadId", "item/agentMessage/delta")?;
                let turn_id = string_field(params, "turnId", "item/agentMessage/delta")?;
                validate_matching_ids(
                    thread_id,
                    turn_id,
                    self.thread_id.as_deref(),
                    self.turn_id.as_deref(),
                )?;
                let item_id = string_field(params, "itemId", "item/agentMessage/delta")?;
                self.expect_started_agent_message(item_id)?;
                string_field(params, "delta", "item/agentMessage/delta")?;
                Ok(Transition::Continue)
            }
            "thread/tokenUsage/updated" => {
                self.require_turn_started("thread/tokenUsage/updated")?;
                let params = object_params(object, "thread/tokenUsage/updated")?;
                let thread_id = string_field(params, "threadId", "thread/tokenUsage/updated")?;
                let turn_id = string_field(params, "turnId", "thread/tokenUsage/updated")?;
                validate_matching_ids(
                    thread_id,
                    turn_id,
                    self.thread_id.as_deref(),
                    self.turn_id.as_deref(),
                )?;
                let token_usage =
                    expect_object_field(params, "tokenUsage", "thread/tokenUsage/updated")?;
                expect_object_field(token_usage, "last", "thread/tokenUsage/updated tokenUsage")?;
                expect_object_field(token_usage, "total", "thread/tokenUsage/updated tokenUsage")?;
                Ok(Transition::Continue)
            }
            "turn/completed" => {
                self.require_turn_started("turn/completed")?;
                let params = object_params(object, "turn/completed")?;
                let thread_id = string_field(params, "threadId", "turn/completed")?;
                let turn = expect_object_field(params, "turn", "turn/completed")?;
                validate_matching_thread_id(
                    thread_id,
                    self.thread_id.as_deref(),
                    "turn/completed",
                )?;
                let turn_id = validate_turn_object(
                    turn,
                    self.thread_id.as_deref(),
                    Some(EXPECTED_TURN_COMPLETED_STATUS),
                    true,
                    &self.expected_idempotency_key,
                )?;
                if let Some(expected_turn_id) = self.turn_id.as_deref()
                    && turn_id != expected_turn_id
                {
                    bail!("turn/completed turn id does not match");
                }
                if !self.ack_completed || !self.turn_contains_ack(turn) {
                    bail!("turn/completed must follow the exact ack");
                }
                self.reconcile_completed_turn(turn)?;
                self.phase = Phase::Completed;
                self.turn_id = Some(turn_id);
                Ok(Transition::Completed)
            }
            other => bail!("unsupported notification method {other}"),
        }
    }

    fn require_turn_started(&self, context: &str) -> Result<()> {
        if !self.thread_started_seen || !self.turn_started_seen {
            bail!("{context} is only accepted after turn/started");
        }
        Ok(())
    }

    fn complete_item(&mut self, item_id: &str, kind: ItemKind, text: Option<&str>) -> Result<()> {
        match self.started_items.get(item_id) {
            Some(ItemState::Started(seen_kind)) if *seen_kind == kind => {}
            Some(ItemState::Completed(_)) => bail!("duplicate item/completed for {item_id}"),
            Some(ItemState::Started(_)) => {
                bail!("item/completed type does not match item/started for {item_id}")
            }
            None => bail!("item/completed arrived before item/started for {item_id}"),
        }
        if let ItemKind::AgentMessage = kind {
            if self.ack_completed {
                bail!("duplicate agentMessage ack");
            }
            let text = text.ok_or_else(|| anyhow::anyhow!("agentMessage item must carry text"))?;
            validate_ack_text(text, &self.expected_idempotency_key)?;
            self.ack_completed = true;
        }
        self.started_items
            .insert(item_id.to_owned(), ItemState::Completed(kind));
        Ok(())
    }

    fn expect_started_agent_message(&self, item_id: &str) -> Result<()> {
        match self.started_items.get(item_id) {
            Some(ItemState::Started(ItemKind::AgentMessage)) => Ok(()),
            Some(ItemState::Started(_)) => bail!("item {item_id} is not an agentMessage"),
            Some(ItemState::Completed(_)) => bail!("item {item_id} is already completed"),
            None => bail!("item/agentMessage/delta arrived before item/started for {item_id}"),
        }
    }

    fn turn_contains_ack(&self, turn: &Map<String, Value>) -> bool {
        turn.get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().any(|item| {
                    item.as_object()
                        .and_then(|object| object.get("type").and_then(Value::as_str))
                        == Some("agentMessage")
                        && item
                            .as_object()
                            .and_then(|object| object.get("text").and_then(Value::as_str))
                            .is_some_and(|text| {
                                validate_ack_text(text, &self.expected_idempotency_key).is_ok()
                            })
                })
            })
            .unwrap_or(false)
    }

    fn turn_items_view_is_summary(turn: &Map<String, Value>) -> bool {
        turn.get("itemsView").and_then(Value::as_str) == Some("summary")
    }

    fn reconcile_completed_turn(&self, turn: &Map<String, Value>) -> Result<()> {
        for (item_id, state) in &self.started_items {
            if matches!(state, ItemState::Started(_)) {
                bail!("turn/completed has incomplete tracked item {item_id}");
            }
        }

        let items = turn
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("turn.items must be an array"))?;
        let summary_items_view = Self::turn_items_view_is_summary(turn);
        let mut final_items = HashMap::with_capacity(items.len());
        for value in items {
            let item = validate_thread_item(value)?;
            if final_items.insert(item.id.clone(), item.kind).is_some() {
                bail!("turn/completed has duplicate item id {}", item.id);
            }
            match self.started_items.get(&item.id) {
                Some(ItemState::Completed(kind)) if *kind == item.kind => {}
                Some(ItemState::Completed(_)) => {
                    bail!("turn/completed item {} kind does not match", item.id)
                }
                Some(ItemState::Started(_)) => unreachable!(),
                None => bail!("turn/completed item {} was not tracked", item.id),
            }
        }
        for item_id in self.started_items.keys() {
            if !final_items.contains_key(item_id) {
                match self.started_items.get(item_id) {
                    Some(ItemState::Completed(ItemKind::UserMessage)) if summary_items_view => {}
                    _ => bail!("turn/completed is missing tracked item {item_id}"),
                }
            }
        }
        Ok(())
    }
}

fn parse_line(line: &[u8]) -> Result<Value> {
    if line.is_empty() {
        bail!("app server line is empty");
    }
    if line.len() > MAX_LINE_BYTES {
        bail!("app server line exceeds {MAX_LINE_BYTES} bytes");
    }
    let text = std::str::from_utf8(line)?;
    if text.trim().is_empty() {
        bail!("app server line is empty");
    }
    let mut de = serde_json::Deserializer::from_str(text);
    let value = Value::deserialize(&mut de)?;
    de.end()?;
    Ok(value)
}

fn expect_response_id(object: &Map<String, Value>, expected: u64) -> Result<()> {
    match object.get("id") {
        Some(Value::Number(number)) if number.as_u64() == Some(expected) => Ok(()),
        Some(value) => bail!("expected response id {expected}, got {value}"),
        None => bail!("missing response id {expected}"),
    }
}

fn expect_success_response(object: &Map<String, Value>) -> Result<&Map<String, Value>> {
    if object.contains_key("error") {
        bail!("error responses are rejected");
    }
    object
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("successful response must carry a result object"))
}

fn object_params<'a>(
    object: &'a Map<String, Value>,
    method: &str,
) -> Result<&'a Map<String, Value>> {
    object
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("{method} params must be an object"))
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str, context: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{context} field {key} must be a string"))
}

fn bool_field(object: &Map<String, Value>, key: &str, context: &str) -> Result<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("{context} field {key} must be a boolean"))
}

fn expect_object_field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a Map<String, Value>> {
    object
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("{context} field {key} must be an object"))
}

fn validate_thread_start_response(
    result: &Map<String, Value>,
    expected_cwd: &str,
    required_version: &str,
) -> Result<String> {
    if string_field(result, "approvalPolicy", "thread/start response")? != EXPECTED_APPROVAL_POLICY
    {
        bail!("thread/start response approvalPolicy must be never");
    }
    let cwd = string_field(result, "cwd", "thread/start response")?;
    if cwd != expected_cwd {
        bail!("thread/start response cwd does not match");
    }
    let sandbox = expect_object_field(result, "sandbox", "thread/start response")?;
    if string_field(sandbox, "type", "thread/start response sandbox")? != EXPECTED_SANDBOX_TYPE {
        bail!("thread/start response sandbox must be readOnly");
    }
    if bool_field(sandbox, "networkAccess", "thread/start response sandbox")? {
        bail!("thread/start response sandbox networkAccess must be false");
    }
    nonempty_string_field(result, "model", "thread/start response")?;
    if string_field(result, "modelProvider", "thread/start response")? != EXPECTED_MODEL_PROVIDER {
        bail!("thread/start response modelProvider does not match");
    }
    match string_field(result, "approvalsReviewer", "thread/start response")? {
        "user" | "auto_review" | "guardian_subagent" => {}
        other => bail!("thread/start response approvalsReviewer {other} is not accepted"),
    }
    let thread = expect_object_field(result, "thread", "thread/start response")?;
    let thread_id = validate_thread_object(
        thread,
        Some(expected_cwd),
        Some(true),
        true,
        required_version,
    )?;
    if let Some(approval_policy) = result.get("approvalPolicy")
        && !approval_policy.is_string()
    {
        bail!("thread/start response approvalPolicy must be a string");
    }
    Ok(thread_id)
}

fn validate_thread_started(
    params: &Map<String, Value>,
    expected_thread_id: Option<&str>,
    expected_cwd: &str,
    required_version: &str,
) -> Result<()> {
    let thread = expect_object_field(params, "thread", "thread/started")?;
    let thread_id = validate_thread_object(
        thread,
        Some(expected_cwd),
        Some(true),
        true,
        required_version,
    )?;
    if let Some(expected_thread_id) = expected_thread_id
        && thread_id != expected_thread_id
    {
        bail!("thread/started thread id does not match");
    }
    Ok(())
}

fn validate_turn_start_response(
    result: &Map<String, Value>,
    expected_thread_id: Option<&str>,
) -> Result<String> {
    let turn = expect_object_field(result, "turn", "turn/start response")?;
    validate_turn_object(
        turn,
        expected_thread_id,
        Some(EXPECTED_THREAD_STATUS),
        false,
        "",
    )?;
    validate_empty_turn_items(turn, "turn/start response")?;
    Ok(string_field(turn, "id", "turn/start response turn")?.to_owned())
}

fn validate_turn_started(
    params: &Map<String, Value>,
    expected_thread_id: Option<&str>,
    expected_turn_id: Option<&str>,
) -> Result<()> {
    let thread_id = string_field(params, "threadId", "turn/started")?;
    validate_matching_thread_id(thread_id, expected_thread_id, "turn/started")?;
    let turn = expect_object_field(params, "turn", "turn/started")?;
    let turn_id = validate_turn_object(
        turn,
        expected_thread_id,
        Some(EXPECTED_THREAD_STATUS),
        false,
        "",
    )?;
    validate_empty_turn_items(turn, "turn/started")?;
    if let Some(expected_turn_id) = expected_turn_id
        && turn_id != expected_turn_id
    {
        bail!("turn/started turn id does not match");
    }
    if string_field(turn, "status", "turn/started")? != EXPECTED_THREAD_STATUS {
        bail!("turn/started status must be inProgress");
    }
    Ok(())
}

fn validate_turn_object(
    turn: &Map<String, Value>,
    expected_thread_id: Option<&str>,
    expected_status: Option<&str>,
    require_completed_ack: bool,
    expected_idempotency_key: &str,
) -> Result<String> {
    let turn_id = string_field(turn, "id", "turn")?;
    validate_uuid_v7(turn_id, "turn id")?;
    if let Some(expected_thread_id) = expected_thread_id
        && turn_id == expected_thread_id
    {
        bail!("turn id must differ from thread id");
    }
    let status = string_field(turn, "status", "turn")?;
    if Some(status) != expected_status {
        bail!("turn status {status} is not accepted");
    }
    if require_completed_ack
        && let Some(error) = turn.get("error")
        && !error.is_null()
    {
        bail!("turn/completed error must be null or absent");
    }
    let items = turn
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("turn.items must be an array"))?;
    validate_turn_items(items, require_completed_ack, expected_idempotency_key)?;
    Ok(turn_id.to_owned())
}

fn validate_turn_items(
    items: &[Value],
    require_completed_ack: bool,
    expected_idempotency_key: &str,
) -> Result<()> {
    let mut agent_message_count = 0usize;
    let mut ack_seen = false;
    for item in items {
        let item = validate_thread_item(item)?;
        if item.kind == ItemKind::AgentMessage {
            agent_message_count += 1;
            if let Some(text) = item.text.as_deref() {
                if require_completed_ack {
                    validate_ack_text(text, expected_idempotency_key)?;
                    ack_seen = true;
                }
            } else {
                bail!("agentMessage items must carry text");
            }
        }
    }
    if require_completed_ack && (!ack_seen || agent_message_count != 1) {
        bail!("turn/completed must follow the exact ack");
    }
    Ok(())
}

struct ValidatedItem {
    id: String,
    kind: ItemKind,
    text: Option<String>,
}

fn validate_thread_item(value: &Value) -> Result<ValidatedItem> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("item must be an object"))?;
    let id = string_field(object, "id", "item")?;
    validate_item_id(id)?;
    let kind = match string_field(object, "type", "item")? {
        "userMessage" => {
            expect_array_field(object, "content", "userMessage item")?;
            ItemKind::UserMessage
        }
        "reasoning" => {
            if let Some(content) = object.get("content")
                && !content.is_array()
            {
                bail!("reasoning item content must be an array");
            }
            if let Some(summary) = object.get("summary")
                && !summary.is_array()
            {
                bail!("reasoning item summary must be an array");
            }
            ItemKind::Reasoning
        }
        "agentMessage" => ItemKind::AgentMessage,
        other => bail!("unsupported item type {other}"),
    };
    let text = if kind == ItemKind::AgentMessage {
        Some(string_field(object, "text", "agentMessage item")?.to_owned())
    } else {
        None
    };
    Ok(ValidatedItem {
        id: id.to_owned(),
        kind,
        text,
    })
}

fn validate_item_started(params: &Map<String, Value>) -> Result<ValidatedItem> {
    expect_integer_field(params, "startedAtMs", "item/started")?;
    let item = expect_object_field(params, "item", "item/started")?;
    validate_thread_item(&Value::Object(item.clone()))
}

fn validate_item_completed(params: &Map<String, Value>) -> Result<ValidatedItem> {
    expect_integer_field(params, "completedAtMs", "item/completed")?;
    let item = expect_object_field(params, "item", "item/completed")?;
    validate_thread_item(&Value::Object(item.clone()))
}

fn expect_array_field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a Vec<Value>> {
    object
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("{context} field {key} must be an array"))
}

fn expect_integer_field(object: &Map<String, Value>, key: &str, context: &str) -> Result<i64> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("{context} field {key} must be an integer"))
}

fn validate_matching_ids(
    thread_id: &str,
    turn_id: &str,
    expected_thread_id: Option<&str>,
    expected_turn_id: Option<&str>,
) -> Result<()> {
    validate_matching_thread_id(thread_id, expected_thread_id, "notification")?;
    if let Some(expected_turn_id) = expected_turn_id
        && turn_id != expected_turn_id
    {
        bail!("turn id does not match");
    }
    Ok(())
}

fn validate_matching_thread_id(actual: &str, expected: Option<&str>, context: &str) -> Result<()> {
    if let Some(expected) = expected
        && actual != expected
    {
        bail!("{context} thread id does not match");
    }
    Ok(())
}

fn validate_thread_object(
    thread: &Map<String, Value>,
    expected_cwd: Option<&str>,
    expect_ephemeral: Option<bool>,
    require_idle_status: bool,
    required_version: &str,
) -> Result<String> {
    let thread_id = string_field(thread, "id", "thread")?;
    validate_uuid_v7(thread_id, "thread id")?;
    let cli_version = nonempty_string_field(thread, "cliVersion", "thread")?;
    if cli_version != required_version {
        bail!("thread cliVersion does not match");
    }
    if string_field(thread, "modelProvider", "thread")? != EXPECTED_MODEL_PROVIDER {
        bail!("thread modelProvider does not match");
    }
    string_field(thread, "preview", "thread")?;
    nonempty_string_field(thread, "sessionId", "thread")?;
    expect_integer_field(thread, "createdAt", "thread")?;
    expect_integer_field(thread, "updatedAt", "thread")?;
    match thread.get("projectId") {
        Some(Value::String(_)) | Some(Value::Null) => {}
        Some(_) => bail!("thread field projectId must be a string or null"),
        None => bail!("thread field projectId is required"),
    }
    if string_field(thread, "source", "thread")? != EXPECTED_THREAD_SOURCE {
        bail!("thread source does not match");
    }
    let status = expect_object_field(thread, "status", "thread")?;
    let status_type = nonempty_string_field(status, "type", "thread status")?;
    if require_idle_status && status_type != "idle" {
        bail!("thread status type must be idle");
    }
    expect_array_field(thread, "turns", "thread")?;
    if let Some(expected_cwd) = expected_cwd {
        let cwd = string_field(thread, "cwd", "thread")?;
        if cwd != expected_cwd {
            bail!("thread cwd does not match");
        }
    }
    if let Some(expected_ephemeral) = expect_ephemeral
        && bool_field(thread, "ephemeral", "thread")? != expected_ephemeral
    {
        bail!("thread ephemeral flag does not match");
    }
    Ok(thread_id.to_owned())
}

fn validate_empty_turn_items(turn: &Map<String, Value>, context: &str) -> Result<()> {
    let items = turn
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("{context} items must be an array"))?;
    if !items.is_empty() {
        bail!("{context} items must be empty");
    }
    Ok(())
}

fn validate_key(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must be nonempty");
    }
    if value.chars().any(|ch| ch.is_control()) {
        bail!("{label} must not contain control characters");
    }
    if value.len() > MAX_LINE_BYTES {
        bail!("{label} exceeds {MAX_LINE_BYTES} bytes");
    }
    Ok(())
}

fn validate_cwd(value: &str) -> Result<()> {
    validate_absolute_path(value, "cwd")
}

fn validate_required_version(value: &str) -> Result<()> {
    validate_key(value, "required version")
}

fn validate_absolute_path(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must be a nonempty absolute path");
    }
    if !std::path::Path::new(value).is_absolute() {
        bail!("{label} must be a nonempty absolute path");
    }
    if value.chars().any(|ch| ch.is_control()) {
        bail!("{label} must not contain control characters");
    }
    if value.len() > MAX_LINE_BYTES {
        bail!("{label} exceeds {MAX_LINE_BYTES} bytes");
    }
    Ok(())
}

fn validate_initialize_response(
    result: &Map<String, Value>,
    expected_codex_home: &str,
    required_version: &str,
) -> Result<()> {
    let codex_home = nonempty_string_field(result, "codexHome", "initialize response")?;
    if !Path::new(codex_home).is_absolute() {
        bail!("initialize response codexHome must be an absolute path");
    }
    if codex_home != expected_codex_home {
        bail!("initialize response codexHome does not match");
    }
    nonempty_string_field(result, "platformFamily", "initialize response")?;
    nonempty_string_field(result, "platformOs", "initialize response")?;
    let user_agent = nonempty_string_field(result, "userAgent", "initialize response")?;
    validate_user_agent(user_agent, required_version)?;
    Ok(())
}

/// The app server reports `{client}/{codexVersion} ({os}; {arch}) {terminal}
/// ({client}; {clientVersion})`, where the originator is the client name we
/// send. Bind exactly the two fields that identify us and pin the Codex
/// version; the platform and terminal text in between is descriptive only.
fn validate_user_agent(user_agent: &str, required_version: &str) -> Result<()> {
    if !user_agent.starts_with(&format!("{CLIENT_NAME}/{required_version} ")) {
        bail!("initialize response userAgent does not match");
    }
    if !user_agent.ends_with(&format!("({CLIENT_NAME}; {})", env!("CARGO_PKG_VERSION"))) {
        bail!("initialize response userAgent does not match");
    }
    Ok(())
}

fn validate_uuid_v7(value: &str, label: &str) -> Result<()> {
    if !is_uuid_v7(value) {
        bail!("{label} must be a UUIDv7");
    }
    Ok(())
}

fn is_uuid_v7(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for &idx in &[8usize, 13, 18, 23] {
        if bytes[idx] != b'-' {
            return false;
        }
    }
    for (idx, byte) in bytes.iter().enumerate() {
        if [8usize, 13, 18, 23].contains(&idx) {
            continue;
        }
        if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    matches!(bytes[14], b'7') && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}

fn validate_ack_text(text: &str, expected_idempotency_key: &str) -> Result<()> {
    let ack: AckPayload = serde_json::from_str(text)?;
    if !ack.accepted || ack.idempotency_key != expected_idempotency_key {
        bail!("turn ack does not match the expected idempotency key");
    }
    Ok(())
}

fn validate_item_id(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("item id must be nonempty");
    }
    if value.chars().any(|ch| ch.is_control()) {
        bail!("item id must not contain control characters");
    }
    if value.len() > MAX_LINE_BYTES {
        bail!("item id exceeds {MAX_LINE_BYTES} bytes");
    }
    Ok(())
}

fn nonempty_string_field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str> {
    let value = string_field(object, key, context)?;
    if value.is_empty() {
        bail!("{context} field {key} must be nonempty");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CWD: &str = "/private/tmp/kanban-app-messages";
    const CODEX_HOME: &str = "/private/tmp/kanban-codex-home";
    const TEST_CODEX_VERSION: &str = "0.150.1";
    const ID_KEY: &str = "ack-key-1";
    const THREAD_ID: &str = "01890f3b-2c3d-7abc-8def-0123456789ab";
    const TURN_ID: &str = "01890f3b-2c3d-7abc-8def-0123456789ac";

    fn new_state() -> StateMachine {
        StateMachine::new(CWD, CODEX_HOME, TEST_CODEX_VERSION, ID_KEY).unwrap()
    }

    fn line(value: Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        bytes
    }

    /// Everything up to the trailing client group of the `userAgent` the
    /// installed HAX `codex-cli 0.150.1` returned for this adapter's own
    /// `initialize` line, spelled out verbatim so the accepting test stays
    /// pinned to the measurement. The measured Codex version stays a literal;
    /// only the trailing client group tracks `CARGO_PKG_VERSION`, so bumping
    /// this crate cannot be mistaken for Codex protocol drift.
    const MEASURED_USER_AGENT_HEAD: &str =
        "kanban-codex-app-server-adapter/0.150.1 (Ubuntu 24.4.0; x86_64) unknown";

    fn measured_user_agent() -> String {
        format!(
            "{MEASURED_USER_AGENT_HEAD} ({CLIENT_NAME}; {})",
            env!("CARGO_PKG_VERSION")
        )
    }

    fn user_agent() -> String {
        format!(
            "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {})",
            env!("CARGO_PKG_VERSION")
        )
    }

    fn init_response() -> Vec<u8> {
        initialize_response(CODEX_HOME, &user_agent(), "unix", "linux")
    }

    fn initialize_response(
        codex_home: &str,
        user_agent: &str,
        platform_family: &str,
        platform_os: &str,
    ) -> Vec<u8> {
        line(json!({
            "id": 1,
            "result": {
                "codexHome": codex_home,
                "platformFamily": platform_family,
                "platformOs": platform_os,
                "userAgent": user_agent
            }
        }))
    }

    fn thread_start_response(cwd: &str, thread_id: &str) -> Vec<u8> {
        line(json!({
            "id": 2,
            "result": {
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "cwd": cwd,
                "model": "codex-1",
                "modelProvider": "openai",
                "sandbox": {
                    "type": "readOnly",
                    "networkAccess": false
                },
                    "thread": {
                    "cliVersion": TEST_CODEX_VERSION,
                    "id": thread_id,
                    "cwd": cwd,
                    "ephemeral": true,
                    "modelProvider": "openai",
                    "preview": "",
                    "sessionId": "session-1",
                    "createdAt": 1,
                    "updatedAt": 2,
                    "projectId": null,
                    "source": "vscode",
                    "status": {"type": "idle"},
                    "turns": []
                }
            }
        }))
    }

    fn turn_start_response(_thread_id: &str, turn_id: &str, status: &str) -> Vec<u8> {
        line(json!({
            "id": 3,
            "result": {
                "turn": {
                    "id": turn_id,
                    "status": status,
                    "items": []
                }
            }
        }))
    }

    fn thread_started(thread_id: &str, cwd: &str) -> Vec<u8> {
        line(json!({
            "method": "thread/started",
            "params": {
                    "thread": {
                    "cliVersion": TEST_CODEX_VERSION,
                    "id": thread_id,
                    "cwd": cwd,
                    "ephemeral": true,
                    "modelProvider": "openai",
                    "preview": "",
                    "sessionId": "session-1",
                    "createdAt": 1,
                    "updatedAt": 2,
                    "projectId": null,
                    "source": "vscode",
                    "status": {"type": "idle"},
                    "turns": []
                }
            }
        }))
    }

    fn turn_started(thread_id: &str, turn_id: &str) -> Vec<u8> {
        turn_started_with_status(thread_id, turn_id, "inProgress")
    }

    fn turn_started_with_status(thread_id: &str, turn_id: &str, status: &str) -> Vec<u8> {
        line(json!({
            "method": "turn/started",
            "params": {
                "threadId": thread_id,
                "turn": {
                    "id": turn_id,
                    "status": status,
                    "items": []
                }
            }
        }))
    }

    fn item_started(thread_id: &str, turn_id: &str, item: Value) -> Vec<u8> {
        line(json!({
            "method": "item/started",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "startedAtMs": 1,
                "item": item
            }
        }))
    }

    fn item_completed(thread_id: &str, turn_id: &str, item: Value) -> Vec<u8> {
        line(json!({
            "method": "item/completed",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "completedAtMs": 2,
                "item": item
            }
        }))
    }

    fn delta(thread_id: &str, turn_id: &str, item_id: &str) -> Vec<u8> {
        line(json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": item_id,
                "delta": "hello"
            }
        }))
    }

    fn token_usage(thread_id: &str, turn_id: &str) -> Vec<u8> {
        line(json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "tokenUsage": {
                    "last": {"input": 1, "output": 2},
                    "total": {"input": 1, "output": 2}
                }
            }
        }))
    }

    fn turn_completed(thread_id: &str, turn_id: &str, items: Vec<Value>) -> Vec<u8> {
        turn_completed_with_items_view(thread_id, turn_id, items, None)
    }

    fn turn_completed_with_items_view(
        thread_id: &str,
        turn_id: &str,
        items: Vec<Value>,
        items_view: Option<&str>,
    ) -> Vec<u8> {
        let mut turn = json!({
            "id": turn_id,
            "status": "completed",
            "error": null,
            "items": items
        });
        if let Some(items_view) = items_view {
            turn.as_object_mut()
                .unwrap()
                .insert("itemsView".to_owned(), Value::String(items_view.to_owned()));
        }
        line(json!({
            "method": "turn/completed",
            "params": {
                "threadId": thread_id,
                "turn": turn
            }
        }))
    }

    fn turn_completed_with_status(
        thread_id: &str,
        turn_id: &str,
        status: &str,
        error: Value,
        items: Vec<Value>,
    ) -> Vec<u8> {
        line(json!({
            "method": "turn/completed",
            "params": {
                "threadId": thread_id,
                "turn": {
                    "id": turn_id,
                    "status": status,
                    "error": error,
                    "items": items
                }
            }
        }))
    }

    fn user_item(id: &str) -> Value {
        json!({
            "id": id,
            "type": "userMessage",
            "content": [{"type": "text", "text": "hi"}]
        })
    }

    fn reasoning_item(id: &str) -> Value {
        json!({
            "id": id,
            "type": "reasoning",
            "content": ["reason"],
            "summary": ["summary"]
        })
    }

    fn agent_item(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "type": "agentMessage",
            "text": text
        })
    }

    fn item_with_type(id: &str, ty: &str) -> Value {
        json!({
            "id": id,
            "type": ty
        })
    }

    fn ack_text() -> String {
        serde_json::to_string(&json!({
            "accepted": true,
            "idempotencyKey": ID_KEY
        }))
        .unwrap()
    }

    fn malformed_ack_text() -> String {
        serde_json::to_string(&json!({
            "accepted": true,
            "idempotencyKey": ID_KEY,
            "extra": 1
        }))
        .unwrap()
    }

    fn streaming_state() -> StateMachine {
        let mut state = new_state();
        state.feed(&init_response()).unwrap();
        state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap();
        state
            .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
            .unwrap();
        state.feed(&thread_started(THREAD_ID, CWD)).unwrap();
        state.feed(&turn_started(THREAD_ID, TURN_ID)).unwrap();
        state
    }

    fn pre_lifecycle_state() -> StateMachine {
        let mut state = new_state();
        state.feed(&init_response()).unwrap();
        state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap();
        state
            .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
            .unwrap();
        state
    }

    fn complete_ack(state: &mut StateMachine) {
        state
            .feed(&item_started(
                THREAD_ID,
                TURN_ID,
                agent_item("a-1", &ack_text()),
            ))
            .unwrap();
        state
            .feed(&item_completed(
                THREAD_ID,
                TURN_ID,
                agent_item("a-1", &ack_text()),
            ))
            .unwrap();
    }

    fn completion_error(state: &mut StateMachine, items: Vec<Value>) -> String {
        state
            .feed(&turn_completed(THREAD_ID, TURN_ID, items))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn happy_sequence_reaches_completion() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { ref thread_id } if thread_id == THREAD_ID
        ));
        assert!(matches!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state.feed(&thread_started(THREAD_ID, CWD)).unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state.feed(&turn_started(THREAD_ID, TURN_ID)).unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&item_started(THREAD_ID, TURN_ID, user_item("u-1")))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, user_item("u-1")))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&item_started(THREAD_ID, TURN_ID, reasoning_item("r-1")))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, reasoning_item("r-1")))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &ack_text())
                ))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state.feed(&delta(THREAD_ID, TURN_ID, "a-1")).unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &ack_text())
                ))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state.feed(&token_usage(THREAD_ID, TURN_ID)).unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&turn_completed(
                    THREAD_ID,
                    TURN_ID,
                    vec![
                        user_item("u-1"),
                        reasoning_item("r-1"),
                        agent_item("a-1", &ack_text())
                    ],
                ))
                .unwrap(),
            Transition::Completed
        ));
        assert!(state.feed(&token_usage(THREAD_ID, TURN_ID)).is_err());
    }

    #[test]
    fn reconciles_exact_completed_item_set() {
        let mut state = streaming_state();
        for item in [user_item("u-1"), reasoning_item("r-1")] {
            state
                .feed(&item_started(THREAD_ID, TURN_ID, item.clone()))
                .unwrap();
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, item))
                .unwrap();
        }
        complete_ack(&mut state);

        assert!(matches!(
            state
                .feed(&turn_completed(
                    THREAD_ID,
                    TURN_ID,
                    vec![
                        user_item("u-1"),
                        reasoning_item("r-1"),
                        agent_item("a-1", &ack_text()),
                    ],
                ))
                .unwrap(),
            Transition::Completed
        ));
    }

    #[test]
    fn reconciles_summary_completed_turn_allows_missing_completed_user_item() {
        let mut state = streaming_state();
        for item in [user_item("u-1"), reasoning_item("r-1")] {
            state
                .feed(&item_started(THREAD_ID, TURN_ID, item.clone()))
                .unwrap();
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, item))
                .unwrap();
        }
        complete_ack(&mut state);

        assert!(matches!(
            state
                .feed(&turn_completed_with_items_view(
                    THREAD_ID,
                    TURN_ID,
                    vec![reasoning_item("r-1"), agent_item("a-1", &ack_text())],
                    Some("summary"),
                ))
                .unwrap(),
            Transition::Completed
        ));
    }

    #[test]
    fn rejects_turn_completion_with_dangling_user_item() {
        let mut state = streaming_state();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        complete_ack(&mut state);

        let error = completion_error(
            &mut state,
            vec![user_item("u-1"), agent_item("a-1", &ack_text())],
        );
        assert!(error.contains("incomplete tracked item u-1"));
    }

    #[test]
    fn rejects_summary_turn_without_items_view_for_missing_completed_user_item() {
        let mut state = streaming_state();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        state
            .feed(&item_completed(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, reasoning_item("r-1")))
            .unwrap();
        state
            .feed(&item_completed(THREAD_ID, TURN_ID, reasoning_item("r-1")))
            .unwrap();
        complete_ack(&mut state);

        let error = completion_error(
            &mut state,
            vec![reasoning_item("r-1"), agent_item("a-1", &ack_text())],
        );
        assert!(error.contains("missing tracked item u-1"));
    }

    #[test]
    fn rejects_turn_completion_with_dangling_reasoning_item() {
        let mut state = streaming_state();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, reasoning_item("r-1")))
            .unwrap();
        complete_ack(&mut state);

        let error = completion_error(
            &mut state,
            vec![reasoning_item("r-1"), agent_item("a-1", &ack_text())],
        );
        assert!(error.contains("incomplete tracked item r-1"));
    }

    #[test]
    fn rejects_summary_turn_missing_reasoning_item() {
        let mut state = streaming_state();
        for item in [user_item("u-1"), reasoning_item("r-1")] {
            state
                .feed(&item_started(THREAD_ID, TURN_ID, item.clone()))
                .unwrap();
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, item))
                .unwrap();
        }
        complete_ack(&mut state);

        let error = state
            .feed(&turn_completed_with_items_view(
                THREAD_ID,
                TURN_ID,
                vec![user_item("u-1"), agent_item("a-1", &ack_text())],
                Some("summary"),
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing tracked item r-1"));
    }

    #[test]
    fn rejects_missing_exact_ack_even_when_items_view_is_summary() {
        let mut state = streaming_state();
        for item in [user_item("u-1"), reasoning_item("r-1")] {
            state
                .feed(&item_started(THREAD_ID, TURN_ID, item.clone()))
                .unwrap();
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, item))
                .unwrap();
        }
        complete_ack(&mut state);

        let error = state
            .feed(&turn_completed_with_items_view(
                THREAD_ID,
                TURN_ID,
                vec![user_item("u-1"), reasoning_item("r-1")],
                Some("summary"),
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("turn/completed must follow the exact ack"));
    }

    #[test]
    fn rejects_summary_mode_missing_incomplete_user_item() {
        let mut state = streaming_state();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, reasoning_item("r-1")))
            .unwrap();
        state
            .feed(&item_completed(THREAD_ID, TURN_ID, reasoning_item("r-1")))
            .unwrap();
        complete_ack(&mut state);

        let error = state
            .feed(&turn_completed_with_items_view(
                THREAD_ID,
                TURN_ID,
                vec![reasoning_item("r-1"), agent_item("a-1", &ack_text())],
                Some("summary"),
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("incomplete tracked item u-1"));
    }

    #[test]
    fn rejects_non_summary_items_view_for_missing_completed_user_item() {
        let mut state = streaming_state();
        for item in [user_item("u-1"), reasoning_item("r-1")] {
            state
                .feed(&item_started(THREAD_ID, TURN_ID, item.clone()))
                .unwrap();
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, item))
                .unwrap();
        }
        complete_ack(&mut state);

        let error = state
            .feed(&turn_completed_with_items_view(
                THREAD_ID,
                TURN_ID,
                vec![reasoning_item("r-1"), agent_item("a-1", &ack_text())],
                Some("full"),
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing tracked item u-1"));
    }

    #[test]
    fn rejects_turn_completion_with_dangling_agent_item() {
        let mut state = streaming_state();
        state
            .feed(&item_started(
                THREAD_ID,
                TURN_ID,
                agent_item("a-dangling", "not completed"),
            ))
            .unwrap();
        complete_ack(&mut state);

        let error = completion_error(&mut state, vec![agent_item("a-1", &ack_text())]);
        assert!(error.contains("incomplete tracked item a-dangling"));
    }

    #[test]
    fn rejects_turn_completion_missing_a_tracked_item() {
        let mut state = streaming_state();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        state
            .feed(&item_completed(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        complete_ack(&mut state);

        let error = completion_error(&mut state, vec![agent_item("a-1", &ack_text())]);
        assert!(error.contains("missing tracked item u-1"));
    }

    #[test]
    fn rejects_turn_completion_with_an_extra_item() {
        let mut state = streaming_state();
        complete_ack(&mut state);

        let error = completion_error(
            &mut state,
            vec![user_item("u-extra"), agent_item("a-1", &ack_text())],
        );
        assert!(error.contains("item u-extra was not tracked"));
    }

    #[test]
    fn rejects_untracked_exact_ack_lookalike() {
        let mut state = streaming_state();
        complete_ack(&mut state);

        let error = completion_error(&mut state, vec![agent_item("a-lookalike", &ack_text())]);
        assert!(error.contains("item a-lookalike was not tracked"));
    }

    #[test]
    fn rejects_turn_completion_with_a_duplicate_item_id() {
        let mut state = streaming_state();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        state
            .feed(&item_completed(THREAD_ID, TURN_ID, user_item("u-1")))
            .unwrap();
        complete_ack(&mut state);

        let error = completion_error(
            &mut state,
            vec![
                user_item("u-1"),
                user_item("u-1"),
                agent_item("a-1", &ack_text()),
            ],
        );
        assert!(error.contains("duplicate item id u-1"));
    }

    #[test]
    fn rejects_turn_completion_with_an_item_kind_mismatch() {
        let mut state = streaming_state();
        state
            .feed(&item_started(THREAD_ID, TURN_ID, user_item("shared-id")))
            .unwrap();
        state
            .feed(&item_completed(THREAD_ID, TURN_ID, user_item("shared-id")))
            .unwrap();
        complete_ack(&mut state);

        let error = completion_error(
            &mut state,
            vec![reasoning_item("shared-id"), agent_item("a-1", &ack_text())],
        );
        assert!(error.contains("item shared-id kind does not match"));
    }

    #[test]
    fn accepts_the_measured_real_world_initialize_user_agent() {
        // The measured head pins the Codex half against the real server; the
        // derived form must agree with it under the versions under test.
        assert!(
            user_agent().starts_with(MEASURED_USER_AGENT_HEAD),
            "the required Codex version drifted from the HAX measurement"
        );
        let mut state = new_state();
        assert!(matches!(
            state
                .feed(&initialize_response(
                    CODEX_HOME,
                    &measured_user_agent(),
                    "unix",
                    "linux"
                ))
                .unwrap(),
            Transition::SendThreadStart
        ));
    }

    #[test]
    fn accepts_initialize_user_agent_with_unfamiliar_descriptive_text() {
        let tolerated = format!(
            "{CLIENT_NAME}/{TEST_CODEX_VERSION} (macOS 26.1; aarch64) ghostty ({CLIENT_NAME}; {})",
            env!("CARGO_PKG_VERSION")
        );
        assert_ne!(tolerated, measured_user_agent());
        let mut state = new_state();
        assert!(matches!(
            state
                .feed(&initialize_response(
                    CODEX_HOME, &tolerated, "unix", "linux"
                ))
                .unwrap(),
            Transition::SendThreadStart
        ));
    }

    /// Every case here must be rejected by `validate_user_agent` specifically.
    /// `validate_initialize_response` can bail on `userAgent` two ways - `does
    /// not match` from `validate_user_agent`, `must be nonempty` from
    /// `nonempty_string_field` - so matching merely on "userAgent" would let a
    /// case silently degrade from a drift rejection to a shape rejection and
    /// still pass. Matching on "does not match" is not enough either, since
    /// `codexHome` bails with that same phrase. The assertion below pins the
    /// exact drift message, which admits neither degradation.
    #[test]
    fn rejects_initialize_response_user_agent_drift() {
        let package_version = env!("CARGO_PKG_VERSION");
        let rejected = [
            // The originator is the client name we send, never `codex-cli`.
            format!(
                "codex-cli/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version})"
            ),
            format!(
                "kanban-codex-app-server-adaptor/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version})"
            ),
            // The Codex version stays exactly pinned.
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION}.1 (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version})"
            ),
            format!(
                "{CLIENT_NAME}/0.150.2 (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version})"
            ),
            format!(
                "{CLIENT_NAME}/0.150 (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version})"
            ),
            // A longer version that starts with the pinned one must not be
            // swallowed by the prefix; only the trailing space rejects this.
            format!(
                "{CLIENT_NAME}/0.150.10 (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version})"
            ),
            // The version must be delimited by a space, not run into the rest.
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION}(Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version})"
            ),
            // The trailing client group is missing.
            format!("{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown"),
            // The trailing client group is altered.
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown [{CLIENT_NAME}; {package_version}]"
            ),
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}: {package_version})"
            ),
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version}"
            ),
            // The trailing client group names someone else.
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown (codex-cli; {package_version})"
            ),
            // The trailing client group carries the wrong package version.
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version}.1)"
            ),
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {TEST_CODEX_VERSION})"
            ),
            // The trailing client group is not at the end.
            format!(
                "{CLIENT_NAME}/{TEST_CODEX_VERSION} (Ubuntu 24.4.0; x86_64) unknown ({CLIENT_NAME}; {package_version}) "
            ),
        ];
        for user_agent in rejected {
            let mut state = new_state();
            let error = state
                .feed(&initialize_response(
                    CODEX_HOME,
                    &user_agent,
                    "unix",
                    "linux",
                ))
                .unwrap_err()
                .to_string();
            assert_eq!(
                error, "initialize response userAgent does not match",
                "{user_agent:?} was rejected for the wrong reason"
            );
        }
    }

    /// An empty `userAgent` is a genuine shape case but NOT a drift case: it
    /// is rejected one step earlier, by `nonempty_string_field`, and never
    /// reaches `validate_user_agent`. Kept here rather than in the drift loop,
    /// and asserted on its own distinct message, so it can neither stand in
    /// for a drift case nor be satisfied by the drift rejection.
    #[test]
    fn rejects_an_empty_initialize_response_user_agent() {
        let mut state = new_state();
        let error = state
            .feed(&initialize_response(CODEX_HOME, "", "unix", "linux"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "initialize response field userAgent must be nonempty"
        );
    }

    #[test]
    fn rejects_initialize_response_shape_mismatches() {
        for response in [
            line(json!({
                "id": 1,
                "result": {
                    "platformFamily": "unix",
                    "platformOs": "linux",
                    "userAgent": user_agent()
                }
            })),
            line(json!({
                "id": 1,
                "result": {
                    "codexHome": 7,
                    "platformFamily": "unix",
                    "platformOs": "linux",
                    "userAgent": user_agent()
                }
            })),
            line(json!({
                "id": 1,
                "result": {
                    "codexHome": "relative/codex-home",
                    "platformFamily": "unix",
                    "platformOs": "linux",
                    "userAgent": user_agent()
                }
            })),
            line(json!({
                "id": 1,
                "result": {
                    "codexHome": "/private/tmp/kanban-codex-home-mismatch",
                    "platformFamily": "unix",
                    "platformOs": "linux",
                    "userAgent": user_agent()
                }
            })),
            line(json!({
                "id": 1,
                "result": {
                    "codexHome": CODEX_HOME,
                    "platformFamily": "",
                    "platformOs": "linux",
                    "userAgent": user_agent()
                }
            })),
            line(json!({
                "id": 1,
                "result": {
                    "codexHome": CODEX_HOME,
                    "platformFamily": "unix",
                    "platformOs": "",
                    "userAgent": user_agent()
                }
            })),
        ] {
            let mut state = new_state();
            assert!(state.feed(&response).is_err());
        }
    }

    #[test]
    fn rejects_model_provider_and_source_drift_at_response_and_notification() {
        for pointer in [
            "/result/modelProvider",
            "/result/thread/modelProvider",
            "/result/thread/source",
        ] {
            let mut state = new_state();
            assert!(matches!(
                state.feed(&init_response()).unwrap(),
                Transition::SendThreadStart
            ));
            let mut response: Value =
                serde_json::from_slice(&thread_start_response(CWD, THREAD_ID)).unwrap();
            *response.pointer_mut(pointer).unwrap() = json!("drifted");
            assert!(state.feed(&line(response)).is_err(), "{pointer}");
        }

        for pointer in ["/params/thread/modelProvider", "/params/thread/source"] {
            let mut state = new_state();
            assert!(matches!(
                state.feed(&init_response()).unwrap(),
                Transition::SendThreadStart
            ));
            assert!(matches!(
                state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
                Transition::SendTurnStart { .. }
            ));
            let mut notification: Value =
                serde_json::from_slice(&thread_started(THREAD_ID, CWD)).unwrap();
            *notification.pointer_mut(pointer).unwrap() = json!("drifted");
            assert!(state.feed(&line(notification)).is_err(), "{pointer}");
        }
    }

    #[test]
    fn rejects_thread_version_drift_and_nonempty_start_items() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(
            state
                .feed(
                    &json!({
                        "id": 2,
                        "result": {
                            "approvalPolicy": "never",
                            "approvalsReviewer": "user",
                            "cwd": CWD,
                            "model": "codex-1",
                            "modelProvider": "openai",
                            "sandbox": {
                                "type": "readOnly",
                                "networkAccess": false
                            },
                            "thread": {
                                "cliVersion": format!("{TEST_CODEX_VERSION}.1"),
                                "id": THREAD_ID,
                                "cwd": CWD,
                                "ephemeral": true,
                                "modelProvider": "openai",
                                "preview": "",
                                "sessionId": "session-1",
                                "createdAt": 1,
                                "updatedAt": 2,
                                "projectId": null,
                                "source": "vscode",
                                "status": {"type": "idle"},
                                "turns": []
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );

        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(
            state
                .feed(
                    &json!({
                        "id": 3,
                        "result": {
                            "turn": {
                                "id": TURN_ID,
                                "status": "inProgress",
                                "items": [json!({
                                    "id": "u-1",
                                    "type": "userMessage",
                                    "content": [{"type": "text", "text": "hi"}]
                                })]
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );

        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(matches!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .unwrap(),
            Transition::Continue
        ));
        assert!(
            state
                .feed(
                    &json!({
                        "method": "turn/started",
                        "params": {
                            "threadId": THREAD_ID,
                            "turn": {
                                "id": TURN_ID,
                                "status": "inProgress",
                                "items": [json!({
                                    "id": "u-1",
                                    "type": "userMessage",
                                    "content": [{"type": "text", "text": "hi"}]
                                })]
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_pre_stream_notifications() {
        let mut state = new_state();
        assert!(state.feed(&thread_started(THREAD_ID, CWD)).is_err());
        assert!(state.feed(&turn_started(THREAD_ID, TURN_ID)).is_err());

        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(state.feed(&thread_started(THREAD_ID, CWD)).is_err());
        assert!(state.feed(&turn_started(THREAD_ID, TURN_ID)).is_err());

        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(matches!(
            state.feed(&thread_started(THREAD_ID, CWD)).unwrap(),
            Transition::Continue
        ));
        assert!(state.feed(&thread_started("bad-thread", CWD)).is_err());
        assert!(state.feed(&thread_started(THREAD_ID, "/wrong")).is_err());
        assert!(state.feed(&turn_started(THREAD_ID, TURN_ID)).is_err());
        assert!(
            state
                .feed(&item_started(THREAD_ID, TURN_ID, agent_item("a-1", "ok")))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, agent_item("a-1", "ok")))
                .is_err()
        );
        assert!(state.feed(&delta(THREAD_ID, TURN_ID, "a-1")).is_err());
        assert!(state.feed(&token_usage(THREAD_ID, TURN_ID)).is_err());
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    TURN_ID,
                    "completed",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
    }

    #[test]
    fn rejects_missing_thread_and_turn_started_notifications() {
        let mut state = pre_lifecycle_state();
        assert!(
            state
                .feed(&item_started(THREAD_ID, TURN_ID, user_item("u-1")))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(THREAD_ID, TURN_ID, user_item("u-1")))
                .is_err()
        );
        assert!(state.feed(&delta(THREAD_ID, TURN_ID, "a-1")).is_err());
        assert!(state.feed(&token_usage(THREAD_ID, TURN_ID)).is_err());
        assert!(
            state
                .feed(&turn_completed(
                    THREAD_ID,
                    TURN_ID,
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );

        let mut state = pre_lifecycle_state();
        assert!(state.feed(&turn_started(THREAD_ID, TURN_ID)).is_err());
        assert!(matches!(
            state.feed(&thread_started(THREAD_ID, CWD)).unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state.feed(&turn_started(THREAD_ID, TURN_ID)).unwrap(),
            Transition::Continue
        ));
    }

    #[test]
    fn rejects_reversed_and_duplicate_lifecycle_notifications() {
        let mut state = pre_lifecycle_state();
        assert!(state.feed(&turn_started(THREAD_ID, TURN_ID)).is_err());
        assert!(matches!(
            state.feed(&thread_started(THREAD_ID, CWD)).unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state.feed(&turn_started(THREAD_ID, TURN_ID)).unwrap(),
            Transition::Continue
        ));

        let mut state = streaming_state();
        assert!(state.feed(&thread_started(THREAD_ID, CWD)).is_err());

        let mut state = streaming_state();
        assert!(state.feed(&turn_started(THREAD_ID, TURN_ID)).is_err());
    }

    #[test]
    fn rejects_wrong_notification_ids_for_each_family() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(matches!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .unwrap(),
            Transition::Continue
        ));

        assert!(state.feed(&thread_started("bad-thread", CWD)).is_err());
        assert!(state.feed(&thread_started(THREAD_ID, "/wrong")).is_err());
        assert!(state.feed(&turn_started("bad-thread", TURN_ID)).is_err());
        assert!(state.feed(&turn_started(THREAD_ID, "bad-turn")).is_err());
        assert!(
            state
                .feed(&item_started(
                    "bad-thread",
                    TURN_ID,
                    agent_item("a-1", "ok")
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    "bad-turn",
                    agent_item("a-1", "ok")
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(
                    "bad-thread",
                    TURN_ID,
                    agent_item("a-1", "ok")
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    "bad-turn",
                    agent_item("a-1", "ok")
                ))
                .is_err()
        );
        assert!(state.feed(&delta("bad-thread", TURN_ID, "a-1")).is_err());
        assert!(state.feed(&delta(THREAD_ID, "bad-turn", "a-1")).is_err());
        assert!(state.feed(&token_usage("bad-thread", TURN_ID)).is_err());
        assert!(state.feed(&token_usage(THREAD_ID, "bad-turn")).is_err());
        assert!(
            state
                .feed(&turn_completed_with_status(
                    "bad-thread",
                    TURN_ID,
                    "completed",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    "bad-turn",
                    "completed",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
    }

    #[test]
    fn rejects_wrong_ids_and_server_requests() {
        let mut state = new_state();
        assert!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .is_err()
        );
        assert!(
            state
                .feed(&json!({"id": 1, "error": {}}).to_string().into_bytes())
                .is_err()
        );

        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(
            state
                .feed(&json!({"id": 1, "result": {}}).to_string().into_bytes())
                .is_err()
        );
        assert!(
            state
                .feed(
                    &json!({"id": 2, "method": "thread/started", "params": {}})
                        .to_string()
                        .into_bytes()
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_identity_policy_and_uuid_drift() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(
            state
                .feed(
                    &json!({
                        "id": 2,
                        "result": {
                            "approvalPolicy": "on-request",
                            "cwd": CWD,
                            "sandbox": {
                                "type": "readOnly",
                                "networkAccess": false
                            },
                            "thread": {
                                "id": THREAD_ID,
                                "cwd": CWD,
                                "ephemeral": true
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
        assert!(
            state
                .feed(
                    &json!({
                        "id": 2,
                        "result": {
                            "approvalPolicy": "never",
                            "cwd": CWD,
                            "sandbox": {
                                "type": "writeThrough",
                                "networkAccess": false
                            },
                            "thread": {
                                "id": THREAD_ID,
                                "cwd": CWD,
                                "ephemeral": true
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
        assert!(
            state
                .feed(
                    &json!({
                        "id": 2,
                        "result": {
                            "approvalPolicy": "never",
                            "cwd": CWD,
                            "sandbox": {
                                "type": "readOnly",
                                "networkAccess": true
                            },
                            "thread": {
                                "id": THREAD_ID,
                                "cwd": CWD,
                                "ephemeral": true
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
        assert!(
            state
                .feed(&thread_start_response("/wrong", THREAD_ID))
                .is_err()
        );
        assert!(
            state
                .feed(&thread_start_response(CWD, "bad-thread-id"))
                .is_err()
        );
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { ref thread_id } if thread_id == THREAD_ID
        ));

        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(
            state
                .feed(&turn_start_response(THREAD_ID, THREAD_ID, "inProgress"))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "completed"))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_started_with_status(THREAD_ID, TURN_ID, "completed"))
                .is_err()
        );
        assert!(
            state
                .feed(
                    &json!({
                        "id": 3,
                        "result": {
                            "turn": {
                                "id": THREAD_ID,
                                "status": "inProgress",
                                "items": []
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_thread_structure_drift() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(
            state
                .feed(
                    &json!({
                        "id": 2,
                        "result": {
                            "approvalPolicy": "never",
                            "approvalsReviewer": "user",
                            "cwd": CWD,
                            "model": "codex-1",
                            "modelProvider": "openai",
                            "sandbox": {
                                "type": "readOnly",
                                "networkAccess": false
                            },
                            "thread": {
                                "cliVersion": TEST_CODEX_VERSION,
                                "id": THREAD_ID,
                                "cwd": CWD,
                                "ephemeral": true,
                                "modelProvider": "openai",
                                "preview": "",
                                "sessionId": "session-1",
                                "createdAt": 1,
                                "updatedAt": 2,
                                "projectId": null,
                                "source": "vscode",
                                "status": {"type": "active"},
                                "turns": []
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
        assert!(
            state
                .feed(
                    &json!({
                        "id": 2,
                        "result": {
                            "approvalPolicy": "never",
                            "approvalsReviewer": "user",
                            "cwd": CWD,
                            "model": "codex-1",
                            "modelProvider": "openai",
                            "sandbox": {
                                "type": "readOnly",
                                "networkAccess": false
                            },
                            "thread": {
                                "cliVersion": TEST_CODEX_VERSION,
                                "id": THREAD_ID,
                                "cwd": CWD,
                                "ephemeral": true,
                                "modelProvider": "openai",
                                "preview": "",
                                "sessionId": "session-1",
                                "createdAt": 1,
                                "updatedAt": 2,
                                "projectId": null,
                                "source": 7,
                                "status": {"type": "idle"},
                                "turns": []
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
        assert!(
            state
                .feed(
                    &json!({
                        "id": 2,
                        "result": {
                            "approvalPolicy": "never",
                            "approvalsReviewer": "user",
                            "cwd": CWD,
                            "model": "codex-1",
                            "modelProvider": "openai",
                            "sandbox": {
                                "type": "readOnly",
                                "networkAccess": false
                            },
                            "thread": {
                                "id": THREAD_ID,
                                "cwd": CWD,
                                "ephemeral": true,
                                "modelProvider": "openai",
                                "preview": "",
                                "sessionId": "session-1",
                                "createdAt": 1,
                                "updatedAt": 2,
                                "projectId": null,
                                "source": "vscode",
                                "status": {"type": "idle"},
                                "turns": []
                            }
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_token_usage_shape_drift() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(matches!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .unwrap(),
            Transition::Continue
        ));
        assert!(
            state
                .feed(
                    &json!({
                        "method": "thread/tokenUsage/updated",
                        "params": {
                            "threadId": THREAD_ID,
                            "turnId": TURN_ID,
                            "tokenUsage": {"last": {"input": 1, "output": 2}}
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
        assert!(
            state
                .feed(
                    &json!({
                        "method": "thread/tokenUsage/updated",
                        "params": {
                            "threadId": THREAD_ID,
                            "turnId": TURN_ID,
                            "tokenUsage": {"last": 7, "total": {"input": 1, "output": 2}}
                        }
                    })
                    .to_string()
                    .into_bytes()
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_item_types_ack_and_completion_rules() {
        let mut state = streaming_state();
        assert!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    TURN_ID,
                    json!({"id": "x", "type": "commandExecution"})
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    TURN_ID,
                    json!({"id": "x", "type": "userMessage"})
                ))
                .is_err()
        );
        for ty in [
            "fileChange",
            "mcpToolCall",
            "dynamicToolCall",
            "webSearch",
            "imageGeneration",
        ] {
            assert!(
                state
                    .feed(&item_started(THREAD_ID, TURN_ID, item_with_type("x", ty)))
                    .is_err()
            );
        }
        assert!(matches!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", "wrong")
                ))
                .unwrap(),
            Transition::Continue
        ));
        assert!(
            state
                .feed(&item_started(THREAD_ID, TURN_ID, agent_item("", "wrong")))
                .is_err()
        );
        assert!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("bad\nid", "wrong")
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", "wrong")
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &malformed_ack_text())
                ))
                .is_err()
        );
    }

    #[test]
    fn rejects_malformed_nonutf8_empty_oversize_trailing_json_and_post_complete() {
        let mut state = new_state();
        assert!(state.feed(&[]).is_err());
        assert!(state.feed(&[0xff]).is_err());
        let oversized = vec![b' '; MAX_LINE_BYTES + 1];
        assert!(state.feed(&oversized).is_err());
        assert!(state.feed(b"{\"id\":1}{\"id\":2}").is_err());
        assert!(state.feed(b"not-json").is_err());

        let mut state = streaming_state();
        assert!(matches!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &ack_text())
                ))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &ack_text())
                ))
                .unwrap(),
            Transition::Continue
        ));
        assert!(matches!(
            state
                .feed(&turn_completed(
                    THREAD_ID,
                    TURN_ID,
                    vec![agent_item("a-1", &ack_text())],
                ))
                .unwrap(),
            Transition::Completed
        ));
        assert!(
            state
                .feed(&turn_completed(
                    THREAD_ID,
                    TURN_ID,
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
        let mut state = streaming_state();
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    TURN_ID,
                    "completed",
                    json!(null),
                    vec![]
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    TURN_ID,
                    "completed",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    TURN_ID,
                    "inProgress",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    TURN_ID,
                    "completed",
                    json!({"message": "nope"}),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
        assert!(matches!(
            state
                .feed(&item_started(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &ack_text())
                ))
                .unwrap(),
            Transition::Continue
        ));
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", "false")
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item(
                        "a-1",
                        "{\"accepted\":false,\"idempotencyKey\":\"ack-key-1\"}"
                    )
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", "{\"accepted\":true,\"idempotencyKey\":\"wrong\"}")
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item(
                        "a-1",
                        &format!(
                            "{{\"accepted\":true,\"idempotencyKey\":\"{}\",\"extra\":1}}",
                            ID_KEY
                        )
                    )
                ))
                .is_err()
        );
        assert!(matches!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &ack_text())
                ))
                .unwrap(),
            Transition::Continue
        ));
        assert!(
            state
                .feed(&item_completed(
                    THREAD_ID,
                    TURN_ID,
                    agent_item("a-1", &ack_text())
                ))
                .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_and_out_of_order_responses() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(state.feed(&init_response()).is_err());

        let mut state = new_state();
        assert!(state.feed(&thread_start_response(CWD, THREAD_ID)).is_err());
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(state.feed(&thread_start_response(CWD, THREAD_ID)).is_err());
        assert!(matches!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .unwrap(),
            Transition::Continue
        ));
        assert!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .is_err()
        );
    }

    #[test]
    fn rejects_status_and_id_drift_on_completion_path() {
        let mut state = new_state();
        assert!(matches!(
            state.feed(&init_response()).unwrap(),
            Transition::SendThreadStart
        ));
        assert!(matches!(
            state.feed(&thread_start_response(CWD, THREAD_ID)).unwrap(),
            Transition::SendTurnStart { .. }
        ));
        assert!(matches!(
            state
                .feed(&turn_start_response(THREAD_ID, TURN_ID, "inProgress"))
                .unwrap(),
            Transition::Continue
        ));
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    TURN_ID,
                    "failed",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    TURN_ID,
                    "interrupted",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
        assert!(
            state
                .feed(&turn_completed_with_status(
                    THREAD_ID,
                    THREAD_ID,
                    "completed",
                    json!(null),
                    vec![agent_item("a-1", &ack_text())]
                ))
                .is_err()
        );
    }
}
