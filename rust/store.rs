use crate::db::{checkpoint as wal_checkpoint, create_backup_target, integrity, open_board};
use crate::model::*;
use crate::registry::now_ms;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

fn nonempty<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{label} is required");
    }
    Ok(trimmed)
}

fn validate(value: &str, allowed: &[&str], label: &str) -> Result<()> {
    if !allowed.contains(&value) {
        bail!("invalid {label} {value}");
    }
    Ok(())
}

/// Queue position: 0 is the most urgent, 9 the least, 3 the default.
///
/// The band follows what the ledger already means by the field rather than
/// imposing a new scale on it: `0` is the routing tier the driver-only tasks
/// use to sort ahead of everything an epic team can claim.
const MOST_URGENT: i64 = 0;
const LEAST_URGENT: i64 = 9;

/// Refuse a priority outside the documented band.
///
/// `claim --next` hands work out in ascending priority, so this is the field
/// that decides what an agent picks up. It accepted any `i64`: no value in the
/// type had a stated meaning, and a negative one took the head of every queue
/// permanently, because nothing can outrank the bottom of the range.
///
/// Only a value a caller supplies is checked. A row already in the ledger — an
/// atmux import, a board written before this rule — keeps whatever it holds:
/// validating input is not a licence to rewrite recorded history to match.
fn validate_priority(value: Option<i64>) -> Result<()> {
    if let Some(value) = value
        && !(MOST_URGENT..=LEAST_URGENT).contains(&value)
    {
        bail!(
            "task priority must be between {MOST_URGENT} (most urgent) and {LEAST_URGENT}, got {value}"
        );
    }
    Ok(())
}

fn parse_value(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or_else(|_| json!({ "legacyInvalidJson": text }))
}

fn parse_strings(text: String) -> Vec<String> {
    serde_json::from_str(&text).unwrap_or_default()
}

fn task_row(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get("id")?,
        task_type: row.get("type")?,
        parent_id: row.get("parent_id")?,
        title: row.get("title")?,
        body: row.get("body")?,
        assignee: row.get("assignee")?,
        lane: row.get("lane")?,
        deliverable: row.get("deliverable")?,
        stale_minutes: row.get("stale_minutes")?,
        driver_only: row.get::<_, i64>("driver_only")? != 0,
        status: row.get("status")?,
        priority: row.get("priority")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        completed_at: row.get("completed_at")?,
        metadata: parse_value(row.get("metadata")?),
    })
}

fn claim_row(row: &Row<'_>) -> rusqlite::Result<Claim> {
    Ok(Claim {
        task_id: row.get("task_id")?,
        agent_id: row.get("agent_id")?,
        session_id: row.get("session_id")?,
        lease_token: row.get("lease_token")?,
        claimed_at: row.get("claimed_at")?,
        heartbeat_at: row.get("heartbeat_at")?,
        expires_at: row.get("expires_at")?,
    })
}

fn note_row(row: &Row<'_>) -> rusqlite::Result<TaskNote> {
    Ok(TaskNote {
        seq: row.get("seq")?,
        task_id: row.get("task_id")?,
        author: row.get("author")?,
        kind: row.get("kind")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
    })
}

fn checkpoint_row(row: &Row<'_>) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        seq: row.get("seq")?,
        task_id: row.get("task_id")?,
        author: row.get("author")?,
        session_id: row.get("session_id")?,
        model: row.get("model")?,
        state: row.get("state")?,
        summary: row.get("summary")?,
        intent: row.get("intent")?,
        next_action: row.get("next_action")?,
        blockers: parse_strings(row.get("blockers")?),
        validations: parse_strings(row.get("validations")?),
        repo_path: row.get("repo_path")?,
        branch: row.get("branch")?,
        head_sha: row.get("head_sha")?,
        dirty_summary: row.get("dirty_summary")?,
        created_at: row.get("created_at")?,
    })
}

fn handoff_row(row: &Row<'_>) -> rusqlite::Result<Handoff> {
    Ok(Handoff {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        checkpoint_seq: row.get("checkpoint_seq")?,
        reason: row.get("reason")?,
        status: row.get("status")?,
        from_agent: row.get("from_agent")?,
        from_session: row.get("from_session")?,
        from_model: row.get("from_model")?,
        to_agent: row.get("to_agent")?,
        summary: row.get("summary")?,
        intent: row.get("intent")?,
        next_action: row.get("next_action")?,
        blockers: parse_strings(row.get("blockers")?),
        validations: parse_strings(row.get("validations")?),
        repo_path: row.get("repo_path")?,
        branch: row.get("branch")?,
        head_sha: row.get("head_sha")?,
        dirty_summary: row.get("dirty_summary")?,
        created_at: row.get("created_at")?,
        accepted_at: row.get("accepted_at")?,
        accepted_by: row.get("accepted_by")?,
        accepted_session: row.get("accepted_session")?,
    })
}

fn get_task(connection: &Connection, id: &str) -> Result<Option<Task>> {
    connection
        .query_row("SELECT * FROM tasks WHERE id=?", [id], task_row)
        .optional()
        .map_err(Into::into)
}

fn require_task(connection: &Connection, id: &str) -> Result<Task> {
    get_task(connection, id)?.with_context(|| format!("task {id} not found"))
}

fn dependencies(connection: &Connection, task_id: &str) -> Result<Vec<Task>> {
    require_task(connection, task_id)?;
    let mut statement = connection.prepare(
        "SELECT t.* FROM tasks t JOIN task_dependencies d ON d.depends_on=t.id WHERE d.task_id=? ORDER BY t.created_at,t.id",
    )?;
    statement
        .query_map([task_id], task_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn event(
    connection: &Connection,
    task_id: Option<&str>,
    kind: &str,
    actor: Option<&str>,
    payload: Value,
) -> Result<()> {
    connection.execute(
        "INSERT INTO events(task_id,kind,actor,payload,created_at) VALUES(?,?,?,?,?)",
        params![task_id, kind, actor, payload.to_string(), now_ms()],
    )?;
    Ok(())
}

/// A board written by the released TypeScript implementation can sit at
/// `user_version=3` with no `task_claims` table, so the migration ladder never
/// creates one. Those boards must still open, and anything reading claims has
/// to tolerate the table's absence rather than assume the schema it expects.
pub(crate) fn has_claims_table(connection: &Connection) -> Result<bool> {
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='task_claims')",
        [],
        |row| row.get(0),
    )?;
    Ok(exists == 1)
}

/// Every live lease among `ids`, so a bulk operation can name all of them at
/// once instead of failing on the first.
///
/// Expired leases are already gone: `Store::open` sweeps before anything
/// reads, so a row still here is genuinely held by someone.
pub(crate) fn live_claims(connection: &Connection, ids: &[String]) -> Result<Vec<Claim>> {
    if ids.is_empty() || !has_claims_table(connection)? {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    let now = now_ms();
    for id in ids {
        if let Some(claim) = active_claim(connection, id, now)? {
            found.push(claim);
        }
    }
    Ok(found)
}

fn active_claim(connection: &Connection, task_id: &str, now: i64) -> Result<Option<Claim>> {
    connection
        .query_row(
            "SELECT * FROM task_claims WHERE task_id=? AND expires_at>?",
            params![task_id, now],
            claim_row,
        )
        .optional()
        .map_err(Into::into)
}

fn require_lease(connection: &Connection, task_id: &str, token: &str, now: i64) -> Result<Claim> {
    let claim = connection
        .query_row(
            "SELECT * FROM task_claims WHERE task_id=? AND lease_token=? AND expires_at>?",
            params![task_id, token, now],
            claim_row,
        )
        .optional()?;
    claim.with_context(|| format!("no active lease for task {task_id}"))
}

fn expire_claims(connection: &Connection, now: i64) -> Result<()> {
    let mut statement =
        connection.prepare("SELECT task_id,agent_id FROM task_claims WHERE expires_at<=?")?;
    let expired = statement
        .query_map([now], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (task_id, agent) in expired {
        connection.execute("DELETE FROM task_claims WHERE task_id=?", [&task_id])?;
        connection.execute(
            "UPDATE tasks SET status='todo',assignee=CASE WHEN assignee=? THEN NULL ELSE assignee END,updated_at=? WHERE id=? AND status='in_progress'",
            params![agent, now, task_id],
        )?;
        event(
            connection,
            Some(&task_id),
            "claim_expired",
            Some(&agent),
            json!({}),
        )?;
    }
    Ok(())
}

/// A live lease is another agent's authority to write this task. Operator
/// commands that would void it (`task move`, `task remove`) refuse by default
/// and name the holder; `--force` seizes it and records who did.
///
/// Without this, `kanban task move t-1 todo --as anyone` silently deleted the
/// claim row, and the holder's next checkpoint failed with "no active lease"
/// after the work was already done.
fn require_free_lease(
    connection: &Connection,
    task_id: &str,
    actor: &str,
    force: bool,
    action: &str,
) -> Result<Option<Claim>> {
    let Some(claim) = active_claim(connection, task_id, now_ms())? else {
        return Ok(None);
    };
    if !force {
        bail!(
            "task {task_id} is leased by {} until {} (session {}); rerun with --force to {action} it anyway",
            claim.agent_id,
            claim.expires_at,
            claim.session_id.as_deref().unwrap_or("-")
        );
    }
    event(
        connection,
        Some(task_id),
        "lease_seized",
        Some(actor),
        json!({"heldBy": claim.agent_id, "action": action, "expiresAt": claim.expires_at}),
    )?;
    Ok(Some(claim))
}

/// Keep the newest `limit` entries of an oldest-first list.
/// Returns true when anything was dropped.
fn keep_newest<T>(list: &mut Vec<T>, limit: usize) -> bool {
    if list.len() <= limit {
        return false;
    }
    list.drain(..list.len() - limit);
    true
}

fn depends_transitively(connection: &Connection, start: &str, target: &str) -> Result<bool> {
    Ok(connection.query_row(
        "WITH RECURSIVE chain(id) AS (SELECT depends_on FROM task_dependencies WHERE task_id=? UNION SELECT d.depends_on FROM task_dependencies d JOIN chain c ON d.task_id=c.id) SELECT EXISTS(SELECT 1 FROM chain WHERE id=?)",
        params![start, target],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

#[derive(Default)]
pub struct UpdateTask {
    pub parent_id: Option<Option<String>>,
    pub title: Option<String>,
    pub body: Option<Option<String>>,
    pub assignee: Option<Option<String>>,
    pub lane: Option<Option<String>>,
    pub deliverable: Option<Option<String>>,
    pub stale_minutes: Option<Option<i64>>,
    pub driver_only: Option<bool>,
    pub priority: Option<i64>,
    pub dependencies: Option<Vec<String>>,
}

pub struct ClaimOptions {
    pub agent_id: String,
    pub session_id: Option<String>,
    pub lease_ms: i64,
    pub caller_lane: Option<String>,
    pub role_filter: Option<String>,
    pub caller_scope: Option<String>,
    pub cross_lane: bool,
    pub allow_reassign: bool,
}

pub struct Store {
    pub connection: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut store = Self {
            connection: open_board(path)?,
        };
        store.sweep_expired_claims()?;
        Ok(store)
    }

    /// Retire leases that have run out, before anything reads the board.
    ///
    /// Expiry used to happen only inside `claim` and `accept_handoff`, so a
    /// vanished agent left its task reading `in_progress · assignee: ghost`
    /// on every read path while `claim --next` handed the same task to someone
    /// else. The board and the scheduler disagreed, and the generated TODO
    /// contradicted itself in one card ("Restart here … Owner: unclaimed").
    ///
    /// Doing it here keeps one definition of what expiry means. The common
    /// case is a single indexed count over `idx_task_claims_expiry` and takes
    /// no write lock; only an actual expiry opens a transaction.
    pub fn sweep_expired_claims(&mut self) -> Result<usize> {
        if !has_claims_table(&self.connection)? {
            return Ok(0);
        }
        let expired: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM task_claims WHERE expires_at<=?",
            [now_ms()],
            |row| row.get(0),
        )?;
        if expired == 0 {
            return Ok(0);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_claims(&transaction, now_ms())?;
        transaction.commit()?;
        Ok(expired as usize)
    }

    /// Recorded ledger events, newest first. The `events` table is the audit
    /// trail for lease seizures and destructive removals, so it needs a reader.
    pub fn events(&self, task: Option<&str>, kind: Option<&str>, limit: i64) -> Result<Vec<Event>> {
        if let Some(id) = task {
            require_task(&self.connection, id)?;
        }
        let mut sql = String::from("SELECT * FROM events WHERE 1=1");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(id) = task {
            sql.push_str(" AND task_id=?");
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
                        task_id: row.get("task_id")?,
                        kind: row.get("kind")?,
                        actor: row.get("actor")?,
                        payload: parse_value(row.get("payload")?),
                        created_at: row.get("created_at")?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn initialize(&self, name: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO board_meta(key,value) VALUES('name',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [nonempty(name, "name")?],
        )?;
        Ok(())
    }

    pub fn board_name(&self) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT value FROM board_meta WHERE key='name'", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn add_task(&mut self, input: AddTask) -> Result<Task> {
        validate(&input.task_type, &TASK_TYPES, "task type")?;
        validate(&input.status, &TASK_STATUSES, "task status")?;
        validate_priority(Some(input.priority))?;
        if input.stale_minutes.is_some_and(|value| value < 0) {
            bail!("stale minutes must be non-negative");
        }
        let id = input.id.unwrap_or_else(|| {
            let prefix = match input.task_type.as_str() {
                "epic" => "e",
                "story" => "s",
                _ => "t",
            };
            format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..8])
        });
        let title = nonempty(&input.title, "title")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        if let Some(parent) = &input.parent_id {
            require_task(&transaction, parent)?;
        }
        transaction.execute(
            "INSERT INTO tasks(id,type,parent_id,title,body,assignee,lane,deliverable,stale_minutes,driver_only,status,priority,created_at,updated_at,completed_at,metadata) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![id,input.task_type,input.parent_id,title,input.body,input.assignee,input.lane,input.deliverable,input.stale_minutes,input.driver_only as i64,input.status,input.priority,now,now,if input.status == "done" { Some(now) } else { None },input.metadata.to_string()],
        )?;
        for dependency in input.dependencies {
            require_task(&transaction, &dependency)?;
            if dependency == id {
                bail!("task cannot depend on itself");
            }
            if depends_transitively(&transaction, &dependency, &id)? {
                bail!("dependency {dependency} would create a cycle");
            }
            transaction.execute(
                "INSERT INTO task_dependencies(task_id,depends_on) VALUES(?,?)",
                params![id, dependency],
            )?;
        }
        event(&transaction, Some(&id), "task_added", None, json!({}))?;
        transaction.commit()?;
        self.require_task(&id)
    }

    pub fn require_task(&self, id: &str) -> Result<Task> {
        require_task(&self.connection, id)
    }

    pub fn list_tasks(&self, status: Option<&str>) -> Result<Vec<Task>> {
        if let Some(value) = status {
            validate(value, &TASK_STATUSES, "task status")?;
        }
        let (sql, values) = if let Some(value) = status {
            (
                "SELECT * FROM tasks WHERE status=? ORDER BY priority,created_at,id",
                vec![value],
            )
        } else {
            (
                "SELECT * FROM tasks ORDER BY priority,created_at,id",
                Vec::new(),
            )
        };
        let mut statement = self.connection.prepare(sql)?;
        statement
            .query_map(params_from_iter(values), task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn dependencies(&self, id: &str) -> Result<Vec<Task>> {
        dependencies(&self.connection, id)
    }

    pub fn ancestors(&self, id: &str) -> Result<Vec<Task>> {
        let mut current = self.require_task(id)?;
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::from([id.to_owned()]);
        while let Some(parent) = current.parent_id.clone() {
            if !seen.insert(parent.clone()) {
                bail!("parent cycle detected at {parent}");
            }
            current = self.require_task(&parent)?;
            out.insert(0, current.clone());
        }
        Ok(out)
    }

    pub fn move_task(
        &mut self,
        id: &str,
        status: &str,
        actor: &str,
        patch: Value,
        force: bool,
    ) -> Result<Task> {
        validate(status, &TASK_STATUSES, "task status")?;
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = require_task(&transaction, id)?;
        let seized = require_free_lease(&transaction, id, &actor, force, "move")?;
        let mut metadata = current.metadata.as_object().cloned().unwrap_or_default();
        let patch = patch
            .as_object()
            .context("metadata patch must be an object")?;
        for (key, value) in patch {
            if value.is_null() {
                metadata.remove(key);
            } else {
                metadata.insert(key.clone(), value.clone());
            }
        }
        let now = now_ms();
        transaction.execute(
            "UPDATE tasks SET status=?,metadata=?,updated_at=?,completed_at=? WHERE id=?",
            params![
                status,
                Value::Object(metadata).to_string(),
                now,
                if status == "done" { Some(now) } else { None },
                id
            ],
        )?;
        if status != "in_progress" {
            transaction.execute("DELETE FROM task_claims WHERE task_id=?", [id])?;
        }
        event(
            &transaction,
            Some(id),
            "task_moved",
            Some(&actor),
            json!({"status": status, "seizedFrom": seized.map(|claim| claim.agent_id)}),
        )?;
        transaction.commit()?;
        self.require_task(id)
    }

    pub fn remove_task(&mut self, id: &str, actor: &str, force: bool) -> Result<()> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = require_task(&transaction, id)?;
        let seized = require_free_lease(&transaction, id, &actor, force, "remove")?;
        // Children have no ON DELETE CASCADE, so the raw foreign-key failure is
        // the only signal the operator would otherwise get. Name the children.
        let mut statement =
            transaction.prepare("SELECT id FROM tasks WHERE parent_id=? ORDER BY id")?;
        let children = statement
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        if !children.is_empty() {
            bail!(
                "task {id} still has {} child task(s): {}; remove or reparent them first",
                children.len(),
                children.join(", ")
            );
        }
        // The delete cascades this task's notes, checkpoints and handoffs, so
        // record what is being destroyed before it is gone.
        let notes = transaction.query_row(
            "SELECT COUNT(*) FROM task_notes WHERE task_id=?",
            [id],
            |row| row.get::<_, i64>(0),
        )?;
        let checkpoints = transaction.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE task_id=?",
            [id],
            |row| row.get::<_, i64>(0),
        )?;
        transaction.execute("DELETE FROM tasks WHERE id=?", [id])?;
        event(
            &transaction,
            Some(id),
            "task_removed",
            Some(&actor),
            json!({
                "task": task,
                "discardedNotes": notes,
                "discardedCheckpoints": checkpoints,
                "seizedFrom": seized.map(|claim| claim.agent_id),
            }),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn patch_metadata(&mut self, id: &str, patch: Value, actor: &str) -> Result<Task> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = require_task(&transaction, id)?;
        let mut metadata = current.metadata.as_object().cloned().unwrap_or_default();
        let object = patch
            .as_object()
            .context("metadata patch must be an object")?;
        for (key, value) in object {
            if value.is_null() {
                metadata.remove(key);
            } else {
                metadata.insert(key.clone(), value.clone());
            }
        }
        transaction.execute(
            "UPDATE tasks SET metadata=?,updated_at=? WHERE id=?",
            params![Value::Object(metadata).to_string(), now_ms(), id],
        )?;
        event(
            &transaction,
            Some(id),
            "task_metadata_patched",
            Some(&actor),
            json!({"keys": object.keys().collect::<Vec<_>>()}),
        )?;
        transaction.commit()?;
        self.require_task(id)
    }

    pub fn update_task(&mut self, id: &str, input: UpdateTask, actor: &str) -> Result<Task> {
        let actor = nonempty(actor, "actor")?.to_owned();
        validate_priority(input.priority)?;
        if input.stale_minutes.flatten().is_some_and(|value| value < 0) {
            bail!("stale minutes must be non-negative");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = require_task(&transaction, id)?;
        let parent = input.parent_id.unwrap_or(current.parent_id);
        if let Some(parent_id) = &parent {
            require_task(&transaction, parent_id)?;
            let mut cursor = Some(parent_id.clone());
            let mut seen = std::collections::HashSet::from([id.to_owned()]);
            while let Some(value) = cursor {
                if !seen.insert(value.clone()) {
                    bail!("parent {parent_id} would create a cycle");
                }
                cursor = require_task(&transaction, &value)?.parent_id;
            }
        }
        let title = input.title.unwrap_or(current.title);
        let body = input.body.unwrap_or(current.body);
        let assignee = input.assignee.unwrap_or(current.assignee);
        let lane = input.lane.unwrap_or(current.lane);
        let deliverable = input.deliverable.unwrap_or(current.deliverable);
        let stale = input.stale_minutes.unwrap_or(current.stale_minutes);
        let driver = input.driver_only.unwrap_or(current.driver_only);
        let priority = input.priority.unwrap_or(current.priority);
        transaction.execute(
            "UPDATE tasks SET parent_id=?,title=?,body=?,assignee=?,lane=?,deliverable=?,stale_minutes=?,driver_only=?,priority=?,updated_at=? WHERE id=?",
            params![parent,nonempty(&title,"title")?,body,assignee,lane,deliverable,stale,driver as i64,priority,now_ms(),id],
        )?;
        if let Some(deps) = input.dependencies {
            let mut unique = Vec::new();
            for dependency in deps {
                if !unique.contains(&dependency) {
                    unique.push(dependency);
                }
            }
            for dependency in &unique {
                require_task(&transaction, dependency)?;
                if dependency == id {
                    bail!("task cannot depend on itself");
                }
                if depends_transitively(&transaction, dependency, id)? {
                    bail!("dependency {dependency} would create a cycle");
                }
            }
            transaction.execute("DELETE FROM task_dependencies WHERE task_id=?", [id])?;
            for dependency in unique {
                transaction.execute(
                    "INSERT INTO task_dependencies(task_id,depends_on) VALUES(?,?)",
                    params![id, dependency],
                )?;
            }
        }
        event(
            &transaction,
            Some(id),
            "task_updated",
            Some(&actor),
            json!({}),
        )?;
        transaction.commit()?;
        self.require_task(id)
    }

    pub fn claim(&mut self, id: Option<&str>, options: ClaimOptions) -> Result<Claim> {
        let agent = nonempty(&options.agent_id, "agent id")?.to_owned();
        if options.lease_ms < 1000 {
            bail!("lease must be at least 1000ms");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        expire_claims(&transaction, now)?;
        let task = if let Some(id) = id {
            require_task(&transaction, id)?
        } else {
            let mut statement = transaction.prepare(
                "SELECT t.* FROM tasks t
                 LEFT JOIN task_claims c ON c.task_id=t.id
                 WHERE t.status='todo'
                   AND c.task_id IS NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM task_dependencies d
                     JOIN tasks dep ON dep.id=d.depends_on
                     WHERE d.task_id=t.id AND dep.status<>'done'
                   )
                 ORDER BY t.priority,t.created_at,t.id",
            )?;
            let candidates = statement
                .query_map([], task_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let candidates = candidates
                .into_iter()
                .filter(|candidate| {
                    (!candidate.driver_only || options.caller_scope.as_deref() == Some("driver"))
                        && (candidate
                            .assignee
                            .as_ref()
                            .is_none_or(|value| value == &agent)
                            || options.allow_reassign)
                })
                .collect::<Vec<_>>();
            let selected = if let Some(role) = options.role_filter.as_deref() {
                candidates
                    .into_iter()
                    .find(|candidate| candidate.lane.as_deref() == Some(role))
            } else if let Some(lane) = options.caller_lane.as_deref() {
                let own_lane = candidates
                    .iter()
                    .find(|candidate| candidate.lane.as_deref() == Some(lane))
                    .cloned();
                if own_lane.is_some() || !options.cross_lane {
                    own_lane
                } else {
                    candidates
                        .into_iter()
                        .find(|candidate| candidate.lane.is_none())
                }
            } else {
                candidates
                    .into_iter()
                    .find(|candidate| candidate.lane.is_none())
            };
            selected.context("no claimable task")?
        };
        if !["todo", "in_progress"].contains(&task.status.as_str()) {
            bail!("task {} is {}, not claimable", task.id, task.status);
        }
        let unmet = dependencies(&transaction, &task.id)?
            .into_iter()
            .filter(|d| d.status != "done")
            .map(|d| d.id)
            .collect::<Vec<_>>();
        if !unmet.is_empty() {
            bail!(
                "task {} has unmet dependencies: {}",
                task.id,
                unmet.join(", ")
            );
        }
        if active_claim(&transaction, &task.id, now)?.is_some() {
            bail!("task {} is already claimed", task.id);
        }
        if task.driver_only && options.caller_scope.as_deref() != Some("driver") {
            bail!("task {} is driver-only", task.id);
        }
        if task.assignee.as_ref().is_some_and(|value| value != &agent) && !options.allow_reassign {
            bail!("task {} is assigned to {}", task.id, task.assignee.unwrap());
        }
        let token = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at) VALUES(?,?,?,?,?,?,?)",
            params![task.id,agent,options.session_id,token,now,now,now+options.lease_ms],
        )?;
        transaction.execute(
            "UPDATE tasks SET status='in_progress',assignee=?,updated_at=? WHERE id=?",
            params![agent, now, task.id],
        )?;
        event(
            &transaction,
            Some(&task.id),
            "task_claimed",
            Some(&agent),
            json!({"expiresAt": now+options.lease_ms}),
        )?;
        let result = active_claim(&transaction, &task.id, now)?.context("claim was not created")?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn get_claim(&self, id: &str) -> Result<Option<Claim>> {
        active_claim(&self.connection, id, now_ms())
    }

    pub fn heartbeat(&mut self, id: &str, token: &str, lease_ms: i64) -> Result<Claim> {
        if lease_ms < 1000 {
            bail!("lease must be at least 1000ms");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let claim = require_lease(&transaction, id, token, now)?;
        transaction.execute(
            "UPDATE task_claims SET heartbeat_at=?,expires_at=? WHERE task_id=? AND lease_token=?",
            params![now, now + lease_ms, id, token],
        )?;
        event(
            &transaction,
            Some(id),
            "claim_heartbeat",
            Some(&claim.agent_id),
            json!({"expiresAt": now+lease_ms}),
        )?;
        let result = active_claim(&transaction, id, now)?.context("claim disappeared")?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn release(&mut self, id: &str, token: &str, keep_status: bool) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claim = require_lease(&transaction, id, token, now_ms())?;
        transaction.execute("DELETE FROM task_claims WHERE task_id=?", [id])?;
        if !keep_status {
            transaction.execute(
                "UPDATE tasks SET status='todo',updated_at=? WHERE id=? AND status='in_progress'",
                params![now_ms(), id],
            )?;
        }
        event(
            &transaction,
            Some(id),
            "claim_released",
            Some(&claim.agent_id),
            json!({}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn add_note(&mut self, id: &str, author: &str, kind: &str, body: &str) -> Result<TaskNote> {
        validate(kind, &NOTE_KINDS, "note kind")?;
        require_task(&self.connection, id)?;
        let now = now_ms();
        self.connection.execute(
            "INSERT INTO task_notes(task_id,author,kind,body,created_at) VALUES(?,?,?,?,?)",
            params![
                id,
                nonempty(author, "author")?,
                kind,
                nonempty(body, "note")?,
                now
            ],
        )?;
        event(
            &self.connection,
            Some(id),
            "note_added",
            Some(author),
            json!({"kind":kind}),
        )?;
        self.notes(id, 1)?.pop().context("note was not created")
    }

    pub fn notes(&self, id: &str, limit: i64) -> Result<Vec<TaskNote>> {
        require_task(&self.connection, id)?;
        let mut statement = self.connection.prepare("SELECT * FROM (SELECT * FROM task_notes WHERE task_id=? ORDER BY seq DESC LIMIT ?) ORDER BY seq ASC")?;
        statement
            .query_map(params![id, limit], note_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn checkpoints(&self, id: &str, limit: i64) -> Result<Vec<Checkpoint>> {
        require_task(&self.connection, id)?;
        let mut statement = self.connection.prepare("SELECT * FROM (SELECT * FROM checkpoints WHERE task_id=? ORDER BY seq DESC LIMIT ?) ORDER BY seq ASC")?;
        statement
            .query_map(params![id, limit], checkpoint_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn checkpoint(&mut self, input: CheckpointInput) -> Result<Checkpoint> {
        validate(
            &input.state,
            &["continue", "blocked", "done"],
            "checkpoint state",
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let claim = require_lease(&transaction, &input.task_id, &input.lease_token, now)?;
        if claim.agent_id != input.author {
            bail!("lease belongs to {}, not {}", claim.agent_id, input.author);
        }
        transaction.execute(
            "INSERT INTO checkpoints(task_id,author,session_id,model,state,summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![input.task_id,input.author,input.session_id,input.model,input.state,nonempty(&input.summary,"summary")?,nonempty(&input.intent,"intent")?,nonempty(&input.next_action,"next action")?,serde_json::to_string(&input.blockers)?,serde_json::to_string(&input.validations)?,input.repo_path,input.branch,input.head_sha,input.dirty_summary,now],
        )?;
        let seq = transaction.last_insert_rowid();
        let (status, completed): (&str, Option<i64>) = match input.state.as_str() {
            "blocked" => ("blocked", None),
            "done" => ("done", Some(now)),
            _ => ("in_progress", None),
        };
        transaction.execute(
            "UPDATE tasks SET status=?,updated_at=?,completed_at=? WHERE id=?",
            params![status, now, completed, input.task_id],
        )?;
        if input.state != "continue" {
            transaction.execute("DELETE FROM task_claims WHERE task_id=?", [&input.task_id])?;
        }
        event(
            &transaction,
            Some(&input.task_id),
            "checkpoint_added",
            Some(&input.author),
            json!({"seq":seq,"state":input.state}),
        )?;
        let result = transaction.query_row(
            "SELECT * FROM checkpoints WHERE seq=?",
            [seq],
            checkpoint_row,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn handoffs(
        &self,
        task: Option<&str>,
        status: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Handoff>> {
        if let Some(value) = status {
            validate(
                value,
                &["pending", "accepted", "cancelled"],
                "handoff status",
            )?;
        }
        if let Some(id) = task {
            require_task(&self.connection, id)?;
        }
        let (sql, values): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match (task, status) {
            (Some(task), Some(status)) => (
                "SELECT * FROM handoffs WHERE task_id=? AND status=? ORDER BY created_at DESC,id DESC LIMIT ?",
                vec![
                    Box::new(task.to_owned()),
                    Box::new(status.to_owned()),
                    Box::new(limit),
                ],
            ),
            (Some(task), None) => (
                "SELECT * FROM handoffs WHERE task_id=? ORDER BY created_at DESC,id DESC LIMIT ?",
                vec![Box::new(task.to_owned()), Box::new(limit)],
            ),
            (None, Some(status)) => (
                "SELECT * FROM handoffs WHERE status=? ORDER BY created_at DESC,id DESC LIMIT ?",
                vec![Box::new(status.to_owned()), Box::new(limit)],
            ),
            (None, None) => (
                "SELECT * FROM handoffs ORDER BY created_at DESC,id DESC LIMIT ?",
                vec![Box::new(limit)],
            ),
        };
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(sql)?;
        statement
            .query_map(params_from_iter(refs), handoff_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn create_handoff(&mut self, input: HandoffInput) -> Result<Handoff> {
        validate(&input.reason, &HANDOFF_REASONS, "handoff reason")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let claim = require_lease(&transaction, &input.task_id, &input.lease_token, now)?;
        if claim.agent_id != input.from_agent {
            bail!(
                "lease belongs to {}, not {}",
                claim.agent_id,
                input.from_agent
            );
        }
        let summary = nonempty(&input.summary, "summary")?.to_owned();
        let intent = nonempty(&input.intent, "intent")?.to_owned();
        let next = nonempty(&input.next_action, "next action")?.to_owned();
        let blockers = serde_json::to_string(&input.blockers)?;
        let validations = serde_json::to_string(&input.validations)?;
        transaction.execute(
            "INSERT INTO checkpoints(task_id,author,session_id,model,state,summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at) VALUES(?,?,?,?,? ,?,?,?,?,?,?,?,?,?,?)",
            params![input.task_id,input.from_agent,input.from_session.clone().or(claim.session_id),input.from_model,"continue",summary,intent,next,blockers,validations,input.repo_path,input.branch,input.head_sha,input.dirty_summary,now],
        )?;
        let checkpoint_seq = transaction.last_insert_rowid();
        let id = format!("h-{}", &Uuid::new_v4().simple().to_string()[..8]);
        transaction.execute(
            "INSERT INTO handoffs(id,task_id,checkpoint_seq,reason,status,from_agent,from_session,from_model,to_agent,summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at,accepted_at,accepted_by,accepted_session) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,NULL,NULL,NULL)",
            params![id,input.task_id,checkpoint_seq,input.reason,"pending",input.from_agent,input.from_session,input.from_model,input.to_agent,summary,intent,next,blockers,validations,input.repo_path,input.branch,input.head_sha,input.dirty_summary,now],
        )?;
        transaction.execute("DELETE FROM task_claims WHERE task_id=?", [&input.task_id])?;
        transaction.execute(
            "UPDATE tasks SET status='todo',updated_at=?,completed_at=NULL WHERE id=?",
            params![now, input.task_id],
        )?;
        event(
            &transaction,
            Some(&input.task_id),
            "handoff_created",
            Some(&input.from_agent),
            json!({"handoffID":id,"checkpointSeq":checkpoint_seq,"reason":input.reason,"toAgent":input.to_agent}),
        )?;
        let result =
            transaction.query_row("SELECT * FROM handoffs WHERE id=?", [&id], handoff_row)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn accept_handoff(
        &mut self,
        id: &str,
        agent: &str,
        session: Option<String>,
        lease_ms: i64,
        caller_scope: Option<&str>,
    ) -> Result<(Handoff, Claim)> {
        let agent = nonempty(agent, "agent id")?.to_owned();
        if lease_ms < 1000 {
            bail!("lease must be at least 1000ms");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        expire_claims(&transaction, now)?;
        let handoff = transaction
            .query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)
            .optional()?
            .with_context(|| format!("handoff {id} not found"))?;
        if handoff.status != "pending" {
            bail!("handoff {id} is {}", handoff.status);
        }
        if handoff
            .to_agent
            .as_ref()
            .is_some_and(|target| target != &agent)
        {
            bail!(
                "handoff {id} targets {}, not {agent}",
                handoff.to_agent.unwrap()
            );
        }
        let task = require_task(&transaction, &handoff.task_id)?;
        if task.status != "todo" {
            bail!("task {} is {}, not claimable", task.id, task.status);
        }
        if task.driver_only && caller_scope != Some("driver") {
            bail!("task {} is driver-only", task.id);
        }
        let unmet = dependencies(&transaction, &task.id)?
            .into_iter()
            .filter(|d| d.status != "done")
            .map(|d| d.id)
            .collect::<Vec<_>>();
        if !unmet.is_empty() {
            bail!(
                "task {} has unmet dependencies: {}",
                task.id,
                unmet.join(", ")
            );
        }
        if active_claim(&transaction, &task.id, now)?.is_some() {
            bail!("task {} is already claimed", task.id);
        }
        let token = Uuid::new_v4().to_string();
        transaction.execute("INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at) VALUES(?,?,?,?,?,?,?)",params![task.id,agent,session,token,now,now,now+lease_ms])?;
        transaction.execute(
            "UPDATE tasks SET status='in_progress',assignee=?,updated_at=? WHERE id=?",
            params![agent, now, task.id],
        )?;
        transaction.execute("UPDATE handoffs SET status='accepted',accepted_at=?,accepted_by=?,accepted_session=? WHERE id=? AND status='pending'",params![now,agent,session,id])?;
        event(
            &transaction,
            Some(&task.id),
            "handoff_accepted",
            Some(&agent),
            json!({"handoffID":id,"expiresAt":now+lease_ms}),
        )?;
        let updated =
            transaction.query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)?;
        let claim =
            active_claim(&transaction, &task.id, now)?.context("accepted claim disappeared")?;
        transaction.commit()?;
        Ok((updated, claim))
    }

    pub fn signoff_story(
        &mut self,
        id: &str,
        actor: &str,
        signed: bool,
        note: Option<&str>,
    ) -> Result<Value> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let story = require_task(&transaction, id)?;
        if story.task_type != "story" {
            bail!("task {id} is not a story");
        }
        if story.metadata.get("workflowStatus").and_then(Value::as_str) != Some("review") {
            bail!("story signoff is only valid in review");
        }
        if !signed
            && story
                .metadata
                .get("mergeTaskID")
                .and_then(Value::as_str)
                .is_some()
        {
            bail!("story {id} signoff has already been consumed");
        }
        let at = now_ms();
        let note = note
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let mut metadata = story.metadata.as_object().cloned().unwrap_or_default();
        let mut audit = metadata
            .get("signoffAudit")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        audit.push(if signed {
            json!({"signedOffBy":actor,"signedOffAt":at,"note":note})
        } else {
            json!({"unsignedBy":actor,"unsignedAt":at,"note":note})
        });
        metadata.insert("reviewSignoff".into(), Value::Bool(signed));
        metadata.insert("signoffAudit".into(), Value::Array(audit));
        transaction.execute(
            "UPDATE tasks SET metadata=?,updated_at=? WHERE id=?",
            params![Value::Object(metadata).to_string(), at, id],
        )?;
        event(
            &transaction,
            Some(id),
            if signed {
                "story_signed_off"
            } else {
                "story_signoff_revoked"
            },
            Some(&actor),
            json!({"note":note}),
        )?;
        transaction.commit()?;
        Ok(json!({"storyID":id,"actor":actor,"at":at,"note":note}))
    }

    pub fn advance_story(
        &mut self,
        id: &str,
        actor: &str,
        target: Option<&str>,
        reviewer: Option<&str>,
        committer: Option<&str>,
    ) -> Result<Value> {
        const FLOW: [&str; 7] = [
            "planning",
            "ready",
            "in-progress",
            "testing",
            "review",
            "merging",
            "done",
        ];
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let story = require_task(&transaction, id)?;
        if story.task_type != "story" {
            bail!("task {id} is not a story");
        }
        let current = story
            .metadata
            .get("workflowStatus")
            .and_then(Value::as_str)
            .unwrap_or("planning")
            .to_owned();
        let merge_mode =
            if story.metadata.get("mergeMode").and_then(Value::as_str) == Some("trunk-direct") {
                "trunk-direct"
            } else {
                "feature-branch"
            };
        let next = match current.as_str() {
            "planning" => Some("ready"),
            "ready" => Some("in-progress"),
            "in-progress" => Some("testing"),
            "testing" => Some("review"),
            "review" if merge_mode == "trunk-direct" => Some("done"),
            "review" => Some("merging"),
            "merging" => Some("done"),
            "done" => None,
            _ => bail!("unsupported story workflow status {current}"),
        };
        let target = target
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(next);
        let target =
            target.with_context(|| format!("story {id} is in terminal state {current}"))?;
        validate(target, &FLOW, "story workflow status")?;
        if target != current && Some(target) != next {
            bail!("illegal story transition {current} -> {target}");
        }
        if target == current {
            return Ok(
                json!({"from":current,"to":target,"parentEpicFlipped":false,"dispatchedTaskID":Value::Null,"noop":true}),
            );
        }

        let mut statement = transaction.prepare(
            "SELECT * FROM tasks WHERE parent_id=? AND type='task' ORDER BY created_at,id",
        )?;
        let children = statement
            .query_map([id], task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        if target == "testing" {
            let blockers = children
                .iter()
                .filter(|task| {
                    task.lane.as_deref().unwrap_or("misc") != "test" && task.status != "done"
                })
                .map(|task| task.id.clone())
                .collect::<Vec<_>>();
            if !blockers.is_empty() {
                bail!("non-test-lane tasks still open: {}", blockers.join(","));
            }
        }
        if target == "review" {
            let blockers = children
                .iter()
                .filter(|task| task.lane.as_deref() == Some("test") && task.status != "done")
                .map(|task| task.id.clone())
                .collect::<Vec<_>>();
            if !blockers.is_empty() {
                bail!("test-lane tasks still open: {}", blockers.join(","));
            }
            nonempty(reviewer.unwrap_or(""), "reviewer")?;
        }
        if target == "merging" {
            if story.metadata.get("reviewSignoff") != Some(&Value::Bool(true)) {
                bail!("reviewer signoff is required");
            }
            nonempty(committer.unwrap_or(""), "committer")?;
        }
        if target == "done" {
            if merge_mode == "trunk-direct" {
                if story.metadata.get("reviewSignoff") != Some(&Value::Bool(true)) {
                    bail!("reviewer signoff is required");
                }
            } else {
                let merge_task = story.metadata.get("mergeTaskID").and_then(Value::as_str);
                let complete = merge_task
                    .map(|task_id| require_task(&transaction, task_id))
                    .transpose()?
                    .is_some_and(|task| task.status == "done");
                if !complete {
                    bail!(
                        "merge task {} is not done",
                        merge_task.unwrap_or("(missing)")
                    );
                }
            }
        }

        let now = now_ms();
        let mut parent_flipped = false;
        if current == "ready"
            && target == "in-progress"
            && let Some(parent_id) = story.parent_id.as_deref()
        {
            let parent = require_task(&transaction, parent_id)?;
            if parent.task_type == "epic"
                && parent
                    .metadata
                    .get("workflowStatus")
                    .and_then(Value::as_str)
                    == Some("ready")
            {
                let mut metadata = parent.metadata.as_object().cloned().unwrap_or_default();
                metadata.insert("workflowStatus".into(), Value::String("in-progress".into()));
                transaction.execute(
                    "UPDATE tasks SET status='in_progress',metadata=?,updated_at=? WHERE id=?",
                    params![Value::Object(metadata).to_string(), now, parent.id],
                )?;
                event(
                    &transaction,
                    Some(&parent.id),
                    "epic_advanced",
                    Some(&actor),
                    json!({"from":"ready","to":"in-progress"}),
                )?;
                parent_flipped = true;
            }
        }

        let mut metadata = story.metadata.as_object().cloned().unwrap_or_default();
        metadata.insert("workflowStatus".into(), Value::String(target.to_owned()));
        metadata.insert("advancedAt".into(), json!(now / 1_000));
        let mut dispatched: Option<String> = None;
        if target == "review" || target == "merging" {
            let entering_review = target == "review";
            let assignee = nonempty(
                if entering_review {
                    reviewer.unwrap_or("")
                } else {
                    committer.unwrap_or("")
                },
                "dispatch assignee",
            )?;
            let child_id = format!("t-{}", &Uuid::new_v4().simple().to_string()[..8]);
            transaction.execute(
                "INSERT INTO tasks(id,type,parent_id,title,body,assignee,lane,deliverable,stale_minutes,driver_only,status,priority,created_at,updated_at,completed_at,metadata) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![child_id,"task",id,format!("{} {id}",if entering_review {"review"} else {"merge"}),format!("Story {id} entered {target}."),assignee,if entering_review {"review"} else {"misc"},Option::<String>::None,Option::<i64>::None,0,"in_progress",1,now,now,Option::<i64>::None,json!({"workflowDispatch":target}).to_string()],
            )?;
            event(
                &transaction,
                Some(&child_id),
                "task_created",
                Some(&actor),
                json!({"storyID":id,"workflowDispatch":target}),
            )?;
            if !entering_review {
                metadata.insert("mergeTaskID".into(), Value::String(child_id.clone()));
            }
            dispatched = Some(child_id);
        }
        let status = match target {
            "planning" => "backlog",
            "ready" => "todo",
            "in-progress" => "in_progress",
            "done" => "done",
            _ => "review",
        };
        transaction.execute(
            "UPDATE tasks SET status=?,metadata=?,updated_at=?,completed_at=? WHERE id=?",
            params![
                status,
                Value::Object(metadata).to_string(),
                now,
                if status == "done" { Some(now) } else { None },
                id
            ],
        )?;
        event(
            &transaction,
            Some(id),
            "story_advanced",
            Some(&actor),
            json!({"from":current,"to":target,"dispatchedTaskID":dispatched}),
        )?;
        transaction.commit()?;
        Ok(
            json!({"from":current,"to":target,"parentEpicFlipped":parent_flipped,"dispatchedTaskID":dispatched,"noop":false}),
        )
    }

    /// Tasks that have overrun their own `stale_minutes` budget.
    ///
    /// Idleness is measured from the claim heartbeat when one exists, and from
    /// `updated_at` otherwise, so a task dispatched into `in_progress` without
    /// a claim is still covered.
    pub fn stale_tasks(&self) -> Result<Vec<StaleTask>> {
        let now = now_ms();
        let mut statement = self.connection.prepare(
            "SELECT t.*, c.heartbeat_at AS claim_heartbeat FROM tasks t
             LEFT JOIN task_claims c ON c.task_id=t.id
             WHERE t.status='in_progress' AND t.stale_minutes IS NOT NULL
             ORDER BY t.priority,t.created_at,t.id",
        )?;
        let rows = statement
            .query_map([], |row| {
                let heartbeat: Option<i64> = row.get("claim_heartbeat")?;
                Ok((task_row(row)?, heartbeat))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for (task, heartbeat) in rows {
            let budget = task.stale_minutes.unwrap_or_default();
            let (since, last_signal) = match heartbeat {
                Some(at) => (at, "heartbeat"),
                None => (task.updated_at, "updated"),
            };
            let idle_minutes = (now - since).max(0) / 60_000;
            if idle_minutes > budget {
                out.push(StaleTask {
                    task,
                    idle_minutes,
                    overdue_minutes: idle_minutes - budget,
                    last_signal: last_signal.to_owned(),
                });
            }
        }
        Ok(out)
    }

    pub fn context_packet(&self, id: &str) -> Result<ContextPacket> {
        const NOTES: usize = 100;
        const CHECKPOINTS: usize = 20;
        const HANDOFFS: usize = 20;
        let task = self.require_task(id)?;
        // Over-fetch by one so "there is older history" is measured, not
        // assumed. `truncated` was hardcoded false, so a resuming agent was
        // told it held the whole record while notes were being dropped.
        let mut notes = self.notes(id, NOTES as i64 + 1)?;
        let mut checkpoints = self.checkpoints(id, CHECKPOINTS as i64 + 1)?;
        let mut handoffs = self.handoffs(Some(id), None, HANDOFFS as i64 + 1)?;
        handoffs.reverse();
        let mut truncated = keep_newest(&mut notes, NOTES);
        truncated |= keep_newest(&mut checkpoints, CHECKPOINTS);
        truncated |= keep_newest(&mut handoffs, HANDOFFS);
        Ok(ContextPacket {
            task,
            ancestors: self.ancestors(id)?,
            dependencies: self.dependencies(id)?,
            claim: self.get_claim(id)?.as_ref().map(ClaimSummary::from),
            notes,
            checkpoints,
            handoffs,
            generated_at: now_ms(),
            truncated,
        })
    }

    pub fn integrity(&self) -> Result<Vec<String>> {
        integrity(&self.connection)
    }

    pub fn foreign_key_violations(&self) -> Result<Vec<String>> {
        crate::db::foreign_key_violations(&self.connection)
    }

    /// Tasks stamped after the moment they are read.
    ///
    /// Leases expire by comparing stamps, so a record from the future is not a
    /// cosmetic oddity: it sorts ahead of real work and, on a claim, holds a
    /// lease that no sweep will ever retire. A minute of slack keeps ordinary
    /// clock drift between hosts sharing a board out of the report.
    pub fn future_dated_tasks(&self) -> Result<Vec<String>> {
        const SLACK_MS: i64 = 60_000;
        let mut statement = self
            .connection
            .prepare("SELECT id FROM tasks WHERE created_at>? OR updated_at>? ORDER BY id")?;
        let horizon = now_ms() + SLACK_MS;
        let rows = statement.query_map([horizon, horizon], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn backup(&self, destination: &Path) -> Result<()> {
        let mut target = create_backup_target(destination)?;
        let backup = rusqlite::backup::Backup::new(&self.connection, &mut target)?;
        backup.run_to_completion(64, std::time::Duration::from_millis(1), None)?;
        Ok(())
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = wal_checkpoint(&self.connection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_is_bounded_to_the_documented_band() {
        for good in [MOST_URGENT, 1, 3, LEAST_URGENT] {
            assert!(validate_priority(Some(good)).is_ok(), "{good} is in band");
        }
        for bad in [-1, LEAST_URGENT + 1, i64::MIN, i64::MAX] {
            let error = validate_priority(Some(bad))
                .expect_err(&format!("priority {bad} must be refused"))
                .to_string();
            assert!(error.contains("most urgent"), "{error}");
        }
        // An absent priority is not a zero: `task update` without --priority
        // must leave whatever the row already holds, in band or not.
        assert!(validate_priority(None).is_ok());
    }

    #[test]
    fn keep_newest_trims_from_the_front_and_reports_it() {
        // Lists arrive oldest-first, so the surplus to drop is the front.
        let mut list = vec![1, 2, 3, 4, 5];
        assert!(keep_newest(&mut list, 3));
        assert_eq!(list, vec![3, 4, 5], "must retain the newest window");
    }

    #[test]
    fn keep_newest_reports_nothing_dropped_when_under_the_cap() {
        let mut exact = vec![1, 2, 3];
        assert!(
            !keep_newest(&mut exact, 3),
            "a full-but-not-over list is not truncated"
        );
        assert_eq!(exact, vec![1, 2, 3]);

        let mut under = vec![1];
        assert!(!keep_newest(&mut under, 3));
        assert_eq!(under, vec![1]);

        let mut empty: Vec<i32> = vec![];
        assert!(!keep_newest(&mut empty, 3));
    }

    #[test]
    fn keep_newest_handles_a_zero_cap() {
        let mut list = vec![1, 2];
        assert!(keep_newest(&mut list, 0));
        assert!(list.is_empty());
    }
}
