use crate::WATCH_BATCH_LIMIT;
use crate::db::read_snapshot;
use crate::model::{BOARD_EVENT_KINDS, Event, TASK_STATUSES};
use crate::registry::{BoardPathState, Registry, data_root, retired_board_message};
use crate::store::Store;
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

const PROTOCOL_VERSION: u8 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const METADATA_LIMIT: usize = 16 * 1024;
const REGISTRY_EVENT_KINDS: &[&str] = &[
    "rule_added",
    "rule_consolidated",
    "rule_retired",
    "rule_updated",
    "snapshot_restored",
    "workspace_alias_name_discarded",
    "workspace_attached",
    "workspace_detached",
    "workspace_retired",
    "workspace_registered",
    "workspace_repointed",
    "workspace_unretired",
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
    Registry {
        root: PathBuf,
    },
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
    resolve_with_source(args, super::direct_board(args))
}

/// `direct_board` carries whether the caller named the path or `KANBAN_DB` did,
/// because the board guard says different things about the two and `watch`
/// opens the file itself instead of going through `store_path_readonly`.
fn resolve_with_source(
    args: &super::Args,
    direct_db: Option<(PathBuf, bool)>,
) -> Result<WatchSpec> {
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
        let registry_root = data_root()?;
        let registry_path = registry_source(&registry_root)?;
        let registry_reader = Registry::open_readonly_at(&registry_root)?;
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
        let source = Source::Registry {
            root: registry_root,
        };
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

    let (board_path, board_name) = if let Some((path, explicit)) = direct_db {
        // `Store::open_readonly` cannot create a file — it passes
        // SQLITE_OPEN_READ_ONLY — so this branch was safe by accident, and said
        // so with a raw `Error code 14`. The guard makes the safety deliberate
        // and the diagnosis the same one every other board path gives.
        super::require_board_file(&path, explicit, super::BoardCreation::Refused)?;
        if path.exists()
            && let Some(BoardPathState::Retired { name, note }) =
                Registry::board_path_state_if_available(&path)?
        {
            bail!(
                "{}",
                retired_board_message(&name, note.as_deref(), "addressing it")
            );
        }
        (path, None)
    } else {
        let path = super::store_path_readonly(args)?;
        let board_name = board_name_for_path(&path)?;
        (path, board_name)
    };
    let board_source = canonical_source_path(&board_path)?;
    let archived = args.has("all");
    let store = Store::open_readonly(&board_path)?;
    if let Some(task_id) = task.as_deref()
        && !store.watch_subject_exists(task_id)?
    {
        bail!("task {task_id} is not present in this board or its event history");
    }
    for relation in &relations {
        let (kind, id) = relation
            .split_once(':')
            .expect("normalized relations always contain a separator");
        if !store.watch_relation_target_exists(kind, id)? {
            bail!(
                "relation {relation} does not name a current task or an exact historical relation target"
            );
        }
    }
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

/// What one poll observed, taken from a single database snapshot.
///
/// `tail_seq` is the highest sequence the unfiltered tail saw in the same
/// snapshot that produced `batch`, so every row up to it was offered to the
/// filtered scan and rejected. That is what makes it safe to move the cursor
/// there: the rows being stepped over are known non-matches, not rows that
/// arrived after the filtered scan had already run.
struct Poll {
    batch: Vec<Event>,
    tail_seq: Option<i64>,
}

/// Read one poll's batch and tail from one snapshot of one connection.
///
/// The two reads used to run on two connections opened moments apart, which is
/// two snapshots. A matching event committing between them was invisible to
/// the filtered scan and visible to the tail, so the tail advanced the cursor
/// past it and the next poll started after it. The event was never delivered
/// and never reported missing. Both reads now sit inside one deferred read
/// transaction, so a commit landing mid-poll is invisible to both and is
/// picked up whole by the next poll.
fn poll_once(spec: &WatchSpec, cursor: i64) -> Result<Poll> {
    match &spec.source {
        Source::Board { path, .. } => {
            let store = Store::open_readonly(path)?;
            read_snapshot(&store, |store| {
                let batch = store.events_since_filtered(
                    spec.key.selector_value.as_deref(),
                    &spec.key.kinds,
                    &spec.key.relations,
                    &spec.key.prior_statuses,
                    &spec.key.current_statuses,
                    &spec.key.tags,
                    cursor,
                    spec.limit,
                    spec.key.archived,
                )?;
                let tail_seq = if batch.is_empty() && spec.follow {
                    store
                        .events_since(
                            None,
                            between_scans(&store.connection, cursor),
                            spec.limit,
                            spec.key.archived,
                        )?
                        .last()
                        .map(|event| event.seq)
                } else {
                    None
                };
                Ok(Poll { batch, tail_seq })
            })
        }
        Source::Registry { root } => {
            let registry = Registry::open_readonly_at(root)?;
            read_snapshot(&registry, |registry| {
                let batch = registry.rule_events_since_filtered(
                    spec.key.selector_value.as_deref(),
                    &spec.key.kinds,
                    cursor,
                    spec.limit,
                )?;
                let tail_seq = if batch.is_empty() && spec.follow {
                    registry
                        .rule_events_since(
                            spec.key.selector_value.as_deref(),
                            None,
                            between_scans(&registry.connection, cursor),
                            spec.limit,
                        )?
                        .last()
                        .map(|event| event.seq)
                } else {
                    None
                };
                Ok(Poll { batch, tail_seq })
            })
        }
    }
}

/// Runs after a poll's filtered scan and before its tail scan, inside the
/// poll's snapshot. Empty in every build but the unit tests, which use it to
/// commit an event into exactly the window a two-snapshot poll used to leak.
///
/// It returns the cursor the tail scan reads from, and is `#[must_use]`, so the
/// tail scan consumes its result and the seam cannot drift after the tail scan
/// without the build going red. A seam that silently slid out of the window
/// would leave the race test asserting nothing while still passing.
#[must_use]
#[cfg(not(test))]
fn between_scans(_connection: &Connection, cursor: i64) -> i64 {
    cursor
}

/// Fires inside a poll's snapshot, between the two scans, and returns the
/// cursor the tail scan then reads from.
#[cfg(test)]
type BetweenScansHook = Box<dyn FnMut(&Connection, i64) -> i64>;

#[cfg(test)]
thread_local! {
    static BETWEEN_SCANS: std::cell::RefCell<Option<BetweenScansHook>> =
        const { std::cell::RefCell::new(None) };
}

#[must_use]
#[cfg(test)]
fn between_scans(connection: &Connection, cursor: i64) -> i64 {
    let hook = BETWEEN_SCANS.with(|slot| slot.borrow_mut().take());
    match hook {
        Some(mut hook) => {
            let cursor = hook(connection, cursor);
            BETWEEN_SCANS.with(|slot| *slot.borrow_mut() = Some(hook));
            cursor
        }
        None => cursor,
    }
}

/// Install (or clear) the hook a poll runs between its two scans.
///
/// The hook is handed the reader connection, so a test can measure what the
/// poll's snapshot exposes while the window is open, and it returns the cursor
/// the tail scan then reads from, so a test can prove the tail really does read
/// through this point rather than around it.
#[cfg(test)]
fn set_between_scans(hook: Option<BetweenScansHook>) {
    BETWEEN_SCANS.with(|slot| *slot.borrow_mut() = hook);
}

/// Uninstalls the hook when it drops, including on the way out of a panic.
///
/// The hook lives in a thread-local, and the test harness reuses threads under
/// `--test-threads=1`, so a panicking test that cleared the hook on its last
/// line would leave it installed for whatever ran next.
#[cfg(test)]
struct BetweenScans;

#[cfg(test)]
impl Drop for BetweenScans {
    fn drop(&mut self) {
        set_between_scans(None);
    }
}

#[cfg(test)]
#[must_use]
fn install_between_scans(hook: BetweenScansHook) -> BetweenScans {
    set_between_scans(Some(hook));
    BetweenScans
}

fn watch(spec: WatchSpec) -> Result<()> {
    stream(&spec, &mut emit)
}

/// The follow loop, with its output behind a sink.
///
/// Split from `watch` so the cursor this loop publishes is measurable. The
/// `advanced` heartbeat is the only place a cursor moves without an event being
/// delivered, so if it encodes the wrong sequence a consumer either re-reads
/// rows forever or steps over rows it never saw — and stdout is the only place
/// that is visible. A sink that returns `Err` ends the stream, which is the
/// same path a closed pipe already takes in production.
fn stream(spec: &WatchSpec, sink: &mut dyn FnMut(&WatchEnvelope) -> Result<()>) -> Result<()> {
    let mut cursor = spec.cursor;
    let mut cursor_token = encode_cursor(&spec.key, cursor)?;
    loop {
        let poll = poll_once(spec, cursor)?;
        if poll.batch.is_empty() {
            if !spec.follow {
                return Ok(());
            }
            if let Some(tail_seq) = poll.tail_seq
                && needs_advanced_heartbeat(Some(cursor), tail_seq)
            {
                cursor = tail_seq;
                cursor_token = encode_cursor(&spec.key, cursor)?;
                sink(&WatchEnvelope {
                    version: PROTOCOL_VERSION,
                    scope: scope_envelope(&spec.key, board_name(&spec.source)),
                    cursor: cursor_token.clone(),
                    kind: "heartbeat",
                    payload: json!({"state":"advanced"}),
                })?;
                sleep(POLL_INTERVAL);
                continue;
            }
            sink(&WatchEnvelope {
                version: PROTOCOL_VERSION,
                scope: scope_envelope(&spec.key, board_name(&spec.source)),
                cursor: cursor_token.clone(),
                kind: "heartbeat",
                payload: json!({"state":"idle"}),
            })?;
            sleep(POLL_INTERVAL);
            continue;
        }
        for event in poll.batch {
            cursor = event.seq;
            cursor_token = encode_cursor(&spec.key, cursor)?;
            sink(&WatchEnvelope {
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
        Source::Registry { .. } => None,
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
    match source {
        Source::Board { path, board_name } => {
            project_board_event(event, path, board_name.as_deref())
        }
        Source::Registry { .. } => project_event(event, None),
    }
}

pub(crate) fn project_board_event(
    event: Event,
    path: &Path,
    board_name: Option<&str>,
) -> Result<Value> {
    project_event(event, Some((path, board_name)))
}

fn project_event(event: Event, board_source: Option<(&Path, Option<&str>)>) -> Result<Value> {
    let mut value = serde_json::to_value(event)?;
    let payload = value
        .get_mut("payload")
        .map(Value::take)
        .unwrap_or(Value::Null);
    let snapshot = match board_source {
        Some(_) => payload
            .as_object()
            .and_then(|object| object.get("_semanticV1"))
            .filter(|snapshot| snapshot.is_object())
            .cloned(),
        None => None,
    };
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.remove("_semanticV1");
    }
    let payload = redact(payload);
    value["payload"] = payload.clone();
    let board = match board_source {
        Some((path, board_name)) => {
            let mut board = serde_json::Map::new();
            board.insert("id".into(), json!(canonical_source_path(path)?));
            if let Some(name) = board_name {
                board.insert("name".into(), json!(name));
            }
            Value::Object(board)
        }
        None => Value::Null,
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

pub(crate) fn secret_key(value: &str) -> bool {
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
        Source::Registry { root } => registry_head(root)?,
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

fn registry_head(root: &Path) -> Result<i64> {
    let registry = Registry::open_readonly_at(root)?;
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

fn registry_source(root: &Path) -> Result<String> {
    canonical_source_path(&root.join("registry.db"))
}

fn board_name_for_path(path: &Path) -> Result<Option<String>> {
    match Registry::board_path_state_if_available(path)? {
        Some(BoardPathState::Active(name)) => return Ok(Some(name)),
        Some(BoardPathState::Retired { name, note }) => {
            bail!(
                "{}",
                retired_board_message(&name, note.as_deref(), "addressing it")
            )
        }
        Some(BoardPathState::External) | None => {}
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
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::rc::Rc;
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

    fn ledger_head(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |row| {
                row.get(0)
            })
            .expect("read board ledger head")
    }

    fn rule_ledger_head(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT COALESCE(MAX(seq),0) FROM rule_events", [], |row| {
                row.get(0)
            })
            .expect("read registry ledger head")
    }

    /// A board watch that follows, matching only `task_moved`.
    fn follow_spec(path: &Path, cursor: i64) -> WatchSpec {
        WatchSpec {
            source: Source::Board {
                path: path.to_path_buf(),
                board_name: None,
            },
            key: StreamKey {
                source_kind: "board".to_owned(),
                source: canonical_source_path(path).expect("canonical source"),
                selector_kind: "board".to_owned(),
                selector_value: None,
                kind: Some("task_moved".to_owned()),
                kinds: vec!["task_moved".to_owned()],
                relations: Vec::new(),
                prior_statuses: Vec::new(),
                current_statuses: Vec::new(),
                tags: Vec::new(),
                archived: false,
            },
            cursor,
            limit: 32,
            follow: true,
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
        let source = board_source();
        let fixture = event(json!({
            "_semanticV1": {
                "subject": {"type":"task", "id":"t-1"},
                "relations": [{"kind":"parent", "type":"story", "id":"s-1"}],
                "priorStatus": "todo",
                "currentStatus": "done",
                "tags": ["infra"]
            },
            "token": "private",
            "note": "visible"
        }));
        let projected = event_payload(fixture.clone(), &source).unwrap();
        let direct = match &source {
            Source::Board { path, board_name } => {
                project_board_event(fixture, path, board_name.as_deref()).unwrap()
            }
            Source::Registry { .. } => unreachable!(),
        };
        assert_eq!(projected, direct);
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
            &Source::Registry {
                root: PathBuf::from("/tmp/kanban-watch-registry"),
            },
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
        let spec = resolve_with_source(&args, Some((path, true))).expect("resolve direct db");
        match spec.source {
            Source::Board { board_name, .. } => assert!(board_name.is_none()),
            Source::Registry { .. } => panic!("expected board source"),
        }
    }

    #[test]
    fn direct_db_watch_rejects_unknown_subjects_and_relation_targets_but_accepts_history() {
        let root = temp_watch_dir("direct-db-subject-relations");
        let path = root.join("board.db");
        let store = Store::open(&path).expect("open test board");
        crate::audit::append_board_event(
            &store.connection,
            Some("t-removed"),
            "task_removed",
            "codex",
            r#"{"_semanticV1":{"relations":[{"kind":"parent","type":"story","id":"s-removed"}]}}"#,
            1,
        )
        .expect("append historical semantic event");

        let resolve = |extra: &[&str]| {
            let mut raw = vec![
                "watch".to_owned(),
                "--db".to_owned(),
                path.to_string_lossy().into_owned(),
            ];
            raw.extend(extra.iter().map(|value| (*value).to_owned()));
            let args = super::super::Args::parse(raw).expect("parse watch args");
            resolve_with_source(&args, Some((path.clone(), true)))
        };

        assert!(resolve(&["--task", "t-removed"]).is_ok());
        assert!(resolve(&["--relation", "parent:s-removed"]).is_ok());
        let subject = resolve(&["--task", "t-unknown"])
            .expect_err("unknown task selector must fail")
            .to_string();
        assert!(subject.contains("not present"), "{subject}");
        let relation = resolve(&["--relation", "parent:s-unknown"])
            .expect_err("unknown relation target must fail")
            .to_string();
        assert!(
            relation.contains("exact historical relation target"),
            "{relation}"
        );
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
        let error = resolve_with_source(&args, Some((path, true)))
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
        let error = resolve_with_source(&args, Some((path, true)))
            .expect_err("future cursor must be rejected")
            .to_string();
        assert!(
            error.contains("ahead of the current ledger head"),
            "{error}"
        );
    }

    /// A poll must never step its cursor over an event its own filtered scan
    /// could not have seen.
    ///
    /// The board is driven at exactly the interleaving that used to lose the
    /// event: the filtered scan runs, a matching event commits, then the tail
    /// scan runs. With the two scans on separate connections the tail saw the
    /// new event, `watch` moved the cursor onto it, and the next poll started
    /// after it — permanent, silent loss. With both scans inside one read
    /// snapshot the commit is invisible to the poll that raced it and the next
    /// poll delivers it.
    #[test]
    fn a_poll_never_advances_past_an_event_that_commits_between_its_two_scans() {
        let root = temp_watch_dir("single-snapshot-poll");
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
        .expect("append the seed event");

        let spec = follow_spec(&path, 1);

        let writer_path = path.clone();
        let committed = Rc::new(Cell::new(false));
        let fired = Rc::clone(&committed);
        // What the poll's own reader can see at the instant the window is open.
        // Pinned at the pre-commit head, this proves the seam fires inside a
        // live snapshot rather than before one was taken.
        let observed = Rc::new(Cell::new(-1_i64));
        let seen = Rc::clone(&observed);
        let raced = {
            let _seam = install_between_scans(Box::new(move |reader: &Connection, cursor: i64| {
                if fired.replace(true) {
                    return cursor;
                }
                let writer = crate::db::open_board(&writer_path).expect("open interposing writer");
                crate::audit::append_board_event(
                    &writer,
                    Some("t-1"),
                    "task_moved",
                    "codex",
                    "{}",
                    2,
                )
                .expect("commit a matching event inside the poll window");
                assert_eq!(
                    ledger_head(&writer),
                    2,
                    "the interposed commit did not land"
                );
                seen.set(ledger_head(reader));
                cursor
            }));
            poll_once(&spec, 1).expect("poll across the commit window")
        };

        assert!(committed.get(), "the interposing commit never ran");
        assert_eq!(
            observed.get(),
            1,
            "the seam fired outside the poll's snapshot: the reader saw head {} while the \
             committed head was 2, so this test is no longer measuring the race window",
            observed.get()
        );
        assert!(
            raced.batch.is_empty(),
            "the snapshot predates the commit, so the filtered scan cannot hold it"
        );
        assert_eq!(
            raced.tail_seq, None,
            "the poll moved its cursor onto an event its filtered scan never saw; \
             that event is dropped and no consumer is told"
        );

        let delivered = poll_once(&spec, 1).expect("poll after the commit window");
        assert_eq!(
            delivered
                .batch
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![2],
            "the next poll must deliver the event the raced poll declined to skip"
        );
        assert_eq!(delivered.batch[0].kind, "task_moved");

        // The tail still has to advance past rows the filters reject, or the
        // cursor would freeze behind unrelated board traffic.
        crate::audit::append_board_event(
            &store.connection,
            None,
            "board_changed",
            "codex",
            "{}",
            3,
        )
        .expect("append an unmatched event");
        let advanced = poll_once(&spec, 2).expect("poll past an unmatched event");
        assert!(advanced.batch.is_empty());
        assert_eq!(
            advanced.tail_seq,
            Some(3),
            "an event already committed before the poll must still move the cursor"
        );
    }

    #[test]
    fn the_between_scans_hook_is_reusable_and_clears_and_passes_the_cursor_through() {
        let root = temp_watch_dir("seam-reuse");
        let path = root.join("board.db");
        let store = Store::open(&path).expect("open test board");
        let calls = Rc::new(RefCell::new(0_usize));
        let counter = Rc::clone(&calls);
        let _seam = install_between_scans(Box::new(move |_, cursor| {
            *counter.borrow_mut() += 1;
            cursor
        }));
        assert_eq!(between_scans(&store.connection, 41), 41);
        assert_eq!(between_scans(&store.connection, 42), 42);
        set_between_scans(None);
        assert_eq!(between_scans(&store.connection, 43), 43);
        assert_eq!(*calls.borrow(), 2);
    }

    /// Collect the envelopes a follow stream emits, stopping after `take`.
    ///
    /// The sink returning `Err` is how the loop ends here, which is the same
    /// path a closed pipe takes in production (`reader_left` in `lib.rs`), so
    /// this drives the real loop rather than a test-only variant of it.
    fn collect_stream(spec: &WatchSpec, take: usize) -> Vec<WatchEnvelope> {
        let mut captured: Vec<WatchEnvelope> = Vec::new();
        let result = stream(spec, &mut |envelope| {
            captured.push(envelope.clone());
            if captured.len() >= take {
                bail!("sink is done");
            }
            Ok(())
        });
        if captured.len() < take {
            result.expect("stream ended before the sink had enough envelopes");
        }
        captured
    }

    fn envelope_state(envelope: &WatchEnvelope) -> String {
        envelope
            .payload
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    /// The `advanced` heartbeat must publish the sequence the tail reached.
    ///
    /// This is the one place a cursor moves with no event delivered, so an
    /// off-by-one here is invisible in the batch path and shows up only as a
    /// consumer that re-reads the same rows forever. The two envelopes matter
    /// jointly: the first pins the token the loop publishes, the second proves
    /// the loop carried that cursor into the next iteration instead of
    /// re-advancing over ground it had already covered.
    #[test]
    fn the_advanced_heartbeat_publishes_the_tail_sequence_and_the_loop_keeps_it() {
        let root = temp_watch_dir("advanced-heartbeat");
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
        .expect("append an unmatched event");

        let spec = follow_spec(&path, 0);
        let envelopes = collect_stream(&spec, 2);

        assert_eq!(envelopes[0].kind, "heartbeat");
        assert_eq!(envelope_state(&envelopes[0]), "advanced");
        assert_eq!(
            decode_cursor(&envelopes[0].cursor, &spec.key).expect("decode advanced cursor"),
            1,
            "the advanced heartbeat published a cursor that is not the sequence the tail reached"
        );

        assert_eq!(envelopes[1].kind, "heartbeat");
        assert_eq!(
            envelope_state(&envelopes[1]),
            "idle",
            "the loop re-advanced over ground it had already covered, so a follower \
             never makes progress"
        );
        assert_eq!(
            decode_cursor(&envelopes[1].cursor, &spec.key).expect("decode idle cursor"),
            1,
            "the loop did not carry the advanced cursor into the next iteration"
        );
    }

    /// Matched events are delivered with their own sequence, and a non-follow
    /// watch terminates on its own.
    #[test]
    fn a_bounded_watch_delivers_matched_events_with_their_cursors_and_then_stops() {
        let root = temp_watch_dir("bounded-delivery");
        let path = root.join("board.db");
        let store = Store::open(&path).expect("open test board");
        for (seq, kind) in [(1, "board_changed"), (2, "task_moved"), (3, "task_moved")] {
            crate::audit::append_board_event(
                &store.connection,
                None,
                kind,
                "codex",
                "{}",
                seq as i64,
            )
            .expect("append event");
        }

        let mut spec = follow_spec(&path, 0);
        spec.follow = false;
        let mut captured = Vec::new();
        stream(&spec, &mut |envelope| {
            captured.push(envelope.clone());
            Ok(())
        })
        .expect("a bounded watch must terminate on its own");
        assert_eq!(captured.len(), 2);
        for (envelope, seq) in captured.iter().zip([2, 3]) {
            assert_eq!(envelope.kind, "event");
            assert_eq!(
                decode_cursor(&envelope.cursor, &spec.key).expect("decode event cursor"),
                seq
            );
        }

        // An empty bounded watch returns without emitting anything at all.
        let mut empty = follow_spec(&path, 3);
        empty.follow = false;
        let mut nothing = Vec::new();
        stream(&empty, &mut |envelope| {
            nothing.push(envelope.clone());
            Ok(())
        })
        .expect("an empty bounded watch must terminate");
        assert!(nothing.is_empty());
    }

    /// The tail scan must read its cursor through the seam.
    ///
    /// Without this, a seam that drifts to *after* the tail scan is invisible:
    /// inside a held snapshot no data a hook can observe distinguishes "before
    /// the tail scan" from "after" it — that indistinguishability is precisely
    /// the property the fix establishes. So the seam is made load-bearing
    /// instead. The hook shifts the cursor the tail reads from, and the tail's
    /// answer has to move with it; if the tail already ran, it answers from the
    /// unshifted cursor and this fails.
    #[test]
    fn the_tail_scan_reads_its_cursor_through_the_seam() {
        let root = temp_watch_dir("seam-placement");
        let path = root.join("board.db");
        let store = Store::open(&path).expect("open test board");
        for seq in 1..=3 {
            crate::audit::append_board_event(
                &store.connection,
                None,
                "board_changed",
                "codex",
                "{}",
                seq,
            )
            .expect("append an unmatched event");
        }

        let mut spec = follow_spec(&path, 0);
        spec.limit = 1;

        // No hook: the tail steps to the first row after cursor 0.
        assert_eq!(
            poll_once(&spec, 0).expect("unshifted poll").tail_seq,
            Some(1)
        );

        // Hook shifts the tail's cursor forward by one, so the tail must answer
        // from sequence 2 instead. A seam sitting after the tail scan cannot
        // move this number.
        let shifted = {
            let _seam = install_between_scans(Box::new(|_, cursor| cursor + 1));
            poll_once(&spec, 0).expect("shifted poll")
        };
        assert_eq!(
            shifted.tail_seq,
            Some(2),
            "the tail scan did not read its cursor through the seam, so the seam is no \
             longer sitting between the two scans and every race test using it is vacuous"
        );
    }

    /// The registry arm loses events across a two-snapshot poll exactly as the
    /// board arm did, so it gets the same proof rather than an argument that it
    /// shares a helper.
    #[test]
    fn a_registry_poll_never_advances_past_a_rule_event_committed_between_its_scans() {
        let root = temp_watch_dir("registry-single-snapshot");
        let registry_db = root.join("registry.db");
        let writer = crate::db::open_registry(&registry_db).expect("create the test registry");
        crate::audit::append_registry_event(&writer, "rule-1", "rule_updated", "codex", "{}", 1)
            .expect("append an unmatched rule event");

        let spec = WatchSpec {
            source: Source::Registry { root: root.clone() },
            key: StreamKey {
                source_kind: "registry".to_owned(),
                source: registry_source(&root).expect("registry source"),
                selector_kind: "registry".to_owned(),
                selector_value: None,
                kind: Some("rule_added".to_owned()),
                kinds: vec!["rule_added".to_owned()],
                relations: Vec::new(),
                prior_statuses: Vec::new(),
                current_statuses: Vec::new(),
                tags: Vec::new(),
                archived: false,
            },
            cursor: 1,
            limit: 32,
            follow: true,
        };

        let registry_path = registry_db.clone();
        let committed = Rc::new(Cell::new(false));
        let fired = Rc::clone(&committed);
        let observed = Rc::new(Cell::new(-1_i64));
        let seen = Rc::clone(&observed);
        let raced = {
            let _seam = install_between_scans(Box::new(move |reader: &Connection, cursor: i64| {
                if fired.replace(true) {
                    return cursor;
                }
                let writer = crate::db::open_registry(&registry_path)
                    .expect("open interposing registry writer");
                crate::audit::append_registry_event(
                    &writer,
                    "rule-2",
                    "rule_added",
                    "codex",
                    "{}",
                    2,
                )
                .expect("commit a matching rule event inside the poll window");
                assert_eq!(
                    rule_ledger_head(&writer),
                    2,
                    "the interposed commit did not land"
                );
                seen.set(rule_ledger_head(reader));
                cursor
            }));
            poll_once(&spec, 1).expect("poll across the commit window")
        };

        assert!(committed.get(), "the interposing commit never ran");
        assert_eq!(
            observed.get(),
            1,
            "the seam fired outside the registry poll's snapshot, so this test is no \
             longer measuring the race window"
        );
        assert!(raced.batch.is_empty());
        assert_eq!(
            raced.tail_seq, None,
            "the registry poll moved its cursor onto a rule event its filtered scan \
             never saw; that event is dropped and no consumer is told"
        );

        let delivered = poll_once(&spec, 1).expect("poll after the commit window");
        assert_eq!(
            delivered
                .batch
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(delivered.batch[0].kind, "rule_added");

        crate::audit::append_registry_event(&writer, "rule-3", "rule_updated", "codex", "{}", 3)
            .expect("append another unmatched rule event");
        let advanced = poll_once(&spec, 2).expect("poll past an unmatched rule event");
        assert!(advanced.batch.is_empty());
        assert_eq!(
            advanced.tail_seq,
            Some(3),
            "a rule event committed before the poll must still move the cursor"
        );
    }
}
