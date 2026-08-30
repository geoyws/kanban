use crate::WATCH_BATCH_LIMIT;
use crate::model::{Event, TASK_STATUSES};
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
const METADATA_LIMIT: usize = 16 * 1024;
const BOARD_EVENT_KINDS: &[&str] = &[
    "archive_swept",
    "attention_raised",
    "attention_reopened",
    "attention_resolved",
    "attention_updated",
    "board_initialized",
    "claim_expired",
    "claim_heartbeat",
    "claim_released",
    "checkpoint_added",
    "deployment_abandoned",
    "deployment_finished",
    "deployment_started",
    "epic_advanced",
    "handoff_accepted",
    "handoff_created",
    "lease_seized",
    "note_added",
    "rule_consolidated",
    "rule_retired",
    "search_rebuilt",
    "sitrep_posted",
    "snapshot_restored",
    "story_advanced",
    "story_signed_off",
    "story_signoff_revoked",
    "tag_added",
    "tag_removed",
    "task_added",
    "task_claimed",
    "task_created",
    "task_metadata_patched",
    "task_moved",
    "task_removed",
    "task_updated",
    "tasks_imported",
];
const REGISTRY_EVENT_KINDS: &[&str] = &[
    "rule_added",
    "rule_consolidated",
    "rule_retired",
    "rule_updated",
    "snapshot_restored",
    "workspace_alias_name_discarded",
    "workspace_attached",
    "workspace_detached",
    "workspace_registered",
    "workspace_repointed",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct StreamKey {
    source_kind: String,
    source: String,
    selector_kind: String,
    selector_value: Option<String>,
    kind: Option<String>,
    kinds: Vec<String>,
    relations: Vec<String>,
    prior_statuses: Vec<String>,
    current_statuses: Vec<String>,
    tags: Vec<String>,
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
    kinds: Vec<String>,
    relations: Vec<String>,
    prior_statuses: Vec<String>,
    current_statuses: Vec<String>,
    tags: Vec<String>,
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
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    relations: Vec<String>,
    #[serde(default)]
    prior_statuses: Vec<String>,
    #[serde(default)]
    current_statuses: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
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

    let kinds = normalized(args.many("kind"));
    let relations = normalize_relations(args.many("relation"))?;
    let prior_statuses = normalize_statuses(args.many("prior-status"), "--prior-status")?;
    let current_statuses = normalize_statuses(args.many("current-status"), "--current-status")?;
    let tags = normalized(args.many("tag"));
    let follow = args.has("follow");
    let limit = args.limit(50)?;
    if limit > WATCH_BATCH_LIMIT {
        bail!("--limit must be between 0 and {WATCH_BATCH_LIMIT}, got {limit}");
    }
    if follow && limit == 0 {
        bail!("--follow requires --limit to be at least 1");
    }

    if registry || rule.is_some() {
        if !relations.is_empty()
            || !prior_statuses.is_empty()
            || !current_statuses.is_empty()
            || !tags.is_empty()
        {
            bail!(
                "--relation, --prior-status, --current-status and --tag apply only to board watch events"
            );
        }
        if args.has("all") {
            bail!("--all applies to board events; registry events do not carry archived history");
        }
        if args.has("project") || args.has("workspace") || args.has("db") {
            bail!(
                "--project, --workspace and --db address boards; registry watch uses the registry trail"
            );
        }
        let registry_path = registry_source()?;
        let registry_reader = Registry::open_readonly()?;
        validate_kinds(&kinds, REGISTRY_EVENT_KINDS, |kind| {
            registry_reader.event_kind_exists(kind)
        })?;
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
                kind: compatibility_kind(&kinds),
                kinds: kinds.clone(),
                relations: Vec::new(),
                prior_statuses: Vec::new(),
                current_statuses: Vec::new(),
                tags: Vec::new(),
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
                kind: compatibility_kind(&kinds),
                kinds,
                relations: Vec::new(),
                prior_statuses: Vec::new(),
                current_statuses: Vec::new(),
                tags: Vec::new(),
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
    let store = Store::open_readonly(&board_path)?;
    validate_kinds(&kinds, BOARD_EVENT_KINDS, |kind| {
        store.event_kind_exists(kind)
    })?;
    if !tags.is_empty() {
        let board_tags = store
            .tags()?
            .into_iter()
            .map(|tag| tag.name)
            .collect::<std::collections::HashSet<_>>();
        for tag in &tags {
            if !board_tags.contains(tag) {
                bail!("tag {tag} is not in this board's master file");
            }
        }
    }
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
            kind: compatibility_kind(&kinds),
            kinds: kinds.clone(),
            relations: relations.clone(),
            prior_statuses: prior_statuses.clone(),
            current_statuses: current_statuses.clone(),
            tags: tags.clone(),
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
            kind: compatibility_kind(&kinds),
            kinds,
            relations,
            prior_statuses,
            current_statuses,
            tags,
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
                store.events_since_filtered(
                    spec.key.selector_value.as_deref(),
                    &spec.key.kinds,
                    &spec.key.relations,
                    &spec.key.prior_statuses,
                    &spec.key.current_statuses,
                    &spec.key.tags,
                    cursor,
                    spec.limit,
                    spec.key.archived,
                )?
            }
            Source::Registry => {
                let registry = Registry::open_readonly()?;
                registry.rule_events_since_filtered(
                    spec.key.selector_value.as_deref(),
                    &spec.key.kinds,
                    cursor,
                    spec.limit,
                )?
            }
        };
        if batch.is_empty() {
            if !spec.follow {
                return Ok(());
            }
            let tail = match &spec.source {
                Source::Board { path, .. } => Store::open_readonly(path)?.events_since(
                    spec.key.selector_value.as_deref(),
                    None,
                    cursor,
                    spec.limit,
                    spec.key.archived,
                )?,
                Source::Registry => Registry::open_readonly()?.rule_events_since(
                    spec.key.selector_value.as_deref(),
                    None,
                    cursor,
                    spec.limit,
                )?,
            };
            if let Some(event) = tail.last()
                && needs_advanced_heartbeat(Some(cursor), event.seq)
            {
                cursor = event.seq;
                cursor_token = encode_cursor(&spec.key, cursor)?;
                emit(&WatchEnvelope {
                    version: PROTOCOL_VERSION,
                    scope: scope_envelope(&spec.key, board_name(&spec.source)),
                    cursor: cursor_token.clone(),
                    kind: "heartbeat",
                    payload: json!({"state":"advanced"}),
                })?;
                sleep(POLL_INTERVAL);
                continue;
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
                payload: event_payload(event, &spec.source)?,
            })?;
        }
        if !spec.follow {
            return Ok(());
        }
    }
}

fn needs_advanced_heartbeat(emitted_cursor: Option<i64>, scan_cursor: i64) -> bool {
    emitted_cursor != Some(scan_cursor)
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
        kinds: key.kinds.clone(),
        relations: key.relations.clone(),
        prior_statuses: key.prior_statuses.clone(),
        current_statuses: key.current_statuses.clone(),
        tags: key.tags.clone(),
        archived: key.archived,
    }
}

fn event_payload(event: Event, source: &Source) -> Result<Value> {
    let mut value = serde_json::to_value(event)?;
    let payload = value
        .get_mut("payload")
        .map(Value::take)
        .unwrap_or(Value::Null);
    let snapshot = match source {
        Source::Board { .. } => payload
            .as_object()
            .and_then(|object| object.get("_semanticV1"))
            .filter(|snapshot| snapshot.is_object())
            .cloned(),
        Source::Registry => None,
    };
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.remove("_semanticV1");
    }
    let payload = redact(payload);
    value["payload"] = payload.clone();
    let board = match source {
        Source::Board { path, board_name } => {
            let mut board = serde_json::Map::new();
            board.insert("id".into(), json!(canonical_source_path(path)?));
            if let Some(name) = board_name {
                board.insert("name".into(), json!(name));
            }
            Value::Object(board)
        }
        Source::Registry => Value::Null,
    };
    let event_id = value.get("eventHash").cloned().unwrap_or(Value::Null);
    let timestamp = value.get("createdAt").cloned().unwrap_or(Value::Null);
    let field = |name: &str| {
        snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.get(name))
            .cloned()
            .unwrap_or(Value::Null)
    };
    value["schemaVersion"] = json!(1);
    value["board"] = board;
    value["eventID"] = event_id;
    value["timestamp"] = timestamp;
    value["subject"] = field("subject");
    value["relations"] = field("relations");
    value["priorStatus"] = field("priorStatus");
    value["currentStatus"] = field("currentStatus");
    value["tags"] = field("tags");
    value["metadata"] = bounded_metadata(payload)?;
    Ok(value)
}

fn bounded_metadata(value: Value) -> Result<Value> {
    let bytes = serde_json::to_vec(&value)?;
    let byte_count = bytes.len();
    let wrapper = json!({
        "value": value,
        "bytes": byte_count,
        "truncated": false,
    });
    if serde_json::to_vec(&wrapper)?.len() <= METADATA_LIMIT {
        Ok(wrapper)
    } else {
        Ok(json!({
            "value": null,
            "bytes": byte_count,
            "truncated": true,
        }))
    }
}

fn normalized(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn normalize_statuses(values: Vec<String>, flag: &str) -> Result<Vec<String>> {
    for value in &values {
        if !TASK_STATUSES.contains(&value.as_str()) {
            bail!(
                "{flag} must be one of {}, got {value:?}",
                TASK_STATUSES.join(", ")
            );
        }
    }
    Ok(normalized(values))
}

fn normalize_relations(values: Vec<String>) -> Result<Vec<String>> {
    for value in &values {
        let Some((kind, id)) = value.split_once(':') else {
            bail!(
                "--relation must be KIND:ID with KIND parent, ancestor or depends-on, got {value:?}"
            );
        };
        if !matches!(kind, "parent" | "ancestor" | "depends-on") || id.is_empty() {
            bail!(
                "--relation must be KIND:ID with KIND parent, ancestor or depends-on, got {value:?}"
            );
        }
    }
    Ok(normalized(values))
}

fn compatibility_kind(kinds: &[String]) -> Option<String> {
    (kinds.len() == 1).then(|| kinds[0].clone())
}

fn validate_kinds<F>(kinds: &[String], builtins: &[&str], mut exists: F) -> Result<()>
where
    F: FnMut(&str) -> Result<bool>,
{
    for kind in kinds {
        if !builtins.contains(&kind.as_str()) && !exists(kind)? {
            bail!("unknown watch event kind {kind:?} for this source");
        }
    }
    Ok(())
}

fn normalize_key(key: &StreamKey) -> StreamKey {
    let mut result = key.clone();
    result.kinds = normalized(key.kinds.clone());
    if result.kinds.is_empty()
        && let Some(kind) = &result.kind
    {
        result.kinds.push(kind.clone());
    }
    result.kind = compatibility_kind(&result.kinds);
    result.relations = normalized(result.relations);
    result.prior_statuses = normalized(result.prior_statuses);
    result.current_statuses = normalized(result.current_statuses);
    result.tags = normalized(result.tags);
    result
}

#[cfg(test)]
fn event_matches(event: &Event, key: &StreamKey) -> bool {
    let key = normalize_key(key);
    if !key.kinds.is_empty() && !key.kinds.iter().any(|kind| kind == &event.kind) {
        return false;
    }
    let snapshot = event
        .payload
        .get("_semanticV1")
        .filter(|snapshot| snapshot.is_object());
    if !key.relations.is_empty() {
        let Some(relations) = snapshot
            .and_then(|snapshot| snapshot.get("relations"))
            .and_then(Value::as_array)
        else {
            return false;
        };
        if !key.relations.iter().any(|filter| {
            let Some((kind, id)) = filter.split_once(':') else {
                return false;
            };
            relations.iter().any(|relation| {
                relation.get("kind").and_then(Value::as_str) == Some(kind)
                    && relation.get("id").and_then(Value::as_str) == Some(id)
            })
        }) {
            return false;
        }
    }
    if !key.prior_statuses.is_empty() {
        let Some(status) = snapshot
            .and_then(|snapshot| snapshot.get("priorStatus"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        if !key
            .prior_statuses
            .iter()
            .any(|candidate| candidate == status)
        {
            return false;
        }
    }
    if !key.current_statuses.is_empty() {
        let Some(status) = snapshot
            .and_then(|snapshot| snapshot.get("currentStatus"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        if !key
            .current_statuses
            .iter()
            .any(|candidate| candidate == status)
        {
            return false;
        }
    }
    if !key.tags.is_empty() {
        let Some(tags) = snapshot
            .and_then(|snapshot| snapshot.get("tags"))
            .and_then(Value::as_array)
        else {
            return false;
        };
        if !key
            .tags
            .iter()
            .any(|candidate| tags.iter().any(|tag| tag.as_str() == Some(candidate)))
        {
            return false;
        }
    }
    true
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
    let key = normalize_key(key);
    let token = CursorToken {
        version: PROTOCOL_VERSION,
        source_kind: key.source_kind.clone(),
        source: key.source.clone(),
        selector_kind: key.selector_kind.clone(),
        selector_value: key.selector_value.clone(),
        kind: key.kind.clone(),
        kinds: key.kinds.clone(),
        relations: key.relations.clone(),
        prior_statuses: key.prior_statuses.clone(),
        current_statuses: key.current_statuses.clone(),
        tags: key.tags.clone(),
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
        kinds: token.kinds,
        relations: token.relations,
        prior_statuses: token.prior_statuses,
        current_statuses: token.current_statuses,
        tags: token.tags,
        archived: token.archived,
    };
    if normalize_key(&actual) != normalize_key(expected) {
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
            kinds: Vec::new(),
            relations: Vec::new(),
            prior_statuses: Vec::new(),
            current_statuses: Vec::new(),
            tags: Vec::new(),
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
            kinds: expected.kinds.clone(),
            relations: expected.relations.clone(),
            prior_statuses: expected.prior_statuses.clone(),
            current_statuses: expected.current_statuses.clone(),
            tags: expected.tags.clone(),
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
            kinds: expected.kinds.clone(),
            relations: expected.relations.clone(),
            prior_statuses: expected.prior_statuses.clone(),
            current_statuses: expected.current_statuses.clone(),
            tags: expected.tags.clone(),
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
            kinds: Vec::new(),
            relations: Vec::new(),
            prior_statuses: Vec::new(),
            current_statuses: Vec::new(),
            tags: Vec::new(),
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
            kinds: expected.kinds.clone(),
            relations: expected.relations.clone(),
            prior_statuses: expected.prior_statuses.clone(),
            current_statuses: expected.current_statuses.clone(),
            tags: expected.tags.clone(),
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

    fn board_source() -> Source {
        Source::Board {
            path: PathBuf::from("/tmp/kanban-watch-board.db"),
            board_name: Some("demo".to_owned()),
        }
    }

    fn event(payload: Value) -> Event {
        Event {
            seq: 7,
            task_id: Some("t-1".to_owned()),
            kind: "task_moved".to_owned(),
            actor: Some("test".to_owned()),
            payload,
            created_at: 123,
            archived: false,
            prev_hash: None,
            event_hash: Some("hash-7".to_owned()),
        }
    }

    #[test]
    fn event_projection_strips_private_snapshot_and_adds_semantic_fields() {
        let projected = event_payload(
            event(json!({
                "_semanticV1": {
                    "subject": {"type":"task", "id":"t-1"},
                    "relations": [{"kind":"parent", "type":"story", "id":"s-1"}],
                    "priorStatus": "todo",
                    "currentStatus": "done",
                    "tags": ["infra"]
                },
                "token": "private",
                "note": "visible"
            })),
            &board_source(),
        )
        .unwrap();
        assert_eq!(projected["schemaVersion"], 1);
        assert_eq!(projected["eventID"], "hash-7");
        assert_eq!(projected["timestamp"], 123);
        assert_eq!(projected["board"]["name"], "demo");
        assert_eq!(projected["subject"]["id"], "t-1");
        assert_eq!(projected["currentStatus"], "done");
        assert_eq!(projected["tags"], json!(["infra"]));
        assert!(projected["payload"].get("_semanticV1").is_none());
        assert!(projected["payload"].get("token").is_none());
        assert_eq!(projected["metadata"]["value"]["note"], "visible");
        assert_eq!(projected["metadata"]["truncated"], false);
    }

    #[test]
    fn registry_projection_does_not_interpret_board_snapshot() {
        let projected = event_payload(
            event(json!({
                "_semanticV1": {
                    "subject": {"type":"task", "id":"t-1"},
                    "relations": [{"kind":"parent", "type":"story", "id":"s-1"}],
                    "priorStatus": "todo",
                    "currentStatus": "done",
                    "tags": ["infra"]
                },
                "token": "private",
                "rule": "visible"
            })),
            &Source::Registry,
        )
        .unwrap();
        assert_eq!(projected["board"], Value::Null);
        for field in [
            "subject",
            "relations",
            "priorStatus",
            "currentStatus",
            "tags",
        ] {
            assert_eq!(projected[field], Value::Null, "registry field {field}");
        }
        assert!(projected["payload"].get("_semanticV1").is_none());
        assert!(projected["payload"].get("token").is_none());
        assert_eq!(projected["metadata"]["value"]["rule"], "visible");
    }

    #[test]
    fn legacy_projection_has_null_semantics_and_metadata_is_bounded() {
        let projected = event_payload(
            event(json!({"large": "x".repeat(METADATA_LIMIT)})),
            &board_source(),
        )
        .unwrap();
        for field in [
            "subject",
            "relations",
            "priorStatus",
            "currentStatus",
            "tags",
        ] {
            assert_eq!(projected[field], Value::Null, "legacy field {field}");
        }
        assert_eq!(projected["metadata"]["value"], Value::Null);
        assert_eq!(projected["metadata"]["truncated"], true);
        assert!(projected["metadata"]["bytes"].as_u64().unwrap() > METADATA_LIMIT as u64);
        assert!(serde_json::to_vec(&projected["metadata"]).unwrap().len() <= METADATA_LIMIT);
    }

    #[test]
    fn metadata_wrapper_boundary_is_measured_after_wrapper_serialization() {
        let mut low = 0;
        let mut high = METADATA_LIMIT;
        while low < high {
            let size = (low + high).div_ceil(2);
            let metadata = bounded_metadata(json!({"blob": "x".repeat(size)})).unwrap();
            let serialized = serde_json::to_vec(&metadata).unwrap();
            if metadata["truncated"] == false && serialized.len() <= METADATA_LIMIT {
                low = size;
            } else {
                high = size - 1;
            }
        }
        let within = bounded_metadata(json!({"blob": "x".repeat(low)})).unwrap();
        let within_bytes = serde_json::to_vec(&within).unwrap().len();
        assert!(within_bytes <= METADATA_LIMIT);
        assert_eq!(within["truncated"], false);
        let above = bounded_metadata(json!({"blob": "x".repeat(low + 1)})).unwrap();
        let above_bytes = serde_json::to_vec(&above).unwrap().len();
        assert!(above_bytes <= METADATA_LIMIT);
        assert_eq!(above["value"], Value::Null);
        assert_eq!(above["truncated"], true);
    }

    #[test]
    fn semantic_filters_are_and_across_families_and_or_within_a_family() {
        let event = event(json!({
            "_semanticV1": {
                "relations": [{"kind":"parent", "type":"story", "id":"s-1"}],
                "priorStatus": "todo",
                "currentStatus": "done",
                "tags": ["infra", "rust"]
            }
        }));
        let mut key = identity();
        key.kind = None;
        key.kinds = vec!["task_moved".to_owned(), "task_added".to_owned()];
        key.relations = vec!["parent:s-1".to_owned(), "ancestor:e-1".to_owned()];
        key.prior_statuses = vec!["blocked".to_owned(), "todo".to_owned()];
        key.current_statuses = vec!["done".to_owned()];
        key.tags = vec!["docs".to_owned(), "infra".to_owned()];
        assert!(event_matches(&event, &key));
        key.current_statuses = vec!["review".to_owned()];
        assert!(!event_matches(&event, &key));
        key.current_statuses = vec!["done".to_owned()];
        key.relations = vec!["depends-on:t-2".to_owned()];
        assert!(!event_matches(&event, &key));
        assert!(!event_matches(
            &event,
            &StreamKey {
                kinds: Vec::new(),
                relations: vec!["parent:s-1".to_owned()],
                ..identity()
            }
        ));
    }

    #[test]
    fn cursor_accepts_old_singular_kind_and_binds_new_filters() {
        let legacy = identity();
        let encoded = encode_cursor(&legacy, 12).unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        let mut old = serde_json::from_slice::<Value>(&decoded).unwrap();
        old.as_object_mut().unwrap().remove("kinds");
        old.as_object_mut().unwrap().remove("relations");
        old.as_object_mut().unwrap().remove("priorStatuses");
        old.as_object_mut().unwrap().remove("currentStatuses");
        old.as_object_mut().unwrap().remove("tags");
        let old = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&old).unwrap());
        assert_eq!(decode_cursor(&old, &legacy).unwrap(), 12);

        let mut expected = legacy.clone();
        expected.kind = None;
        expected.kinds = vec!["task_added".to_owned(), "task_moved".to_owned()];
        expected.tags = vec!["infra".to_owned()];
        assert!(
            decode_cursor(
                &old,
                &StreamKey {
                    kinds: vec!["task_added".to_owned(), "task_moved".to_owned()],
                    tags: Vec::new(),
                    ..expected.clone()
                }
            )
            .is_err()
        );
        let token = encode_cursor(&expected, 12).unwrap();
        let mut mismatch = expected.clone();
        mismatch.tags = vec!["other".to_owned()];
        assert!(decode_cursor(&token, &mismatch).is_err());
    }

    #[test]
    fn advanced_heartbeat_is_needed_after_an_unmatched_tail() {
        assert!(!needs_advanced_heartbeat(Some(8), 8));
        assert!(needs_advanced_heartbeat(Some(7), 8));
        assert!(needs_advanced_heartbeat(None, 8));
    }

    #[test]
    fn kind_validation_allows_builtins_and_existing_extensions_only() {
        assert!(
            validate_kinds(&["task_added".to_owned()], BOARD_EVENT_KINDS, |_| Ok(false)).is_ok()
        );
        assert!(
            validate_kinds(
                &["extension_kind".to_owned()],
                BOARD_EVENT_KINDS,
                |kind| Ok(kind == "extension_kind"),
            )
            .is_ok()
        );
        assert!(
            validate_kinds(&["not-present".to_owned()], BOARD_EVENT_KINDS, |_| Ok(
                false
            ),)
            .is_err()
        );
        assert!(
            validate_kinds(
                &["workspace_registered".to_owned()],
                REGISTRY_EVENT_KINDS,
                |_| Ok(false),
            )
            .is_ok()
        );
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
            kinds: Vec::new(),
            relations: Vec::new(),
            prior_statuses: Vec::new(),
            current_statuses: Vec::new(),
            tags: Vec::new(),
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
