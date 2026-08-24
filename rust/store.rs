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

fn validate_rule_body(value: &str) -> Result<()> {
    nonempty(value, "rule body")?;
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
    nonempty(value, "author")
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

/// The only task type an agent can be handed to execute.
///
/// An epic and a story are containers, and their status is *derived*: a story
/// walks its own gate under `story advance`, which dispatches a separate task
/// row for the work and flips the parent epic when the first story starts. A
/// lease asserts the opposite — that one named agent is executing this row now
/// — and taking one writes `status='in_progress'` and an assignee straight
/// onto the row.
///
/// Handing a container out therefore makes the ledger state two contradictory
/// things about one row: the board reads `in_progress` while the gate still
/// reads `planning`. It also parks a lease nobody can discharge, because a
/// container is never finished by working it, and it hides the container from
/// `claim --next` for the whole lease while none of its children moved.
const CLAIMABLE_TYPE: &str = "task";

/// Refuse a lease on anything but a task.
///
/// Both lease-minting paths call this — `claim` and `handoff accept` — because
/// eligibility is a property of the row, not of the verb that reached it. A
/// board written before this rule, or imported from atmux, can still carry a
/// pending handoff addressed to a container; the guard is what stops that from
/// becoming a live lease.
fn require_claimable_type(id: &str, task_type: &str) -> Result<()> {
    if task_type != CLAIMABLE_TYPE {
        let remedy = match task_type {
            "story" => "advance it with `story advance` and claim the task that dispatches",
            _ => "claim one of its children instead",
        };
        bail!(
            "task {id} is {} {task_type}, and only a {CLAIMABLE_TYPE} is claimable: {remedy}",
            article(task_type)
        );
    }
    Ok(())
}

/// The story gate, in order. A story moves one step at a time along this list.
const STORY_FLOW: [&str; 7] = [
    "planning",
    "ready",
    "in-progress",
    "testing",
    "review",
    "merging",
    "done",
];

/// The task status a story's gate state projects onto the row.
///
/// A story carries two fields that say where it is: `workflowStatus` in its
/// metadata, which the gate owns, and the `status` column every other reader
/// uses. The column is not independent data — it is this projection of the
/// gate, written by `advance_story` on every step.
fn story_status_for(workflow: &str) -> &'static str {
    match workflow {
        "planning" => "backlog",
        "ready" => "todo",
        "in-progress" => "in_progress",
        "done" => "done",
        _ => "review",
    }
}

/// Whether a status is one the story gate writes for itself.
///
/// Derived from `STORY_FLOW` through the same projection `advance_story` uses,
/// so a new gate state cannot appear on one side and not the other. `blocked`
/// and `cancelled` are deliberately absent: the gate is linear and cannot
/// express either, so a direct move is the only way to say them and refusing
/// it would remove the capability rather than protect anything.
fn is_gate_owned_status(status: &str) -> bool {
    STORY_FLOW
        .iter()
        .any(|workflow| story_status_for(workflow) == status)
}

/// The article a type name takes, so a refusal reads as English.
///
/// Only `epic` begins with a vowel, but a message an agent is meant to act on
/// should not be the place a reader first wonders whether the tool is careful.
fn article(word: &str) -> &'static str {
    match word.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// How wide a container each type is: an epic contains stories, a story
/// contains tasks, and a task contains nothing.
/// What each type may contain.
///
/// Stated as containment rather than computed from a depth, because the rule is
/// not "narrower than its parent" — an epic may hold another epic. A plan is an
/// epic (its body is the plan, its children are the work), so a programme needs
/// to hold sub-plans, and depth arithmetic could only express that as an
/// exception bolted onto a rule it contradicts.
///
/// A story holds tasks and nothing else; nesting a story in a story has no
/// meaning. A task is a leaf.
fn can_contain(parent_type: &str, child_type: &str) -> bool {
    match parent_type {
        "epic" => true,
        "story" => child_type == "task",
        _ => false,
    }
}

/// A tag name the master file will accept.
///
/// Lowercase, digits and hyphens. The point of a registry is that one concept
/// has one spelling, and `Infra` beside `infra` defeats it before anything else
/// can — so the shape is fixed at the door rather than argued about later.
fn validate_tag_name(name: &str) -> Result<String> {
    let name = nonempty(name, "tag name")?.to_owned();
    let shaped = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !shaped || name.starts_with('-') || name.ends_with('-') {
        bail!(
            "tag {name} is not a usable name: lowercase letters, digits and \
             inner hyphens only, so one concept cannot arrive under two spellings"
        );
    }
    Ok(name)
}

/// Attach registered tags to rows that were already read.
///
/// One query for the whole set rather than one per row: a board with a thousand
/// tasks would otherwise pay a thousand round trips to render a list.
fn attach_tags(connection: &Connection, tasks: &mut [Task]) -> Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let mut statement =
        connection.prepare("SELECT task_id,tag FROM task_tags ORDER BY task_id,tag")?;
    let mut by_task: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (task_id, tag) = row?;
        by_task.entry(task_id).or_default().push(tag);
    }
    for task in tasks.iter_mut() {
        if let Some(tags) = by_task.remove(&task.id) {
            task.tags = tags;
        }
    }
    Ok(())
}

/// Replace a row's tags, refusing any the master file does not hold.
fn set_tags(connection: &Connection, id: &str, tags: &[String]) -> Result<()> {
    let known = connection
        .prepare("SELECT name FROM tags ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    connection.execute("DELETE FROM task_tags WHERE task_id=?", [id])?;
    let mut applied = std::collections::HashSet::new();
    for tag in tags {
        let tag = validate_tag_name(tag)?;
        if !known.contains(&tag) {
            // The same shape as a mistyped flag: name the nearest thing that
            // does exist, and the command that would make this one real.
            let borrowed = known.iter().map(String::as_str).collect::<Vec<_>>();
            let suggestion = crate::nearest(&tag, &borrowed)
                .map(|near| format!(", did you mean {near}?"))
                .unwrap_or_default();
            bail!(
                "tag {tag} is not in this board's master file{suggestion} — \
                 register it first with `tag add {tag}`"
            );
        }
        if applied.insert(tag.clone()) {
            connection.execute(
                "INSERT INTO task_tags(task_id,tag) VALUES(?,?)",
                params![id, tag],
            )?;
        }
    }
    Ok(())
}

/// The nearest ancestor still in draft, if any.
///
/// A draft protects the row it is on and, until this, nothing beneath it. A
/// plan is an epic, so drafting a plan and hanging work under it produced tasks
/// that were immediately claimable: a driver picked up work from a plan nobody
/// had opened yet. Whether the plan was ready was recorded on the plan and
/// consulted by no one.
///
/// The walk is bounded by the same cycle guard the parent chain already has,
/// so a malformed tree cannot hang a claim.
fn draft_ancestor(connection: &Connection, id: &str) -> Result<Option<Task>> {
    let mut current = require_task(connection, id)?;
    let mut seen = std::collections::HashSet::from([id.to_owned()]);
    while let Some(parent) = current.parent_id.clone() {
        if !seen.insert(parent.clone()) {
            bail!("parent cycle detected at {parent}");
        }
        current = require_task(connection, &parent)?;
        if current.status == "draft" {
            return Ok(Some(current));
        }
    }
    Ok(None)
}

/// Refuse work whose plan has not been opened yet.
fn require_no_draft_ancestor(connection: &Connection, id: &str) -> Result<()> {
    if let Some(draft) = draft_ancestor(connection, id)? {
        bail!(
            "task {id} sits under {}, which is still a draft: open it with \
             `task move {} todo` before this can be worked",
            draft.id,
            draft.id
        );
    }
    Ok(())
}

/// Refuse a parent that cannot contain this child.
///
/// The breakdown was implied everywhere and enforced nowhere: `advance_story`
/// flips a parent only when it is an epic, and the id prefixes (`e-`/`s-`/`t-`)
/// read as a hierarchy. All nine type pairings were accepted, so nesting a
/// story under a task recorded a tree that no reader agrees with and produced
/// no signal — the story simply never flips anything, forever, and the operator
/// who mis-typed one `--parent` is never told.
fn require_valid_nesting(child_id: &str, child_type: &str, parent: &Task) -> Result<()> {
    if !can_contain(&parent.task_type, child_type) {
        bail!(
            "task {child_id} is {} {child_type} and cannot nest under {}, which is {} {}: an epic contains epics, stories and tasks; a story contains tasks; a task contains nothing",
            article(child_type),
            parent.id,
            article(&parent.task_type),
            parent.task_type
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
        // Attached after the row is read: a join per task would be a query per
        // task, and the readers below fill these in one pass.
        tags: Vec::new(),
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
        worktree: row.get("worktree")?,
        worktree_kind: row.get("worktree_kind")?,
        branch: row.get("branch")?,
        head_sha: row.get("head_sha")?,
        root_head: row.get("root_head")?,
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
        root_head: row.get("root_head")?,
        created_at: row.get("created_at")?,
    })
}

fn sitrep_row(row: &Row<'_>) -> rusqlite::Result<Sitrep> {
    Ok(Sitrep {
        id: row.get("id")?,
        lane: row.get("lane")?,
        task_id: row.get("task_id")?,
        author: row.get("author")?,
        body: row.get("body")?,
        worktree: row.get("worktree")?,
        branch: row.get("branch")?,
        head_sha: row.get("head_sha")?,
        root_head: row.get("root_head")?,
        dirty_summary: row.get("dirty_summary")?,
        archived: row.get::<_, i64>("archived")? != 0,
        created_at: row.get("created_at")?,
    })
}

fn rule_row(row: &Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: row.get("id")?,
        body: row.get("body")?,
        author: row.get("author")?,
        archived: row.get::<_, i64>("archived")? != 0,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        board_tags: None,
    })
}

fn attention_row(row: &Row<'_>) -> rusqlite::Result<Attention> {
    Ok(Attention {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        kind: row.get("kind")?,
        body: row.get("body")?,
        raised_by: row.get("raised_by")?,
        created_at: row.get("created_at")?,
        status: row.get("status")?,
        resolved_at: row.get("resolved_at")?,
        resolved_by: row.get("resolved_by")?,
        resolution: row.get("resolution")?,
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
        root_head: row.get("root_head")?,
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

/// Resolve the caller's lease, or say precisely why it is not theirs.
///
/// Three different situations used to print "no active lease for task X": the
/// task is genuinely unheld, the caller's lease lapsed, and the task is held
/// right now by somebody else. The third is the restart hazard — a runner that
/// crashed and came back holding a token from before someone else reclaimed the
/// work — and telling it "no active lease" states the opposite of the truth. A
/// caller that believes the task is free reasonably goes on to claim it, and
/// races the live holder for a task the ledger just told it nobody owned.
///
/// The current lease token is never named. A refusal identifies the holder,
/// which the caller may act on, and not the secret that authorizes writes.
fn require_lease(connection: &Connection, task_id: &str, token: &str, now: i64) -> Result<Claim> {
    if let Some(claim) = connection
        .query_row(
            "SELECT * FROM task_claims WHERE task_id=? AND lease_token=? AND expires_at>?",
            params![task_id, token, now],
            claim_row,
        )
        .optional()?
    {
        return Ok(claim);
    }
    match active_claim(connection, task_id, now)? {
        Some(held) => bail!(
            "task {task_id} is leased by {} until {}, and that is not the lease you presented: \
             it was superseded, so reacquire the task before writing to it",
            held.agent_id,
            held.expires_at
        ),
        None => bail!(
            "task {task_id} has no active lease: it was never claimed, or the lease expired and \
             was retired; claim the task to write to it"
        ),
    }
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
/// How many sitreps stay *current* in one lane.
///
/// Ten is what a reader will actually read to answer "where are things". The
/// eleventh does not stop existing, it stops being current — which is the
/// distinction that makes bounding this safe.
const CURRENT_SITREPS_PER_LANE: i64 = 10;

// Nothing deletes a sitrep.
//
// A hard retention cap was written here and then removed. It would have been
// the first thing in this ledger that destroys a record — attention items are
// resolved rather than deleted, handoffs outlive the task they were about,
// and the event trail is append-only. Archiving bounds the *view*, which is
// what "old entries get archived" asks for; bounding the *table* is a
// deliberate operator-run prune over a whole board, not a silent side effect
// of somebody posting an update.
//
// The growth this leaves is the growth `events` and `task_notes` already
// have, so singling this table out would have been inconsistent as well as
// destructive. Measured 2026-08-24: 9.6 MB across all thirteen boards.

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
    /// `None` leaves tags alone; `Some(list)` replaces them wholesale.
    pub tags: Option<Vec<String>>,
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
    /// Where the claimer is standing, resolved by the caller so the store stays
    /// free of subprocesses and remains testable without a repository.
    pub git: Option<crate::gitctx::GitContext>,
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
            let parent = require_task(&transaction, parent)?;
            require_valid_nesting(&id, &input.task_type, &parent)?;
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
        // Every other event kind names who did it; creating a task was the one
        // action the trail could not attribute, because there was no `--as` to
        // record. Measured 2026-08-21 across the live boards, 132 of these
        // carried no actor. An absent actor is still recorded as absent —
        // inventing one would be worse than the gap.
        set_tags(&transaction, &id, &input.tags)?;
        event(
            &transaction,
            Some(&id),
            "task_added",
            input.actor.as_deref(),
            json!({ "type": input.task_type, "status": input.status }),
        )?;
        transaction.commit()?;
        self.require_task(&id)
    }

    pub fn require_task(&self, id: &str) -> Result<Task> {
        let mut one = vec![require_task(&self.connection, id)?];
        attach_tags(&self.connection, &mut one)?;
        Ok(one.remove(0))
    }

    /// Rows, optionally narrowed by status and by tag.
    ///
    /// A tag filter checks the master file first: asking for one that was never
    /// registered returns an empty list otherwise, which reads exactly like
    /// "nothing is tagged that" and is how a typo becomes a wrong answer.
    pub fn list_tasks(&self, status: Option<&str>, tag: Option<&str>) -> Result<Vec<Task>> {
        if let Some(value) = status {
            validate(value, &TASK_STATUSES, "task status")?;
        }
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(value) = status {
            clauses.push("status=?");
            values.push(Box::new(value.to_owned()));
        }
        if let Some(tag) = tag {
            let tag = validate_tag_name(tag)?;
            let known: Option<String> = self
                .connection
                .query_row("SELECT name FROM tags WHERE name=?", [&tag], |row| {
                    row.get(0)
                })
                .optional()?;
            if known.is_none() {
                let names = self.tags()?.into_iter().map(|t| t.name).collect::<Vec<_>>();
                let borrowed = names.iter().map(String::as_str).collect::<Vec<_>>();
                let suggestion = crate::nearest(&tag, &borrowed)
                    .map(|near| format!(", did you mean {near}?"))
                    .unwrap_or_default();
                bail!(
                    "tag {tag} is not in this board's master file{suggestion} — \
                     an unregistered tag would filter to nothing and read like an answer"
                );
            }
            clauses.push("id IN (SELECT task_id FROM task_tags WHERE tag=?)");
            values.push(Box::new(tag));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let sql = format!("SELECT * FROM tasks{where_clause} ORDER BY priority,created_at,id");
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement
            .query_map(params_from_iter(refs), task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        attach_tags(&self.connection, &mut rows)?;
        Ok(rows)
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
        // A story's status column is a projection of its gate, not a field the
        // caller owns. Writing it directly leaves the row asserting one thing
        // and its gate another — and a direct move to `done` stamps
        // completed_at while the story never took a review signoff, never
        // dispatched a merge task, and never flipped its parent epic.
        let gate_bypassed = current.task_type == "story" && is_gate_owned_status(status);
        if gate_bypassed && !force {
            let workflow = current
                .metadata
                .get("workflowStatus")
                .and_then(Value::as_str)
                .unwrap_or("planning");
            bail!(
                "story {id} status is projected from its gate, now at {workflow}: advance it with `story advance` (--force overwrites the projection and records it)"
            );
        }
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
            json!({"status": status, "seizedFrom": seized.map(|claim| claim.agent_id), "gateBypassed": gate_bypassed}),
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
        let previous_parent = current.parent_id.clone();
        let parent = input.parent_id.unwrap_or(current.parent_id);
        if let Some(parent_id) = &parent {
            let parent_task = require_task(&transaction, parent_id)?;
            require_valid_nesting(id, &current.task_type, &parent_task)?;
            let mut cursor = Some(parent_id.clone());
            let mut seen = std::collections::HashSet::from([id.to_owned()]);
            while let Some(value) = cursor {
                if !seen.insert(value.clone()) {
                    bail!("parent {parent_id} would create a cycle");
                }
                cursor = require_task(&transaction, &value)?.parent_id;
            }
        }
        // Snapshot before the fields below consume `current`. A plan is an
        // epic's body, so an edit that leaves no record of what it replaced
        // destroys the previous plan outright — and `task_updated` recorded an
        // empty payload, saying that something changed and nothing about what.
        let previous_title = current.title.clone();
        let previous_body = current.body.clone();
        let previous_assignee = current.assignee.clone();
        let previous_lane = current.lane.clone();
        let previous_deliverable = current.deliverable.clone();
        let previous_stale = current.stale_minutes;
        let previous_driver = current.driver_only;
        let previous_priority = current.priority;
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
        if let Some(tags) = &input.tags {
            set_tags(&transaction, id, tags)?;
        }
        // Name what moved, and keep the one value whose loss is unrecoverable.
        // Everything else can be read off the row; a replaced body cannot.
        let mut changed = Vec::new();
        for (field, moved) in [
            ("title", title != previous_title),
            ("body", body != previous_body),
            ("assignee", assignee != previous_assignee),
            ("lane", lane != previous_lane),
            ("deliverable", deliverable != previous_deliverable),
            ("staleMinutes", stale != previous_stale),
            ("driverOnly", driver != previous_driver),
            ("priority", priority != previous_priority),
            ("parentID", parent != previous_parent),
        ] {
            if moved {
                changed.push(field);
            }
        }
        let mut payload = json!({ "changed": changed });
        if body != previous_body {
            payload["previousBody"] = json!(previous_body);
        }
        event(
            &transaction,
            Some(id),
            "task_updated",
            Some(&actor),
            payload,
        )?;
        transaction.commit()?;
        self.require_task(id)
    }

    pub fn claim(&mut self, id: Option<&str>, options: ClaimOptions) -> Result<ClaimReceipt> {
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
                   AND t.type=?
                   AND c.task_id IS NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM task_dependencies d
                     JOIN tasks dep ON dep.id=d.depends_on
                     WHERE d.task_id=t.id AND dep.status<>'done'
                   )
                 ORDER BY t.priority,t.created_at,t.id",
            )?;
            let candidates = statement
                .query_map([CLAIMABLE_TYPE], task_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            // Work under an unopened plan is not offered. Filtered here rather
            // than in the query so that `--next` and an explicit claim refuse
            // for the same reason, computed once.
            let mut eligible = Vec::new();
            for candidate in candidates {
                let routable = (!candidate.driver_only
                    || options.caller_scope.as_deref() == Some("driver"))
                    && (candidate
                        .assignee
                        .as_ref()
                        .is_none_or(|value| value == &agent)
                        || options.allow_reassign);
                if routable && draft_ancestor(&transaction, &candidate.id)?.is_none() {
                    eligible.push(candidate);
                }
            }
            let candidates = eligible;
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
        require_claimable_type(&task.id, &task.task_type)?;
        require_no_draft_ancestor(&transaction, &task.id)?;
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
            "INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at,worktree,worktree_kind,branch,head_sha,root_head) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                task.id,agent,options.session_id,token,now,now,now+options.lease_ms,
                options.git.as_ref().map(|g| g.worktree.clone()),
                options.git.as_ref().map(|g| g.worktree_kind.to_owned()),
                options.git.as_ref().and_then(|g| g.branch.clone()),
                options.git.as_ref().map(|g| g.head.clone()),
                options.git.as_ref().and_then(|g| g.root_head.clone()),
            ],
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
        Ok(ClaimReceipt {
            claim: result,
            rules: self.rule_summaries(false)?,
        })
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
            "INSERT INTO checkpoints(task_id,author,session_id,model,state,summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at,root_head) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![input.task_id,input.author,input.session_id,input.model,input.state,nonempty(&input.summary,"summary")?,nonempty(&input.intent,"intent")?,nonempty(&input.next_action,"next action")?,serde_json::to_string(&input.blockers)?,serde_json::to_string(&input.validations)?,input.repo_path,input.branch,input.head_sha,input.dirty_summary,now,input.root_head],
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

    /// Handoffs, newest first, narrowed by any combination of filters.
    ///
    /// `to_agent` is the one that makes a session handoff findable. A task
    /// handoff is reachable through its task; a session handoff is about no
    /// task, so without a way to ask "what is waiting for driver-2" the only
    /// route to it would be reading the whole list. The successor knows who it
    /// is, and that is the key it should be able to look itself up by.
    /// Record something only the operator can retire.
    ///
    /// The agent raising it names the kind, because "what sort of thing is
    /// this" is the part a reader needs first and the part an agent knows and
    /// a reader would have to infer from prose.
    /// Register a tag in this board's master file.
    pub fn add_tag(
        &mut self,
        name: &str,
        description: Option<&str>,
        actor: Option<&str>,
    ) -> Result<Tag> {
        let name = validate_tag_name(name)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: Option<String> = transaction
            .query_row("SELECT name FROM tags WHERE name=?", [&name], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_some() {
            bail!("tag {name} is already in the master file");
        }
        let now = now_ms();
        transaction.execute(
            "INSERT INTO tags(name,description,created_by,created_at) VALUES(?,?,?,?)",
            params![name, description, actor, now],
        )?;
        event(
            &transaction,
            None,
            "tag_added",
            actor,
            json!({ "tag": name }),
        )?;
        transaction.commit()?;
        self.tag(&name)
    }

    fn tag(&self, name: &str) -> Result<Tag> {
        self.tags()?
            .into_iter()
            .find(|tag| tag.name == name)
            .with_context(|| format!("tag {name} not found"))
    }

    /// The master file, with how many rows carry each entry.
    pub fn tags(&self) -> Result<Vec<Tag>> {
        let mut statement = self.connection.prepare(
            "SELECT t.name,t.description,t.created_by,t.created_at,
                    (SELECT count(*) FROM task_tags x WHERE x.tag=t.name) AS uses
             FROM tags t ORDER BY t.name",
        )?;
        statement
            .query_map([], |row| {
                Ok(Tag {
                    name: row.get("name")?,
                    description: row.get("description")?,
                    created_by: row.get("created_by")?,
                    created_at: row.get("created_at")?,
                    uses: row.get("uses")?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Retire a tag. One still in use needs `--force`, and says how many rows.
    pub fn remove_tag(&mut self, name: &str, actor: Option<&str>, force: bool) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let uses: i64 = transaction.query_row(
            "SELECT count(*) FROM task_tags WHERE tag=?",
            [name],
            |row| row.get(0),
        )?;
        if uses > 0 && !force {
            bail!(
                "tag {name} is carried by {uses} row{}; removing it would strip them \
                 silently — pass --force to do it anyway",
                if uses == 1 { "" } else { "s" }
            );
        }
        transaction.execute("DELETE FROM task_tags WHERE tag=?", [name])?;
        let removed = transaction.execute("DELETE FROM tags WHERE name=?", [name])?;
        if removed == 0 {
            bail!("tag {name} is not in the master file");
        }
        event(
            &transaction,
            None,
            "tag_removed",
            actor,
            json!({ "tag": name, "strippedFrom": uses }),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Add an operator rule at the end of this board's rule document.
    pub fn add_rule(&mut self, body: &str, actor: &str) -> Result<Rule> {
        validate_rule_body(body)?;
        let body = body.to_owned();
        let actor = validate_rule_actor(actor)?.to_owned();
        let id = format!("r-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous_created: Option<i64> =
            transaction.query_row("SELECT max(created_at) FROM rules", [], |row| row.get(0))?;
        let now = now_ms().max(previous_created.unwrap_or(0).saturating_add(1));
        transaction.execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at) VALUES(?,?,?,0,?,?)",
            params![id, body, actor, now, now],
        )?;
        event(
            &transaction,
            None,
            "rule_added",
            Some(&actor),
            json!({"ruleID": id}),
        )?;
        let result = transaction.query_row("SELECT * FROM rules WHERE id=?", [&id], rule_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Active rules are a document, so their order is oldest first.
    pub fn rules(&self, include_archived: bool) -> Result<Vec<Rule>> {
        let clause = if include_archived {
            ""
        } else {
            " WHERE archived=0"
        };
        let sql = format!("SELECT * FROM rules{clause} ORDER BY created_at,id");
        let mut statement = self.connection.prepare(&sql)?;
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
                    scope: "project".into(),
                    id: rule.id,
                    headline,
                    has_more,
                    bytes: rule.body.len(),
                    board_tags: None,
                })
            })
            .collect()
    }

    pub fn rule(&self, id: &str) -> Result<Rule> {
        self.connection
            .query_row("SELECT * FROM rules WHERE id=?", [id], rule_row)
            .optional()?
            .with_context(|| format!("rule {id} not found"))
    }

    /// Revise a rule without destroying the text it replaced.
    pub fn update_rule(&mut self, id: &str, body: &str, actor: &str) -> Result<Rule> {
        validate_rule_body(body)?;
        let body = body.to_owned();
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = transaction
            .query_row("SELECT body FROM rules WHERE id=?", [id], |row| row.get(0))
            .optional()?;
        let previous = previous.with_context(|| format!("rule {id} not found"))?;
        transaction.execute(
            "UPDATE rules SET body=?,author=?,updated_at=? WHERE id=?",
            params![body, actor, now_ms(), id],
        )?;
        event(
            &transaction,
            None,
            "rule_updated",
            Some(&actor),
            json!({"ruleID": id, "previousBody": previous}),
        )?;
        let result = transaction.query_row("SELECT * FROM rules WHERE id=?", [id], rule_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Retire a rule from the active set without erasing it.
    pub fn retire_rule(&mut self, id: &str, actor: &str) -> Result<Rule> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE rules SET archived=1,author=?,updated_at=? WHERE id=? AND archived=0",
            params![actor, now_ms(), id],
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
        event(
            &transaction,
            None,
            "rule_retired",
            Some(&actor),
            json!({"ruleID": id}),
        )?;
        let result = transaction.query_row("SELECT * FROM rules WHERE id=?", [id], rule_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Post where a lane stands, and retire what that supersedes.
    ///
    /// No lease, no task, no ceremony: the cost of writing one has to stay low
    /// enough that an agent writes twenty a day, because the alternative it is
    /// competing with is a reply that scrolls away.
    ///
    /// Provenance is captured rather than asked for, the same way a claim's is
    /// — an update that says "tests green" without saying which checkout is a
    /// claim nobody can check.
    pub fn post_sitrep(
        &mut self,
        lane: &str,
        body: &str,
        author: &str,
        task_id: Option<&str>,
        git: Option<&crate::gitctx::GitContext>,
    ) -> Result<Sitrep> {
        let lane = nonempty(lane, "lane")?.to_owned();
        let body = nonempty(body, "sitrep body")?.to_owned();
        let author = nonempty(author, "author")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(id) = task_id {
            require_task(&transaction, id)?;
        }
        let now = now_ms();
        let id = format!("sr-{}", &Uuid::new_v4().simple().to_string()[..8]);
        transaction.execute(
            "INSERT INTO sitreps(id,lane,task_id,author,body,worktree,branch,head_sha,root_head,dirty_summary,archived,created_at) \
             VALUES(?,?,?,?,?,?,?,?,?,?,0,?)",
            params![
                id,
                lane,
                task_id,
                author,
                body,
                git.map(|c| c.worktree.clone()),
                git.and_then(|c| c.branch.clone()),
                git.map(|c| c.head.clone()),
                git.and_then(|c| c.root_head.clone()),
                git.map(crate::gitctx::dirty_summary),
                now,
            ],
        )?;
        // Archiving happens on write rather than on a timer. Nothing has to be
        // scheduled, and the current view is bounded the moment it would have
        // stopped being current.
        let archived = transaction.execute(
            "UPDATE sitreps SET archived=1 WHERE lane=? AND archived=0 AND id NOT IN \
             (SELECT id FROM sitreps WHERE lane=? AND archived=0 ORDER BY created_at DESC, id DESC LIMIT ?)",
            params![lane, lane, CURRENT_SITREPS_PER_LANE],
        )?;
        event(
            &transaction,
            task_id,
            "sitrep_posted",
            Some(&author),
            json!({"sitrepID": id, "lane": lane, "archived": archived}),
        )?;
        let result =
            transaction.query_row("SELECT * FROM sitreps WHERE id=?", [&id], sitrep_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Where things stand, newest first.
    ///
    /// Newest-first, unlike `attention`: the question this answers is "what is
    /// true now", and the newest update is the answer. Archived rows are
    /// excluded by default and readable on request — hidden, never gone.
    pub fn sitreps(
        &self,
        lane: Option<&str>,
        include_archived: bool,
        task: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Sitrep>> {
        if let Some(id) = task {
            require_task(&self.connection, id)?;
        }
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(lane) = lane {
            clauses.push("lane=?");
            values.push(Box::new(lane.to_owned()));
        }
        if let Some(task) = task {
            clauses.push("task_id=?");
            values.push(Box::new(task.to_owned()));
        }
        if !include_archived {
            clauses.push("archived=0");
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        values.push(Box::new(limit));
        let sql = format!(
            "SELECT * FROM sitreps{where_clause} ORDER BY created_at DESC, id DESC LIMIT ?"
        );
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(params_from_iter(refs), sitrep_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn raise_attention(
        &mut self,
        body: &str,
        kind: &str,
        raised_by: &str,
        task_id: Option<&str>,
    ) -> Result<Attention> {
        validate(kind, &ATTENTION_KINDS, "attention kind")?;
        let body = nonempty(body, "attention body")?.to_owned();
        let raised_by = nonempty(raised_by, "raised by")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(id) = task_id {
            require_task(&transaction, id)?;
        }
        let now = now_ms();
        let id = format!("a-{}", &Uuid::new_v4().simple().to_string()[..8]);
        transaction.execute(
            "INSERT INTO attention(id,task_id,kind,body,raised_by,created_at,status,resolved_at,resolved_by,resolution) VALUES(?,?,?,?,?,?,'open',NULL,NULL,NULL)",
            params![id, task_id, kind, body, raised_by, now],
        )?;
        event(
            &transaction,
            task_id,
            "attention_raised",
            Some(&raised_by),
            json!({"attentionID": id, "kind": kind}),
        )?;
        let result =
            transaction.query_row("SELECT * FROM attention WHERE id=?", [&id], attention_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Open items first, oldest first within each state.
    ///
    /// Oldest-first is the opposite of every other listing here, and
    /// deliberate: an unanswered question does not get less urgent by being
    /// ignored, and newest-first buries exactly the item that has been waiting
    /// longest.
    pub fn attention(
        &self,
        status: Option<&str>,
        kind: Option<&str>,
        task: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Attention>> {
        if let Some(value) = status {
            validate(value, &["open", "resolved"], "attention status")?;
        }
        if let Some(value) = kind {
            validate(value, &ATTENTION_KINDS, "attention kind")?;
        }
        if let Some(id) = task {
            require_task(&self.connection, id)?;
        }
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(status) = status {
            clauses.push("status=?");
            values.push(Box::new(status.to_owned()));
        }
        if let Some(kind) = kind {
            clauses.push("kind=?");
            values.push(Box::new(kind.to_owned()));
        }
        if let Some(task) = task {
            clauses.push("task_id=?");
            values.push(Box::new(task.to_owned()));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        values.push(Box::new(limit));
        let sql = format!(
            "SELECT * FROM attention{where_clause} ORDER BY status='resolved',created_at ASC,id ASC LIMIT ?"
        );
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(params_from_iter(refs), attention_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Settle an item. The row stays; only its state moves.
    pub fn resolve_attention(
        &mut self,
        id: &str,
        actor: &str,
        resolution: Option<&str>,
    ) -> Result<Attention> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row("SELECT * FROM attention WHERE id=?", [id], attention_row)
            .optional()?
            .with_context(|| format!("attention {id} not found"))?;
        // Resolving twice would overwrite who settled it and when, which is
        // the part of the record worth keeping.
        if existing.status != "open" {
            bail!(
                "attention {id} was already resolved by {} — it is history, not a queue entry",
                existing.resolved_by.unwrap_or_else(|| "someone".into())
            );
        }
        let now = now_ms();
        transaction.execute(
            "UPDATE attention SET status='resolved',resolved_at=?,resolved_by=?,resolution=? WHERE id=?",
            params![now, actor, resolution, id],
        )?;
        event(
            &transaction,
            existing.task_id.as_deref(),
            "attention_resolved",
            Some(&actor),
            json!({"attentionID": id, "kind": existing.kind}),
        )?;
        let result =
            transaction.query_row("SELECT * FROM attention WHERE id=?", [id], attention_row)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn handoffs(
        &self,
        task: Option<&str>,
        status: Option<&str>,
        to_agent: Option<&str>,
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
        // Built up rather than enumerated: three optional filters is eight
        // hand-written queries, and the eighth is the one that gets forgotten.
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(task) = task {
            clauses.push("task_id=?");
            values.push(Box::new(task.to_owned()));
        }
        if let Some(status) = status {
            clauses.push("status=?");
            values.push(Box::new(status.to_owned()));
        }
        if let Some(agent) = to_agent {
            clauses.push("to_agent=?");
            values.push(Box::new(agent.to_owned()));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        values.push(Box::new(limit));
        let sql = format!(
            "SELECT * FROM handoffs{where_clause} ORDER BY created_at DESC,id DESC LIMIT ?"
        );
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
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
        // A task and a lease travel together: a lease exists only over a task,
        // and handing a task over without one would let any caller move work
        // they do not hold. Neither half is meaningful alone, so the pair is
        // resolved once here rather than checked at each use.
        let claim = match (&input.task_id, &input.lease_token) {
            (Some(task_id), Some(token)) => {
                let claim = require_lease(&transaction, task_id, token, now)?;
                if claim.agent_id != input.from_agent {
                    bail!(
                        "lease belongs to {}, not {}",
                        claim.agent_id,
                        input.from_agent
                    );
                }
                Some(claim)
            }
            (None, None) => None,
            (Some(_), None) => bail!("handing over a task needs its lease: pass --lease"),
            (None, Some(_)) => {
                bail!("a lease is held over a task, so --lease needs the task id it belongs to")
            }
        };
        let summary = nonempty(&input.summary, "summary")?.to_owned();
        let intent = nonempty(&input.intent, "intent")?.to_owned();
        let next = nonempty(&input.next_action, "next action")?.to_owned();
        let blockers = serde_json::to_string(&input.blockers)?;
        let validations = serde_json::to_string(&input.validations)?;
        // A task handoff closes the task with a checkpoint, so a successor
        // resumes from the durable record rather than the handoff's prose. A
        // session handoff has no task to checkpoint, and inventing one would
        // put a checkpoint on a row that was never worked.
        let checkpoint_seq = match (&input.task_id, claim) {
            (Some(task_id), claim) => {
                transaction.execute(
                    "INSERT INTO checkpoints(task_id,author,session_id,model,state,summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at,root_head) VALUES(?,?,?,?,? ,?,?,?,?,?,?,?,?,?,?,?)",
                    params![task_id,input.from_agent,input.from_session.clone().or(claim.and_then(|claim| claim.session_id)),input.from_model,"continue",summary,intent,next,blockers,validations,input.repo_path,input.branch,input.head_sha,input.dirty_summary,now,input.root_head],
                )?;
                Some(transaction.last_insert_rowid())
            }
            (None, _) => None,
        };
        let id = format!("h-{}", &Uuid::new_v4().simple().to_string()[..8]);
        transaction.execute(
            "INSERT INTO handoffs(id,task_id,checkpoint_seq,reason,status,from_agent,from_session,from_model,to_agent,summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at,root_head,accepted_at,accepted_by,accepted_session) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,NULL,NULL,NULL)",
            params![id,input.task_id,checkpoint_seq,input.reason,"pending",input.from_agent,input.from_session,input.from_model,input.to_agent,summary,intent,next,blockers,validations,input.repo_path,input.branch,input.head_sha,input.dirty_summary,now,input.root_head],
        )?;
        // Releasing the lease and returning the task to the queue is the point
        // of a task handoff. A session handoff holds nothing and releases
        // nothing, so there is no task state to disturb.
        if let Some(task_id) = &input.task_id {
            transaction.execute("DELETE FROM task_claims WHERE task_id=?", [task_id])?;
            transaction.execute(
                "UPDATE tasks SET status='todo',updated_at=?,completed_at=NULL WHERE id=?",
                params![now, task_id],
            )?;
        }
        event(
            &transaction,
            input.task_id.as_deref(),
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
        git: Option<crate::gitctx::GitContext>,
    ) -> Result<(Handoff, Option<Claim>)> {
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
        // A session handoff carries no task, so there is nothing to lease and
        // nothing to make claimable. Accepting it is an acknowledgement: it
        // records who picked the thread up and stops it being offered again.
        let Some(task_id) = handoff.task_id.clone() else {
            transaction.execute("UPDATE handoffs SET status='accepted',accepted_at=?,accepted_by=?,accepted_session=? WHERE id=? AND status='pending'",params![now,agent,session,id])?;
            event(
                &transaction,
                None,
                "handoff_accepted",
                Some(&agent),
                json!({"handoffID":id,"session":true}),
            )?;
            let updated =
                transaction.query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)?;
            transaction.commit()?;
            return Ok((updated, None));
        };
        let task = require_task(&transaction, &task_id)?;
        require_claimable_type(&task.id, &task.task_type)?;
        require_no_draft_ancestor(&transaction, &task.id)?;
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
        transaction.execute("INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at,worktree,worktree_kind,branch,head_sha,root_head) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",params![
            task.id,agent,session,token,now,now,now+lease_ms,
            git.as_ref().map(|g| g.worktree.clone()),
            git.as_ref().map(|g| g.worktree_kind.to_owned()),
            git.as_ref().and_then(|g| g.branch.clone()),
            git.as_ref().map(|g| g.head.clone()),
            git.as_ref().and_then(|g| g.root_head.clone()),
        ])?;
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
        Ok((updated, Some(claim)))
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
        validate(target, &STORY_FLOW, "story workflow status")?;
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
        let status = story_status_for(target);
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
        const SITREPS: usize = 20;
        let task = self.require_task(id)?;
        // Over-fetch by one so "there is older history" is measured, not
        // assumed. `truncated` was hardcoded false, so a resuming agent was
        // told it held the whole record while notes were being dropped.
        let mut notes = self.notes(id, NOTES as i64 + 1)?;
        let mut checkpoints = self.checkpoints(id, CHECKPOINTS as i64 + 1)?;
        let mut handoffs = self.handoffs(Some(id), None, None, HANDOFFS as i64 + 1)?;
        handoffs.reverse();
        // Archived ones included: the packet is what a successor reads to
        // reconstruct the work, and "superseded as the current view" is not
        // the same as "not worth knowing" to somebody starting cold.
        let mut sitreps = self.sitreps(None, true, Some(id), SITREPS as i64 + 1)?;
        sitreps.reverse();
        let mut truncated = keep_newest(&mut notes, NOTES);
        truncated |= keep_newest(&mut checkpoints, CHECKPOINTS);
        truncated |= keep_newest(&mut handoffs, HANDOFFS);
        truncated |= keep_newest(&mut sitreps, SITREPS);
        Ok(ContextPacket {
            task,
            ancestors: self.ancestors(id)?,
            dependencies: self.dependencies(id)?,
            claim: self.get_claim(id)?.as_ref().map(ClaimSummary::from),
            notes,
            checkpoints,
            handoffs,
            rules: self.rule_summaries(false)?,
            sitreps,
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
    fn a_rule_requires_a_nonempty_first_line_and_preserves_valid_body_text() {
        for invalid in ["", "   ", "\nDetail without a headline", "  \nDetail"] {
            assert!(
                validate_rule_body(invalid).is_err(),
                "invalid rule body was accepted: {invalid:?}"
            );
        }

        for valid in [
            "One-line rule.",
            "Headline.\n\nSupporting detail remains available lazily.\n",
        ] {
            assert!(
                validate_rule_body(valid).is_ok(),
                "valid rule body was refused: {valid:?}"
            );
        }
    }

    #[test]
    fn a_rule_actor_must_name_its_author() {
        for invalid in ["", " ", "\n\t"] {
            let error = validate_rule_actor(invalid)
                .expect_err("an empty rule author was accepted")
                .to_string();
            assert!(error.contains("author"), "{error}");
        }
        assert_eq!(validate_rule_actor("codex@driver").unwrap(), "codex@driver");
    }

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
    fn a_refusal_names_a_type_with_the_right_article() {
        assert_eq!(article("epic"), "an");
        for consonant in ["story", "task"] {
            assert_eq!(article(consonant), "a", "{consonant}");
        }
        let error = require_claimable_type("e-1", "epic")
            .expect_err("an epic is not claimable")
            .to_string();
        assert!(error.contains("is an epic"), "{error}");
        assert!(!error.contains("a epic"), "{error}");
    }

    #[test]
    fn a_container_holds_only_what_it_can_contain() {
        let parent_of = |kind: &str| Task {
            id: format!("p-{kind}"),
            task_type: kind.to_owned(),
            parent_id: None,
            title: "parent".into(),
            body: None,
            assignee: None,
            lane: None,
            deliverable: None,
            stale_minutes: None,
            driver_only: false,
            status: "todo".into(),
            priority: 3,
            created_at: 0,
            updated_at: 0,
            completed_at: None,
            metadata: json!({}),
            tags: Vec::new(),
        };

        // An epic holds anything, including another epic: a plan is an epic, so
        // a programme plan has to be able to hold its sub-plans.
        for (child, parent) in [
            ("epic", "epic"),
            ("story", "epic"),
            ("task", "epic"),
            ("task", "story"),
        ] {
            assert!(
                require_valid_nesting("c-1", child, &parent_of(parent)).is_ok(),
                "{child} under {parent} must be allowed"
            );
        }
        // The rest still have no meaning: a story in a story, a leaf holding
        // anything, or a container inside something narrower than itself.
        for (child, parent) in [
            ("epic", "story"),
            ("epic", "task"),
            ("story", "story"),
            ("story", "task"),
            ("task", "task"),
        ] {
            let error = require_valid_nesting("c-1", child, &parent_of(parent))
                .expect_err(&format!("{child} must not nest under {parent}"))
                .to_string();
            assert!(error.contains(child) && error.contains(parent), "{error}");
        }
    }

    #[test]
    fn every_gate_state_projects_a_status_the_gate_owns() {
        // The guard and `advance_story` must read the same projection, or a new
        // gate state becomes writable by hand on one side and not the other.
        for workflow in STORY_FLOW {
            let projected = story_status_for(workflow);
            assert!(
                TASK_STATUSES.contains(&projected),
                "{workflow} projects {projected}, which is not a task status"
            );
            assert!(
                is_gate_owned_status(projected),
                "{workflow} projects {projected}, which the guard does not claim"
            );
        }
    }

    #[test]
    fn the_gate_does_not_own_what_it_cannot_express() {
        // The gate is linear: it has no blocked and no cancelled state, so a
        // direct move is the only way to say either. Guarding them would remove
        // the capability rather than protect the projection.
        for free in ["blocked", "cancelled"] {
            assert!(
                !is_gate_owned_status(free),
                "{free} must stay writable — the gate cannot express it"
            );
            assert!(
                !STORY_FLOW.iter().any(|w| story_status_for(w) == free),
                "{free} became reachable from the gate; the guard must follow"
            );
        }
    }

    #[test]
    fn only_a_task_is_claimable() {
        assert!(
            require_claimable_type("t-1", CLAIMABLE_TYPE).is_ok(),
            "a task is the one claimable type"
        );

        // The message has to name the row, the type that disqualified it, and
        // what to do instead — a bare refusal leaves the agent with no next move.
        let story = require_claimable_type("s-1", "story")
            .expect_err("a story must not be claimable")
            .to_string();
        assert!(story.contains("s-1"), "{story}");
        assert!(story.contains("story"), "{story}");
        assert!(story.contains("story advance"), "{story}");

        let epic = require_claimable_type("e-1", "epic")
            .expect_err("an epic must not be claimable")
            .to_string();
        assert!(epic.contains("e-1"), "{epic}");
        assert!(epic.contains("children"), "{epic}");
    }

    #[test]
    fn a_tag_name_has_exactly_one_spelling() {
        for good in ["infra", "queuer", "askie", "px-crm", "v2", "a"] {
            assert_eq!(
                validate_tag_name(good).expect("a plain name is usable"),
                good
            );
        }

        // The whole value of a master file is that one concept has one
        // spelling. Case, spaces and punctuation are each a way for a second
        // spelling of the same thing to enter, so all three are refused at the
        // door rather than deduplicated afterwards.
        for bad in ["Infra", "in fra", "in_fra", "in.fra", "infra!", "INFRA"] {
            let error = validate_tag_name(bad)
                .expect_err(&format!("tag {bad} must be refused"))
                .to_string();
            assert!(error.contains("one concept"), "{error}");
        }

        // A leading or trailing hyphen is legal ASCII but reads as the same tag
        // as its trimmed form, which is the collision this is guarding.
        for edge in ["-infra", "infra-", "-"] {
            assert!(
                validate_tag_name(edge).is_err(),
                "tag {edge} must be refused"
            );
        }

        // An empty name is caught before the shape check, so it must still say
        // which field was empty rather than talking about hyphens.
        let empty = validate_tag_name("  ")
            .expect_err("an empty tag name is not a tag")
            .to_string();
        assert!(empty.contains("tag name"), "{empty}");
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
