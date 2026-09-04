use crate::WATCH_BATCH_LIMIT;
use crate::db::{
    SnapshotSource, checkpoint, create_backup_target, integrity, open_board, open_registry,
    open_registry_readonly, own_private_dir,
};
use crate::model::{
    Event, ProjectRecord, Rule, RuleMigrationReport, RuleSummary, UnreachableRoot, WorkspaceRecord,
};
use crate::store::{Store, event, validate_tag_name};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use uuid::Uuid;

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
    })
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

pub struct Registry {
    pub connection: Connection,
    root: PathBuf,
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
        let connection = open_registry(&root.join("registry.db"))?;
        Ok(Self { connection, root })
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

    /// The nearest registered workspace strictly above `workspace`, if any.
    /// Read-only: unlike `resolve`, it does not touch `last_used_at`.
    fn enclosing(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let mut cursor = workspace.to_path_buf();
        while cursor.pop() {
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
        }
        Ok(None)
    }

    pub fn exact(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let text = workspace.to_string_lossy();
        let canonical = self
            .connection
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
            .optional()?;
        if canonical.is_some() {
            return Ok(canonical);
        }
        Ok(None)
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
    use std::fs;
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
}
