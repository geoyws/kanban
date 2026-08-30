use crate::WATCH_BATCH_LIMIT;
use crate::model::Event;
use crate::registry::{Registry, data_root};
use crate::store::Store;
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

const PROTOCOL_VERSION: u8 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
struct StreamKey {
    source_kind: String,
    source: String,
    selector_kind: String,
    selector_value: Option<String>,
    kind: Option<String>,
    archived: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeEnvelope {
    source_kind: String,
    source: String,
    board_name: Option<String>,
    selector_kind: String,
    selector_value: Option<String>,
    kind: Option<String>,
    archived: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct CursorToken {
    version: u8,
    source_kind: String,
    source: String,
    selector_kind: String,
    selector_value: Option<String>,
    kind: Option<String>,
    archived: bool,
    seq: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchEnvelope {
    version: u8,
    scope: ScopeEnvelope,
    cursor: String,
    #[serde(rename = "type")]
    kind: &'static str,
    payload: Value,
}

#[derive(Clone, Debug)]
enum Source {
    Board {
        path: PathBuf,
        board_name: Option<String>,
    },
    Registry,
}

#[derive(Debug)]
struct WatchSpec {
    source: Source,
    key: StreamKey,
    cursor: i64,
    limit: i64,
    follow: bool,
}

pub(crate) fn run(args: &super::Args) -> Result<()> {
    let spec = resolve(args)?;
    watch(spec)
}

fn resolve(args: &super::Args) -> Result<WatchSpec> {
    resolve_with_source(args, super::direct_db(args))
}

fn resolve_with_source(args: &super::Args, direct_db: Option<PathBuf>) -> Result<WatchSpec> {
    let task = args.one("task").map(str::to_owned);
    let rule = args.one("rule").map(str::to_owned);
    let registry = args.has("registry");
    let selector_count = task.is_some() as u8 + rule.is_some() as u8 + registry as u8;
    if selector_count > 1 {
        bail!("--task, --rule and --registry address different event trails; pass one");
    }

    let kind = args.one("kind").map(str::to_owned);
    let follow = args.has("follow");
    let limit = args.limit(50)?;
    if limit > WATCH_BATCH_LIMIT {
        bail!("--limit must be between 0 and {WATCH_BATCH_LIMIT}, got {limit}");
    }
    if follow && limit == 0 {
        bail!("--follow requires --limit to be at least 1");
    }

    if registry || rule.is_some() {
        if args.has("all") {
            bail!("--all applies to board events; registry events do not carry archived history");
        }
        if args.has("project") || args.has("workspace") || args.has("db") {
            bail!(
                "--project, --workspace and --db address boards; registry watch uses the registry trail"
            );
        }
        let registry_path = registry_source()?;
        let selector_kind = if registry {
            "registry".to_owned()
        } else {
            "rule".to_owned()
        };
        let selector_value = rule.clone();
        let cursor = parse_cursor(
            args.one("cursor"),
            &StreamKey {
                source_kind: "registry".to_owned(),
                source: registry_path.clone(),
                selector_kind: selector_kind.clone(),
                selector_value: selector_value.clone(),
                kind: kind.clone(),
                archived: false,
            },
        )?;
        let source = Source::Registry;
        ensure_cursor_within_head(&source, cursor)?;
        return Ok(WatchSpec {
            source,
            key: StreamKey {
                source_kind: "registry".to_owned(),
                source: registry_path,
                selector_kind,
                selector_value,
                kind,
                archived: false,
            },
            cursor,
            limit,
            follow,
        });
    }

    let (board_path, board_name) = if let Some(path) = direct_db {
        (path, None)
    } else {
        let path = super::store_path_readonly(args)?;
        let board_name = board_name_for_path(&path)?;
        (path, board_name)
    };
    let board_source = canonical_source_path(&board_path)?;
    let archived = args.has("all");
    let selector_kind = if task.is_some() {
        "task".to_owned()
    } else {
        "board".to_owned()
    };
    let selector_value = task.clone();
    let cursor = parse_cursor(
        args.one("cursor"),
        &StreamKey {
            source_kind: "board".to_owned(),
            source: board_source.clone(),
            selector_kind: selector_kind.clone(),
            selector_value: selector_value.clone(),
            kind: kind.clone(),
            archived,
        },
    )?;
    let source = Source::Board {
        path: board_path,
        board_name,
    };
    ensure_cursor_within_head(&source, cursor)?;
    Ok(WatchSpec {
        source,
        key: StreamKey {
            source_kind: "board".to_owned(),
            source: board_source,
            selector_kind,
            selector_value,
            kind,
            archived,
        },
        cursor,
        limit,
        follow,
    })
}

fn watch(spec: WatchSpec) -> Result<()> {
    let mut cursor = spec.cursor;
    let mut cursor_token = encode_cursor(&spec.key, cursor)?;
    loop {
        let batch = match &spec.source {
            Source::Board { path, .. } => {
                let store = Store::open_readonly(path)?;
                store.events_since(
                    spec.key.selector_value.as_deref(),
                    spec.key.kind.as_deref(),
                    cursor,
                    spec.limit,
                    spec.key.archived,
                )?
            }
            Source::Registry => {
                let registry = Registry::open_readonly()?;
                registry.rule_events_since(
                    spec.key.selector_value.as_deref(),
                    spec.key.kind.as_deref(),
                    cursor,
                    spec.limit,
                )?
            }
        };
        if batch.is_empty() {
            if !spec.follow {
                return Ok(());
            }
            emit(&WatchEnvelope {
                version: PROTOCOL_VERSION,
                scope: scope_envelope(&spec.key, board_name(&spec.source)),
                cursor: cursor_token.clone(),
                kind: "heartbeat",
                payload: json!({"state":"idle"}),
            })?;
            sleep(POLL_INTERVAL);
            continue;
        }
        for event in batch {
            cursor = event.seq;
            cursor_token = encode_cursor(&spec.key, cursor)?;
            emit(&WatchEnvelope {
                version: PROTOCOL_VERSION,
                scope: scope_envelope(&spec.key, board_name(&spec.source)),
                cursor: cursor_token.clone(),
                kind: "event",
                payload: event_payload(event)?,
            })?;
        }
        if !spec.follow {
            return Ok(());
        }
    }
}

fn board_name(source: &Source) -> Option<String> {
    match source {
        Source::Board { board_name, .. } => board_name.clone(),
        Source::Registry => None,
    }
}

fn scope_envelope(key: &StreamKey, board_name: Option<String>) -> ScopeEnvelope {
    ScopeEnvelope {
        source_kind: key.source_kind.clone(),
        source: key.source.clone(),
        board_name,
        selector_kind: key.selector_kind.clone(),
        selector_value: key.selector_value.clone(),
        kind: key.kind.clone(),
        archived: key.archived,
    }
}

fn event_payload(event: Event) -> Result<Value> {
    let mut value = serde_json::to_value(event)?;
    if let Some(payload) = value.get_mut("payload") {
        *payload = redact(payload.take());
    }
    Ok(value)
}

fn redact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(key, value)| {
                    if secret_key(&key) {
                        None
                    } else {
                        Some((key, redact(value)))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact).collect()),
        other => other,
    }
}

fn secret_key(value: &str) -> bool {
    let normalized = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect::<String>();
    [
        "token",
        "tokenvalue",
        "secret",
        "secretvalue",
        "credential",
        "credentialvalue",
        "material",
        "materialvalue",
    ]
    .iter()
    .any(|suffix| normalized.ends_with(suffix))
}

fn encode_cursor(key: &StreamKey, seq: i64) -> Result<String> {
    let token = CursorToken {
        version: PROTOCOL_VERSION,
        source_kind: key.source_kind.clone(),
        source: key.source.clone(),
        selector_kind: key.selector_kind.clone(),
        selector_value: key.selector_value.clone(),
        kind: key.kind.clone(),
        archived: key.archived,
        seq,
    };
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&token)?))
}

fn parse_cursor(raw: Option<&str>, expected: &StreamKey) -> Result<i64> {
    match raw {
        None => Ok(0),
        Some("0") => Ok(0),
        Some(raw) if raw.chars().all(|ch| ch.is_ascii_digit()) => {
            bail!("--cursor must be 0 or the opaque watch token for this stream")
        }
        Some(raw) => decode_cursor(raw, expected),
    }
}

fn decode_cursor(raw: &str, expected: &StreamKey) -> Result<i64> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .with_context(|| "--cursor is not a valid watch token")?;
    let token: CursorToken =
        serde_json::from_slice(&bytes).with_context(|| "--cursor is not a valid watch token")?;
    if token.version != PROTOCOL_VERSION {
        bail!(
            "--cursor uses unsupported protocol version {}",
            token.version
        );
    }
    if token.seq < 0 {
        bail!("--cursor sequence must be zero or more, got {}", token.seq);
    }
    let actual = StreamKey {
        source_kind: token.source_kind,
        source: token.source,
        selector_kind: token.selector_kind,
        selector_value: token.selector_value,
        kind: token.kind,
        archived: token.archived,
    };
    if &actual != expected {
        bail!("--cursor belongs to a different watch stream");
    }
    Ok(token.seq)
}

fn ensure_cursor_within_head(source: &Source, cursor: i64) -> Result<()> {
    let head = match source {
        Source::Board { path, .. } => board_head(path)?,
        Source::Registry => registry_head()?,
    };
    if cursor > head {
        bail!("--cursor {cursor} is ahead of the current ledger head {head}");
    }
    Ok(())
}

fn board_head(path: &Path) -> Result<i64> {
    let store = Store::open_readonly(path)?;
    Ok(store
        .connection
        .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |row| {
            row.get(0)
        })?)
}

fn registry_head() -> Result<i64> {
    let registry = Registry::open_readonly()?;
    Ok(registry.connection.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM rule_events",
        [],
        |row| row.get(0),
    )?)
}

fn canonical_source_path(path: &Path) -> Result<String> {
    Ok(path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned())
}

fn registry_source() -> Result<String> {
    canonical_source_path(&data_root()?.join("registry.db"))
}

fn board_name_for_path(path: &Path) -> Result<Option<String>> {
    let registry = Registry::open_readonly()?;
    let source = canonical_source_path(path)?;
    for project in registry.projects()? {
        if canonical_source_path(Path::new(&project.board_path))? == source {
            return Ok(Some(project.name));
        }
    }
    Ok(None)
}

fn emit(envelope: &WatchEnvelope) -> Result<()> {
    let value = redact(serde_json::to_value(envelope)?);
    crate::emit(&serde_json::to_string(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn identity() -> StreamKey {
        StreamKey {
            source_kind: "board".to_owned(),
            source: "/tmp/kanban/board.db".to_owned(),
            selector_kind: "task".to_owned(),
            selector_value: Some("task-1".to_owned()),
            kind: Some("updated".to_owned()),
            archived: true,
        }
    }

    fn temp_watch_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kanban-watch-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp watch dir");
        path
    }

    #[test]
    fn cursor_roundtrips_and_bootstrap_zero_is_allowed() {
        let expected = identity();
        assert_eq!(parse_cursor(None, &expected).unwrap(), 0);
        assert_eq!(parse_cursor(Some("0"), &expected).unwrap(), 0);
        let token = encode_cursor(&expected, 42).unwrap();
        assert_eq!(decode_cursor(&token, &expected).unwrap(), 42);
    }

    #[test]
    fn cursor_rejects_malformed_unknown_version_identity_negative_and_other_bare_numbers() {
        let expected = identity();
        let token = encode_cursor(&expected, 42).unwrap();

        let malformed = URL_SAFE_NO_PAD.encode(b"not json");
        assert!(decode_cursor(&malformed, &expected).is_err());

        let mut unknown = serde_json::to_value(CursorToken {
            version: PROTOCOL_VERSION,
            source_kind: expected.source_kind.clone(),
            source: expected.source.clone(),
            selector_kind: expected.selector_kind.clone(),
            selector_value: expected.selector_value.clone(),
            kind: expected.kind.clone(),
            archived: expected.archived,
            seq: 42,
        })
        .unwrap();
        unknown["unexpected"] = json!(true);
        let unknown = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&unknown).unwrap());
        assert!(decode_cursor(&unknown, &expected).is_err());

        let mut version = serde_json::to_value(CursorToken {
            version: PROTOCOL_VERSION,
            source_kind: expected.source_kind.clone(),
            source: expected.source.clone(),
            selector_kind: expected.selector_kind.clone(),
            selector_value: expected.selector_value.clone(),
            kind: expected.kind.clone(),
            archived: expected.archived,
            seq: 42,
        })
        .unwrap();
        version["version"] = json!(2);
        let version = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&version).unwrap());
        assert!(decode_cursor(&version, &expected).is_err());

        let mismatch = StreamKey {
            source_kind: "registry".to_owned(),
            source: "/tmp/kanban/registry.db".to_owned(),
            selector_kind: "registry".to_owned(),
            selector_value: None,
            kind: Some("updated".to_owned()),
            archived: false,
        };
        assert!(decode_cursor(&token, &mismatch).is_err());
        let negative = serde_json::to_value(CursorToken {
            version: PROTOCOL_VERSION,
            source_kind: expected.source_kind.clone(),
            source: expected.source.clone(),
            selector_kind: expected.selector_kind.clone(),
            selector_value: expected.selector_value.clone(),
            kind: expected.kind.clone(),
            archived: expected.archived,
            seq: -1,
        })
        .unwrap();
        let negative = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&negative).unwrap());
        assert!(decode_cursor(&negative, &expected).is_err());
        assert!(parse_cursor(Some("7"), &expected).is_err());
    }

    #[test]
    fn redaction_drops_secret_keys_recursively_and_at_the_top_level() {
        let value = json!({
            "refreshTokenValue": "lease-1",
            "auth_token_secret": "lease-2",
            "credential": "cap-1",
            "token": "root",
            "tokenCount": 3,
            "tokenized": "keep-me",
            "nested": {
                "materialValue": "auth-1",
                "tokenCount": 9,
                "items": [
                    { "lease token": "inner-1", "keep": "yes" },
                    { "capabilityToken": "inner-2", "tokenized": "still-here" }
                ]
            }
        });
        let redacted = redact(value);
        assert!(redacted.get("refreshTokenValue").is_none());
        assert!(redacted.get("auth_token_secret").is_none());
        assert!(redacted.get("credential").is_none());
        assert!(redacted.get("token").is_none());
        assert_eq!(redacted["tokenCount"], 3);
        assert_eq!(redacted["tokenized"], "keep-me");
        assert!(redacted["nested"].get("materialValue").is_none());
        assert_eq!(redacted["nested"]["tokenCount"], 9);
        assert!(redacted["nested"]["items"][0].get("lease token").is_none());
        assert_eq!(redacted["nested"]["items"][0]["keep"], "yes");
        assert!(
            redacted["nested"]["items"][1]
                .get("capabilityToken")
                .is_none()
        );
        assert_eq!(redacted["nested"]["items"][1]["tokenized"], "still-here");
    }

    #[test]
    fn direct_db_watch_does_not_need_registry_name_lookup() {
        let root = temp_watch_dir("direct-db");
        let path = root.join("board.db");
        let _store = Store::open(&path).expect("open test board");
        let args = super::super::Args::parse(
            vec![
                "watch".to_owned(),
                "--db".to_owned(),
                path.to_string_lossy().into_owned(),
            ]
            .into_iter()
            .collect(),
        )
        .expect("parse args");
        let spec = resolve_with_source(&args, Some(path)).expect("resolve direct db");
        match spec.source {
            Source::Board { board_name, .. } => assert!(board_name.is_none()),
            Source::Registry => panic!("expected board source"),
        }
    }

    #[test]
    fn follow_refuses_a_zero_limit_before_starting() {
        let root = temp_watch_dir("follow-zero");
        let path = root.join("board.db");
        let _store = Store::open(&path).expect("open test board");
        let args = super::super::Args::parse(
            vec![
                "watch".to_owned(),
                "--db".to_owned(),
                path.to_string_lossy().into_owned(),
                "--follow".to_owned(),
                "--limit".to_owned(),
                "0".to_owned(),
            ]
            .into_iter()
            .collect(),
        )
        .expect("parse args");
        let error = resolve_with_source(&args, Some(path))
            .expect_err("zero limit with follow must fail")
            .to_string();
        assert!(error.contains("at least 1"), "{error}");
    }

    #[test]
    fn future_cursor_is_rejected_before_the_watch_starts() {
        let root = temp_watch_dir("future-cursor");
        let path = root.join("board.db");
        let store = Store::open(&path).expect("open test board");
        crate::audit::append_board_event(
            &store.connection,
            None,
            "board_changed",
            "codex",
            "{}",
            1,
        )
        .expect("append board event");
        let key = StreamKey {
            source_kind: "board".to_owned(),
            source: canonical_source_path(&path).expect("canonical source"),
            selector_kind: "board".to_owned(),
            selector_value: None,
            kind: None,
            archived: false,
        };
        let cursor = encode_cursor(&key, 2).expect("encode future cursor");
        let args = super::super::Args::parse(
            vec![
                "watch".to_owned(),
                "--db".to_owned(),
                path.to_string_lossy().into_owned(),
                "--cursor".to_owned(),
                cursor,
            ]
            .into_iter()
            .collect(),
        )
        .expect("parse args");
        let error = resolve_with_source(&args, Some(path))
            .expect_err("future cursor must be rejected")
            .to_string();
        assert!(
            error.contains("ahead of the current ledger head"),
            "{error}"
        );
    }
}
