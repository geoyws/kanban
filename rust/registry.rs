use crate::db::{
    checkpoint, create_backup_target, integrity, open_board, open_registry, open_registry_readonly,
    own_private_dir,
};
use crate::model::{
    Event, ProjectRecord, Rule, RuleMigrationReport, RuleSummary, UnreachableRoot, WorkspaceRecord,
};
use crate::store::{Store, event, validate_tag_name};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde_json::json;
use std::collections::HashSet;
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

fn global_rule_row(record: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    let encoded: String = record.get("board_tags")?;
    let board_tags = serde_json::from_str(&encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            encoded.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    let encoded_task_tags: String = record.get("task_tags")?;
    let task_tags = serde_json::from_str(&encoded_task_tags).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            encoded_task_tags.len(),
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
        board_tags: Some(board_tags),
        task_tags,
    })
}

fn board_tags_apply(tags: Option<&[String]>, board_name: Option<&str>) -> bool {
    let tags = tags.unwrap_or(&[]);
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

fn row(record: &rusqlite::Row<'_>, canonical: bool) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        root_path: record.get("root_path")?,
        name: record.get("name")?,
        board_path: record.get("board_path")?,
        canonical,
        created_at: record.get("created_at")?,
        last_used_at: record.get("last_used_at")?,
        archived: record.get::<_, i64>("archived")? != 0,
        archived_at: record.get("archived_at")?,
        archived_by: record.get("archived_by")?,
    })
}

fn history_row(record: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        root_path: record.get("root_path")?,
        name: record.get("name")?,
        board_path: record.get("board_path")?,
        canonical: false,
        created_at: record.get("created_at")?,
        last_used_at: record.get("last_used_at")?,
        archived: true,
        archived_at: record.get("archived_at")?,
        archived_by: record.get("archived_by")?,
    })
}

pub struct Registry {
    pub connection: Connection,
    root: PathBuf,
}

impl Registry {
    pub fn open() -> Result<Self> {
        let root = data_root()?;
        own_private_dir(&root)?;
        let connection = open_registry(&root.join("registry.db"))?;
        Ok(Self { connection, root })
    }

    pub fn open_readonly() -> Result<Self> {
        let root = data_root()?;
        let connection = open_registry_readonly(&root.join("registry.db"))?;
        Ok(Self { connection, root })
    }

    pub fn register(
        &mut self,
        workspace: &Path,
        name: &str,
        force: bool,
    ) -> Result<WorkspaceRecord> {
        let root_path = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        let root_text = root_path.to_string_lossy().into_owned();
        let now = now_ms();
        // An init below a registered root used to create a second board that
        // shadowed the first: tasks added from the subdirectory resolved to the
        // nearer board and were invisible from the project root. Attaching is
        // almost always what was meant; nesting has to be asked for.
        if !force
            && self.exact(&root_path)?.is_none()
            && let Some(enclosing) = self.enclosing(&root_path)?
        {
            bail!(
                "{} is already inside Kanban project {} ({}).\n\
                 To share that project's board:   kanban workspace attach --to {}\n\
                 To create a separate board here: kanban init --name {name} --force",
                root_path.display(),
                enclosing.name,
                enclosing.root_path,
                enclosing.root_path
            );
        }
        if let Some(existing) = self.exact(&root_path)? {
            let table = if existing.canonical {
                "workspaces"
            } else {
                "workspace_aliases"
            };
            self.connection.execute(
                &format!("UPDATE {table} SET name=?,last_used_at=? WHERE root_path=?"),
                params![name, now, root_text],
            )?;
            return self
                .exact(&root_path)?
                .context("registered workspace disappeared");
        }
        let board_path = self
            .root
            .join("boards")
            .join(format!("{}.db", Uuid::new_v4()));
        self.connection.execute(
            "INSERT INTO workspaces(root_path,name,board_path,created_at,last_used_at) VALUES(?,?,?,?,?)",
            params![root_text, name, board_path.to_string_lossy(), now, now],
        )?;
        self.exact(&root_path)?
            .context("registered workspace not found")
    }

    /// The nearest registered workspace strictly above `workspace`, if any.
    /// Read-only: unlike `resolve`, it does not touch `last_used_at`.
    fn enclosing(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let mut cursor = workspace.to_path_buf();
        while cursor.pop() {
            if let Some(found) = self.exact(&cursor)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    pub fn exact(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let text = workspace.to_string_lossy();
        let canonical = self
            .connection
            .query_row(
                "SELECT * FROM workspaces WHERE root_path=? AND archived=0",
                [text.as_ref()],
                |r| row(r, true),
            )
            .optional()?;
        if canonical.is_some() {
            return Ok(canonical);
        }
        self.connection
            .query_row(
                "SELECT * FROM workspace_aliases WHERE root_path=? AND archived=0",
                [text.as_ref()],
                |r| row(r, false),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn resolve(&mut self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let mut cursor = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        loop {
            if let Some(found) = self.exact(&cursor)? {
                let table = if found.canonical {
                    "workspaces"
                } else {
                    "workspace_aliases"
                };
                self.connection.execute(
                    &format!("UPDATE {table} SET last_used_at=? WHERE root_path=?"),
                    params![now_ms(), found.root_path],
                )?;
                return self.exact(&cursor);
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
            if !cursor.pop() {
                return Ok(None);
            }
        }
    }

    pub fn attach(
        &mut self,
        workspace: &Path,
        project_workspace: &Path,
    ) -> Result<WorkspaceRecord> {
        let root = workspace.canonicalize()?;
        let project = self.resolve(project_workspace)?.with_context(|| {
            format!("no Kanban project contains {}", project_workspace.display())
        })?;
        if let Some(existing) = self.exact(&root)? {
            if existing.board_path != project.board_path {
                bail!(
                    "{} is already attached to another Kanban project",
                    root.display()
                );
            }
            return Ok(existing);
        }
        let canonical_name: String = self.connection.query_row(
            "SELECT name FROM workspaces WHERE board_path=?",
            [&project.board_path],
            |row| row.get(0),
        )?;
        let now = now_ms();
        self.connection.execute(
            "INSERT INTO workspace_aliases(root_path,name,board_path,created_at,last_used_at) VALUES(?,?,?,?,?)",
            params![root.to_string_lossy(), canonical_name, project.board_path, now, now],
        )?;
        self.exact(&root)?.context("attached workspace not found")
    }

    pub fn list(&self, include_archived: bool) -> Result<Vec<WorkspaceRecord>> {
        let mut out = Vec::new();
        let suffix = if include_archived {
            " ORDER BY last_used_at DESC"
        } else {
            " WHERE archived=0 ORDER BY last_used_at DESC"
        };
        for (table, canonical) in [("workspaces", true), ("workspace_aliases", false)] {
            let sql = format!("SELECT * FROM {table}{suffix}");
            let mut statement = self.connection.prepare(&sql)?;
            out.extend(
                statement
                    .query_map([], |r| row(r, canonical))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        if include_archived {
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
        let alias = self
            .connection
            .query_row(
                "SELECT * FROM workspace_aliases WHERE root_path=?",
                [root_path],
                |record| row(record, false),
            )
            .optional()?;
        let Some(alias) = alias else {
            if self
                .connection
                .query_row(
                    "SELECT 1 FROM workspaces WHERE root_path=? AND archived=0",
                    [root_path],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                bail!(
                    "{root_path} is a canonical project root and cannot be detached; repoint it or attach another worktree instead"
                );
            }
            let retired = self
                .connection
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
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO workspace_alias_history(root_path,name,board_path,created_at,last_used_at,archived_at,archived_by) VALUES(?,?,?,?,?,?,?)",
            params![alias.root_path,alias.name,alias.board_path,alias.created_at,alias.last_used_at,now,actor],
        )?;
        transaction.execute(
            "DELETE FROM workspace_aliases WHERE root_path=?",
            [root_path],
        )?;
        let retired = WorkspaceRecord {
            archived: true,
            archived_at: Some(now),
            archived_by: Some(actor.to_owned()),
            ..alias
        };
        transaction.commit()?;
        Ok(retired)
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
            match self.by_name(name)?.len() {
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
        for project in self.projects()? {
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
            let mut statement = self.connection.prepare(
                "SELECT name,board_path FROM workspaces WHERE archived=0 ORDER BY name,board_path",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut report = RuleMigrationReport {
            boards_migrated: 0,
            boards_already_migrated: 0,
            rules_imported: 0,
            rules_already_imported: 0,
            source_rules_retired: 0,
        };

        for (board_name, board_path) in projects {
            let duplicate_names: i64 = self.connection.query_row(
                "SELECT count(*) FROM workspaces WHERE archived=0 AND name=?",
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
                    tags.extend(source.task_tags.iter().cloned());
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
                    transaction.execute(
                        "INSERT INTO rule_events(rule_id,kind,actor,payload,created_at) VALUES(?,?,?,?,?)",
                        params![
                            id,
                            "rule_consolidated",
                            actor,
                            json!({
                                "ruleID": id,
                                "sourceBoard": board_name,
                                "sourceRuleID": source.id,
                                "tags": tags,
                            })
                            .to_string(),
                            now,
                        ],
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

    pub fn add_global_rule(
        &mut self,
        body: &str,
        actor: &str,
        board_tags: &[String],
        task_tags: &[String],
    ) -> Result<Rule> {
        validate_rule_body(body)?;
        let task_tags = self.canonical_rule_task_tags(task_tags)?;
        let actor = validate_rule_actor(actor)?.to_owned();
        let id = format!("g-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<i64> =
            transaction.query_row("SELECT max(created_at) FROM global_rules", [], |row| {
                row.get(0)
            })?;
        let now = now_ms().max(previous.unwrap_or(0).saturating_add(1));
        transaction.execute(
            "INSERT INTO global_rules(id,body,author,archived,created_at,updated_at,board_tags,task_tags) VALUES(?,?,?,0,?,?,?,?)",
            params![id, body, actor, now, now, serde_json::to_string(board_tags)?, serde_json::to_string(&task_tags)?],
        )?;
        transaction.execute(
            "INSERT INTO global_rule_events(rule_id,kind,actor,payload,created_at) VALUES(?,?,?,?,?)",
            params![id, "global_rule_added", actor, json!({"ruleID": id, "boardTags": board_tags, "taskTags": task_tags}).to_string(), now],
        )?;
        let rule = transaction.query_row(
            "SELECT * FROM global_rules WHERE id=?",
            [&id],
            global_rule_row,
        )?;
        transaction.commit()?;
        Ok(rule)
    }

    pub fn global_rules(&self, include_archived: bool) -> Result<Vec<Rule>> {
        let clause = if include_archived {
            ""
        } else {
            " WHERE archived=0"
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT * FROM global_rules{clause} ORDER BY created_at,id"
        ))?;
        statement
            .query_map([], global_rule_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn global_rule_summaries(&self, include_archived: bool) -> Result<Vec<RuleSummary>> {
        self.global_rules(include_archived)?
            .into_iter()
            .map(|rule| {
                let headline = rule
                    .body
                    .lines()
                    .next()
                    .context("stored global rule has no headline")?
                    .trim()
                    .to_owned();
                let has_more = rule
                    .body
                    .lines()
                    .skip(1)
                    .any(|line| !line.trim().is_empty());
                Ok(RuleSummary {
                    scope: "global".into(),
                    id: rule.id,
                    headline,
                    has_more,
                    bytes: rule.body.len(),
                    board_tags: rule.board_tags,
                    task_tags: rule.task_tags,
                })
            })
            .collect()
    }

    pub fn global_rule_summaries_for(
        &self,
        board_name: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<RuleSummary>> {
        Ok(self
            .global_rule_summaries(include_archived)?
            .into_iter()
            .filter(|rule| board_tags_apply(rule.board_tags.as_deref(), board_name))
            .collect())
    }

    pub fn global_rules_for(
        &self,
        board_name: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<Rule>> {
        Ok(self
            .global_rules(include_archived)?
            .into_iter()
            .filter(|rule| board_tags_apply(rule.board_tags.as_deref(), board_name))
            .collect())
    }

    pub fn global_rule(&self, id: &str) -> Result<Rule> {
        self.connection
            .query_row(
                "SELECT * FROM global_rules WHERE id=?",
                [id],
                global_rule_row,
            )
            .optional()?
            .with_context(|| format!("global rule {id} not found"))
    }

    pub fn update_global_rule(
        &mut self,
        id: &str,
        body: Option<&str>,
        board_tags: Option<&[String]>,
        task_tags: Option<&[String]>,
        actor: &str,
    ) -> Result<Rule> {
        if body.is_none() && board_tags.is_none() && task_tags.is_none() {
            bail!(
                "global rule update requires --body/--body-file, --board/--except-board, --tag, or --clear-tags"
            );
        }
        if let Some(body) = body {
            validate_rule_body(body)?;
        }
        let task_tags = task_tags
            .map(|tags| self.canonical_rule_task_tags(tags))
            .transpose()?;
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<Rule> = transaction
            .query_row(
                "SELECT * FROM global_rules WHERE id=?",
                [id],
                global_rule_row,
            )
            .optional()?;
        let previous = previous.with_context(|| format!("global rule {id} not found"))?;
        let now = now_ms();
        transaction.execute(
            "UPDATE global_rules SET body=?,board_tags=?,task_tags=?,author=?,updated_at=? WHERE id=?",
            params![
                body.unwrap_or(&previous.body),
                serde_json::to_string(
                    board_tags.unwrap_or(previous.board_tags.as_deref().unwrap_or(&[]))
                )?,
                serde_json::to_string(task_tags.as_deref().unwrap_or(&previous.task_tags))?,
                actor,
                now,
                id
            ],
        )?;
        let mut changed = Vec::new();
        if body.is_some() {
            changed.push("body");
        }
        if board_tags.is_some() {
            changed.push("boardTags");
        }
        if task_tags.is_some() {
            changed.push("taskTags");
        }
        transaction.execute(
            "INSERT INTO global_rule_events(rule_id,kind,actor,payload,created_at) VALUES(?,?,?,?,?)",
            params![id, "global_rule_updated", actor, json!({
                "ruleID": id,
                "previousBody": previous.body,
                "previousBoardTags": previous.board_tags,
                "previousTaskTags": previous.task_tags,
                "changed": changed,
            }).to_string(), now],
        )?;
        let rule = transaction.query_row(
            "SELECT * FROM global_rules WHERE id=?",
            [id],
            global_rule_row,
        )?;
        transaction.commit()?;
        Ok(rule)
    }

    pub fn retire_global_rule(&mut self, id: &str, actor: &str) -> Result<Rule> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let changed = transaction.execute(
            "UPDATE global_rules SET archived=1,author=?,updated_at=? WHERE id=? AND archived=0",
            params![actor, now, id],
        )?;
        if changed == 0 {
            let archived: Option<i64> = transaction
                .query_row(
                    "SELECT archived FROM global_rules WHERE id=?",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            match archived {
                None => bail!("global rule {id} not found"),
                Some(_) => bail!("global rule {id} is already retired"),
            }
        }
        transaction.execute(
            "INSERT INTO global_rule_events(rule_id,kind,actor,payload,created_at) VALUES(?,?,?,?,?)",
            params![id, "global_rule_retired", actor, json!({"ruleID": id}).to_string(), now],
        )?;
        let rule = transaction.query_row(
            "SELECT * FROM global_rules WHERE id=?",
            [id],
            global_rule_row,
        )?;
        transaction.commit()?;
        Ok(rule)
    }

    /// Global rule history, newest first, from the same registry that owns the
    /// single-copy document.
    pub fn global_rule_events(
        &self,
        rule: Option<&str>,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Event>> {
        if let Some(id) = rule {
            self.global_rule(id)?;
        }
        let mut sql = String::from("SELECT * FROM global_rule_events WHERE 1=1");
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
                |row| {
                    Ok(Event {
                        seq: row.get("seq")?,
                        task_id: None,
                        kind: row.get("kind")?,
                        actor: Some(row.get("actor")?),
                        payload: serde_json::from_str(&row.get::<_, String>("payload")?)
                            .unwrap_or(json!({})),
                        created_at: row.get("created_at")?,
                        archived: false,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn projects(&self) -> Result<Vec<ProjectRecord>> {
        let mut statement = self.connection.prepare("SELECT * FROM workspaces")?;
        let canonical = statement
            .query_map([], |r| row(r, true))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let aliases = self
            .list(false)?
            .into_iter()
            .filter(|r| !r.canonical)
            .collect::<Vec<_>>();
        let mut projects = canonical
            .into_iter()
            .map(|record| {
                let project_aliases = aliases.iter().filter(|a| a.board_path == record.board_path);
                let mut roots = vec![record.root_path.clone()];
                let mut last = record.last_used_at;
                for alias in project_aliases {
                    roots.push(alias.root_path.clone());
                    last = last.max(alias.last_used_at);
                }
                ProjectRecord {
                    name: record.name,
                    board_path: record.board_path,
                    canonical_root: record.root_path,
                    workspace_roots: roots,
                    last_used_at: last,
                }
            })
            .collect::<Vec<_>>();
        projects.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        Ok(projects)
    }

    /// Projects carrying `name`. Names are not unique in the registry, so
    /// callers must handle 0 and >1 rather than assume a single hit.
    pub fn by_name(&self, name: &str) -> Result<Vec<ProjectRecord>> {
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
        for table in ["workspaces", "workspace_aliases"] {
            self.connection.execute(
                &format!("UPDATE {table} SET last_used_at=? WHERE board_path=?"),
                params![now, board_path],
            )?;
        }
        Ok(())
    }

    pub fn integrity(&self) -> Result<Vec<String>> {
        integrity(&self.connection)
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
                canonical: record.canonical,
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
    pub fn repoint(&mut self, root_path: &str) -> Result<UnreachableRoot> {
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
        let table = if broken.canonical {
            "workspaces"
        } else {
            "workspace_aliases"
        };
        self.connection.execute(
            &format!("UPDATE {table} SET root_path=?, last_used_at=? WHERE root_path=?"),
            params![target, now_ms(), root_path],
        )?;
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
    use std::time::Duration;

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
}
