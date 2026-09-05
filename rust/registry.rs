use crate::WATCH_BATCH_LIMIT;
use crate::db::{
    SnapshotSource, checkpoint, create_backup_target, create_private_dir_all,
    finalize_adopted_board, foreign_key_violations, integrity, open_board, open_registry,
    open_registry_readonly, own_private_dir, read_snapshot,
};
use crate::model::{
    Event, ProjectRecord, Rule, RuleMigrationReport, RuleSummary, RuleTransferBundle,
    RuleTransferItem, RuleTransferReport, UnreachableRoot, WorkspaceAdoptReceipt, WorkspaceRecord,
};
use crate::store::{Store, event, validate_tag_name};
use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, params, params_from_iter,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

pub(crate) const WORKSPACE_ADOPT_HELPER_COMMAND: &str = "__workspace-adopt-helper";
const WORKSPACE_ADOPT_HELPER_ROOT_FD: i32 = 37;
const WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD: i32 = 38;

fn validate_rule_body(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("rule body is required");
    }
    if value
        .lines()
        .next()
        .is_none_or(|line| line.trim().is_empty())
    {
        bail!("rule headline is required on the first line");
    }
    Ok(())
}

fn validate_rule_actor(value: &str) -> Result<&str> {
    let actor = value.trim();
    if actor.is_empty() {
        bail!("author is required");
    }
    Ok(actor)
}

fn validate_registry_uuid(value: &str) -> Result<&str> {
    let uuid = value.trim();
    if uuid.is_empty() {
        bail!("rule transfer bundle is missing sourceRegistryUuid");
    }
    Uuid::parse_str(uuid)
        .with_context(|| format!("rule transfer bundle sourceRegistryUuid {uuid} is invalid"))?;
    Ok(uuid)
}

#[allow(dead_code)]
fn deny_secret_material(body: &str) -> Result<()> {
    const DENYLIST: &[&str] = &[
        "BEGIN PRIVATE KEY",
        "BEGIN OPENSSH PRIVATE KEY",
        "BEGIN RSA PRIVATE KEY",
        "PRIVATE KEY-----",
        "client_secret",
        "credential",
        "credentials",
        "password",
        "secret",
        "token",
    ];
    let lower = body.to_lowercase();
    if DENYLIST
        .iter()
        .any(|needle| lower.contains(&needle.to_lowercase()))
    {
        bail!("rule body contains secret material");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rule_fingerprint(
    source_registry_uuid: &str,
    source_rule_id: &str,
    body: &str,
    author: &str,
    archived: bool,
    created_at: i64,
    updated_at: i64,
    tags: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_registry_uuid.as_bytes());
    hasher.update([0]);
    hasher.update(source_rule_id.as_bytes());
    hasher.update([0]);
    hasher.update(body.as_bytes());
    hasher.update([0]);
    hasher.update(author.as_bytes());
    hasher.update([0]);
    hasher.update([archived as u8]);
    hasher.update([0]);
    hasher.update(created_at.to_le_bytes());
    hasher.update([0]);
    hasher.update(updated_at.to_le_bytes());
    hasher.update([0]);
    hasher.update(tags.join("\u{1f}").as_bytes());
    crate::audit::bytes_sha256(&hasher.finalize())
}

fn parse_json_string_array(encoded: &str) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            encoded.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn sorted_unique_boards(boards: &[String]) -> Result<Vec<String>> {
    if boards.is_empty() {
        bail!("rule transfer bundle is missing sourceBoards");
    }
    let sorted = boards.to_vec();
    let mut dedup = sorted.clone();
    dedup.sort();
    dedup.dedup();
    if dedup.len() != sorted.len() || dedup != sorted {
        bail!("rule transfer bundle sourceBoards must be sorted and unique");
    }
    Ok(sorted)
}

fn rule_row(record: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    let encoded: String = record.get("tags")?;
    let tags = serde_json::from_str(&encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            encoded.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(Rule {
        id: record.get("id")?,
        body: record.get("body")?,
        author: record.get("author")?,
        archived: record.get("archived")?,
        created_at: record.get("created_at")?,
        updated_at: record.get("updated_at")?,
        tags,
        source_board: record.get("source_board")?,
        source_rule_id: record.get("source_rule_id")?,
        source_registry_uuid: record.get::<_, Option<String>>("source_registry_uuid")?,
        source_boards: record
            .get::<_, Option<String>>("source_boards")?
            .map(|encoded| parse_json_string_array(&encoded))
            .transpose()?,
        source_content_sha256: record.get::<_, Option<String>>("source_content_sha256")?,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveRuleSelectorError {
    pub rule_id: String,
    pub selector: String,
    pub active_board_count: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveRuleSelectorHealth {
    pub healthy: bool,
    pub errors: Vec<ActiveRuleSelectorError>,
}

fn named_selector_board(tag: &str) -> Option<&str> {
    tag.strip_prefix("ONLY:")
        .or_else(|| tag.strip_prefix("EXCEPT:"))
}

fn active_board_count(
    connection: &Connection,
    board_name: &str,
    excluded_board_path: Option<&str>,
) -> Result<i64> {
    connection
        .query_row(
            "SELECT count(*) FROM boards WHERE name=? AND archived=0 AND (? IS NULL OR board_path != ?)",
            params![board_name, excluded_board_path, excluded_board_path],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn active_named_selector_errors<'a>(
    connection: &Connection,
    rules: impl IntoIterator<Item = (&'a str, &'a [String])>,
    selector_board: Option<&str>,
    excluded_board_path: Option<&str>,
) -> Result<Vec<ActiveRuleSelectorError>> {
    let mut board_counts = HashMap::new();
    let mut errors = Vec::new();
    for (rule_id, tags) in rules {
        for selector in tags {
            let Some(board_name) = named_selector_board(selector) else {
                continue;
            };
            if selector_board.is_some_and(|selected| selected != board_name) {
                continue;
            }
            let active_board_count = if let Some(count) = board_counts.get(board_name) {
                *count
            } else {
                let count = active_board_count(connection, board_name, excluded_board_path)?;
                board_counts.insert(board_name.to_owned(), count);
                count
            };
            if active_board_count != 1 {
                errors.push(ActiveRuleSelectorError {
                    rule_id: rule_id.to_owned(),
                    selector: selector.clone(),
                    active_board_count,
                });
            }
        }
    }
    errors.sort_by(|left, right| {
        left.rule_id
            .cmp(&right.rule_id)
            .then(left.selector.cmp(&right.selector))
            .then(left.active_board_count.cmp(&right.active_board_count))
    });
    errors.dedup_by(|left, right| {
        left.rule_id == right.rule_id
            && left.selector == right.selector
            && left.active_board_count == right.active_board_count
    });
    Ok(errors)
}

fn active_rule_tags(connection: &Connection) -> Result<Vec<(String, Vec<String>)>> {
    let mut statement =
        connection.prepare("SELECT id,tags FROM rules WHERE archived=0 ORDER BY id")?;
    statement
        .query_map([], |row| {
            let encoded = row.get::<_, String>(1)?;
            Ok((row.get::<_, String>(0)?, parse_json_string_array(&encoded)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn validate_active_rule_selectors(
    connection: &Connection,
    rule_id: &str,
    tags: &[String],
) -> Result<()> {
    let errors = active_named_selector_errors(connection, [(rule_id, tags)], None, None)?;
    if let Some(error) = errors.first() {
        bail!(
            "rule {} selector {} requires exactly one active board, but found {}",
            error.rule_id,
            error.selector,
            error.active_board_count
        );
    }
    Ok(())
}

fn selector_tags_apply(tags: &[String], board_name: Option<&str>) -> bool {
    if tags.iter().any(|tag| tag == "ALL") {
        return board_name.is_none_or(|name| {
            !tags
                .iter()
                .any(|tag| tag.strip_prefix("EXCEPT:") == Some(name))
        });
    }
    board_name.is_some_and(|name| {
        tags.iter()
            .any(|tag| tag.strip_prefix("ONLY:") == Some(name))
    })
}

fn is_selector_tag(tag: &str) -> bool {
    tag == "ALL" || tag.starts_with("ONLY:") || tag.starts_with("EXCEPT:")
}

fn rule_tags_apply(
    tags: &[String],
    board_name: Option<&str>,
    task_tags: Option<&HashSet<String>>,
) -> bool {
    if !selector_tags_apply(tags, board_name) {
        return false;
    }
    let subsystems = tags.iter().filter(|tag| !is_selector_tag(tag));
    let mut saw_subsystem = false;
    for tag in subsystems {
        saw_subsystem = true;
        if task_tags.is_some_and(|task_tags| task_tags.contains(tag)) {
            return true;
        }
    }
    !saw_subsystem
}

#[allow(dead_code)]
fn validate_event_limit(limit: i64) -> Result<()> {
    if !(0..=WATCH_BATCH_LIMIT).contains(&limit) {
        bail!("--limit must be between 0 and {WATCH_BATCH_LIMIT}, got {limit}");
    }
    Ok(())
}

#[allow(dead_code)]
fn rule_event_row(record: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        seq: record.get("seq")?,
        task_id: None,
        kind: record.get("kind")?,
        actor: Some(record.get("actor")?),
        payload: serde_json::from_str(&record.get::<_, String>("payload")?).unwrap_or(json!({})),
        created_at: record.get("created_at")?,
        archived: false,
        prev_hash: record.get("prev_hash")?,
        event_hash: record.get("event_hash")?,
    })
}

/// Milliseconds since the Unix epoch.
///
/// Infallible by construction rather than by `expect`: a panic here would
/// abort mid-command with a Rust backtrace instead of an error an agent can
/// read, and every caller writes the result into a durable record. The clamps
/// are unreachable in practice because [`require_sane_clock`] refuses the run
/// before any command gets this far — they exist so the function has no panic
/// path at all, not as a fallback anybody should lean on.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, millis_of)
}

/// `Duration` to milliseconds, saturating rather than wrapping.
///
/// `as i64` on the `u128` this comes from truncates the high bits, so a clock
/// far enough in the future produced a small or negative "now" — an ordering
/// error rather than an obvious one.
fn millis_of(elapsed: std::time::Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// Refuse to run on a clock that cannot produce a timestamp.
///
/// Every task, note, checkpoint, event and lease is stamped, and leases expire
/// by comparing stamps, so a clock before the epoch does not degrade the
/// ledger — it makes every ordering in it meaningless. Saying so once, up
/// front, beats writing records that are wrong in a way nothing later can
/// detect.
pub fn require_sane_clock() -> Result<()> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            anyhow::anyhow!(
                "the system clock reads before 1970-01-01; kanban stamps every record it writes \
                 and orders leases by those stamps, so it will not write a time it cannot compute"
            )
        })?;
    Ok(())
}

pub fn data_root() -> Result<PathBuf> {
    if let Some(value) = env::var_os("KANBAN_DATA_DIR") {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(value).join("kanban"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/kanban"))
}

pub(crate) fn prepare_live_root_for_adoption() -> Result<()> {
    pin_live_root_for_adoption().map(|_| ())
}

pub(crate) fn preflight_live_root_for_adoption() -> Result<()> {
    let root = data_root()?;
    if let Err(error) = secure_registry_dirs(&root, false) {
        if error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        }) {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

pub(crate) struct PinnedAdoptionRoot {
    pub(crate) root: PathBuf,
    pub(crate) root_dir: File,
    pub(crate) boards: File,
}

pub(crate) fn pin_live_root_for_adoption() -> Result<PinnedAdoptionRoot> {
    let root = data_root()?;
    let (root_dir, boards) = secure_registry_dirs(&root, true)?;
    verify_pinned_dir(&root, &root_dir, "registry data root")?;
    verify_registry_chain(&root, &boards)?;
    Ok(PinnedAdoptionRoot {
        root,
        root_dir,
        boards,
    })
}

fn row(record: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        root_path: record.get("root_path")?,
        name: record.get("name")?,
        board_path: record.get("board_path")?,
        created_at: record.get("created_at")?,
        last_used_at: record.get("last_used_at")?,
        archived: record.get::<_, i64>("archived")? != 0,
        archived_at: record.get("archived_at")?,
        archived_by: record.get("archived_by")?,
        archived_note: record.get("archived_note")?,
        rootless: record.get::<_, i64>("rootless")? != 0,
    })
}

fn history_row(record: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        root_path: record.get("root_path")?,
        name: record.get("name")?,
        board_path: record.get("board_path")?,
        created_at: record.get("created_at")?,
        last_used_at: record.get("last_used_at")?,
        archived: true,
        archived_at: record.get("archived_at")?,
        archived_by: record.get("archived_by")?,
        archived_note: record.get("archived_note")?,
        rootless: false,
    })
}

fn rootless_row(record: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        root_path: record.get("root_path")?,
        name: record.get("name")?,
        board_path: record.get("board_path")?,
        created_at: record.get("created_at")?,
        last_used_at: record.get("last_used_at")?,
        archived: record.get::<_, i64>("archived")? != 0,
        archived_at: record.get("archived_at")?,
        archived_by: record.get("archived_by")?,
        archived_note: record.get("archived_note")?,
        rootless: record.get::<_, i64>("rootless")? != 0,
    })
}

fn push_unique_root(roots: &mut Vec<String>, seen: &mut HashSet<String>, root: String) {
    if seen.insert(root.clone()) {
        roots.push(root);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoardPathState {
    Active(String),
    Retired { name: String, note: Option<String> },
    External,
}

pub(crate) fn retired_board_message(name: &str, note: Option<&str>, action: &str) -> String {
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    match note {
        Some(note) => format!(
            "Kanban project {name} is retired (retirement note: {note}); use `kanban workspace unretire {name} --as ACTOR` before {action}"
        ),
        None => format!(
            "Kanban project {name} is retired; use `kanban workspace unretire {name} --as ACTOR` before {action}"
        ),
    }
}

type BoardRow = (
    String,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn board_roots(
    connection: &Connection,
    board_path: &str,
    retirement_id: Option<&str>,
) -> Result<Vec<String>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    if let Some(retirement_id) = retirement_id {
        let mut statement = connection.prepare(
            "SELECT root_path FROM workspace_alias_history WHERE board_path=? AND retirement_id=? ORDER BY last_used_at DESC,root_path",
        )?;
        for root in statement.query_map(params![board_path, retirement_id], |row| {
            row.get::<_, String>(0)
        })? {
            push_unique_root(&mut roots, &mut seen, root?);
        }
    } else {
        let mut statement = connection.prepare(
            "SELECT root_path FROM workspace_roots WHERE board_path=? ORDER BY last_used_at DESC,root_path",
        )?;
        for root in statement.query_map([board_path], |row| row.get::<_, String>(0))? {
            push_unique_root(&mut roots, &mut seen, root?);
        }
    }
    Ok(roots)
}

fn latest_history_root(
    connection: &Connection,
    workspace: &Path,
) -> Result<Option<WorkspaceRecord>> {
    let text = workspace.to_string_lossy();
    let latest = connection
        .query_row(
            "SELECT root_path,\
                    name,\
                    board_path,\
                    created_at,\
                    last_used_at,\
                    archived_at,\
                    archived_by,\
                    archived_note \
             FROM workspace_alias_history \
             WHERE root_path=? \
             ORDER BY archived_at DESC,seq DESC \
             LIMIT 1",
            [text.as_ref()],
            |record| {
                Ok(WorkspaceRecord {
                    root_path: record.get("root_path")?,
                    name: record.get("name")?,
                    board_path: record.get("board_path")?,
                    created_at: record.get("created_at")?,
                    last_used_at: record.get("last_used_at")?,
                    archived: true,
                    archived_at: record.get("archived_at")?,
                    archived_by: record.get("archived_by")?,
                    archived_note: record.get("archived_note")?,
                    rootless: false,
                })
            },
        )
        .optional()?;
    Ok(latest)
}

fn board_archived(connection: &Connection, board_path: &str) -> Result<Option<bool>> {
    connection
        .query_row(
            "SELECT archived FROM boards WHERE board_path=?",
            [board_path],
            |row| Ok(row.get::<_, i64>(0)? != 0),
        )
        .optional()
        .map_err(Into::into)
}

fn active_named(
    connection: &Connection,
    name: &str,
    exclude_board_path: Option<&str>,
) -> Result<bool> {
    let mut sql = String::from("SELECT 1 FROM boards WHERE name=? AND archived=0");
    if exclude_board_path.is_some() {
        sql.push_str(" AND board_path<>?");
    }
    sql.push_str(" LIMIT 1");
    let found = if let Some(exclude_board_path) = exclude_board_path {
        connection
            .query_row(&sql, params![name, exclude_board_path], |_| Ok(()))
            .optional()?
            .is_some()
    } else {
        connection
            .query_row(&sql, [name], |_| Ok(()))
            .optional()?
            .is_some()
    };
    Ok(found)
}

fn target_looks_like_path(target: &str) -> bool {
    let path = Path::new(target);
    path.has_root()
        || target.contains(std::path::MAIN_SEPARATOR)
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir
                    | std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

fn read_only_busy_backoff(count: i32) -> bool {
    let pause = 1_u64 << (count.clamp(0, 7) as u32);
    std::thread::sleep(std::time::Duration::from_millis(pause));
    count < 8
}

fn open_staged_snapshot_readonly(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open staged source snapshot {}", path.display()))?;
    connection.busy_handler(Some(read_only_busy_backoff))?;
    connection.pragma_update(None, "query_only", true)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

fn pinned_regular_file(path: &Path, label: &str) -> Result<(File, fs::Metadata)> {
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if before.file_type().is_symlink() {
        bail!(
            "{label} {} is a symlink; pass the exact non-symlink path",
            path.display()
        );
    }
    if !before.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {label} {} without following links", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspect opened {label} {}", path.display()))?;
    if !opened.file_type().is_file() || before.dev() != opened.dev() || before.ino() != opened.ino()
    {
        bail!(
            "{label} {} changed identity while it was opened",
            path.display()
        );
    }
    Ok((file, opened))
}

fn canonical_regular_source_path(path: &Path) -> Result<(PathBuf, File, fs::Metadata)> {
    if path
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        bail!(
            "source board path {} contains parent traversal; pass the exact board path",
            path.display()
        );
    }
    let (file, metadata) = pinned_regular_file(path, "source board path")?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve source board {}", path.display()))?;
    let resolved = fs::symlink_metadata(&canonical)
        .with_context(|| format!("inspect resolved source board {}", canonical.display()))?;
    if !resolved.file_type().is_file()
        || resolved.dev() != metadata.dev()
        || resolved.ino() != metadata.ino()
    {
        bail!(
            "source board {} changed identity while it was resolved",
            path.display()
        );
    }
    Ok((canonical, file, metadata))
}

fn require_foreign_key_clean(connection: &Connection, label: &str) -> Result<()> {
    let violations = foreign_key_violations(connection)?;
    if !violations.is_empty() {
        bail!(
            "{label} has foreign key violations: {}",
            violations.join("; ")
        );
    }
    Ok(())
}

fn board_meta_value(connection: &Connection, key: &str) -> Result<Option<String>> {
    connection
        .query_row("SELECT value FROM board_meta WHERE key=?", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(Into::into)
}

fn exact_on(connection: &Connection, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
    let text = workspace.to_string_lossy();
    connection
        .query_row(
            "SELECT workspace_roots.root_path AS root_path,\
                    boards.name AS name,\
                    workspace_roots.board_path AS board_path,\
                    workspace_roots.created_at AS created_at,\
                    workspace_roots.last_used_at AS last_used_at,\
                    0 AS archived,\
                    NULL AS archived_at,\
                    NULL AS archived_by,\
                    NULL AS archived_note,\
                    0 AS rootless \
             FROM workspace_roots JOIN boards ON boards.board_path=workspace_roots.board_path \
             WHERE workspace_roots.root_path=? AND boards.archived=0",
            [text.as_ref()],
            row,
        )
        .optional()
        .map_err(Into::into)
}

fn enclosing_on(connection: &Connection, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
    let mut cursor = workspace.to_path_buf();
    while cursor.pop() {
        if let Some(found) = exact_on(connection, &cursor)? {
            return Ok(Some(found));
        }
        // A retired board must not resolve from an enclosing checkout, and it
        // must say so rather than reading as "no board here".
        if let Some(history) = latest_history_root(connection, &cursor)?
            && board_archived(connection, &history.board_path)?.unwrap_or(false)
        {
            bail!(
                "{} belongs to {}",
                cursor.display(),
                retired_board_message(
                    &history.name,
                    history.archived_note.as_deref(),
                    "addressing it"
                )
            );
        }
    }
    Ok(None)
}

fn database_artifact_path(path: &Path, suffix: &str) -> PathBuf {
    let mut artifact = path.as_os_str().to_owned();
    artifact.push(suffix);
    PathBuf::from(artifact)
}

fn adoption_marker_path(root: &Path) -> PathBuf {
    root.join(".workspace-adopt.json")
}

fn adoption_staging_root(root: &Path) -> PathBuf {
    root.join(".workspace-adopt-staging")
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdoptionMarker {
    board_name: String,
    board_path: String,
    root_path: Option<String>,
    source_board_path: String,
    staging_dir: String,
    created_at: i64,
}

fn write_adoption_marker(path: &Path, marker: &AdoptionMarker) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create adoption marker {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, marker)
        .with_context(|| format!("write adoption marker {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync adoption marker {}", path.display()))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn read_adoption_marker(path: &Path) -> Result<AdoptionMarker> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read adoption marker {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse adoption marker {}", path.display()))
}

fn adoption_path_within_root(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

fn cleanup_database_artifacts(path: &Path) -> Result<()> {
    let mut failures = Vec::new();
    for artifact in [
        path.to_path_buf(),
        database_artifact_path(path, "-wal"),
        database_artifact_path(path, "-shm"),
        database_artifact_path(path, "-journal"),
    ] {
        match fs::remove_file(&artifact) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}: {error}", artifact.display())),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "failed to remove adoption artifacts; retained evidence: {}",
            failures.join("; ")
        )
    }
}

fn combine_cleanup<T>(result: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => {
            let cleanup_text = format!("{cleanup:#}");
            Err(cleanup.context(format!("adoption cleanup failed: {cleanup_text}")))
        }
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("adoption cleanup also failed: {cleanup:#}")))
        }
    }
}

fn metadata_identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn metadata_stable(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    metadata_identity(before) == metadata_identity(after)
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
}

fn copy_pinned_prefix(source: &mut File, target: &Path, length: u64) -> Result<()> {
    source.seek(SeekFrom::Start(0))?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)
        .with_context(|| format!("create private snapshot component {}", target.display()))?;
    let mut limited = source.take(length);
    let copied = std::io::copy(&mut limited, &mut target_file)?;
    if copied != length {
        bail!(
            "source component changed length while capturing it: expected {length} bytes, copied {copied}"
        );
    }
    target_file.sync_all()?;
    Ok(())
}

fn create_staging_dir() -> Result<PathBuf> {
    for _ in 0..8 {
        let path = env::temp_dir().join(format!("kanban-adopt-{}", Uuid::new_v4()));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create adoption staging directory {}", path.display())
                });
            }
        }
    }
    bail!("could not allocate a unique adoption staging directory")
}

fn cleanup_staging_dir(path: &Path) -> Result<()> {
    let mut failures = Vec::new();
    match fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => match fs::remove_file(entry.path()) {
                        Ok(()) => {}
                        Err(error) => failures.push(format!("{}: {error}", entry.path().display())),
                    },
                    Err(error) => failures.push(format!("read {}: {error}", path.display())),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => failures.push(format!("read {}: {error}", path.display())),
    }
    if failures.is_empty()
        && let Err(error) = fs::remove_dir(path)
    {
        failures.push(format!("{}: {error}", path.display()));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "failed to clean adoption staging; retained evidence: {}",
            failures.join("; ")
        )
    }
}

fn remove_optional_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn adoption_marker_matches_root(root: &Path, marker: &AdoptionMarker) -> bool {
    let boards_root = root.join("boards");
    let adoption_root = adoption_staging_root(root);
    let Some(board_parent) = Path::new(&marker.board_path).parent() else {
        return false;
    };
    adoption_path_within_root(&boards_root, board_parent)
        && adoption_path_within_root(&adoption_root, Path::new(&marker.staging_dir))
}

fn reconcile_pending_adoption(root: &Path) -> Result<()> {
    let marker_path = adoption_marker_path(root);
    if !marker_path.exists() {
        return Ok(());
    }

    let marker = read_adoption_marker(&marker_path)?;
    if !adoption_marker_matches_root(root, &marker) {
        bail!(
            "adoption marker {} points outside registry-owned storage",
            marker_path.display()
        );
    }

    let board_path = PathBuf::from(&marker.board_path);
    let staging_dir = PathBuf::from(&marker.staging_dir);
    let registry_path = root.join("registry.db");
    let committed = if registry_path.exists() {
        let registry = open_registry_readonly(&registry_path).with_context(|| {
            format!(
                "reopen registry for adoption recovery {}",
                registry_path.display()
            )
        })?;
        registry
            .query_row(
                "SELECT 1 FROM boards WHERE board_path=? LIMIT 1",
                [board_path.to_string_lossy().as_ref()],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
    } else {
        false
    };

    if !committed {
        cleanup_database_artifacts(&board_path)?;
    }
    cleanup_staging_dir(&staging_dir)?;
    remove_optional_file(&marker_path)?;
    if let Some(parent) = staging_dir.parent()
        && parent.exists()
        && fs::read_dir(parent)
            .with_context(|| format!("read adoption staging root {}", parent.display()))?
            .next()
            .is_none()
    {
        let _ = fs::remove_dir(parent);
    }
    sync_directory(root)?;
    Ok(())
}

struct AdoptionRun {
    root: PathBuf,
    marker_path: PathBuf,
    staging_dir: PathBuf,
    adoption_root: PathBuf,
    board_path: PathBuf,
}

impl AdoptionRun {
    fn start(
        root: &Path,
        board_name: &str,
        board_path: &Path,
        source_board_path: &Path,
        root_path: Option<String>,
    ) -> Result<Self> {
        let root = root.to_path_buf();
        let marker_path = adoption_marker_path(&root);
        let adoption_root = adoption_staging_root(&root);
        let staging_dir = adoption_root.join(Uuid::new_v4().to_string());
        let result = (|| {
            create_private_dir_all(&adoption_root)?;
            fs::DirBuilder::new().mode(0o700).create(&staging_dir)?;
            let marker = AdoptionMarker {
                board_name: board_name.to_owned(),
                board_path: board_path.to_string_lossy().into_owned(),
                root_path,
                source_board_path: source_board_path.to_string_lossy().into_owned(),
                staging_dir: staging_dir.to_string_lossy().into_owned(),
                created_at: now_ms(),
            };
            write_adoption_marker(&marker_path, &marker)?;
            sync_directory(&adoption_root)?;
            sync_directory(&root)?;
            Ok(Self {
                root: root.clone(),
                marker_path: marker_path.clone(),
                staging_dir: staging_dir.clone(),
                adoption_root: adoption_root.clone(),
                board_path: board_path.to_path_buf(),
            })
        })();
        match result {
            Ok(run) => Ok(run),
            Err(error) => {
                let cleanup = cleanup_staging_dir(&staging_dir)
                    .and_then(|_| remove_optional_file(&marker_path))
                    .and_then(|_| sync_directory(&root));
                combine_cleanup(Err(error), cleanup)
            }
        }
    }

    fn cleanup_after_success(&self) -> Result<()> {
        (|| {
            cleanup_staging_dir(&self.staging_dir)?;
            remove_optional_file(&self.marker_path)?;
            if self.adoption_root.exists()
                && fs::read_dir(&self.adoption_root)
                    .with_context(|| {
                        format!(
                            "read adoption staging root {}",
                            self.adoption_root.display()
                        )
                    })?
                    .next()
                    .is_none()
            {
                let _ = fs::remove_dir(&self.adoption_root);
            }
            sync_directory(&self.root)?;
            Ok(())
        })()
    }

    fn cleanup_after_failure(&self, published: bool) -> Result<()> {
        (|| {
            if published {
                cleanup_database_artifacts(&self.board_path)?;
            }
            cleanup_staging_dir(&self.staging_dir)?;
            remove_optional_file(&self.marker_path)?;
            if self.adoption_root.exists()
                && fs::read_dir(&self.adoption_root)
                    .with_context(|| {
                        format!(
                            "read adoption staging root {}",
                            self.adoption_root.display()
                        )
                    })?
                    .next()
                    .is_none()
            {
                let _ = fs::remove_dir(&self.adoption_root);
            }
            sync_directory(&self.root)?;
            Ok(())
        })()
    }
}

fn hash_file(file: &mut File) -> Result<(String, u64)> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        bail!("adopted snapshot handle is not a regular file");
    }
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    if bytes != metadata.len() {
        bail!("adopted snapshot changed length while hashing it");
    }
    Ok((format!("{:x}", digest.finalize()), bytes))
}

fn c_name(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")
}

fn open_dir_at(parent: &File, name: &Path, create: bool) -> Result<File> {
    let name = c_name(name)?;
    let mut fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 && create && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        let made = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if made < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
            return Err(std::io::Error::last_os_error())
                .context("create registry directory component");
        }
        fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("registry path component is a symlink or non-directory");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn secure_registry_dirs(root: &Path, create: bool) -> Result<(File, File)> {
    if root
        .components()
        .any(|component| component == Component::ParentDir)
    {
        bail!(
            "registry data root {} contains parent traversal",
            root.display()
        );
    }
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        env::current_dir()?.join(root)
    };
    if let Ok(metadata) = fs::symlink_metadata(&absolute)
        && metadata.file_type().is_symlink()
    {
        bail!("registry data root {} is a symlink", root.display());
    }
    // macOS publishes /var as a stable operating-system alias. Normalize that
    // one platform path before the no-follow walk; no caller-controlled
    // component is canonicalized or followed.
    #[cfg(target_os = "macos")]
    let secure_absolute = absolute
        .strip_prefix("/var")
        .map(|rest| Path::new("/private/var").join(rest))
        .unwrap_or(absolute);
    #[cfg(not(target_os = "macos"))]
    let secure_absolute = absolute;
    let mut current = File::open("/").context("open filesystem root")?;
    for component in secure_absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                current = open_dir_at(&current, Path::new(name), create).with_context(|| {
                    format!("open registry path component {}", name.to_string_lossy())
                })?
            }
            Component::ParentDir | Component::Prefix(_) => {
                bail!("unsupported registry data root {}", root.display())
            }
        }
    }
    let mode = current.metadata()?.permissions().mode();
    if mode & 0o077 != 0 {
        let rc = unsafe { libc::fchmod(current.as_raw_fd(), 0o700) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("secure registry data root");
        }
    }
    let boards = open_dir_at(&current, Path::new("boards"), create)
        .context("open registry boards directory")?;
    Ok((current, boards))
}

fn verify_registry_chain(root: &Path, pinned_boards: &File) -> Result<()> {
    let (_root, current_boards) = secure_registry_dirs(root, false)?;
    if metadata_identity(&current_boards.metadata()?)
        != metadata_identity(&pinned_boards.metadata()?)
    {
        bail!(
            "registry boards directory {} changed identity",
            root.join("boards").display()
        );
    }
    Ok(())
}

fn verify_pinned_dir(path: &Path, pinned: &File, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    let opened = pinned.metadata()?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || metadata_identity(&metadata) != metadata_identity(&opened)
    {
        bail!(
            "{label} {} changed identity or became a symlink",
            path.display()
        );
    }
    Ok(())
}

fn rename_into_dir(
    source_dir: &File,
    source_name: &Path,
    destination_dir: &File,
    destination_name: &Path,
) -> Result<()> {
    let source_name = c_name(source_name)?;
    let destination_name = c_name(destination_name)?;
    let rc = unsafe {
        libc::renameat(
            source_dir.as_raw_fd(),
            source_name.as_ptr(),
            destination_dir.as_raw_fd(),
            destination_name.as_ptr(),
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("atomically publish adopted board");
    }
    Ok(())
}

pub(crate) struct PreparedAdoption {
    source_board_path: PathBuf,
    staging_dir: PathBuf,
    snapshot: Option<Connection>,
    name: String,
    cleanup_attempted: bool,
}

impl PreparedAdoption {
    pub(crate) fn prepare(source_path: &Path, name: &str) -> Result<Self> {
        Self::prepare_with_hook(source_path, name, || Ok(()))
    }

    fn prepare_with_hook<Hook: FnOnce() -> Result<()>>(
        source_path: &Path,
        name: &str,
        after_capture: Hook,
    ) -> Result<Self> {
        let staging_dir = create_staging_dir()?;
        let result = (|| {
            let (source_board_path, mut source_file, source_metadata) =
                canonical_regular_source_path(source_path)?;
            let captured_path = staging_dir.join("captured.db");
            copy_pinned_prefix(&mut source_file, &captured_path, source_metadata.len())?;

            let wal_path = database_artifact_path(&source_board_path, "-wal");
            let wal_capture = match pinned_regular_file(&wal_path, "source WAL") {
                Ok((mut wal, before)) => {
                    let mut header_before = [0_u8; 32];
                    if before.len() >= 32 {
                        wal.read_exact(&mut header_before)?;
                    }
                    copy_pinned_prefix(
                        &mut wal,
                        &database_artifact_path(&captured_path, "-wal"),
                        before.len(),
                    )?;
                    let after = wal.metadata()?;
                    let mut header_after = [0_u8; 32];
                    wal.seek(SeekFrom::Start(0))?;
                    if after.len() >= 32 {
                        wal.read_exact(&mut header_after)?;
                    }
                    Some((before, after, header_before, header_after))
                }
                Err(error)
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                    }) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            let source_after = source_file.metadata()?;
            let source_path_after =
                fs::symlink_metadata(&source_board_path).with_context(|| {
                    format!("recheck source identity {}", source_board_path.display())
                })?;
            if !metadata_stable(&source_metadata, &source_after)
                || !source_path_after.file_type().is_file()
                || metadata_identity(&source_path_after) != metadata_identity(&source_after)
            {
                bail!(
                    "source board changed while its consistent snapshot was captured; retry adoption"
                );
            }
            if let Some((before, after, header_before, header_after)) = &wal_capture
                && (metadata_identity(before) != metadata_identity(after)
                    || after.len() < before.len()
                    || header_before != header_after)
            {
                bail!(
                    "source WAL was reset while its consistent snapshot was captured; retry adoption"
                );
            }
            if let Some((before, _, _, _)) = &wal_capture {
                let wal_path_after = fs::symlink_metadata(&wal_path).with_context(|| {
                    format!("recheck source WAL identity {}", wal_path.display())
                })?;
                if !wal_path_after.file_type().is_file()
                    || metadata_identity(&wal_path_after) != metadata_identity(before)
                {
                    bail!(
                        "source WAL changed identity while its snapshot was captured; retry adoption"
                    );
                }
            }

            after_capture()?;
            let captured = open_staged_snapshot_readonly(&captured_path)?;
            let snapshot_path = staging_dir.join("snapshot.db");
            let snapshot = read_snapshot(&captured, |source| {
                let integrity_rows = integrity(source)?;
                if integrity_rows.as_slice() != ["ok"] {
                    bail!(
                        "source board {} failed integrity check: {}",
                        source_board_path.display(),
                        integrity_rows.join(", ")
                    );
                }
                require_foreign_key_clean(source, "source board")?;
                let source_schema = crate::db::schema_version(source)?;
                if source_schema > crate::db::BOARD_SCHEMA_VERSION {
                    bail!(
                        "source board {} is schema version {source_schema}, newer than supported version {}",
                        source_board_path.display(),
                        crate::db::BOARD_SCHEMA_VERSION
                    );
                }
                let audit = crate::audit::verify_board(source)?;
                if !audit.healthy {
                    bail!(
                        "source board {} has an invalid audit chain: {}",
                        source_board_path.display(),
                        audit.errors.join("; ")
                    );
                }
                let source_name = board_meta_value(source, "name")?
                    .context("source board name metadata is missing")?;
                if source_name != name {
                    bail!("source board name is {source_name}, not {name}");
                }
                let mut target = create_backup_target(&snapshot_path)?;
                let backup = rusqlite::backup::Backup::new(source, &mut target)?;
                backup.run_to_completion(64, std::time::Duration::from_millis(1), None)?;
                drop(backup);
                validate_board_snapshot(&target, "copied source snapshot", name, source_schema)?;
                Ok(target)
            })?;
            cleanup_database_artifacts(&captured_path)?;
            Ok(Self {
                source_board_path,
                staging_dir: staging_dir.clone(),
                snapshot: Some(snapshot),
                name: name.to_owned(),
                cleanup_attempted: false,
            })
        })();
        match result {
            Ok(prepared) => Ok(prepared),
            Err(error) => combine_cleanup(Err(error), cleanup_staging_dir(&staging_dir)),
        }
    }

    pub(crate) fn cleanup(mut self) -> Result<()> {
        if self.cleanup_attempted {
            return Ok(());
        }
        self.cleanup_attempted = true;
        drop(self.snapshot.take());
        cleanup_staging_dir(&self.staging_dir)
    }

    pub(crate) fn abort(self, error: anyhow::Error) -> anyhow::Error {
        match self.cleanup() {
            Ok(()) => error,
            Err(cleanup) => error.context(format!("adoption cleanup also failed: {cleanup:#}")),
        }
    }
}

impl Drop for PreparedAdoption {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            drop(self.snapshot.take());
            let _ = cleanup_staging_dir(&self.staging_dir);
        }
    }
}

fn duplicate_workspace_adopt_fd(source_fd: i32) -> std::io::Result<i32> {
    let fd = unsafe {
        libc::fcntl(
            source_fd,
            libc::F_DUPFD_CLOEXEC,
            WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD + 1,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

fn clear_workspace_adopt_cloexec(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let cleared = flags & !libc::FD_CLOEXEC;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, cleared) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn remap_workspace_adopt_fd(source_fd: i32, target_fd: i32) -> std::io::Result<()> {
    if source_fd == target_fd {
        return clear_workspace_adopt_cloexec(target_fd);
    }
    if unsafe { libc::dup2(source_fd, target_fd) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    clear_workspace_adopt_cloexec(target_fd)
}

fn install_workspace_adopt_fds(root_fd: i32, snapshot_fd: i32) -> std::io::Result<()> {
    let remapped_root = duplicate_workspace_adopt_fd(root_fd)?;
    let remapped_snapshot = duplicate_workspace_adopt_fd(snapshot_fd)?;
    let result = (|| {
        remap_workspace_adopt_fd(remapped_root, WORKSPACE_ADOPT_HELPER_ROOT_FD)?;
        remap_workspace_adopt_fd(remapped_snapshot, WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD)?;
        Ok(())
    })();
    unsafe {
        libc::close(remapped_root);
        libc::close(remapped_snapshot);
    }
    result
}

#[cfg(debug_assertions)]
fn workspace_adopt_test_hook(phase: &str) -> Result<()> {
    if env::var("KANBAN_TEST_WORKSPACE_ADOPT_HOOK").ok().as_deref() == Some(phase) {
        loop {
            if unsafe { libc::getppid() } == 1 {
                bail!("workspace adoption parent exited during debug pause");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn workspace_adopt_test_hook(_phase: &str) -> Result<()> {
    Ok(())
}

fn workspace_adopt_snapshot_uri() -> String {
    format!("file:/dev/fd/{WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD}?mode=ro&immutable=1")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceAdoptHelperRequest {
    name: String,
    workspace: Option<String>,
    rootless: bool,
    actor: String,
    source_board_path: String,
}

pub(crate) fn spawn_workspace_adopt_helper(
    prepared: &PreparedAdoption,
    workspace: Option<&Path>,
    rootless: bool,
    actor: &str,
) -> Result<WorkspaceAdoptReceipt> {
    let PinnedAdoptionRoot {
        root: _root,
        root_dir,
        boards: _boards,
    } = pin_live_root_for_adoption()?;
    let snapshot_path = prepared.staging_dir.join("snapshot.db");
    let snapshot = File::open(&snapshot_path)
        .with_context(|| format!("open staged snapshot {}", snapshot_path.display()))?;
    let request = WorkspaceAdoptHelperRequest {
        name: prepared.name.clone(),
        workspace: workspace.map(|path| path.to_string_lossy().into_owned()),
        rootless,
        actor: actor.to_owned(),
        source_board_path: prepared.source_board_path.to_string_lossy().into_owned(),
    };
    let root_fd = root_dir.as_raw_fd();
    let snapshot_fd = snapshot.as_raw_fd();
    let mut command = Command::new(env::current_exe().context("resolve kanban executable")?);
    unsafe {
        command
            .arg(WORKSPACE_ADOPT_HELPER_COMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .pre_exec(move || {
                install_workspace_adopt_fds(root_fd, snapshot_fd)?;
                if libc::fchdir(WORKSPACE_ADOPT_HELPER_ROOT_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
    }
    let mut child = command.spawn().context("spawn workspace adoption helper")?;
    let mut stdin = child
        .stdin
        .take()
        .context("capture workspace adoption helper stdin")?;
    serde_json::to_writer(&mut stdin, &request).context("write workspace adoption request")?;
    stdin.write_all(b"\n")?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .context("wait for workspace adoption helper")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("workspace adoption helper failed: {stderr}");
    }
    serde_json::from_slice(&output.stdout).context("decode workspace adoption receipt")
}

pub(crate) fn run_workspace_adopt_helper() -> Result<()> {
    let request: WorkspaceAdoptHelperRequest = serde_json::from_reader(std::io::stdin().lock())
        .context("read workspace adoption request")?;
    if unsafe { libc::fchdir(WORKSPACE_ADOPT_HELPER_ROOT_FD) } < 0 {
        return Err(std::io::Error::last_os_error()).context("fchdir workspace adoption root");
    }
    let snapshot_uri = workspace_adopt_snapshot_uri();
    let snapshot = Connection::open_with_flags(
        &snapshot_uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open staged snapshot {}", snapshot_uri))?;
    snapshot.busy_handler(Some(read_only_busy_backoff))?;
    snapshot.pragma_update(None, "query_only", true)?;
    snapshot.pragma_update(None, "foreign_keys", true)?;
    let mut registry = Registry::open_for_adoption()?;
    let receipt = registry.adopt_snapshot(
        &snapshot,
        &PathBuf::from(request.source_board_path),
        &request.name,
        request.workspace.as_deref().map(Path::new),
        request.rootless,
        &request.actor,
    )?;
    serde_json::to_writer(std::io::stdout().lock(), &receipt)
        .context("write workspace adoption receipt")?;
    std::io::stdout().lock().write_all(b"\n")?;
    Ok(())
}

fn validate_board_snapshot(
    connection: &Connection,
    label: &str,
    name: &str,
    expected_schema: usize,
) -> Result<()> {
    let integrity_rows = integrity(connection)?;
    if integrity_rows.as_slice() != ["ok"] {
        bail!(
            "{label} failed integrity check: {}",
            integrity_rows.join(", ")
        );
    }
    require_foreign_key_clean(connection, label)?;
    let schema = crate::db::schema_version(connection)?;
    if schema != expected_schema {
        bail!("{label} is schema version {schema}, expected {expected_schema}");
    }
    let actual_name = board_meta_value(connection, "name")?
        .context(format!("{label} name metadata is missing"))?;
    if actual_name != name {
        bail!("{label} name is {actual_name}, not {name}");
    }
    let audit = crate::audit::verify_board(connection)?;
    if !audit.healthy {
        bail!(
            "{label} has an invalid audit chain: {}",
            audit.errors.join("; ")
        );
    }
    Ok(())
}

pub struct Registry {
    pub connection: Connection,
    root: PathBuf,
    adoption_boards: Option<File>,
}

impl SnapshotSource for Registry {
    fn snapshot_connection(&self) -> &Connection {
        &self.connection
    }
}

impl Registry {
    pub fn open() -> Result<Self> {
        let root = data_root()?;
        own_private_dir(&root)?;
        reconcile_pending_adoption(&root)?;
        let connection = open_registry(&root.join("registry.db"))?;
        Ok(Self {
            connection,
            root,
            adoption_boards: None,
        })
    }

    pub(crate) fn open_for_adoption() -> Result<Self> {
        let root = data_root()?;
        let (root_dir, boards) = secure_registry_dirs(&root, true)?;
        verify_pinned_dir(&root, &root_dir, "registry data root")?;
        verify_registry_chain(&root, &boards)?;
        reconcile_pending_adoption(&root)?;
        let connection = open_registry(&root.join("registry.db"))?;
        verify_pinned_dir(&root, &root_dir, "registry data root")?;
        verify_registry_chain(&root, &boards)?;
        Ok(Self {
            connection,
            root,
            adoption_boards: Some(boards),
        })
    }

    pub fn open_readonly() -> Result<Self> {
        Self::open_readonly_at(&data_root()?)
    }

    /// Open a named registry root read-only, without consulting the environment.
    ///
    /// `open_readonly` re-resolves `data_root()` on every call, which is wrong
    /// for anything that polls: a watcher would re-read the environment four
    /// times a second and could silently repoint mid-stream. Callers that
    /// resolved their root once should keep using the root they resolved.
    pub(crate) fn open_readonly_at(root: &Path) -> Result<Self> {
        let connection = open_registry_readonly(&root.join("registry.db"))?;
        Ok(Self {
            connection,
            root: root.to_path_buf(),
            adoption_boards: None,
        })
    }

    pub(crate) fn board_path_state(&self, path: &Path) -> Result<BoardPathState> {
        let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut statement = self.connection.prepare(
            "SELECT board_path,name,archived,archived_note FROM boards ORDER BY board_path",
        )?;
        for row in statement.query_map([], |record| {
            Ok((
                record.get::<_, String>(0)?,
                record.get::<_, String>(1)?,
                record.get::<_, i64>(2)?,
                record.get::<_, Option<String>>(3)?,
            ))
        })? {
            let (board_path, name, archived, archived_note) = row?;
            let project_path = Path::new(&board_path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&board_path));
            if project_path == resolved {
                if archived != 0 {
                    return Ok(BoardPathState::Retired {
                        name,
                        note: archived_note,
                    });
                }
                return Ok(BoardPathState::Active(name));
            }
        }
        Ok(BoardPathState::External)
    }

    pub(crate) fn board_path_state_if_available(path: &Path) -> Result<Option<BoardPathState>> {
        let root = data_root()?;
        let registry_path = root.join("registry.db");
        if !registry_path.exists() {
            return Ok(None);
        }
        Ok(Some(
            Registry::open_readonly_at(&root)?.board_path_state(path)?,
        ))
    }

    fn project_for_board_path_internal(
        &self,
        board_path: &str,
        include_archived: bool,
    ) -> Result<ProjectRecord> {
        let (
            name,
            board_path,
            last_used_at,
            archived_at,
            archived_by,
            archived_note,
            retirement_id,
        ): BoardRow = if include_archived {
            self.connection.query_row(
                "SELECT name,board_path,last_used_at,archived_at,archived_by,archived_note,retirement_id \
                 FROM boards WHERE board_path=?",
                [board_path],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?
        } else {
            self.connection.query_row(
                "SELECT name,board_path,last_used_at,NULL AS archived_at,NULL AS archived_by,NULL AS archived_note,NULL AS retirement_id \
                 FROM boards WHERE board_path=? AND archived=0",
                [board_path],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?
        };
        let workspace_roots = board_roots(&self.connection, &board_path, retirement_id.as_deref())?;
        Ok(ProjectRecord {
            name,
            board_path,
            workspace_roots,
            last_used_at,
            archived: archived_at.is_some(),
            archived_at,
            archived_by,
            archived_note,
        })
    }

    fn projects_by_name(
        connection: &Connection,
        name: &str,
        include_archived: bool,
    ) -> Result<Vec<ProjectRecord>> {
        let mut sql = String::from(
            "SELECT board_path,name,last_used_at,archived_at,archived_by,archived_note,retirement_id FROM boards WHERE name=?",
        );
        if !include_archived {
            sql.push_str(" AND archived=0");
        }
        sql.push_str(" ORDER BY board_path");
        let mut statement = connection.prepare(&sql)?;
        let boards = statement
            .query_map([name], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut projects = Vec::with_capacity(boards.len());
        for (
            board_path,
            name,
            last_used_at,
            archived_at,
            archived_by,
            archived_note,
            retirement_id,
        ) in boards
        {
            let workspace_roots = board_roots(connection, &board_path, retirement_id.as_deref())?;
            projects.push(ProjectRecord {
                name,
                board_path,
                workspace_roots,
                last_used_at,
                archived: archived_at.is_some(),
                archived_at,
                archived_by,
                archived_note,
            });
        }
        projects.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        Ok(projects)
    }

    fn project_for_board_path(&self, board_path: &str) -> Result<ProjectRecord> {
        self.project_for_board_path_internal(board_path, false)
    }

    fn project_for_board_path_all(&self, board_path: &str) -> Result<ProjectRecord> {
        self.project_for_board_path_internal(board_path, true)
    }

    pub fn register(
        &mut self,
        workspace: Option<&Path>,
        name: &str,
        force: bool,
        actor: &str,
    ) -> Result<ProjectRecord> {
        let actor = validate_rule_actor(actor)?;
        let now = now_ms();
        let root_path = workspace
            .map(|workspace| {
                workspace
                    .canonicalize()
                    .with_context(|| format!("resolve workspace {}", workspace.display()))
            })
            .transpose()?;
        if let Some(root_path) = &root_path {
            // An init below a registered root used to create a second board
            // that shadowed the first: tasks added from the subdirectory
            // resolved to the nearer board and were invisible from the project
            // root. Attaching is almost always what was meant; nesting has to
            // be asked for.
            if !force
                && self.exact(root_path)?.is_none()
                && let Some(enclosing) = self.enclosing(root_path)?
            {
                bail!(
                    "{} is already inside Kanban project {}.\n\
                     To share that board:        kanban workspace attach --to {}\n\
                     To create a separate board: kanban init --name {name} --force",
                    root_path.display(),
                    enclosing.name,
                    enclosing.name
                );
            }
            if let Some(existing) = self.exact(root_path)? {
                let project = self.project_for_board_path(&existing.board_path)?;
                if project.name != name {
                    bail!(
                        "{} is already registered to board {}; `init` does not rename boards",
                        root_path.display(),
                        project.name
                    );
                }
                let transaction = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute(
                    "UPDATE boards SET last_used_at=? WHERE board_path=?",
                    params![now, existing.board_path],
                )?;
                transaction.execute(
                    "UPDATE workspace_roots SET last_used_at=? WHERE root_path=?",
                    params![now, existing.root_path],
                )?;
                crate::audit::append_registry_event(
                    &transaction,
                    &format!("workspace:{}", existing.root_path),
                    "workspace_registered",
                    actor,
                    &json!({
                        "rootPath": existing.root_path,
                        "name": project.name,
                        "boardPath": existing.board_path,
                        "existing": true,
                    })
                    .to_string(),
                    now,
                )?;
                transaction.commit()?;
                return self.project_for_board_path(&existing.board_path);
            }
        }
        let board_path = self
            .root
            .join("boards")
            .join(format!("{}.db", Uuid::new_v4()));
        let root_path_json = root_path
            .as_ref()
            .map(|root_path| root_path.to_string_lossy().to_string());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if active_named(&transaction, name, None)? {
            bail!("a Kanban board is already named {name}");
        }
        transaction.execute(
            "INSERT INTO boards(board_path,name,created_at,last_used_at) VALUES(?,?,?,?)",
            params![board_path.to_string_lossy(), name, now, now],
        )?;
        if let Some(root_path) = root_path {
            transaction.execute(
                "INSERT INTO workspace_roots(root_path,board_path,created_at,last_used_at) VALUES(?,?,?,?)",
                params![root_path.to_string_lossy(), board_path.to_string_lossy(), now, now],
            )?;
        }
        crate::audit::append_registry_event(
            &transaction,
            &format!("workspace:{}", board_path.to_string_lossy()),
            "workspace_registered",
            actor,
            &json!({
                "boardPath": board_path,
                "name": name,
                "rootPath": root_path_json,
            })
            .to_string(),
            now,
        )?;
        transaction.commit()?;
        self.project_for_board_path(&board_path.to_string_lossy())
    }

    #[allow(dead_code)]
    pub fn adopt(
        &mut self,
        source_path: &Path,
        name: &str,
        workspace: Option<&Path>,
        rootless: bool,
        actor: &str,
    ) -> Result<WorkspaceAdoptReceipt> {
        let prepared = PreparedAdoption::prepare(source_path, name)?;
        self.adopt_prepared(prepared, workspace, rootless, actor)
    }

    pub(crate) fn adopt_prepared(
        &mut self,
        mut prepared: PreparedAdoption,
        workspace: Option<&Path>,
        rootless: bool,
        actor: &str,
    ) -> Result<WorkspaceAdoptReceipt> {
        let result = self.adopt_prepared_inner(&mut prepared, workspace, rootless, actor);
        let cleanup = prepared.cleanup();
        combine_cleanup(result, cleanup)
    }

    fn adopt_prepared_inner(
        &mut self,
        prepared: &mut PreparedAdoption,
        workspace: Option<&Path>,
        rootless: bool,
        actor: &str,
    ) -> Result<WorkspaceAdoptReceipt> {
        let snapshot = prepared
            .snapshot
            .as_ref()
            .context("prepared source snapshot connection is unavailable")?;
        self.adopt_snapshot(
            snapshot,
            &prepared.source_board_path,
            &prepared.name,
            workspace,
            rootless,
            actor,
        )
    }

    fn adopt_snapshot(
        &mut self,
        snapshot: &Connection,
        source_board_path: &Path,
        name: &str,
        workspace: Option<&Path>,
        rootless: bool,
        actor: &str,
    ) -> Result<WorkspaceAdoptReceipt> {
        let actor = validate_rule_actor(actor)?;
        if rootless && workspace.is_some() {
            bail!("--rootless cannot be combined with --workspace");
        }
        if !rootless && workspace.is_none() {
            bail!("workspace adopt requires either --workspace PATH or --rootless");
        }
        let root_path = workspace
            .map(|workspace| {
                workspace
                    .canonicalize()
                    .with_context(|| format!("resolve workspace {}", workspace.display()))
            })
            .transpose()?;
        let root_path_json = root_path
            .as_ref()
            .map(|root_path| root_path.to_string_lossy().to_string());
        let board_name = format!("{}.db", Uuid::new_v4());
        let board_path = self.root.join("boards").join(&board_name);

        let (_root_dir, fallback_boards);
        let boards = if let Some(boards) = &self.adoption_boards {
            boards
        } else {
            (_root_dir, fallback_boards) = secure_registry_dirs(&self.root, true)?;
            &fallback_boards
        };
        verify_registry_chain(&self.root, boards)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let adoption_run = AdoptionRun::start(
            &self.root,
            &board_name,
            &board_path,
            source_board_path,
            root_path_json.clone(),
        )?;
        let mut published = false;
        let mut committed = false;
        let result = (|| {
            workspace_adopt_test_hook("after_marker")?;
            if let Some(root) = &root_path {
                if let Some(existing) = exact_on(&transaction, root)? {
                    bail!(
                        "{} is already registered to board {}",
                        root.display(),
                        existing.name
                    );
                }
                if let Some(enclosing) = enclosing_on(&transaction, root)? {
                    bail!(
                        "{} is already inside Kanban project {}.\n\
                         To share that board:        kanban workspace attach --to {}\n\
                         To create a separate board: use a different --workspace, or pass --rootless",
                        root.display(),
                        enclosing.name,
                        enclosing.name,
                    );
                }
            }
            if active_named(&transaction, name, None)? {
                bail!("a Kanban board is already named {name}");
            }

            let adopted_path = adoption_run.staging_dir.join("adopted.db");
            let mut destination = create_backup_target(&adopted_path)?;
            let mut adopted_file = pinned_regular_file(&adopted_path, "staged adopted board")?.0;
            let backup = rusqlite::backup::Backup::new(snapshot, &mut destination)?;
            backup.run_to_completion(64, std::time::Duration::from_millis(1), None)?;
            drop(backup);
            finalize_adopted_board(&mut destination)?;
            validate_board_snapshot(
                &destination,
                "adopted board",
                name,
                crate::db::BOARD_SCHEMA_VERSION,
            )?;
            checkpoint(&destination)?;
            adopted_file
                .sync_all()
                .context("sync staged adopted board")?;
            let staged_after = fs::symlink_metadata(&adopted_path)?;
            if staged_after.file_type().is_symlink()
                || metadata_identity(&staged_after) != metadata_identity(&adopted_file.metadata()?)
            {
                bail!("staged adopted board changed identity during migration or validation");
            }
            let (source_sha256, source_bytes) = hash_file(&mut adopted_file)?;
            drop(adopted_file);
            drop(destination);

            // Remove sidecars explicitly while retaining the main database for
            // the directory-relative atomic publish below.
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = database_artifact_path(&adopted_path, suffix);
                match fs::remove_file(&sidecar) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("remove staged SQLite sidecar {}", sidecar.display())
                        });
                    }
                }
            }
            let staging_dir = File::open(&adoption_run.staging_dir)?;
            verify_registry_chain(&self.root, boards)?;
            rename_into_dir(
                &staging_dir,
                Path::new("adopted.db"),
                boards,
                Path::new(&board_name),
            )?;
            published = true;
            workspace_adopt_test_hook("after_publish")?;
            boards
                .sync_all()
                .context("sync registry boards directory")?;
            sync_directory(&adoption_run.adoption_root)?;

            let now = now_ms();
            transaction.execute(
                "INSERT INTO boards(board_path,name,created_at,last_used_at) VALUES(?,?,?,?)",
                params![board_path.to_string_lossy(), name, now, now],
            )?;
            if let Some(root_path) = &root_path {
                transaction.execute(
                    "INSERT INTO workspace_roots(root_path,board_path,created_at,last_used_at) VALUES(?,?,?,?)",
                    params![root_path.to_string_lossy(), board_path.to_string_lossy(), now, now],
                )?;
            }
            crate::audit::append_registry_event(
                &transaction,
                &format!("workspace:{}", board_path.to_string_lossy()),
                "board_adopted",
                actor,
                &json!({
                    "boardPath": board_path.to_string_lossy().to_string(),
                    "name": name,
                    "rootPath": root_path_json.clone(),
                    "sourceBoardPath": source_board_path.to_string_lossy().to_string(),
                    "sourceSha256": source_sha256,
                    "sourceBytes": source_bytes,
                })
                .to_string(),
                now,
            )?;
            verify_registry_chain(&self.root, boards)?;
            transaction.commit()?;
            committed = true;
            adoption_run.cleanup_after_success()?;
            Ok(WorkspaceAdoptReceipt {
                project: ProjectRecord {
                    name: name.to_owned(),
                    board_path: board_path.to_string_lossy().into_owned(),
                    workspace_roots: root_path_json.clone().into_iter().collect(),
                    last_used_at: now,
                    // A board that has just been adopted is active by
                    // construction; retirement is a separate audited step.
                    archived: false,
                    archived_at: None,
                    archived_by: None,
                    archived_note: None,
                },
                root_path: root_path_json,
                source_board_path: source_board_path.to_string_lossy().into_owned(),
                source_sha256,
                source_bytes,
            })
        })();
        if result.is_err() && !committed {
            let cleanup = adoption_run.cleanup_after_failure(published);
            return combine_cleanup(result, cleanup);
        }
        result
    }

    /// The nearest registered workspace strictly above `workspace`, if any.
    /// Read-only: unlike `resolve`, it does not touch `last_used_at`.
    fn enclosing(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        enclosing_on(&self.connection, workspace)
    }

    pub fn exact(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        exact_on(&self.connection, workspace)
    }

    pub fn resolve(&mut self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let mut cursor = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        loop {
            if let Some(found) = self.exact(&cursor)? {
                self.connection.execute(
                    "UPDATE workspace_roots SET last_used_at=? WHERE root_path=?",
                    params![now_ms(), found.root_path],
                )?;
                self.connection.execute(
                    "UPDATE boards SET last_used_at=? WHERE board_path=?",
                    params![now_ms(), found.board_path],
                )?;
                return self.exact(&cursor);
            }
            if let Some(history) = latest_history_root(&self.connection, &cursor)?
                && board_archived(&self.connection, &history.board_path)?.unwrap_or(false)
            {
                bail!(
                    "{} belongs to {}",
                    cursor.display(),
                    retired_board_message(
                        &history.name,
                        history.archived_note.as_deref(),
                        "addressing it"
                    )
                );
            }
            if !cursor.pop() {
                return Ok(None);
            }
        }
    }

    pub fn resolve_readonly(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let mut cursor = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        loop {
            if let Some(found) = self.exact(&cursor)? {
                return Ok(Some(found));
            }
            if let Some(history) = latest_history_root(&self.connection, &cursor)?
                && board_archived(&self.connection, &history.board_path)?.unwrap_or(false)
            {
                bail!(
                    "{} belongs to {}",
                    cursor.display(),
                    retired_board_message(
                        &history.name,
                        history.archived_note.as_deref(),
                        "addressing it"
                    )
                );
            }
            if !cursor.pop() {
                return Ok(None);
            }
        }
    }

    pub fn attach(
        &mut self,
        workspace: &Path,
        target: &str,
        actor: &str,
    ) -> Result<WorkspaceRecord> {
        let actor = validate_rule_actor(actor)?;
        let root = workspace.canonicalize()?;
        let project = if target_looks_like_path(target) {
            let project_workspace = Path::new(target);
            let found = self.resolve(project_workspace)?.with_context(|| {
                format!("no Kanban project contains {}", project_workspace.display())
            })?;
            self.project_for_board_path(&found.board_path)?
        } else {
            let matches = self.by_name(target)?;
            match matches.as_slice() {
                [project] => project.clone(),
                [] => {
                    let retired = self.by_name_all(target)?;
                    match retired.as_slice() {
                        [] => bail!("no Kanban project named {target}"),
                        [_project] => bail!(
                            "Kanban project {target} is retired; use `kanban workspace unretire {target} --as ACTOR` before attaching to it"
                        ),
                        many => bail!(
                            "{} retired Kanban projects are named {target}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                            many.len(),
                            crate::project_candidates(many)
                        ),
                    }
                }
                many => {
                    let rootless = many
                        .iter()
                        .filter(|project| project.workspace_roots.is_empty())
                        .collect::<Vec<_>>();
                    match rootless.as_slice() {
                        [project] => (*project).clone(),
                        [] => bail!(
                            "{} Kanban projects are named {target}; disambiguate with --workspace PATH: {}",
                            many.len(),
                            many.iter()
                                .map(|project| {
                                    if project.workspace_roots.is_empty() {
                                        format!("{} (rootless)", project.name)
                                    } else {
                                        format!(
                                            "{} [{}]",
                                            project.name,
                                            project.workspace_roots.join(", ")
                                        )
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        _ => bail!(
                            "{} rootless Kanban projects are named {target}; fix the registry so only one rootless board can carry a name: {}",
                            rootless.len(),
                            many.iter()
                                .map(|project| {
                                    if project.workspace_roots.is_empty() {
                                        format!("{} (rootless)", project.name)
                                    } else {
                                        format!(
                                            "{} [{}]",
                                            project.name,
                                            project.workspace_roots.join(", ")
                                        )
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    }
                }
            }
        };
        if let Some(existing) = self.exact(&root)? {
            if existing.board_path != project.board_path {
                bail!(
                    "{} is already attached to another Kanban project",
                    root.display()
                );
            }
            return Ok(existing);
        }
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO workspace_roots(root_path,board_path,created_at,last_used_at) VALUES(?,?,?,?)",
            params![root.to_string_lossy(), project.board_path, now, now],
        )?;
        transaction.execute(
            "UPDATE boards SET last_used_at=? WHERE board_path=?",
            params![now, project.board_path],
        )?;
        crate::audit::append_registry_event(
            &transaction,
            &format!("workspace:{}", root.to_string_lossy()),
            "workspace_attached",
            actor,
            &json!({"rootPath":root,"boardPath":project.board_path,"name":project.name})
                .to_string(),
            now,
        )?;
        transaction.commit()?;
        self.exact(&root)?.context("attached workspace not found")
    }

    pub fn list(&self, include_archived: bool) -> Result<Vec<WorkspaceRecord>> {
        let mut out = Vec::new();
        let active_sql = "SELECT workspace_roots.root_path AS root_path,\
                    boards.name AS name,\
                    workspace_roots.board_path AS board_path,\
                    workspace_roots.created_at AS created_at,\
                    workspace_roots.last_used_at AS last_used_at,\
                    0 AS archived,\
                    NULL AS archived_at,\
                    NULL AS archived_by,\
                    NULL AS archived_note,\
                    0 AS rootless \
             FROM workspace_roots JOIN boards ON boards.board_path=workspace_roots.board_path \
             WHERE boards.archived=0 \
             ORDER BY workspace_roots.last_used_at DESC, workspace_roots.root_path";
        let mut statement = self.connection.prepare(active_sql)?;
        let mut active = statement
            .query_map([], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        active.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        out.extend(active);
        let mut statement = self.connection.prepare(
            "SELECT '' AS root_path,\
                    boards.name AS name,\
                    boards.board_path AS board_path,\
                    boards.created_at AS created_at,\
                    boards.last_used_at AS last_used_at,\
                    0 AS archived,\
                    NULL AS archived_at,\
                    NULL AS archived_by,\
                    NULL AS archived_note,\
                    1 AS rootless \
             FROM boards \
             WHERE boards.archived=0 \
               AND NOT EXISTS (SELECT 1 FROM workspace_roots WHERE workspace_roots.board_path=boards.board_path) \
             ORDER BY boards.last_used_at DESC, boards.board_path",
        )?;
        let mut rootless = statement
            .query_map([], rootless_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rootless.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        out.extend(rootless);
        if include_archived {
            let mut statement = self.connection.prepare(
                "SELECT '' AS root_path,\
                        boards.name AS name,\
                        boards.board_path AS board_path,\
                        boards.created_at AS created_at,\
                        boards.last_used_at AS last_used_at,\
                        1 AS archived,\
                        boards.archived_at AS archived_at,\
                        boards.archived_by AS archived_by,\
                        boards.archived_note AS archived_note,\
                        1 AS rootless \
                 FROM boards \
                 WHERE boards.archived=1 \
                   AND NOT EXISTS (SELECT 1 FROM workspace_roots WHERE workspace_roots.board_path=boards.board_path) \
                   AND (boards.retirement_id IS NULL OR NOT EXISTS (SELECT 1 FROM workspace_alias_history WHERE workspace_alias_history.board_path=boards.board_path AND workspace_alias_history.retirement_id=boards.retirement_id)) \
                 ORDER BY boards.last_used_at DESC, boards.board_path",
            )?;
            let mut archived_rootless = statement
                .query_map([], rootless_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            archived_rootless.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
            out.extend(archived_rootless);
            let mut statement = self.connection.prepare(
                "SELECT * FROM workspace_alias_history ORDER BY archived_at DESC,seq DESC",
            )?;
            out.extend(
                statement
                    .query_map([], history_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        Ok(out)
    }

    /// Retire one attached worktree without deleting its historical registry
    /// row or risking the canonical registration that keeps the board named.
    pub fn detach(&mut self, root_path: &str, actor: &str) -> Result<WorkspaceRecord> {
        let actor = actor.trim();
        if actor.is_empty() {
            bail!("actor is required");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let alias = transaction
            .query_row(
                "SELECT workspace_roots.root_path AS root_path,\
                        boards.name AS name,\
                        workspace_roots.board_path AS board_path,\
                        workspace_roots.created_at AS created_at,\
                        workspace_roots.last_used_at AS last_used_at,\
                        0 AS archived,\
                        NULL AS archived_at,\
                        NULL AS archived_by,\
                        NULL AS archived_note,\
                        0 AS rootless \
                 FROM workspace_roots JOIN boards ON boards.board_path=workspace_roots.board_path \
                 WHERE workspace_roots.root_path=? AND boards.archived=0",
                [root_path],
                row,
            )
            .optional()?;
        let Some(alias) = alias else {
            if transaction
                .query_row(
                    "SELECT 1 FROM workspace_roots WHERE root_path=?",
                    [root_path],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                bail!("{root_path} is already detached");
            }
            let retired = transaction
                .query_row(
                    "SELECT 1 FROM workspace_alias_history WHERE root_path=? LIMIT 1",
                    [root_path],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if retired {
                bail!("workspace alias {root_path} is already detached");
            }
            bail!("workspace alias {root_path} is not registered");
        };
        let remaining_roots: i64 = transaction.query_row(
            "SELECT count(*) FROM workspace_roots WHERE board_path=?",
            [&alias.board_path],
            |row| row.get(0),
        )?;
        if remaining_roots == 1 && active_named(&transaction, &alias.name, Some(&alias.board_path))?
        {
            bail!(
                "detaching the last root from {root_path} would create a second active board named {}",
                alias.name
            );
        }
        let now = now_ms();
        transaction.execute(
            "INSERT INTO workspace_alias_history(root_path,name,board_path,created_at,last_used_at,archived_at,archived_by,archived_note,retirement_id) VALUES(?,?,?,?,?,?,?,?,?)",
            params![alias.root_path,alias.name,alias.board_path,alias.created_at,alias.last_used_at,now,actor,None::<String>,None::<String>],
        )?;
        transaction.execute("DELETE FROM workspace_roots WHERE root_path=?", [root_path])?;
        crate::audit::append_registry_event(
            &transaction,
            &format!("workspace:{root_path}"),
            "workspace_detached",
            actor,
            &json!({"rootPath":root_path,"boardPath":alias.board_path}).to_string(),
            now,
        )?;
        let retired = WorkspaceRecord {
            archived: true,
            archived_at: Some(now),
            archived_by: Some(actor.to_owned()),
            rootless: alias.rootless,
            ..alias
        };
        transaction.commit()?;
        Ok(retired)
    }

    pub fn retire(&mut self, name: &str, actor: &str, note: &str) -> Result<ProjectRecord> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let note = note.trim();
        if note.is_empty() {
            bail!("note is required");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active = Self::projects_by_name(&transaction, name, false)?;
        let project = match active.as_slice() {
            [project] => project.clone(),
            [] => {
                let retired = Self::projects_by_name(&transaction, name, true)?;
                match retired.as_slice() {
                    [] => bail!("no Kanban project named {name}"),
                    [project] => bail!(
                        "{}",
                        retired_board_message(
                            &project.name,
                            project.archived_note.as_deref(),
                            "retiring it"
                        )
                    ),
                    many => bail!(
                        "{} retired Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                        many.len(),
                        crate::project_candidates(many)
                    ),
                }
            }
            many => bail!(
                "{} active Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                many.len(),
                crate::project_candidates(many)
            ),
        };
        let board_path = project.board_path.clone();
        let board_name = project.name.clone();
        let active_rules = active_rule_tags(&transaction)?;
        let selector_errors = active_named_selector_errors(
            &transaction,
            active_rules
                .iter()
                .map(|(id, tags)| (id.as_str(), tags.as_slice())),
            Some(&board_name),
            Some(&board_path),
        )?;
        if !selector_errors.is_empty() {
            let mut blocker_ids = selector_errors
                .iter()
                .map(|error| error.rule_id.as_str())
                .collect::<Vec<_>>();
            blocker_ids.sort_unstable();
            blocker_ids.dedup();
            let selector_counts = selector_errors
                .iter()
                .map(|error| {
                    format!(
                        "{} on {} -> {} active boards",
                        error.selector, error.rule_id, error.active_board_count
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "cannot retire Kanban project {board_name}: active named rule selectors would become invalid ({selector_counts}); blocking rule IDs: {}; update or retire those rules before retiring the workspace",
                blocker_ids.join(", ")
            );
        }
        let retirement_id = Uuid::new_v4().simple().to_string();
        {
            let transaction = transaction;
            let roots = {
                let mut roots_statement = transaction.prepare(
                    "SELECT root_path,created_at,last_used_at FROM workspace_roots WHERE board_path=? ORDER BY last_used_at DESC,root_path",
                )?;
                roots_statement
                    .query_map([&board_path], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let now = now_ms();
            transaction.execute(
                "UPDATE boards SET archived=1,archived_at=?,archived_by=?,archived_note=?,retirement_id=? WHERE board_path=?",
                params![now, actor, note, &retirement_id, board_path],
            )?;
            for (root_path, created_at, last_used_at) in &roots {
                transaction.execute(
                    "INSERT INTO workspace_alias_history(root_path,name,board_path,created_at,last_used_at,archived_at,archived_by,archived_note,retirement_id) VALUES(?,?,?,?,?,?,?,?,?)",
                    params![root_path, board_name, board_path, created_at, last_used_at, now, actor, note, &retirement_id],
                )?;
            }
            transaction.execute(
                "DELETE FROM workspace_roots WHERE board_path=?",
                [&board_path],
            )?;
            crate::audit::append_registry_event(
                &transaction,
                &board_path,
                "workspace_retired",
                &actor,
                &json!({
                    "boardPath": board_path,
                    "name": board_name,
                    "retirementId": &retirement_id,
                    "workspaceRoots": roots.iter().map(|(root_path, _, _)| root_path).collect::<Vec<_>>(),
                    "archivedNote": note,
                })
                .to_string(),
                now,
            )?;
            transaction.commit()?;
        }
        self.project_for_board_path_all(&board_path)
    }

    pub fn unretire(&mut self, name: &str, actor: &str) -> Result<ProjectRecord> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active = Self::projects_by_name(&transaction, name, false)?;
        match active.as_slice() {
            [project] => {
                bail!("Kanban project {} is already active", project.name)
            }
            [] => {}
            many => bail!(
                "{} active Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                many.len(),
                crate::project_candidates(many)
            ),
        }
        let archived = Self::projects_by_name(&transaction, name, true)?;
        let project = match archived.as_slice() {
            [] => bail!("no retired Kanban project named {name}"),
            [project] => project.clone(),
            many => bail!(
                "{} retired Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                many.len(),
                crate::project_candidates(many)
            ),
        };
        let board_path = project.board_path.clone();
        let board_name = project.name.clone();
        let retirement_id = transaction
            .query_row(
                "SELECT retirement_id FROM boards WHERE board_path=?",
                [&board_path],
                |row| row.get::<_, Option<String>>(0),
            )?
            .context("retired board is missing retirement id")?;
        {
            let transaction = transaction;
            let roots = {
                let mut roots_statement = transaction.prepare(
                    "SELECT root_path,created_at,last_used_at FROM workspace_alias_history WHERE board_path=? AND retirement_id=? ORDER BY last_used_at DESC,root_path",
                )?;
                roots_statement
                    .query_map(params![&board_path, &retirement_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut conflicts = Vec::new();
            let mut restored = Vec::new();
            for (root_path, created_at, last_used_at) in roots {
                let existing = transaction
                    .query_row(
                        "SELECT board_path FROM workspace_roots WHERE root_path=?",
                        [&root_path],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                match existing {
                    None => restored.push((root_path, created_at, last_used_at)),
                    Some(active_board_path) if active_board_path == board_path => {}
                    Some(active_board_path) => conflicts.push((root_path, active_board_path)),
                }
            }
            if let Some((root_path, board_path)) = conflicts.first() {
                bail!(
                    "{} is already the active root of {}, so {} cannot be unretired",
                    root_path,
                    board_path,
                    board_name
                );
            }
            let now = now_ms();
            transaction.execute(
                "UPDATE boards SET archived=0,archived_at=NULL,archived_by=NULL,archived_note=NULL,retirement_id=NULL,last_used_at=? WHERE board_path=?",
                params![now, board_path],
            )?;
            for (root_path, created_at, last_used_at) in &restored {
                transaction.execute(
                    "INSERT INTO workspace_roots(root_path,board_path,created_at,last_used_at) VALUES(?,?,?,?)",
                    params![root_path, board_path, created_at, last_used_at],
                )?;
            }
            crate::audit::append_registry_event(
                &transaction,
                &board_path,
                "workspace_unretired",
                &actor,
                &json!({
                    "boardPath": board_path,
                    "name": board_name,
                    "retirementId": &retirement_id,
                    "restoredRoots": restored.iter().map(|(root_path, _, _)| root_path).collect::<Vec<_>>(),
                })
                .to_string(),
                now,
            )?;
            transaction.commit()?;
        }
        self.project_for_board_path(&board_path)
    }

    /// Turn operator-facing include/exclude flags into the one canonical tag
    /// representation stored and returned by every adapter.
    pub fn canonical_board_tags(
        &self,
        boards: &[String],
        except_boards: &[String],
    ) -> Result<Vec<String>> {
        let boards = if boards.is_empty() {
            vec!["ALL".to_owned()]
        } else {
            boards.to_vec()
        };
        let mut seen = HashSet::new();
        for name in boards.iter().chain(except_boards) {
            if !seen.insert(name) {
                bail!("board target {name:?} was given more than once");
            }
        }
        let all = boards.iter().any(|name| name == "ALL");
        if all && boards.len() != 1 {
            bail!("ALL cannot be combined with named --board targets");
        }
        if !all && !except_boards.is_empty() {
            bail!("--except-board requires ALL scope; omit --board or pass --board ALL");
        }
        for name in boards
            .iter()
            .filter(|name| name.as_str() != "ALL")
            .chain(except_boards)
        {
            if name == "ALL" {
                bail!("ALL is only valid as --board ALL, never --except-board ALL");
            }
            match self.by_name_active(name)?.len() {
                1 => {}
                0 => bail!("no registered Kanban board named {name}"),
                count => bail!(
                    "{count} registered Kanban boards are named {name}; board tags require a unique name"
                ),
            }
        }
        let mut tags = if all {
            vec!["ALL".to_owned()]
        } else {
            boards
                .into_iter()
                .map(|name| format!("ONLY:{name}"))
                .collect()
        };
        tags.extend(except_boards.iter().map(|name| format!("EXCEPT:{name}")));
        Ok(tags)
    }

    /// Validate global task-tag selectors against the union of active board
    /// master files. The canonical name is the cross-board matching key; each
    /// board still owns its description and decides whether it registers it.
    pub fn canonical_rule_task_tags(&self, tags: &[String]) -> Result<Vec<String>> {
        let mut known = HashSet::new();
        for project in self.projects_active()? {
            let path = Path::new(&project.board_path);
            if !path.is_file() {
                continue;
            }
            let connection = open_board(path)?;
            let mut statement = connection.prepare("SELECT name FROM tags ORDER BY name")?;
            for name in statement.query_map([], |row| row.get::<_, String>(0))? {
                known.insert(name?);
            }
        }
        let mut seen = HashSet::new();
        let mut canonical = Vec::new();
        for tag in tags {
            let tag = validate_tag_name(tag)?;
            if !seen.insert(tag.clone()) {
                bail!("global rule task tag {tag:?} was given more than once");
            }
            if !known.contains(&tag) {
                let mut borrowed = known.iter().map(String::as_str).collect::<Vec<_>>();
                borrowed.sort_unstable();
                let suggestion = crate::nearest(&tag, &borrowed)
                    .map(|near| format!(", did you mean {near}?"))
                    .unwrap_or_default();
                bail!(
                    "global rule task tag {tag} is not registered on any active board{suggestion}"
                );
            }
            canonical.push(tag);
        }
        canonical.sort();
        Ok(canonical)
    }

    /// Consolidate every legacy board-local rule into ADR-027's canonical
    /// registry document. Registry insertion happens before source retirement,
    /// so an interruption can duplicate no data and lose no active rule: the
    /// unique source key lets the next run finish the retirement safely.
    pub fn consolidate_board_rules(&mut self, actor: &str) -> Result<RuleMigrationReport> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let projects = {
            let mut statement = self
                .connection
                .prepare("SELECT name,board_path FROM boards ORDER BY name,board_path")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut report = RuleMigrationReport {
            legacy_registry_migrated: false,
            legacy_registry_already_migrated: false,
            legacy_rules_imported: 0,
            legacy_rules_updated: 0,
            legacy_events_imported: 0,
            legacy_rules_retired: 0,
            boards_migrated: 0,
            boards_already_migrated: 0,
            rules_imported: 0,
            rules_already_imported: 0,
            source_rules_retired: 0,
        };

        self.consolidate_legacy_registry_rules(&actor, &mut report)?;

        for (board_name, board_path) in projects {
            let duplicate_names: i64 = self.connection.query_row(
                "SELECT count(*) FROM boards WHERE name=?",
                [&board_name],
                |row| row.get(0),
            )?;
            if duplicate_names != 1 {
                bail!(
                    "cannot consolidate rules for board {board_name}: {duplicate_names} active boards share that name"
                );
            }
            if self
                .connection
                .query_row(
                    "SELECT 1 FROM rule_board_migrations WHERE board_path=?",
                    [&board_path],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                report.boards_already_migrated += 1;
                continue;
            }
            let path = Path::new(&board_path);
            if !path.is_file() {
                bail!(
                    "cannot consolidate rules for board {board_name}: {} is not a readable board file",
                    path.display()
                );
            }
            let mut board = Store::open(path)
                .with_context(|| format!("open board {board_name} for rule consolidation"))?;
            let source_rules = board.rules(true)?;
            for source in &source_rules {
                let imported_id = self
                    .connection
                    .query_row(
                        "SELECT id FROM rules WHERE source_board=? AND source_rule_id=?",
                        params![board_name, source.id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let canonical_id = if let Some(id) = imported_id {
                    report.rules_already_imported += 1;
                    id
                } else {
                    let mut id = source.id.clone();
                    while self
                        .connection
                        .query_row("SELECT 1 FROM rules WHERE id=?", [&id], |_| Ok(()))
                        .optional()?
                        .is_some()
                    {
                        id = format!("r-{}", &Uuid::new_v4().simple().to_string()[..8]);
                    }
                    let mut tags = vec![format!("ONLY:{board_name}")];
                    tags.extend(source.tags.iter().cloned());
                    let now = now_ms();
                    let transaction = self
                        .connection
                        .transaction_with_behavior(TransactionBehavior::Immediate)?;
                    transaction.execute(
                        "INSERT INTO rules(id,body,author,archived,created_at,updated_at,tags,source_board,source_rule_id) VALUES(?,?,?,?,?,?,?,?,?)",
                        params![
                            id,
                            source.body,
                            source.author,
                            source.archived,
                            source.created_at,
                            source.updated_at,
                            serde_json::to_string(&tags)?,
                            board_name,
                            source.id,
                        ],
                    )?;
                    crate::audit::append_registry_event(
                        &transaction,
                        &id,
                        "rule_consolidated",
                        &actor,
                        &json!({
                            "ruleID": id,
                            "sourceBoard": board_name,
                            "sourceRuleID": source.id,
                            "tags": tags,
                        })
                        .to_string(),
                        now,
                    )?;
                    transaction.commit()?;
                    report.rules_imported += 1;
                    id
                };

                if !source.archived {
                    board.retire_rule(&source.id, &actor)?;
                    event(
                        &board.connection,
                        None,
                        "rule_consolidated",
                        Some(&actor),
                        json!({
                            "ruleID": source.id,
                            "canonicalBoard": board_name,
                            "canonicalRuleID": canonical_id,
                        }),
                    )?;
                    report.source_rules_retired += 1;
                }
            }
            self.connection.execute(
                "INSERT INTO rule_board_migrations(board_path,board_name,source_count,actor,migrated_at) VALUES(?,?,?,?,?)",
                params![board_path, board_name, source_rules.len(), actor, now_ms()],
            )?;
            report.boards_migrated += 1;
        }
        Ok(report)
    }

    /// Synchronize writes made by a rolling-upgrade client after registry v9
    /// initially copied `global_rules`. The marker makes the operation
    /// idempotent; current state and the exact multiplicity of audit events are
    /// copied before active legacy rows are retired.
    fn consolidate_legacy_registry_rules(
        &mut self,
        actor: &str,
        report: &mut RuleMigrationReport,
    ) -> Result<()> {
        const SOURCE: &str = "registry:global_rules";
        if self
            .connection
            .query_row(
                "SELECT 1 FROM rule_board_migrations WHERE board_path=?",
                [SOURCE],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            report.legacy_registry_already_migrated = true;
            return Ok(());
        }

        type LegacyRule = (String, String, String, bool, i64, i64, String, String);
        let legacy_rules = {
            let mut statement = self.connection.prepare(
                "SELECT id,body,author,archived,created_at,updated_at,board_tags,task_tags \
                 FROM global_rules ORDER BY created_at,id",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<LegacyRule>>>()?
        };
        for (id, body, author, archived, created_at, updated_at, board_tags, task_tags) in
            &legacy_rules
        {
            let mut tags = serde_json::from_str::<Vec<String>>(board_tags)?;
            tags.extend(serde_json::from_str::<Vec<String>>(task_tags)?);
            let previous = self
                .connection
                .query_row("SELECT updated_at FROM rules WHERE id=?", [id], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?;
            match previous {
                None => {
                    self.connection.execute(
                        "INSERT INTO rules(id,body,author,archived,created_at,updated_at,tags) \
                         VALUES(?,?,?,?,?,?,?)",
                        params![
                            id,
                            body,
                            author,
                            archived,
                            created_at,
                            updated_at,
                            serde_json::to_string(&tags)?,
                        ],
                    )?;
                    report.legacy_rules_imported += 1;
                }
                Some(previous_updated) if *updated_at > previous_updated => {
                    self.connection.execute(
                        "UPDATE rules SET body=?,author=?,archived=?,updated_at=?,tags=? WHERE id=?",
                        params![
                            body,
                            author,
                            archived,
                            updated_at,
                            serde_json::to_string(&tags)?,
                            id,
                        ],
                    )?;
                    report.legacy_rules_updated += 1;
                }
                Some(_) => {}
            }
        }

        type LegacyEvent = (String, String, String, String, i64);
        let legacy_events = {
            let mut statement = self.connection.prepare(
                "SELECT rule_id,kind,actor,payload,created_at FROM global_rule_events ORDER BY seq",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<LegacyEvent>>>()?
        };
        let mut occurrences: HashMap<LegacyEvent, i64> = HashMap::new();
        for event in legacy_events {
            let (rule_id, kind, event_actor, payload, created_at) = &event;
            let occurrence = occurrences.entry(event.clone()).or_default();
            *occurrence += 1;
            let copied: i64 = self.connection.query_row(
                "SELECT count(*) FROM rule_events WHERE rule_id=? AND kind=? AND actor=? AND payload=? AND created_at=?",
                params![rule_id, kind, event_actor, payload, created_at],
                |row| row.get(0),
            )?;
            if copied < *occurrence {
                crate::audit::append_registry_event(
                    &self.connection,
                    rule_id,
                    kind,
                    event_actor,
                    payload,
                    *created_at,
                )?;
                report.legacy_events_imported += 1;
            }
        }

        report.legacy_rules_retired = self
            .connection
            .execute("UPDATE global_rules SET archived=1 WHERE archived=0", [])?;
        self.connection.execute(
            "INSERT INTO rule_board_migrations(board_path,board_name,source_count,actor,migrated_at) \
             VALUES(?,?,?,?,?)",
            params![SOURCE, "legacy-registry", legacy_rules.len(), actor, now_ms()],
        )?;
        report.legacy_registry_migrated = true;
        Ok(())
    }

    fn transfer_rule_boards(&self, requested_boards: &[String]) -> Result<Vec<(String, String)>> {
        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        for name in requested_boards {
            if !seen.insert(name) {
                bail!("board {name:?} was given more than once");
            }
            let boards = self.by_name(name)?;
            match boards.as_slice() {
                [project] => {
                    let board_path = project.board_path.clone();
                    if !Path::new(&board_path).is_file() {
                        bail!(
                            "cannot transfer rules for board {name}: {board_path} is not a readable board file"
                        );
                    }
                    selected.push((name.clone(), board_path));
                }
                [] => {
                    bail!(
                        "cannot transfer rules for board {name}: it is not registered in this registry"
                    )
                }
                many => bail!(
                    "cannot transfer rules for board {name}: {} active boards share that name",
                    many.len()
                ),
            }
        }
        selected.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        Ok(selected)
    }

    fn registry_uuid(&self) -> Result<String> {
        let value = self
            .connection
            .query_row(
                "SELECT value FROM registry_meta WHERE key='registry_uuid'",
                [],
                |row| row.get::<_, String>(0),
            )
            .context("registry_uuid metadata is missing")?;
        validate_registry_uuid(&value)?;
        Ok(value.trim().to_owned())
    }

    pub fn export_rules(
        &self,
        actor: &str,
        requested_boards: &[String],
    ) -> Result<RuleTransferBundle> {
        let exported_by = validate_rule_actor(actor)?.to_owned();
        let source_boards = self.transfer_rule_boards(requested_boards)?;
        let source_board_names = source_boards
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let allowed_boards = source_board_names.iter().cloned().collect::<HashSet<_>>();
        let bundle_source_registry_uuid = self.registry_uuid()?;
        let source_registry_audit = self.audit()?;
        if !source_registry_audit.healthy {
            bail!(
                "source registry audit is unhealthy: {:?}",
                source_registry_audit.errors
            );
        }
        let exported_at = now_ms();
        let mut rules = Vec::new();
        for rule in self.rules(false)? {
            deny_secret_material(&rule.body)?;
            if rule
                .tags
                .iter()
                .any(|tag| tag == "ALL" || tag.starts_with("EXCEPT:"))
            {
                bail!(
                    "rule {} has selectors outside the allowlisted board set",
                    rule.id
                );
            }
            let selected_board = source_boards
                .iter()
                .map(|(name, _)| name)
                .find(|name| selector_tags_apply(&rule.tags, Some(name.as_str())));
            let Some(selected_board) = selected_board else {
                let only_boards = rule
                    .tags
                    .iter()
                    .filter_map(|tag| tag.strip_prefix("ONLY:"))
                    .collect::<Vec<_>>();
                if only_boards
                    .iter()
                    .any(|board| !allowed_boards.contains(*board))
                {
                    bail!(
                        "rule {} reaches outside the requested board allowlist",
                        rule.id
                    );
                }
                continue;
            };
            let only_boards = rule
                .tags
                .iter()
                .filter_map(|tag| tag.strip_prefix("ONLY:"))
                .collect::<Vec<_>>();
            if only_boards
                .iter()
                .any(|board| !allowed_boards.contains(*board))
            {
                bail!(
                    "rule {} reaches outside the requested board allowlist",
                    rule.id
                );
            }
            let selected_board = selected_board.to_string();
            let source_registry_uuid = rule
                .source_registry_uuid
                .clone()
                .unwrap_or_else(|| bundle_source_registry_uuid.clone());
            validate_registry_uuid(&source_registry_uuid)?;
            let source_rule_id = rule
                .source_rule_id
                .clone()
                .unwrap_or_else(|| rule.id.clone());
            let source_content_sha256 = rule.source_content_sha256.clone().unwrap_or_else(|| {
                rule_fingerprint(
                    &source_registry_uuid,
                    &source_rule_id,
                    &rule.body,
                    &rule.author,
                    rule.archived,
                    rule.created_at,
                    rule.updated_at,
                    &rule.tags,
                )
            });
            let expected_content_sha256 = rule_fingerprint(
                &source_registry_uuid,
                &source_rule_id,
                &rule.body,
                &rule.author,
                rule.archived,
                rule.created_at,
                rule.updated_at,
                &rule.tags,
            );
            if source_content_sha256 != expected_content_sha256 {
                bail!(
                    "rule {} source content hash does not match its canonical fingerprint",
                    rule.id
                );
            }
            rules.push(RuleTransferItem {
                source_board: Some(selected_board),
                source_registry_uuid,
                source_rule_id,
                source_boards: source_board_names.clone(),
                source_content_sha256,
                body: rule.body,
                author: rule.author,
                archived: rule.archived,
                created_at: rule.created_at,
                updated_at: rule.updated_at,
                tags: rule.tags,
            });
        }
        rules.sort_by(|left, right| {
            left.source_board
                .cmp(&right.source_board)
                .then(left.source_rule_id.cmp(&right.source_rule_id))
                .then(left.source_content_sha256.cmp(&right.source_content_sha256))
        });
        Ok(RuleTransferBundle {
            format_version: 1,
            exported_by,
            exported_at,
            source_registry_uuid: bundle_source_registry_uuid,
            source_registry_audit,
            source_boards: source_board_names,
            rules,
        })
    }

    pub fn import_rules(
        &mut self,
        actor: &str,
        bundle: RuleTransferBundle,
    ) -> Result<RuleTransferReport> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let RuleTransferBundle {
            format_version,
            exported_by,
            exported_at,
            source_registry_uuid,
            source_registry_audit,
            source_boards,
            rules,
        } = bundle;
        if format_version != 1 {
            bail!(
                "rule transfer bundle version {} is not supported",
                format_version
            );
        }
        if exported_by.trim().is_empty() {
            bail!("rule transfer bundle is missing exportedBy");
        }
        validate_registry_uuid(&source_registry_uuid)?;
        if !source_registry_audit.healthy {
            bail!(
                "rule transfer bundle source registry audit is unhealthy: {:?}",
                source_registry_audit.errors
            );
        }
        let source_boards = sorted_unique_boards(&source_boards)?;
        let source_registry_audit_head = source_registry_audit.head.clone();
        let verified_boards = self.transfer_rule_boards(&source_boards)?;
        let verified_names = verified_boards
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        let allowed_boards = source_boards.iter().cloned().collect::<HashSet<_>>();
        for rule in &rules {
            validate_registry_uuid(&rule.source_registry_uuid)?;
            if rule.source_registry_uuid != source_registry_uuid {
                bail!(
                    "rule transfer bundle item {} claims source registry {} but the bundle sourceRegistryUuid is {}",
                    rule.source_rule_id,
                    rule.source_registry_uuid,
                    source_registry_uuid
                );
            }
            deny_secret_material(&rule.body)?;
            if rule.source_boards != source_boards {
                bail!(
                    "rule transfer bundle item {} does not carry the full source board selector set",
                    rule.source_rule_id
                );
            }
            let rule_source_board = rule
                .source_board
                .clone()
                .context("rule transfer bundle item is missing sourceBoard")?;
            if !allowed_boards.contains(&rule_source_board) {
                bail!(
                    "rule transfer bundle references board {} that was not exported",
                    rule_source_board
                );
            }
            if !selector_tags_apply(&rule.tags, Some(&rule_source_board)) {
                bail!(
                    "rule transfer bundle item {} does not apply to source board {}",
                    rule.source_rule_id,
                    rule_source_board
                );
            }
            if rule
                .tags
                .iter()
                .any(|tag| tag == "ALL" || tag.starts_with("EXCEPT:"))
            {
                bail!(
                    "rule transfer bundle item {} contains forbidden selector tags",
                    rule.source_rule_id
                );
            }
            if let Some(selector) = rule.tags.iter().find(|tag| {
                tag.strip_prefix("ONLY:")
                    .is_some_and(|board| !allowed_boards.contains(board))
            }) {
                bail!(
                    "rule transfer bundle item {} selector {} reaches outside the bundle sourceBoards allowlist [{}]; re-export from the source registry with `rule export --board {} --as ACTOR` naming every board the rule selects, or drop that selector from the source rule before exporting",
                    rule.source_rule_id,
                    selector,
                    source_boards.join(", "),
                    source_boards
                        .iter()
                        .map(String::as_str)
                        .chain(std::iter::once(&selector["ONLY:".len()..]))
                        .collect::<Vec<_>>()
                        .join(" --board ")
                );
            }
            let expected_content_sha256 = rule_fingerprint(
                &rule.source_registry_uuid,
                &rule.source_rule_id,
                &rule.body,
                &rule.author,
                rule.archived,
                rule.created_at,
                rule.updated_at,
                &rule.tags,
            );
            if rule.source_content_sha256 != expected_content_sha256 {
                bail!(
                    "rule transfer bundle item {} has a content hash mismatch",
                    rule.source_rule_id
                );
            }
            if rule.archived {
                bail!(
                    "rule transfer bundle contains archived rule {} from {}",
                    rule.source_rule_id,
                    rule_source_board
                );
            }
        }

        let mut seen_sources = HashSet::new();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut imported_rules = 0usize;
        let mut already_imported_rules = 0usize;
        for rule in rules {
            let rule_source_board = rule
                .source_board
                .clone()
                .context("rule transfer bundle item is missing sourceBoard")?;
            let source_key = (
                rule.source_registry_uuid.clone(),
                rule.source_rule_id.clone(),
            );
            if !seen_sources.insert(source_key.clone()) {
                bail!(
                    "rule transfer bundle contains duplicate source rule {} from registry {}",
                    rule.source_rule_id,
                    rule.source_registry_uuid
                );
            }
            validate_active_rule_selectors(&transaction, &rule.source_rule_id, &rule.tags)?;
            if !verified_names.contains(&rule_source_board) {
                bail!(
                    "cannot import rule {} from {}: it is not registered in the destination registry",
                    rule.source_rule_id,
                    rule_source_board
                );
            }
            let existing = transaction
                .query_row(
                    "SELECT id,body,author,archived,created_at,updated_at,tags,source_board,source_boards,source_content_sha256,source_registry_uuid FROM rules WHERE source_registry_uuid=? AND source_rule_id=?",
                    params![rule.source_registry_uuid, rule.source_rule_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)? != 0,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, Option<String>>(7)?,
                            row.get::<_, Option<String>>(8)?,
                            row.get::<_, Option<String>>(9)?,
                            row.get::<_, Option<String>>(10)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((
                existing_id,
                body,
                author,
                archived,
                created_at,
                updated_at,
                tags,
                stored_source_board,
                stored_source_boards,
                stored_source_content_sha256,
                stored_source_registry_uuid,
            )) = existing
            {
                if body != rule.body
                    || author != rule.author
                    || archived != rule.archived
                    || created_at != rule.created_at
                    || updated_at != rule.updated_at
                    || serde_json::from_str::<Vec<String>>(&tags)? != rule.tags
                    || stored_source_board.as_deref() != Some(rule_source_board.as_str())
                    || stored_source_registry_uuid.as_deref()
                        != Some(rule.source_registry_uuid.as_str())
                    || stored_source_content_sha256.as_deref()
                        != Some(rule.source_content_sha256.as_str())
                    || stored_source_boards
                        .map(|encoded| parse_json_string_array(&encoded))
                        .transpose()?
                        .unwrap_or_default()
                        != rule.source_boards
                {
                    bail!(
                        "destination already has source rule {} from registry {} with different content",
                        rule.source_rule_id,
                        rule.source_registry_uuid
                    );
                }
                let existing_ledger = transaction
                    .query_row(
                        "SELECT destination_rule_id,source_content_sha256 FROM rule_import_ledger WHERE source_registry_uuid=? AND source_rule_id=?",
                        params![rule.source_registry_uuid, rule.source_rule_id],
                        |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        },
                    )
                    .optional()?;
                if let Some((destination_rule_id, source_content_sha256)) = existing_ledger {
                    if destination_rule_id != existing_id
                        || source_content_sha256 != rule.source_content_sha256
                    {
                        bail!(
                            "destination already has source rule {} from registry {} with different ledger content",
                            rule.source_rule_id,
                            rule.source_registry_uuid
                        );
                    }
                } else {
                    transaction.execute(
                        "INSERT INTO rule_import_ledger(source_registry_uuid,source_rule_id,source_content_sha256,destination_rule_id,imported_at,imported_by) VALUES(?,?,?,?,?,?)",
                        params![
                            rule.source_registry_uuid,
                            rule.source_rule_id,
                            rule.source_content_sha256,
                            existing_id,
                            now_ms(),
                            &actor,
                        ],
                    )?;
                }
                already_imported_rules += 1;
                continue;
            }

            let mut id = format!("r-{}", &Uuid::new_v4().simple().to_string()[..8]);
            while transaction
                .query_row("SELECT 1 FROM rules WHERE id=?", [&id], |_| Ok(()))
                .optional()?
                .is_some()
            {
                id = format!("r-{}", &Uuid::new_v4().simple().to_string()[..8]);
            }
            transaction.execute(
                "INSERT INTO rules(id,body,author,archived,created_at,updated_at,tags,source_board,source_rule_id,source_registry_uuid,source_boards,source_content_sha256) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    id,
                    rule.body,
                    rule.author,
                    rule.archived,
                    rule.created_at,
                    rule.updated_at,
                    serde_json::to_string(&rule.tags)?,
                    rule_source_board,
                    rule.source_rule_id,
                    rule.source_registry_uuid,
                    serde_json::to_string(&rule.source_boards)?,
                    rule.source_content_sha256,
                ],
            )?;
            transaction.execute(
                "INSERT INTO rule_import_ledger(source_registry_uuid,source_rule_id,source_content_sha256,destination_rule_id,imported_at,imported_by) VALUES(?,?,?,?,?,?)",
                params![
                    rule.source_registry_uuid,
                    rule.source_rule_id,
                    rule.source_content_sha256,
                    id,
                    now_ms(),
                    &actor,
                ],
            )?;
            crate::audit::append_registry_event(
                &transaction,
                &id,
                "rule_imported",
                &actor,
                &json!({
                    "ruleID": id,
                    "sourceBoard": source_key.0,
                    "sourceRuleID": source_key.1,
                    "sourceRegistryUUID": rule.source_registry_uuid,
                    "sourceContentSHA256": rule.source_content_sha256,
                    "sourceBoards": rule.source_boards,
                    "sourceRegistryAuditHead": source_registry_audit_head,
                    "exportedBy": exported_by.clone(),
                    "exportedAt": exported_at,
                    "tags": rule.tags,
                })
                .to_string(),
                now_ms(),
            )?;
            imported_rules += 1;
        }
        transaction.commit()?;

        Ok(RuleTransferReport {
            imported_rules,
            already_imported_rules,
            destination_boards_verified: verified_boards.len(),
            source_registry_uuid,
            source_registry_audit_head,
        })
    }

    pub fn canonical_rule_tags(
        &self,
        boards: &[String],
        except_boards: &[String],
        subsystem_tags: &[String],
    ) -> Result<Vec<String>> {
        let mut tags = self.canonical_board_tags(boards, except_boards)?;
        tags.extend(self.canonical_rule_task_tags(subsystem_tags)?);
        Ok(tags)
    }

    pub fn add_rule(&mut self, body: &str, actor: &str, tags: &[String]) -> Result<Rule> {
        validate_rule_body(body)?;
        let actor = validate_rule_actor(actor)?.to_owned();
        let id = format!("r-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_active_rule_selectors(&transaction, &id, tags)?;
        let previous: Option<i64> =
            transaction.query_row("SELECT max(created_at) FROM rules", [], |row| row.get(0))?;
        let now = now_ms().max(previous.unwrap_or(0).saturating_add(1));
        transaction.execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at,tags) VALUES(?,?,?,0,?,?,?)",
            params![id, body, actor, now, now, serde_json::to_string(tags)?],
        )?;
        crate::audit::append_registry_event(
            &transaction,
            &id,
            "rule_added",
            &actor,
            &json!({"ruleID": id, "tags": tags}).to_string(),
            now,
        )?;
        let rule = transaction.query_row("SELECT * FROM rules WHERE id=?", [&id], rule_row)?;
        transaction.commit()?;
        Ok(rule)
    }

    pub fn rules(&self, include_archived: bool) -> Result<Vec<Rule>> {
        let clause = if include_archived {
            ""
        } else {
            " WHERE archived=0"
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT * FROM rules{clause} ORDER BY created_at,id"
        ))?;
        statement
            .query_map([], rule_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn active_rule_selector_health(&self) -> Result<ActiveRuleSelectorHealth> {
        let active_rules = active_rule_tags(&self.connection)?;
        let errors = active_named_selector_errors(
            &self.connection,
            active_rules
                .iter()
                .map(|(id, tags)| (id.as_str(), tags.as_slice())),
            None,
            None,
        )?;
        Ok(ActiveRuleSelectorHealth {
            healthy: errors.is_empty(),
            errors,
        })
    }

    pub fn rule_summaries(&self, include_archived: bool) -> Result<Vec<RuleSummary>> {
        self.rules(include_archived)?
            .into_iter()
            .map(|rule| {
                let headline = rule
                    .body
                    .lines()
                    .next()
                    .context("stored rule has no headline")?
                    .trim()
                    .to_owned();
                let has_more = rule
                    .body
                    .lines()
                    .skip(1)
                    .any(|line| !line.trim().is_empty());
                Ok(RuleSummary {
                    id: rule.id,
                    headline,
                    has_more,
                    bytes: rule.body.len(),
                    tags: rule.tags,
                })
            })
            .collect()
    }

    pub fn applicable_rule_summaries(
        &self,
        board_name: Option<&str>,
        task_tags: Option<&HashSet<String>>,
        include_archived: bool,
    ) -> Result<Vec<RuleSummary>> {
        Ok(self
            .rule_summaries(include_archived)?
            .into_iter()
            .filter(|rule| rule_tags_apply(&rule.tags, board_name, task_tags))
            .collect())
    }

    pub fn applicable_rules(
        &self,
        board_name: Option<&str>,
        task_tags: Option<&HashSet<String>>,
        include_archived: bool,
    ) -> Result<Vec<Rule>> {
        Ok(self
            .rules(include_archived)?
            .into_iter()
            .filter(|rule| rule_tags_apply(&rule.tags, board_name, task_tags))
            .collect())
    }

    pub fn rules_targeting_board(
        &self,
        board_name: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<Rule>> {
        Ok(self
            .rules(include_archived)?
            .into_iter()
            .filter(|rule| selector_tags_apply(&rule.tags, board_name))
            .collect())
    }

    pub fn rule(&self, id: &str) -> Result<Rule> {
        self.connection
            .query_row("SELECT * FROM rules WHERE id=?", [id], rule_row)
            .optional()?
            .with_context(|| format!("rule {id} not found"))
    }

    pub fn update_rule(
        &mut self,
        id: &str,
        body: Option<&str>,
        selector_tags: Option<&[String]>,
        subsystem_tags: Option<&[String]>,
        actor: &str,
    ) -> Result<Rule> {
        if body.is_none() && selector_tags.is_none() && subsystem_tags.is_none() {
            bail!(
                "rule update requires --body/--body-file, --board/--except-board, --tag, or --clear-tags"
            );
        }
        if let Some(body) = body {
            validate_rule_body(body)?;
        }
        let subsystem_tags = subsystem_tags
            .map(|tags| self.canonical_rule_task_tags(tags))
            .transpose()?;
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<Rule> = transaction
            .query_row("SELECT * FROM rules WHERE id=?", [id], rule_row)
            .optional()?;
        let previous = previous.with_context(|| format!("rule {id} not found"))?;
        let previous_selectors = previous
            .tags
            .iter()
            .filter(|tag| is_selector_tag(tag))
            .cloned()
            .collect::<Vec<_>>();
        let previous_subsystems = previous
            .tags
            .iter()
            .filter(|tag| !is_selector_tag(tag))
            .cloned()
            .collect::<Vec<_>>();
        let mut tags = selector_tags.unwrap_or(&previous_selectors).to_vec();
        tags.extend(
            subsystem_tags
                .as_deref()
                .unwrap_or(&previous_subsystems)
                .iter()
                .cloned(),
        );
        if !previous.archived {
            validate_active_rule_selectors(&transaction, id, &tags)?;
        }
        let now = now_ms();
        transaction.execute(
            "UPDATE rules SET body=?,tags=?,author=?,updated_at=? WHERE id=?",
            params![
                body.unwrap_or(&previous.body),
                serde_json::to_string(&tags)?,
                actor,
                now,
                id
            ],
        )?;
        let mut changed = Vec::new();
        if body.is_some() {
            changed.push("body");
        }
        if selector_tags.is_some() {
            changed.push("selectorTags");
        }
        if subsystem_tags.is_some() {
            changed.push("subsystemTags");
        }
        crate::audit::append_registry_event(
            &transaction,
            id,
            "rule_updated",
            &actor,
            &json!({
                "ruleID": id,
                "previousBody": previous.body,
                "previousTags": previous.tags,
                "changed": changed,
            })
            .to_string(),
            now,
        )?;
        let rule = transaction.query_row("SELECT * FROM rules WHERE id=?", [id], rule_row)?;
        transaction.commit()?;
        Ok(rule)
    }

    pub fn retire_rule(&mut self, id: &str, actor: &str) -> Result<Rule> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let changed = transaction.execute(
            "UPDATE rules SET archived=1,author=?,updated_at=? WHERE id=? AND archived=0",
            params![actor, now, id],
        )?;
        if changed == 0 {
            let archived: Option<i64> = transaction
                .query_row("SELECT archived FROM rules WHERE id=?", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            match archived {
                None => bail!("rule {id} not found"),
                Some(_) => bail!("rule {id} is already retired"),
            }
        }
        crate::audit::append_registry_event(
            &transaction,
            id,
            "rule_retired",
            &actor,
            &json!({"ruleID": id}).to_string(),
            now,
        )?;
        let rule = transaction.query_row("SELECT * FROM rules WHERE id=?", [id], rule_row)?;
        transaction.commit()?;
        Ok(rule)
    }

    /// Rule history, newest first, from the registry-owned document.
    pub fn rule_events(
        &self,
        rule: Option<&str>,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Event>> {
        if let Some(id) = rule {
            self.rule(id)?;
        }
        let mut sql = String::from("SELECT * FROM rule_events WHERE 1=1");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(id) = rule {
            sql.push_str(" AND rule_id=?");
            values.push(Box::new(id.to_owned()));
        }
        if let Some(kind) = kind {
            sql.push_str(" AND kind=?");
            values.push(Box::new(kind.to_owned()));
        }
        sql.push_str(" ORDER BY seq DESC LIMIT ?");
        values.push(Box::new(limit));
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(
                params_from_iter(values.iter().map(|value| value.as_ref())),
                rule_event_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn rule_events_since(
        &self,
        rule: Option<&str>,
        kind: Option<&str>,
        cursor: i64,
        limit: i64,
    ) -> Result<Vec<Event>> {
        validate_event_limit(limit)?;
        if let Some(id) = rule {
            self.rule(id)?;
        }
        let mut sql = String::from(
            "SELECT seq,rule_id,kind,actor,payload,created_at,prev_hash,event_hash \
             FROM rule_events WHERE seq>?",
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(cursor)];
        if let Some(id) = rule {
            sql.push_str(" AND rule_id=?");
            values.push(Box::new(id.to_owned()));
        }
        if let Some(kind) = kind {
            sql.push_str(" AND kind=?");
            values.push(Box::new(kind.to_owned()));
        }
        sql.push_str(" ORDER BY seq ASC LIMIT ?");
        values.push(Box::new(limit));
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(
                params_from_iter(values.iter().map(|value| value.as_ref())),
                rule_event_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Ascending registry watch rows with multi-kind predicates applied before
    /// LIMIT. Registry events have no board semantic snapshot, so only kinds
    /// are supported here.
    pub fn rule_events_since_filtered(
        &self,
        rule: Option<&str>,
        kinds: &[String],
        cursor: i64,
        limit: i64,
    ) -> Result<Vec<Event>> {
        validate_event_limit(limit)?;
        if let Some(id) = rule {
            self.rule(id)?;
        }
        let mut sql = String::from(
            "SELECT seq,rule_id,kind,actor,payload,created_at,prev_hash,event_hash \
             FROM rule_events WHERE seq>?",
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(cursor)];
        if let Some(id) = rule {
            sql.push_str(" AND rule_id=?");
            values.push(Box::new(id.to_owned()));
        }
        if !kinds.is_empty() {
            sql.push_str(" AND kind IN (");
            sql.push_str(
                &std::iter::repeat_n("?", kinds.len())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            sql.push(')');
            values.extend(
                kinds
                    .iter()
                    .cloned()
                    .map(|kind| Box::new(kind) as Box<dyn rusqlite::ToSql>),
            );
        }
        sql.push_str(" ORDER BY seq ASC LIMIT ?");
        values.push(Box::new(limit));
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(
                params_from_iter(values.iter().map(|value| value.as_ref())),
                rule_event_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn event_kind_exists(&self, kind: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM rule_events WHERE kind=?)",
            [kind],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    fn projects_internal(&self, include_archived: bool) -> Result<Vec<ProjectRecord>> {
        let boards = {
            let mut sql = String::from(
                "SELECT board_path,name,last_used_at,archived_at,archived_by,archived_note,retirement_id FROM boards",
            );
            if !include_archived {
                sql.push_str(" WHERE archived=0");
            }
            sql.push_str(" ORDER BY board_path");
            let mut statement = self.connection.prepare(&sql)?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut projects = Vec::with_capacity(boards.len());
        for (
            board_path,
            name,
            last_used_at,
            archived_at,
            archived_by,
            archived_note,
            retirement_id,
        ) in boards
        {
            let workspace_roots =
                board_roots(&self.connection, &board_path, retirement_id.as_deref())?;
            projects.push(ProjectRecord {
                name,
                board_path,
                workspace_roots,
                last_used_at,
                archived: archived_at.is_some(),
                archived_at,
                archived_by,
                archived_note,
            });
        }
        projects.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        Ok(projects)
    }

    pub fn projects(&self) -> Result<Vec<ProjectRecord>> {
        self.projects_internal(true)
    }

    pub fn projects_active(&self) -> Result<Vec<ProjectRecord>> {
        self.projects_internal(false)
    }

    /// Projects carrying `name`. Names are not unique in the registry, so
    /// callers must handle 0 and >1 rather than assume a single hit.
    pub fn by_name(&self, name: &str) -> Result<Vec<ProjectRecord>> {
        Ok(self
            .projects_active()?
            .into_iter()
            .filter(|project| project.name == name)
            .collect())
    }

    pub fn by_name_active(&self, name: &str) -> Result<Vec<ProjectRecord>> {
        self.by_name(name)
    }

    pub fn by_name_all(&self, name: &str) -> Result<Vec<ProjectRecord>> {
        Ok(self
            .projects()?
            .into_iter()
            .filter(|project| project.name == name)
            .collect())
    }

    /// Mark a board used. Path-based resolution updates `last_used_at` as a
    /// side effect of walking the registry; name-based resolution has no walk,
    /// so without this the dashboard's recency ordering would freeze for any
    /// project addressed only by name.
    pub fn touch_board(&self, board_path: &str) -> Result<()> {
        let now = now_ms();
        self.connection.execute(
            "UPDATE boards SET last_used_at=? WHERE board_path=?",
            params![now, board_path],
        )?;
        Ok(())
    }

    pub fn integrity(&self) -> Result<Vec<String>> {
        integrity(&self.connection)
    }

    pub fn audit(&self) -> Result<crate::audit::AuditReport> {
        crate::audit::verify_registry(&self.connection)
    }

    pub fn record_system_event(&self, kind: &str, actor: &str, payload: Value) -> Result<()> {
        crate::audit::append_registry_event(
            &self.connection,
            "registry",
            kind,
            validate_rule_actor(actor)?,
            &payload.to_string(),
            now_ms(),
        )
    }

    pub fn backup(&self, destination: &Path) -> Result<()> {
        let mut target = create_backup_target(destination)?;
        let backup = rusqlite::backup::Backup::new(&self.connection, &mut target)?;
        backup.run_to_completion(64, std::time::Duration::from_millis(1), None)?;
        Ok(())
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = checkpoint(&self.connection);
    }
}

impl Registry {
    /// Registered roots that no longer resolve to themselves.
    pub fn unreachable_roots(&self) -> Result<Vec<UnreachableRoot>> {
        let mut out = Vec::new();
        for record in self.list(false)? {
            if record.rootless {
                continue;
            }
            let stored = Path::new(&record.root_path);
            let resolved = stored.canonicalize().ok();
            let reachable = resolved
                .as_ref()
                .is_some_and(|actual| actual.as_path() == stored);
            if reachable {
                continue;
            }
            out.push(UnreachableRoot {
                name: record.name,
                root_path: record.root_path,
                board_path: record.board_path,
                resolves_to: resolved.map(|p| p.to_string_lossy().into_owned()),
            });
        }
        Ok(out)
    }

    /// Point a registered root at where it actually lives now.
    ///
    /// The board, its name and every other root keep their identity: only the
    /// spelling of one path changes, which is the whole defect. A root that is
    /// simply gone is refused rather than guessed at — there is nothing to
    /// repoint it to, and inventing a path would be worse than the gap.
    pub fn repoint(&mut self, root_path: &str, actor: &str) -> Result<UnreachableRoot> {
        let actor = validate_rule_actor(actor)?;
        let broken = self
            .unreachable_roots()?
            .into_iter()
            .find(|item| item.root_path == root_path)
            .with_context(|| {
                format!("{root_path} is not a registered root that needs repointing")
            })?;
        let Some(target) = broken.resolves_to.clone() else {
            bail!(
                "{root_path} does not exist, so there is nowhere to repoint it. \
                 Register the project where it lives now with `kanban init`, or \
                 drop this root."
            );
        };
        // A row already standing at the destination would collide on the
        // primary key, and silently dropping either one loses a registration.
        if self.exact(Path::new(&target))?.is_some() {
            bail!(
                "{target} is already registered, so {root_path} has nothing to \
                 repoint to — the two would be one row"
            );
        }
        let now = now_ms();
        let board_path = broken.board_path.clone();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE workspace_roots SET root_path=?, last_used_at=? WHERE root_path=?",
            params![target.clone(), now, root_path],
        )?;
        transaction.execute(
            "UPDATE boards SET last_used_at=? WHERE board_path=?",
            params![now, board_path.clone()],
        )?;
        crate::audit::append_registry_event(
            &transaction,
            &format!("workspace:{target}"),
            "workspace_repointed",
            actor,
            &json!({"previousRootPath":root_path,"rootPath":target,"boardPath":board_path})
                .to_string(),
            now,
        )?;
        transaction.commit()?;
        Ok(UnreachableRoot {
            root_path: target.clone(),
            resolves_to: Some(target),
            ..broken
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AddTask;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::IntoRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::time::Duration;

    fn registry_db_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("kanban-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp registry dir");
        path.join("registry.db")
    }

    fn test_registry(name: &str) -> Registry {
        let root = registry_db_path(name)
            .parent()
            .expect("registry db parent")
            .to_path_buf();
        Registry {
            connection: open_registry(&root.join("registry.db")).expect("open test registry"),
            root,
            adoption_boards: None,
        }
    }

    fn insert_rule(registry: &Registry, id: &str, archived: i64) {
        registry
            .connection
            .execute(
                "INSERT INTO rules(id,body,author,archived,created_at,updated_at,tags) \
                 VALUES(?,?,?,?,?,?,?)",
                params![
                    id,
                    "Headline.\n\nBody.",
                    "codex",
                    archived,
                    1_i64,
                    1_i64,
                    "[\"ALL\"]",
                ],
            )
            .expect("insert rule");
    }

    fn rule_event_seqs(events: &[Event]) -> Vec<i64> {
        events.iter().map(|event| event.seq).collect()
    }

    fn source_board_path(registry: &Registry, name: &str) -> PathBuf {
        registry.root.join(format!("{name}.db"))
    }

    fn make_source_board(registry: &Registry, name: &str) -> Store {
        let path = source_board_path(registry, name);
        let mut store = Store::open(&path).expect("open source board");
        store
            .initialize(name, "codex")
            .expect("initialize source board");
        store
    }

    fn add_source_task(store: &mut Store, id: &str) {
        store
            .add_task(AddTask {
                id: Some(id.to_owned()),
                task_type: "task".to_owned(),
                parent_id: None,
                title: format!("Task {id}"),
                body: Some("Body.".to_owned()),
                assignee: None,
                lane: None,
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "todo".to_owned(),
                priority: 3,
                dependencies: Vec::new(),
                metadata: json!({}),
                actor: Some("codex".to_owned()),
                tags: Vec::new(),
            })
            .expect("add source task");
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SourceFileImage {
        name: String,
        bytes: Vec<u8>,
        mode: u32,
        device: u64,
        inode: u64,
        owner: u32,
        group: u32,
        length: u64,
        accessed: (i64, i64),
        modified: (i64, i64),
        changed: (i64, i64),
    }

    fn directory_image(path: &Path) -> Vec<SourceFileImage> {
        let mut image = fs::read_dir(path)
            .expect("read source directory")
            .map(|entry| {
                let entry = entry.expect("read source directory entry");
                let bytes = fs::read(entry.path()).expect("read source directory file");
                let metadata = entry.metadata().expect("stat source directory entry");
                SourceFileImage {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    bytes,
                    mode: metadata.permissions().mode(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    owner: metadata.uid(),
                    group: metadata.gid(),
                    length: metadata.len(),
                    accessed: (metadata.atime(), metadata.atime_nsec()),
                    modified: (metadata.mtime(), metadata.mtime_nsec()),
                    changed: (metadata.ctime(), metadata.ctime_nsec()),
                }
            })
            .collect::<Vec<_>>();
        image.sort_by(|left, right| left.name.cmp(&right.name));
        image
    }

    fn temp_rw_file(label: &str, contents: &[u8]) -> (PathBuf, i32) {
        let path = std::env::temp_dir().join(format!("kanban-{label}-{}", Uuid::new_v4()));
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create temp file");
        file.write_all(contents).expect("seed temp file");
        file.sync_all().expect("sync temp file");
        file.seek(SeekFrom::Start(0)).expect("rewind temp file");
        (path, file.into_raw_fd())
    }

    fn fd_cloexec(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC != 0 }
    }

    fn fd_identity(fd: i32) -> (u64, u64) {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        unsafe {
            assert!(libc::fstat(fd, stat.as_mut_ptr()) >= 0);
            let stat = stat.assume_init();
            (stat.st_dev as u64, stat.st_ino)
        }
    }

    #[test]
    #[ignore = "manipulates fixed helper fds and must be run in isolation"]
    fn workspace_adopt_fd_remap_handles_source_fd_equal_to_target_and_clears_cloexec() {
        let (root_path, root_fd) = temp_rw_file("workspace-adopt-remap-root", b"root");
        let (snapshot_path, snapshot_fd) =
            temp_rw_file("workspace-adopt-remap-snapshot", b"snapshot");
        let root_identity = fd_identity(root_fd);
        let snapshot_identity = fd_identity(snapshot_fd);
        unsafe {
            assert!(libc::dup2(root_fd, WORKSPACE_ADOPT_HELPER_ROOT_FD) >= 0);
            assert!(libc::dup2(snapshot_fd, WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD + 1) >= 0);
            assert!(
                libc::fcntl(
                    WORKSPACE_ADOPT_HELPER_ROOT_FD,
                    libc::F_SETFD,
                    libc::FD_CLOEXEC,
                ) >= 0
            );
            assert!(
                libc::fcntl(
                    WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD + 1,
                    libc::F_SETFD,
                    libc::FD_CLOEXEC,
                ) >= 0
            );
        }

        install_workspace_adopt_fds(
            WORKSPACE_ADOPT_HELPER_ROOT_FD,
            WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD + 1,
        )
        .expect("remap source fd that equals the target");

        assert_eq!(fd_identity(WORKSPACE_ADOPT_HELPER_ROOT_FD), root_identity);
        assert_eq!(
            fd_identity(WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD),
            snapshot_identity
        );
        assert!(!fd_cloexec(WORKSPACE_ADOPT_HELPER_ROOT_FD));
        assert!(!fd_cloexec(WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD));

        unsafe {
            libc::close(WORKSPACE_ADOPT_HELPER_ROOT_FD);
            libc::close(WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD);
            libc::close(WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD + 1);
            libc::close(root_fd);
            libc::close(snapshot_fd);
        }
        fs::remove_file(root_path).expect("remove root temp file");
        fs::remove_file(snapshot_path).expect("remove snapshot temp file");
    }

    #[test]
    #[ignore = "manipulates fixed helper fds and must be run in isolation"]
    fn workspace_adopt_fd_remap_handles_crossed_sources_and_clears_cloexec() {
        let (root_path, root_fd) = temp_rw_file("workspace-adopt-cross-root", b"root");
        let (snapshot_path, snapshot_fd) =
            temp_rw_file("workspace-adopt-cross-snapshot", b"snapshot");
        let root_identity = fd_identity(root_fd);
        let snapshot_identity = fd_identity(snapshot_fd);
        unsafe {
            assert!(libc::dup2(root_fd, WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD) >= 0);
            assert!(libc::dup2(snapshot_fd, WORKSPACE_ADOPT_HELPER_ROOT_FD) >= 0);
            assert!(
                libc::fcntl(
                    WORKSPACE_ADOPT_HELPER_ROOT_FD,
                    libc::F_SETFD,
                    libc::FD_CLOEXEC,
                ) >= 0
            );
            assert!(
                libc::fcntl(
                    WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD,
                    libc::F_SETFD,
                    libc::FD_CLOEXEC,
                ) >= 0
            );
        }

        install_workspace_adopt_fds(
            WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD,
            WORKSPACE_ADOPT_HELPER_ROOT_FD,
        )
        .expect("remap crossed source fds");

        assert_eq!(fd_identity(WORKSPACE_ADOPT_HELPER_ROOT_FD), root_identity);
        assert_eq!(
            fd_identity(WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD),
            snapshot_identity
        );
        assert!(!fd_cloexec(WORKSPACE_ADOPT_HELPER_ROOT_FD));
        assert!(!fd_cloexec(WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD));

        unsafe {
            libc::close(WORKSPACE_ADOPT_HELPER_ROOT_FD);
            libc::close(WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD);
            libc::close(root_fd);
            libc::close(snapshot_fd);
        }
        fs::remove_file(root_path).expect("remove root temp file");
        fs::remove_file(snapshot_path).expect("remove snapshot temp file");
    }

    #[test]
    fn millis_saturate_instead_of_wrapping() {
        assert_eq!(millis_of(Duration::from_millis(1_234)), 1_234);
        assert_eq!(millis_of(Duration::ZERO), 0);
        // The old `as i64` truncated the high bits of the u128 here, so an
        // absurd clock read as a small — or negative — instant.
        assert_eq!(millis_of(Duration::MAX), i64::MAX);
        assert!(millis_of(Duration::MAX) > 0);
    }

    #[test]
    fn a_working_clock_is_accepted_and_produces_a_positive_now() {
        // The refusal branch needs a system clock set before 1970, which this
        // test will not do to the host it runs on. What is asserted here is
        // that a sane clock is not refused, and that `now_ms` has no panic
        // path left: `millis_saturate_instead_of_wrapping` covers the
        // conversion that used to be the unsound part.
        assert!(require_sane_clock().is_ok());
        assert!(now_ms() > 1_700_000_000_000, "now_ms went backwards");
    }

    #[test]
    fn selector_and_subsystem_tags_intersect_fail_closed() {
        let tags = vec!["ONLY:ONE".to_owned(), "infra".to_owned()];
        let infra = HashSet::from(["infra".to_owned()]);
        let docs = HashSet::from(["docs".to_owned()]);
        assert!(rule_tags_apply(&tags, Some("ONE"), Some(&infra)));
        assert!(!rule_tags_apply(&tags, Some("TWO"), Some(&infra)));
        assert!(!rule_tags_apply(&tags, Some("ONE"), Some(&docs)));
        assert!(!rule_tags_apply(&tags, Some("ONE"), None));

        let general = vec!["ALL".to_owned(), "EXCEPT:TWO".to_owned()];
        assert!(rule_tags_apply(&general, Some("ONE"), None));
        assert!(!rule_tags_apply(&general, Some("TWO"), None));
    }

    #[test]
    fn ascending_registry_rule_events_resume_without_skips() {
        let registry = test_registry("registry-events");
        insert_rule(&registry, "rule-1", 1);
        insert_rule(&registry, "rule-2", 0);

        crate::audit::append_registry_event(
            &registry.connection,
            "rule-1",
            "rule_created",
            "codex",
            r#"{"step":1}"#,
            10,
        )
        .expect("append first registry event");
        crate::audit::append_registry_event(
            &registry.connection,
            "rule-2",
            "rule_created",
            "codex",
            r#"{"step":2}"#,
            11,
        )
        .expect("append second registry event");
        crate::audit::append_registry_event(
            &registry.connection,
            "rule-1",
            "rule_updated",
            "codex",
            r#"{"step":3}"#,
            12,
        )
        .expect("append third registry event");
        crate::audit::append_registry_event(
            &registry.connection,
            "rule-1",
            "rule_retired",
            "codex",
            r#"{"step":4}"#,
            13,
        )
        .expect("append fourth registry event");

        let all = registry
            .rule_events_since(None, None, 0, 10)
            .expect("read all registry events");
        assert_eq!(rule_event_seqs(&all), vec![1, 2, 3, 4]);
        assert!(all.iter().all(|event| !event.archived));

        let rule_events = registry
            .rule_events_since(Some("rule-1"), None, 1, 10)
            .expect("resume rule events");
        assert_eq!(rule_event_seqs(&rule_events), vec![3, 4]);

        let kind_events = registry
            .rule_events_since(Some("rule-1"), Some("rule_retired"), 0, 10)
            .expect("filter by kind");
        assert_eq!(rule_event_seqs(&kind_events), vec![4]);

        let empty = registry
            .rule_events_since(None, None, 0, 0)
            .expect("zero limit is allowed");
        assert!(empty.is_empty());

        let first_batch = registry
            .rule_events_since(Some("rule-1"), None, 0, 1)
            .expect("first batch");
        assert_eq!(rule_event_seqs(&first_batch), vec![1]);

        let second_batch = registry
            .rule_events_since(Some("rule-1"), None, first_batch.last().unwrap().seq, 1)
            .expect("second batch");
        assert_eq!(rule_event_seqs(&second_batch), vec![3]);

        let third_batch = registry
            .rule_events_since(Some("rule-1"), None, second_batch.last().unwrap().seq, 1)
            .expect("third batch");
        assert_eq!(rule_event_seqs(&third_batch), vec![4]);

        let negative = registry
            .rule_events_since(None, None, 0, -1)
            .expect_err("negative limits must be rejected")
            .to_string();
        assert!(negative.contains("1000"), "{negative}");

        let over = registry
            .rule_events_since(None, None, 0, crate::WATCH_BATCH_LIMIT + 1)
            .expect_err("over-cap limits must be rejected")
            .to_string();
        assert!(over.contains("1000"), "{over}");
    }

    #[test]
    fn filtered_registry_watch_rows_apply_multi_kind_before_limit() {
        let registry = test_registry("registry-filtered-events");
        insert_rule(&registry, "rule-1", 0);
        for (kind, step) in [("legacy", 1), ("wanted", 2), ("other", 3)] {
            crate::audit::append_registry_event(
                &registry.connection,
                "rule-1",
                kind,
                "codex",
                &format!(r#"{{"step":{step}}}"#),
                step,
            )
            .unwrap();
        }
        let kinds = vec!["wanted".to_owned(), "other".to_owned()];
        let rows = registry
            .rule_events_since_filtered(Some("rule-1"), &kinds, 0, 1)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "wanted");
        assert!(
            registry
                .rule_events_since_filtered(Some("rule-1"), &["never-seen".to_owned()], 0, 10,)
                .unwrap()
                .is_empty()
        );
        assert!(registry.event_kind_exists("wanted").unwrap());
        assert!(!registry.event_kind_exists("never-seen").unwrap());
    }

    #[test]
    fn registering_the_same_root_and_name_reuses_the_board_and_marks_it_existing() {
        let mut registry = test_registry("registry-reregister");
        let root = registry.root.join("workspace");
        fs::create_dir_all(&root).expect("create workspace root");

        let first = registry
            .register(Some(&root), "Alpha", false, "codex")
            .expect("initial register");
        let second = registry
            .register(Some(&root), "Alpha", false, "codex")
            .expect("repeat register");

        assert_eq!(first.board_path, second.board_path);
        assert_eq!(first.name, second.name);
        assert_eq!(second.workspace_roots.len(), 1);
        assert_eq!(
            second.workspace_roots[0],
            root.canonicalize()
                .expect("canonicalize root")
                .to_string_lossy()
                .into_owned()
        );

        let event_count: i64 = registry
            .connection
            .query_row(
                "SELECT count(*) FROM rule_events WHERE kind='workspace_registered'",
                [],
                |row| row.get(0),
            )
            .expect("count register events");
        assert_eq!(event_count, 2);

        let payload: String = registry
            .connection
            .query_row(
                "SELECT payload FROM rule_events ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("read register payload");
        let payload: serde_json::Value = serde_json::from_str(&payload).expect("parse payload");
        assert_eq!(payload["existing"], true);
        assert_eq!(payload["name"], "Alpha");
        assert_eq!(payload["boardPath"], first.board_path);
    }

    #[test]
    fn retiring_and_unretiring_a_project_preserves_identity_and_note() {
        let mut registry = test_registry("registry-retire-unretire");
        let root = registry.root.join("workspace");
        let extra_root = registry.root.join("workspace-spare");
        fs::create_dir_all(&root).expect("create workspace root");
        fs::create_dir_all(&extra_root).expect("create spare workspace root");
        let root = root.canonicalize().expect("canonicalize workspace root");
        let extra_root = extra_root
            .canonicalize()
            .expect("canonicalize spare workspace root");
        let root_text = root.to_string_lossy().into_owned();
        let extra_text = extra_root.to_string_lossy().into_owned();

        let created = registry
            .register(Some(&root), "Alpha", false, "codex")
            .expect("register project");
        assert_eq!(created.name, "Alpha");
        assert_eq!(created.workspace_roots, vec![root_text.clone()]);

        let attached = registry
            .attach(&extra_root, "Alpha", "geo")
            .expect("attach spare root");
        assert_eq!(attached.root_path, extra_text);
        assert_eq!(attached.board_path, created.board_path);

        let active = registry.by_name("Alpha").expect("active lookup");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].workspace_roots.len(), 2);
        assert!(active[0].workspace_roots.contains(&root_text));
        assert!(active[0].workspace_roots.contains(&extra_text));

        let retired = registry
            .retire("Alpha", "geo", "retire for test")
            .expect("retire project");
        assert_eq!(retired.name, "Alpha");
        assert!(retired.archived_at.is_some());
        assert_eq!(retired.archived_by.as_deref(), Some("geo"));
        assert_eq!(retired.archived_note.as_deref(), Some("retire for test"));
        assert_eq!(retired.workspace_roots.len(), 2);
        assert!(retired.workspace_roots.contains(&root_text));
        assert!(retired.workspace_roots.contains(&extra_text));
        assert!(registry.by_name("Alpha").expect("active lookup").is_empty());

        let archived = registry.by_name_all("Alpha").expect("archived lookup");
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].archived_note.as_deref(),
            Some("retire for test")
        );
        assert_eq!(archived[0].workspace_roots.len(), 2);
        assert!(archived[0].workspace_roots.contains(&root_text));
        assert!(archived[0].workspace_roots.contains(&extra_text));

        let restored = registry.unretire("Alpha", "geo").expect("unretire project");
        assert_eq!(restored.name, "Alpha");
        assert!(restored.archived_at.is_none());
        assert!(restored.archived_by.is_none());
        assert!(restored.archived_note.is_none());
        assert_eq!(restored.workspace_roots.len(), 2);
        assert!(restored.workspace_roots.contains(&root_text));
        assert!(restored.workspace_roots.contains(&extra_text));
        assert_eq!(registry.by_name("Alpha").expect("active lookup").len(), 1);
        assert!(
            registry
                .by_name_all("Alpha")
                .expect("all lookup")
                .iter()
                .any(|project| project.archived_at.is_none())
        );
    }

    #[test]
    fn adopting_a_board_rejects_a_duplicate_active_name() {
        let mut registry = test_registry("adopt-duplicate-name");
        registry
            .register(None, "Alpha", false, "codex")
            .expect("seed duplicate active board");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");

        let err = registry
            .adopt(
                &source_board_path(&registry, "Alpha"),
                "Alpha",
                None,
                true,
                "codex",
            )
            .expect_err("duplicate active name must be refused")
            .to_string();
        assert!(err.contains("already named Alpha"), "{err}");
    }

    #[test]
    fn adopting_a_board_rejects_a_wrong_source_name() {
        let mut registry = test_registry("adopt-wrong-name");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");

        let err = registry
            .adopt(
                &source_board_path(&registry, "Alpha"),
                "Beta",
                None,
                true,
                "codex",
            )
            .expect_err("wrong source name must be refused")
            .to_string();
        assert!(
            err.contains("source board name is Alpha, not Beta"),
            "{err}"
        );
    }

    #[test]
    fn adopting_a_board_rejects_a_corrupt_source_file() {
        let mut registry = test_registry("adopt-corrupt-source");
        let source = source_board_path(&registry, "corrupt");
        fs::write(&source, b"not a sqlite database").expect("write corrupt source");

        let err = registry
            .adopt(&source, "corrupt", None, true, "codex")
            .expect_err("corrupt source must be refused")
            .to_string();
        assert!(
            err.contains("open source board")
                || err.contains("readable Kanban board file")
                || err.contains("file is not a database"),
            "{err}"
        );
    }

    #[test]
    fn adopting_a_board_rejects_an_audit_invalid_source() {
        let mut registry = test_registry("adopt-invalid-audit");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        drop(source);

        let source_path = source_board_path(&registry, "Alpha");
        let connection = Connection::open(&source_path).expect("reopen source board");
        connection
            .execute(
                "UPDATE events SET event_hash='bad' WHERE seq=(SELECT max(seq) FROM events)",
                [],
            )
            .expect("tamper event hash");

        let err = registry
            .adopt(&source_path, "Alpha", None, true, "codex")
            .expect_err("audit-invalid source must be refused")
            .to_string();
        assert!(err.contains("invalid audit chain"), "{err}");
    }

    #[test]
    fn adopting_a_board_rejects_a_newer_schema_source() {
        let mut registry = test_registry("adopt-newer-schema");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        drop(source);

        let source_path = source_board_path(&registry, "Alpha");
        let connection = Connection::open(&source_path).expect("reopen source board");
        connection
            .pragma_update(
                None,
                "user_version",
                (crate::db::BOARD_SCHEMA_VERSION as i64) + 1,
            )
            .expect("bump schema version");

        let err = registry
            .adopt(&source_path, "Alpha", None, true, "codex")
            .expect_err("newer schema source must be refused")
            .to_string();
        assert!(err.contains("newer than supported version"), "{err}");
    }

    #[test]
    fn adopting_a_board_rejects_a_symlink_source_path() {
        let mut registry = test_registry("adopt-symlink-source");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        let source_path = source_board_path(&registry, "Alpha");
        let source_link = registry.root.join("source-link.db");
        symlink(&source_path, &source_link).expect("create source symlink");

        let err = registry
            .adopt(&source_link, "Alpha", None, true, "codex")
            .expect_err("symlink source must be refused")
            .to_string();
        assert!(err.contains("is a symlink"), "{err}");

        let registered: i64 = registry
            .connection
            .query_row("SELECT count(*) FROM boards", [], |row| row.get(0))
            .expect("count registered boards");
        assert_eq!(registered, 0, "symlink refusal published a board row");
    }

    #[test]
    fn source_preflight_does_not_create_or_modify_source_sidecars() {
        let source_dir =
            std::env::temp_dir().join(format!("kanban-adopt-source-{}", Uuid::new_v4()));
        fs::create_dir(&source_dir).expect("create independent source directory");
        let source_path = source_dir.join("Alpha.db");
        let mut source = Store::open(&source_path).expect("open independent source board");
        source
            .initialize("Alpha", "codex")
            .expect("initialize independent source board");
        add_source_task(&mut source, "t-alpha");
        assert!(
            database_artifact_path(&source_path, "-wal").exists(),
            "fixture did not exercise a live WAL"
        );
        assert!(
            database_artifact_path(&source_path, "-shm").exists(),
            "fixture did not exercise an existing SHM sidecar"
        );
        let before = directory_image(&source_dir);

        let prepared = PreparedAdoption::prepare(&source_path, "Alpha").expect("preflight source");
        let after = directory_image(&source_dir);
        assert_eq!(after, before, "preflight mutated source files or sidecars");
        prepared.cleanup().expect("cleanup prepared snapshot");
        drop(source);
        fs::remove_dir_all(&source_dir).expect("remove source fixture");
    }

    #[test]
    fn adoption_rejects_registry_boards_symlink_without_external_write_or_event() {
        let mut registry = test_registry("adopt-boards-symlink");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        drop(source);
        let source_path = source_board_path(&registry, "Alpha");
        let prepared = PreparedAdoption::prepare(&source_path, "Alpha").expect("preflight source");
        let external =
            std::env::temp_dir().join(format!("kanban-adopt-external-{}", Uuid::new_v4()));
        fs::create_dir(&external).expect("create external target");
        symlink(&external, registry.root.join("boards")).expect("plant boards symlink");

        let error = registry
            .adopt_prepared(prepared, None, true, "codex")
            .expect_err("boards symlink must be refused")
            .to_string();
        assert!(
            error.contains("registry boards directory")
                || error.contains("symlink")
                || error.contains("non-directory"),
            "{error}"
        );
        assert_eq!(
            fs::read_dir(&external).unwrap().count(),
            0,
            "adoption wrote outside registry root"
        );
        let boards: i64 = registry
            .connection
            .query_row("SELECT count(*) FROM boards", [], |row| row.get(0))
            .unwrap();
        let events: i64 = registry
            .connection
            .query_row(
                "SELECT count(*) FROM rule_events WHERE kind='board_adopted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(boards, 0);
        assert_eq!(events, 0);
        fs::remove_file(registry.root.join("boards")).expect("remove fixture symlink");
        fs::remove_dir(&external).expect("remove external fixture");
    }

    #[test]
    fn adoption_registry_walk_rejects_an_intermediate_symlink_without_external_creation() {
        let base = std::env::temp_dir().join(format!("kanban-adopt-chain-{}", Uuid::new_v4()));
        let external =
            std::env::temp_dir().join(format!("kanban-adopt-chain-out-{}", Uuid::new_v4()));
        fs::create_dir(&base).expect("create registry chain fixture");
        fs::create_dir(&external).expect("create external chain target");
        symlink(&external, base.join("redirect")).expect("plant intermediate symlink");
        let escaped_root = base.join("redirect").join("live");

        let error = secure_registry_dirs(&escaped_root, true)
            .expect_err("intermediate registry symlink must be refused")
            .to_string();
        assert!(error.contains("registry path component"), "{error}");
        assert!(
            !external.join("live").exists(),
            "registry walk created state beyond an intermediate symlink"
        );
        fs::remove_file(base.join("redirect")).expect("remove fixture symlink");
        fs::remove_dir(&base).expect("remove registry chain fixture");
        fs::remove_dir(&external).expect("remove external chain fixture");
    }

    #[test]
    fn adopting_a_board_rejects_parent_traversal_in_the_source_path() {
        let mut registry = test_registry("adopt-parent-traversal-source");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        let child = registry.root.join("child");
        fs::create_dir_all(&child).expect("create traversal fixture directory");
        let traversing_source = child.join("..").join("Alpha.db");

        let err = registry
            .adopt(&traversing_source, "Alpha", None, true, "codex")
            .expect_err("parent-traversing source must be refused")
            .to_string();
        assert!(err.contains("contains parent traversal"), "{err}");

        let registered: i64 = registry
            .connection
            .query_row("SELECT count(*) FROM boards", [], |row| row.get(0))
            .expect("count registered boards");
        assert_eq!(registered, 0, "path refusal published a board row");
    }

    #[test]
    fn adopting_a_board_rejects_foreign_key_corruption() {
        let mut registry = test_registry("adopt-foreign-key-corruption");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        drop(source);

        let source_path = source_board_path(&registry, "Alpha");
        let connection = Connection::open(&source_path).expect("reopen source board");
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("disable foreign keys for corruption fixture");
        connection
            .execute(
                "UPDATE tasks SET parent_id='t-missing' WHERE id='t-alpha'",
                [],
            )
            .expect("write orphaned parent reference");
        drop(connection);

        let err = registry
            .adopt(&source_path, "Alpha", None, true, "codex")
            .expect_err("foreign-key-invalid source must be refused")
            .to_string();
        assert!(err.contains("foreign key violations"), "{err}");

        let registered: i64 = registry
            .connection
            .query_row("SELECT count(*) FROM boards", [], |row| row.get(0))
            .expect("count registered boards");
        assert_eq!(registered, 0, "foreign-key refusal published a board row");
    }

    #[test]
    fn adopting_a_rootless_board_records_null_root_and_snapshot_provenance() {
        let mut registry = test_registry("adopt-rootless");
        let source_path = source_board_path(&registry, "Alpha");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        let source_path = source_path.canonicalize().expect("canonicalize source");

        let receipt = registry
            .adopt(&source_path, "Alpha", None, true, "codex")
            .expect("rootless adopt");
        assert!(receipt.root_path.is_none());
        assert!(receipt.project.workspace_roots.is_empty());
        assert_eq!(receipt.source_sha256.len(), 64);
        assert!(receipt.source_bytes > 0);
        assert_eq!(
            receipt.source_board_path,
            source_path.to_string_lossy().into_owned()
        );

        let payload: (String, String, String) = registry
            .connection
            .query_row(
                "SELECT rule_id,kind,payload FROM rule_events ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read adoption event");
        assert_eq!(
            payload.0,
            format!("workspace:{}", receipt.project.board_path)
        );
        assert_eq!(payload.1, "board_adopted");
        let payload: serde_json::Value =
            serde_json::from_str(&payload.2).expect("parse adoption event payload");
        assert_eq!(payload["rootPath"], serde_json::Value::Null);
        assert_eq!(
            payload["sourceBoardPath"],
            serde_json::Value::String(source_path.to_string_lossy().into_owned())
        );
        assert_eq!(payload["sourceSha256"], receipt.source_sha256);
        assert_eq!(payload["sourceBytes"], json!(receipt.source_bytes));
    }

    #[test]
    fn adoption_pins_validation_and_backup_to_one_snapshot_during_a_concurrent_commit() {
        let mut registry = test_registry("adopt-concurrent-source");
        let workspace = registry.root.join("workspace");
        fs::create_dir_all(&workspace).expect("create destination workspace");
        let source_path = source_board_path(&registry, "Alpha");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-before-snapshot");
        let source_path = source_path.canonicalize().expect("canonicalize source");
        let prepared = PreparedAdoption::prepare_with_hook(&source_path, "Alpha", || {
            add_source_task(&mut source, "t-after-snapshot");
            Ok(())
        })
        .expect("prepare pinned source snapshot");
        let receipt = registry
            .adopt_prepared(prepared, Some(&workspace), false, "codex")
            .expect("adopt pinned source snapshot");

        assert_eq!(
            receipt.source_sha256,
            crate::audit::file_sha256(Path::new(&receipt.project.board_path)).unwrap()
        );
        assert_eq!(
            receipt.source_bytes,
            fs::metadata(&receipt.project.board_path).unwrap().len()
        );
        assert!(
            source.require_task("t-after-snapshot").is_ok(),
            "the concurrent source commit did not land"
        );
        let adopted = Store::open(Path::new(&receipt.project.board_path)).expect("open adopted");
        assert!(adopted.require_task("t-before-snapshot").is_ok());
        assert!(
            adopted.require_task("t-after-snapshot").is_err(),
            "adopted board included a commit outside the validated snapshot"
        );
    }

    #[test]
    fn adoption_does_not_reopen_a_replaced_source_path_after_preflight() {
        let mut registry = test_registry("adopt-replaced-source");
        let source_path = source_board_path(&registry, "Alpha");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-pinned");
        drop(source);
        let prepared = PreparedAdoption::prepare(&source_path, "Alpha").expect("preflight source");
        let original = registry.root.join("original-source.db");
        fs::rename(&source_path, &original).expect("move checked source inode");
        fs::write(&source_path, b"replacement is not sqlite").expect("replace source path");

        let receipt = registry
            .adopt_prepared(prepared, None, true, "codex")
            .expect("publish the already prepared inode snapshot");
        let adopted =
            Store::open(Path::new(&receipt.project.board_path)).expect("open adopted board");
        assert!(
            adopted.require_task("t-pinned").is_ok(),
            "adoption reopened and followed the replacement source path"
        );
        assert_eq!(
            fs::read(&source_path).unwrap(),
            b"replacement is not sqlite"
        );
    }

    #[test]
    fn adoption_failure_removes_database_wal_shm_and_rollback_journal() {
        let staging = create_staging_dir().expect("create cleanup fixture");
        let attempted_path = staging.join("partial.db");
        for artifact in [
            attempted_path.clone(),
            database_artifact_path(&attempted_path, "-wal"),
            database_artifact_path(&attempted_path, "-shm"),
            database_artifact_path(&attempted_path, "-journal"),
        ] {
            fs::write(&artifact, b"partial").expect("write partial artifact");
        }
        cleanup_database_artifacts(&attempted_path).expect("cleanup every SQLite artifact");
        for artifact in [
            attempted_path.clone(),
            database_artifact_path(&attempted_path, "-wal"),
            database_artifact_path(&attempted_path, "-shm"),
            database_artifact_path(&attempted_path, "-journal"),
        ] {
            assert!(
                !artifact.exists(),
                "failed adoption left {} behind",
                artifact.display()
            );
        }
        fs::remove_dir(&staging).expect("remove empty staging fixture");
    }

    #[test]
    fn adoption_cleanup_failure_is_reported_and_retains_evidence() {
        let mut registry = test_registry("adopt-cleanup-failure");
        let mut source = make_source_board(&registry, "Alpha");
        add_source_task(&mut source, "t-alpha");
        drop(source);
        let prepared = PreparedAdoption::prepare(&source_board_path(&registry, "Alpha"), "Alpha")
            .expect("prepare adoption");
        let staging = prepared.staging_dir.clone();
        let evidence = staging.join("cannot-remove-as-file");
        fs::create_dir(&evidence).expect("create unremovable entry fixture");
        let error = registry
            .adopt_prepared(prepared, None, true, "codex")
            .expect_err("cleanup failure must abort publication")
            .to_string();
        assert!(error.contains("adoption cleanup failed"), "{error}");
        assert!(evidence.is_dir(), "cleanup failure erased its evidence");
        let registered: i64 = registry
            .connection
            .query_row("SELECT count(*) FROM boards", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            registered, 1,
            "cleanup failure rolled back the registry row"
        );
        assert_eq!(
            fs::read_dir(registry.root.join("boards")).unwrap().count(),
            1,
            "cleanup failure left the published destination in place"
        );
        fs::remove_dir(&evidence).expect("remove fixture evidence");
        fs::remove_dir(&staging).expect("remove fixture staging");
    }

    #[test]
    fn reconciling_a_pending_adoption_marker_removes_the_orphan_board_and_staging() {
        let registry = test_registry("adopt-reconcile-marker");
        let board_name = format!("{}.db", Uuid::new_v4());
        let board_path = registry.root.join("boards").join(&board_name);
        let staging_dir = adoption_staging_root(&registry.root).join(Uuid::new_v4().to_string());
        create_private_dir_all(&staging_dir).expect("create adoption staging");
        fs::create_dir_all(board_path.parent().unwrap()).expect("create boards dir");
        fs::write(&board_path, b"orphan board").expect("write orphan board");
        fs::write(database_artifact_path(&board_path, "-wal"), b"wal").expect("write wal");
        fs::write(database_artifact_path(&board_path, "-shm"), b"shm").expect("write shm");
        fs::write(database_artifact_path(&board_path, "-journal"), b"journal")
            .expect("write journal");
        let marker = AdoptionMarker {
            board_name: "Alpha".to_owned(),
            board_path: board_path.to_string_lossy().into_owned(),
            root_path: None,
            source_board_path: registry
                .root
                .join("source.db")
                .to_string_lossy()
                .into_owned(),
            staging_dir: staging_dir.to_string_lossy().into_owned(),
            created_at: now_ms(),
        };
        write_adoption_marker(&adoption_marker_path(&registry.root), &marker)
            .expect("write adoption marker");

        reconcile_pending_adoption(&registry.root).expect("reconcile pending adoption");

        assert!(
            !adoption_marker_path(&registry.root).exists(),
            "reconciliation left the marker behind"
        );
        assert!(
            !board_path.exists(),
            "reconciliation left the orphan board behind"
        );
        assert!(
            !staging_dir.exists(),
            "reconciliation left the staging directory behind"
        );
    }
}
