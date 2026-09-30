use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

const MAX_RENDER_BYTES: usize = 64 * 1024;
const INITIALIZE_ID: u64 = 1;
const THREAD_START_ID: u64 = 2;
const TURN_START_ID: u64 = 3;
pub(crate) const CLIENT_NAME: &str = "kanban-codex-app-server-adapter";
const BASE_INSTRUCTIONS: &str = "Return only the JSON acknowledgement.";
const DEVELOPER_INSTRUCTIONS: &str =
    "Do not use tools, files, network, or commands. Return only the JSON acknowledgement.";
const AT_LEAST_ONCE_INSTRUCTION: &str = "At-least-once delivery; deduplicate by idempotency key.";

/// The exact `ServerNotification` method names this adapter opts out of at
/// `initialize`. The server accepts this array free-form, so a misspelling is
/// not rejected at the handshake: the notification simply keeps arriving and
/// the fail-closed arm in `codex_app_server_state::feed_notification` aborts
/// the first real handshake with `unsupported notification method ...`.
///
/// `configWarning` is the notification the server emits when the adapter's cwd
/// is an untrusted project directory. It is sent after `initialize` is
/// processed, which is why opting out suppresses it at all. Verified by
/// negative control against installed codex-cli 0.150.1 on 2026-09-01, 3 runs
/// each: spelled `configWarnings` the notification still arrives unsuppressed;
/// spelled `configWarning` it is absent.
///
/// Nothing here is vendored. Every name is checked at adapter startup against
/// the `ServerNotification` variants of the protocol schema the installed
/// codex generates, by
/// `codex_app_server_adapter::verify_opt_out_methods_are_declared`.
pub(crate) const OPT_OUT_NOTIFICATION_METHODS: [&str; 8] = [
    "configWarning",
    "remoteControl/status/changed",
    "mcpServer/startupStatus/updated",
    "thread/status/changed",
    "account/rateLimits/updated",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
];

#[derive(Serialize)]
struct RequestLine<P> {
    id: u64,
    method: &'static str,
    params: P,
}

#[derive(Serialize)]
struct NotificationLine<P> {
    method: &'static str,
    params: P,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    client_info: ClientInfo,
    capabilities: InitializeCapabilities,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeCapabilities {
    experimental_api: bool,
    opt_out_notification_methods: [&'static str; 8],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadStartParams {
    cwd: String,
    approval_policy: &'static str,
    sandbox: &'static str,
    ephemeral: bool,
    base_instructions: &'static str,
    developer_instructions: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartParams {
    thread_id: String,
    approval_policy: &'static str,
    sandbox_policy: SandboxPolicy,
    input: Vec<InputItem>,
    output_schema: AcknowledgementSchema,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SandboxPolicy {
    #[serde(rename = "type")]
    kind: &'static str,
    network_access: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InputItem {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartPrompt {
    instruction: &'static str,
    idempotency_key: String,
    event: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcknowledgementSchema {
    #[serde(rename = "type")]
    kind: &'static str,
    additional_properties: bool,
    properties: AcknowledgementProperties,
    required: [&'static str; 2],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcknowledgementProperties {
    accepted: AcceptedProperty,
    idempotency_key: IdempotencyKeyProperty,
}

#[derive(Serialize)]
struct AcceptedProperty {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "const")]
    value: bool,
}

#[derive(Serialize)]
struct IdempotencyKeyProperty {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "const")]
    value: String,
}

#[derive(Serialize)]
struct EmptyParams {}

fn contains_private_key(value: &Value) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| crate::watch::secret_key(key) || contains_private_key(value)),
        Value::Array(values) => values.iter().any(contains_private_key),
        _ => false,
    }
}

fn reject_private_keys(value: &Value) -> Result<()> {
    if contains_private_key(value) {
        bail!("event contains a private key");
    }
    Ok(())
}

fn validate_cwd(cwd: &str) -> Result<()> {
    if cwd.is_empty() {
        bail!("cwd must be a nonempty absolute path");
    }
    if !std::path::Path::new(cwd).is_absolute() {
        bail!("cwd must be a nonempty absolute path");
    }
    if cwd.chars().any(|ch| ch.is_control()) {
        bail!("cwd must not contain control characters");
    }
    if cwd.len() > MAX_RENDER_BYTES {
        bail!("cwd exceeds {MAX_RENDER_BYTES} bytes");
    }
    Ok(())
}

fn validate_id(id: &str, label: &str) -> Result<()> {
    if id.is_empty() {
        bail!("{label} must be nonempty");
    }
    if id.chars().any(|ch| ch.is_control()) {
        bail!("{label} must not contain control characters");
    }
    if id.len() > MAX_RENDER_BYTES {
        bail!("{label} exceeds {MAX_RENDER_BYTES} bytes");
    }
    Ok(())
}

fn render_line<T: Serialize>(value: &T) -> Result<String> {
    let rendered = serde_json::to_string(value)?;
    if rendered.len() + 1 > MAX_RENDER_BYTES {
        bail!("rendered line exceeds {MAX_RENDER_BYTES} bytes");
    }
    Ok(format!("{rendered}\n"))
}

fn render_json_text<T: Serialize>(value: &T, label: &str) -> Result<String> {
    let rendered = serde_json::to_string(value)?;
    if rendered.len() > MAX_RENDER_BYTES {
        bail!("{label} exceeds {MAX_RENDER_BYTES} bytes");
    }
    Ok(rendered)
}

fn acknowledgement_schema(idempotency_key: &str) -> AcknowledgementSchema {
    AcknowledgementSchema {
        kind: "object",
        additional_properties: false,
        properties: AcknowledgementProperties {
            accepted: AcceptedProperty {
                kind: "boolean",
                value: true,
            },
            idempotency_key: IdempotencyKeyProperty {
                kind: "string",
                value: idempotency_key.to_owned(),
            },
        },
        required: ["accepted", "idempotencyKey"],
    }
}

pub(crate) fn initialize_line() -> Result<String> {
    render_line(&RequestLine {
        id: INITIALIZE_ID,
        method: "initialize",
        params: InitializeParams {
            client_info: ClientInfo {
                name: CLIENT_NAME,
                version: env!("CARGO_PKG_VERSION"),
            },
            capabilities: InitializeCapabilities {
                experimental_api: false,
                opt_out_notification_methods: OPT_OUT_NOTIFICATION_METHODS,
            },
        },
    })
}

pub(crate) fn initialized_line() -> Result<String> {
    render_line(&NotificationLine {
        method: "initialized",
        params: EmptyParams {},
    })
}

pub(crate) fn thread_start_line(cwd: &str) -> Result<String> {
    validate_cwd(cwd)?;
    render_line(&RequestLine {
        id: THREAD_START_ID,
        method: "thread/start",
        params: ThreadStartParams {
            cwd: cwd.to_owned(),
            approval_policy: "never",
            sandbox: "read-only",
            ephemeral: true,
            base_instructions: BASE_INSTRUCTIONS,
            developer_instructions: DEVELOPER_INSTRUCTIONS,
        },
    })
}

pub(crate) fn turn_start_line(
    thread_id: &str,
    idempotency_key: &str,
    event: &Value,
) -> Result<String> {
    validate_id(thread_id, "thread id")?;
    validate_id(idempotency_key, "idempotency key")?;
    reject_private_keys(event)?;

    let prompt = TurnStartPrompt {
        instruction: AT_LEAST_ONCE_INSTRUCTION,
        idempotency_key: idempotency_key.to_owned(),
        event: event.clone(),
    };
    let input = render_json_text(&prompt, "turn/start input")?;
    let schema = acknowledgement_schema(idempotency_key);
    let schema_rendered = serde_json::to_string(&schema)?;
    if schema_rendered.len() > MAX_RENDER_BYTES {
        bail!("turn/start output schema exceeds {MAX_RENDER_BYTES} bytes");
    }

    render_line(&RequestLine {
        id: TURN_START_ID,
        method: "turn/start",
        params: TurnStartParams {
            thread_id: thread_id.to_owned(),
            approval_policy: "never",
            sandbox_policy: SandboxPolicy {
                kind: "readOnly",
                network_access: false,
            },
            input: vec![InputItem {
                kind: "text",
                text: input,
            }],
            output_schema: schema,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_line(line: &str) -> Value {
        assert!(line.ends_with('\n'));
        serde_json::from_str(&line[..line.len() - 1]).unwrap()
    }

    #[test]
    fn initialize_line_has_the_exact_shape() {
        assert_eq!(
            parse_line(&initialize_line().unwrap()),
            json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "kanban-codex-app-server-adapter",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "experimentalApi": false,
                        "optOutNotificationMethods": [
                            "configWarning",
                            "remoteControl/status/changed",
                            "mcpServer/startupStatus/updated",
                            "thread/status/changed",
                            "account/rateLimits/updated",
                            "item/reasoning/summaryTextDelta",
                            "item/reasoning/summaryPartAdded",
                            "item/reasoning/textDelta",
                        ]
                    }
                }
            })
        );
    }

    #[test]
    fn initialized_line_has_the_exact_shape() {
        assert_eq!(
            parse_line(&initialized_line().unwrap()),
            json!({
                "method": "initialized",
                "params": {}
            })
        );
    }

    #[test]
    fn thread_start_line_has_the_exact_shape() {
        assert_eq!(
            parse_line(&thread_start_line("/private/tmp/kanban").unwrap()),
            json!({
                "id": 2,
                "method": "thread/start",
                "params": {
                    "cwd": "/private/tmp/kanban",
                    "approvalPolicy": "never",
                    "sandbox": "read-only",
                    "ephemeral": true,
                    "baseInstructions": BASE_INSTRUCTIONS,
                    "developerInstructions": DEVELOPER_INSTRUCTIONS,
                }
            })
        );
    }

    #[test]
    fn turn_start_line_has_the_exact_shape() {
        let event = json!({
            "eventID": "a".repeat(64),
            "eventHash": "b".repeat(64),
            "timestamp": 123,
        });
        let expected_input = "{\"instruction\":\"At-least-once delivery; deduplicate by idempotency key.\",\"idempotencyKey\":\"key-1\",\"event\":{\"eventHash\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"eventID\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"timestamp\":123}}";
        assert_eq!(
            parse_line(&turn_start_line("thread-1", "key-1", &event).unwrap()),
            json!({
                "id": 3,
                "method": "turn/start",
                "params": {
                    "threadId": "thread-1",
                    "approvalPolicy": "never",
                    "sandboxPolicy": {
                        "type": "readOnly",
                        "networkAccess": false,
                    },
                    "input": [{
                        "type": "text",
                        "text": expected_input,
                    }],
                    "outputSchema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "accepted": {
                                "type": "boolean",
                                "const": true,
                            },
                            "idempotencyKey": {
                                "type": "string",
                                "const": "key-1",
                            },
                        },
                        "required": ["accepted", "idempotencyKey"],
                    }
                }
            })
        );
    }

    #[test]
    fn renderers_reject_invalid_cwd_thread_id_and_key() {
        assert!(thread_start_line("").is_err());
        assert!(thread_start_line("relative/path").is_err());
        assert!(thread_start_line(&format!("/tmp/{}", "a".repeat(MAX_RENDER_BYTES))).is_err());
        assert!(thread_start_line("/tmp/kanban\nbad").is_err());
        assert!(turn_start_line("", "key-1", &json!({})).is_err());
        assert!(turn_start_line("thread-1", "", &json!({})).is_err());
        assert!(turn_start_line(&"t".repeat(MAX_RENDER_BYTES + 1), "key-1", &json!({})).is_err());
        assert!(turn_start_line("thread\n1", "key-1", &json!({})).is_err());
        assert!(turn_start_line("thread-1", "key\u{7f}1", &json!({})).is_err());
        assert!(turn_start_line("thread-1", "key\n1", &json!({})).is_err());
    }

    #[test]
    fn turn_start_line_rejects_private_key_shaped_event_content() {
        let event = json!({
            "nested": {
                "refreshTokenValue": "secret",
            }
        });
        let err = turn_start_line("thread-1", "key-1", &event)
            .unwrap_err()
            .to_string();
        assert!(err.contains("private key"));
    }

    #[test]
    fn renderers_enforce_bounds_and_single_newline_framing() {
        let line = initialize_line().unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
        assert!(!line[..line.len() - 1].contains('\n'));
        assert!(!line[..line.len() - 1].contains('\r'));

        let large_event = json!({
            "blob": "x".repeat(MAX_RENDER_BYTES),
        });
        assert!(turn_start_line("thread-1", "key-1", &large_event).is_err());

        let large_key = "k".repeat(MAX_RENDER_BYTES + 1);
        assert!(turn_start_line("thread-1", &large_key, &json!({})).is_err());
    }
}
