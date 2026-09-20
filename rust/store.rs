use crate::LIMIT_CEILING;
use crate::authz::AuthzContext;
use crate::db::{
    SnapshotSource, checkpoint as wal_checkpoint, create_backup_target, integrity, open_board,
    open_board_readonly,
};
use crate::model::*;
use crate::registry::now_ms;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter, types::Type};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use uuid::Uuid;

fn nonempty<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{label} is required");
    }
    Ok(trimmed)
}

/// A `--lane` filter value, refused when it names nothing.
///
/// No task carries an empty lane, so `--lane ""` would return an empty list
/// that reads exactly like "nothing is in that lane".
fn lane_filter(value: &str) -> Result<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!(
            "--lane must name a lane, got an empty value: pass the lane to filter to, or drop --lane"
        );
    }
    Ok(trimmed)
}

fn validate_rule_actor(value: &str) -> Result<&str> {
    nonempty(value, "author")
}

#[cfg(test)]
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

/// "a", "b" or "c", with the Oxford comma the capability refusal already uses.
/// A refusal that names every value it accepts is the ADR-008 shape: the
/// caller reads the exit status and the fix in one message.
fn expected_values(allowed: &[&str]) -> String {
    match allowed.len() {
        0 => String::new(),
        1 => allowed[0].to_owned(),
        2 => format!("{} or {}", allowed[0], allowed[1]),
        _ => {
            let (last, rest) = allowed.split_last().expect("non-empty");
            format!("{}, or {last}", rest.join(", "))
        }
    }
}

fn validate(value: &str, allowed: &[&str], label: &str) -> Result<()> {
    if !allowed.contains(&value) {
        bail!(
            "invalid {label} {value}; expected {}",
            expected_values(allowed)
        );
    }
    Ok(())
}

fn subscription_identifier(value: &str, label: &str, max: usize) -> Result<String> {
    let value = nonempty(value, label)?;
    if value.len() > max
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!(
            "{label} must be at most {max} ASCII characters, start with a letter or digit, and contain only letters, digits, dot, underscore, or hyphen"
        );
    }
    Ok(value.to_owned())
}

fn normalized_unique(values: &[String]) -> Vec<String> {
    values
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_subscription_bounds(input: &AddSubscription) -> Result<()> {
    if !(1..=300_000).contains(&input.timeout_ms) {
        bail!("subscription timeout must be between 1 and 300000 milliseconds");
    }
    if !(0..=20).contains(&input.max_retries) {
        bail!("subscription max retries must be between 0 and 20");
    }
    if !(1..=10_000).contains(&input.rate_per_minute) {
        bail!("subscription rate per minute must be between 1 and 10000");
    }
    if !(1..=64).contains(&input.max_concurrency) {
        bail!("subscription max concurrency must be between 1 and 64");
    }
    Ok(())
}

fn validate_delivery_lease_duration(lease_duration_ms: i64) -> Result<()> {
    if !(1..=330_000).contains(&lease_duration_ms) {
        bail!("subscription delivery lease duration must be between 1 and 330000 milliseconds");
    }
    Ok(())
}

fn validate_nonnegative_now(now: i64, label: &str) -> Result<()> {
    if now < 0 {
        bail!("{label} must be non-negative");
    }
    Ok(())
}

fn validate_event_bounds(after: Option<i64>, before: Option<i64>) -> Result<()> {
    if after.is_some_and(|value| value < 0) {
        bail!("--after must be non-negative");
    }
    if before.is_some_and(|value| value < 0) {
        bail!("--before must be non-negative");
    }
    if after
        .zip(before)
        .is_some_and(|(after, before)| after > before)
    {
        bail!("--after must not be later than --before");
    }
    Ok(())
}

fn validate_delivery_error_code(value: &str) -> Result<String> {
    let value = nonempty(value, "delivery error code")?;
    if value != value.to_ascii_lowercase() {
        bail!("delivery error code must be lowercase");
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b':')
    }) {
        bail!(
            "delivery error code must contain only lowercase letters, digits, underscore, hyphen, or colon"
        );
    }
    Ok(value.to_owned())
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

/// Refuse a story verb on anything but a story.
///
/// The mirror of [`require_claimable_type`]: `story advance` and `story
/// signoff` walk a story's gate, and an epic or a task has none. An epic's
/// status is moved by the gate exactly once and then by `task move` — no
/// `epic advance` verb exists — so the refusal names the verb that does work
/// rather than only saying "not a story" (ADR-008: a refusal is its own fix).
fn require_story_type(id: &str, task_type: &str, verb: &str) -> Result<()> {
    if task_type == "story" {
        return Ok(());
    }
    let remedy = match task_type {
        "epic" => format!("move it with `task move {id} <status>` instead"),
        _ => format!("a {task_type} has no workflow gate"),
    };
    bail!(
        "task {id} is {} {task_type}, and only a story {verb}: {remedy}",
        article(task_type)
    );
}

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

/// The statuses that assert work is under way on a row, or finished.
///
/// A completion gate governs exactly these. The rest are bookkeeping about
/// where a row sits — `draft` and `backlog` say it is not scheduled, `todo`
/// that it is queued, `blocked` that it is stuck, `cancelled` that it will not
/// be done — and gating those would take away the routes a holder needs when
/// a prerequisite reopens underneath it. `review` is work: it asserts the
/// deliverable exists and is being judged.
fn is_work_bearing_status(status: &str) -> bool {
    matches!(status, "in_progress" | "review" | "done")
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

/// The store-layer band on an event read's `LIMIT`.
///
/// Not a duplicate of `Args::limit`: `LIMIT -1` means *no limit* in SQLite,
/// and these two reads are reached by `watch`'s poll loop and by the MCP and
/// serve adapters as well as by the CLI, so the floor lives where the query
/// is built rather than only where a flag is parsed. It shares
/// [`crate::LIMIT_CEILING`] so the two cannot disagree about the top.
fn validate_event_limit(limit: i64) -> Result<()> {
    if !(0..=LIMIT_CEILING).contains(&limit) {
        bail!("--limit must be between 0 and {LIMIT_CEILING}, got {limit}");
    }
    Ok(())
}

#[allow(dead_code)]
fn board_event_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        seq: row.get("seq")?,
        task_id: row.get("task_id")?,
        kind: row.get("kind")?,
        actor: row.get("actor")?,
        payload: parse_value(row.get("payload")?),
        created_at: row.get("created_at")?,
        archived: row.get::<_, i64>("archived")? != 0,
        prev_hash: row.get("prev_hash")?,
        event_hash: row.get("event_hash")?,
    })
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

/// The estates a namespaced tag may be filed under.
///
/// The source of truth until a registry row supersedes it. A slashed tag is a
/// claim about *whose* subsystem it names, and a claim nothing checks is a
/// typo waiting to become a second vocabulary: `ifac/aix-chat` beside
/// `ifca/aix-chat` is exactly the collision the master file exists to prevent,
/// one level up. Three names, written down once, so adding a fourth is a
/// deliberate edit rather than a side effect of a mistyped filter.
pub(crate) const ESTATES: [&str; 3] = ["ifca", "unum", "geoyws"];

/// A tag name the master file will accept.
///
/// Lowercase, digits and hyphens. The point of a registry is that one concept
/// has one spelling, and `Infra` beside `infra` defeats it before anything else
/// can — so the shape is fixed at the door rather than argued about later.
///
/// A name may also be namespaced as `<estate>/<subsystem>` — one or more
/// segments of that same alphabet joined by single slashes, with no leading,
/// trailing or doubled slash. The first segment of a slashed name must be one
/// of [`ESTATES`]; a slash-free name stays exactly what it always was, so
/// every board that never adopted a namespace is untouched.
pub(crate) fn validate_tag_name(name: &str) -> Result<String> {
    let name = nonempty(name, "tag name")?.to_owned();
    let segment_shaped = |segment: &str| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !segment.starts_with('-')
            && !segment.ends_with('-')
    };
    let shaped = name.split('/').all(segment_shaped);
    if !shaped {
        bail!(
            "tag {name} is not a usable name: lowercase letters, digits and \
             inner hyphens only, so one concept cannot arrive under two spellings"
        );
    }
    if let Some((estate, _)) = name.split_once('/')
        && !ESTATES.contains(&estate)
    {
        bail!(
            "tag {name} names estate {estate}, which is not registered: a namespaced tag \
             is filed under one of {}, so one subsystem cannot arrive under two owners",
            ESTATES.join(", ")
        );
    }
    Ok(name)
}

fn validate_registered_tags(
    connection: &Connection,
    tags: &[String],
    subject: &str,
) -> Result<Vec<String>> {
    let known = connection
        .prepare("SELECT name FROM tags ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut seen = std::collections::HashSet::new();
    let mut canonical = Vec::new();
    for tag in tags {
        let tag = validate_tag_name(tag)?;
        if !seen.insert(tag.clone()) {
            bail!("{subject} tag {tag:?} was given more than once");
        }
        if !known.contains(&tag) {
            let borrowed = known.iter().map(String::as_str).collect::<Vec<_>>();
            let suggestion = crate::nearest(&tag, &borrowed)
                .map(|near| format!(", did you mean {near}?"))
                .unwrap_or_default();
            bail!(
                "{subject} tag {tag} is not in this board's master file{suggestion} — \
                 register it first with `tag add {tag}`"
            );
        }
        canonical.push(tag);
    }
    canonical.sort();
    Ok(canonical)
}

/// Attach registered tags to rows that were already read.
///
/// One query for the whole set rather than one per row: a board with a thousand
/// tasks would otherwise pay a thousand round trips to render a list.
fn attach_tags<'a>(
    connection: &Connection,
    tasks: impl ExactSizeIterator<Item = &'a mut Task>,
) -> Result<()> {
    if tasks.len() == 0 {
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
    for task in tasks {
        if let Some(tags) = by_task.remove(&task.id) {
            task.tags = tags;
        }
    }
    Ok(())
}

/// Attach each row's model allow-list, sorted.
///
/// One query for the whole set, for the reason [`attach_tags`] gives: a
/// listing must not pay a round trip per row to answer a question almost
/// every row answers with the empty list.
fn attach_allowed_models<'a>(
    connection: &Connection,
    tasks: impl ExactSizeIterator<Item = &'a mut Task>,
) -> Result<()> {
    if tasks.len() == 0 {
        return Ok(());
    }
    let mut statement =
        connection.prepare("SELECT task_id,model FROM task_models ORDER BY task_id,model")?;
    let mut by_task: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (task_id, model) = row?;
        by_task.entry(task_id).or_default().push(model);
    }
    for task in tasks {
        if let Some(models) = by_task.remove(&task.id) {
            task.allowed_models = models;
        }
    }
    Ok(())
}

/// One row's allow-list, for the single-row claim paths.
fn allowed_models_of(connection: &Connection, id: &str) -> Result<Vec<String>> {
    Ok(connection
        .prepare("SELECT model FROM task_models WHERE task_id=? ORDER BY model")?
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Replace a row's allow-list, refusing any name the shape does not admit.
///
/// Validated before the first `INSERT`, so a list with one bad name leaves
/// the row exactly as it was rather than half replaced.
pub(crate) fn set_allowed_models(
    connection: &Connection,
    id: &str,
    models: &[String],
) -> Result<()> {
    let canonical = crate::model::validate_model_names(models)?;
    connection.execute("DELETE FROM task_models WHERE task_id=?", [id])?;
    for model in &canonical {
        connection.execute(
            "INSERT INTO task_models(task_id,model) VALUES(?,?)",
            params![id, model],
        )?;
    }
    Ok(())
}

/// The refusal a restricted task answers a claim with.
///
/// One function so the claim path and the handoff-accept path cannot drift:
/// both print the same two sentences, and both print the list, because a
/// refusal that does not name the models it wants tells the caller nothing it
/// can act on (ADR-008). A typo in an allow-list is visible here.
fn require_allowed_model(id: &str, allowed: &[String], model: Option<&str>) -> Result<()> {
    if allowed.is_empty() {
        return Ok(());
    }
    let list = allowed.join(", ");
    match model {
        None => bail!(
            "task {id} is restricted to models [{list}]; pass --model with one of them to claim it"
        ),
        Some(model) if !allowed.iter().any(|allowed| allowed == model) => {
            bail!("task {id} is restricted to models [{list}]; model {model} may not claim it")
        }
        Some(_) => Ok(()),
    }
}

fn attach_attention_tags(connection: &Connection, items: &mut [Attention]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut statement = connection
        .prepare("SELECT attention_id,tag FROM attention_tags ORDER BY attention_id,tag")?;
    let mut by_attention: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (attention_id, tag) = row?;
        by_attention.entry(attention_id).or_default().push(tag);
    }
    for item in items.iter_mut() {
        if let Some(tags) = by_attention.remove(&item.id) {
            item.tags = tags;
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

fn set_attention_tags(connection: &Connection, id: &str, tags: &[String]) -> Result<()> {
    let canonical = validate_registered_tags(connection, tags, "attention")?;
    connection.execute("DELETE FROM attention_tags WHERE attention_id=?", [id])?;
    for tag in canonical {
        connection.execute(
            "INSERT INTO attention_tags(attention_id,tag) VALUES(?,?)",
            params![id, tag],
        )?;
    }
    Ok(())
}

/// A task's tags, read (usually under the mutation lock) for the all-of-tag
/// authorization check. An absent row yields no tags, so a caller without
/// board scope still receives the generic denial.
fn task_tags(connection: &Connection, id: &str) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare("SELECT tag FROM task_tags WHERE task_id=? ORDER BY tag")?;
    statement
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// An attention row's tags, read (usually under the mutation lock) for the
/// all-of-tag authorization check. An absent row yields no tags.
fn attention_tags(connection: &Connection, id: &str) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare("SELECT tag FROM attention_tags WHERE attention_id=? ORDER BY tag")?;
    statement
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// The tags one EVENT exposes, which is more than the tags its row carries
/// today.
///
/// Three sets, unioned:
///
/// 1. the row's **real current tags**, read live from `task_tags` — never from
///    the event payload. A row a caller may not see now must not be
///    reconstructible from its history, and the history is exactly where the
///    row's *former* shape is written down.
/// 2. the tag set the event's own `_semanticV1` snapshot froze. A retag event
///    names the resulting set in its snapshot, so an event about a retag would
///    otherwise hand over the very tag the caller lacks — the row would stay
///    invisible while the name of the thing that hid it leaked.
/// 3. for an event ABOUT an attention row — every such payload names its
///    subject as `attentionID` — that row's own tags, live from
///    `attention_tags`, plus the tag sets the payload itself recorded — the
///    row's set under `tags` on a raise, a resolve and a reopen, and the
///    superseded set under `previousTags` on an update. The task an item was
///    raised against is not the item: a `secret` question hung on a
///    `visible` task carried its id, its kind, its tags and its choices
///    through every event tail while all the listings withheld the row
///    itself (`t-1de9c707`). The payload's own record is unioned in for the
///    same reason the snapshot is: a row that has since been retagged or
///    removed must not be able to make its own history MORE visible than the
///    strictest evidence about it. An all-of-tag test over a union is the
///    intersection of the two rights only while BOTH sets are there, which
///    is why every attention emitter writes one down rather than only the
///    two that happened to have a tag argument to hand.
///
/// The union is what the all-of-tag rule is then applied to, so an event is
/// visible only to a caller who could see the row both as it is and as that
/// event recorded it.
///
/// An event whose task has since been removed yields only its snapshot tags:
/// there is no live row left to read, and the snapshot is the strictest
/// evidence remaining. An event whose attention row is gone yields, the same
/// way, whatever that payload wrote down about it.
fn event_tags(connection: &Connection, event: &Event) -> Result<Vec<String>> {
    let mut tags = match event.task_id.as_deref() {
        Some(id) => task_tags(connection, id)?,
        None => Vec::new(),
    };
    let recorded = |pointer: &str| -> Vec<String> {
        match event.payload.pointer(pointer) {
            Some(Value::Array(frozen)) => frozen
                .iter()
                .filter_map(|tag| tag.as_str())
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    };
    tags.extend(recorded("/_semanticV1/tags"));
    if let Some(attention_id) = event
        .payload
        .pointer("/attentionID")
        .and_then(Value::as_str)
    {
        tags.extend(attention_tags(connection, attention_id)?);
        // What the raise, the resolve and the reopen wrote down under `tags`
        // and the update under `previousTags`, so a later retag or removal
        // cannot loosen the event that recorded it.
        tags.extend(recorded("/tags"));
        tags.extend(recorded("/previousTags"));
    }
    tags.sort();
    tags.dedup();
    Ok(tags)
}

/// Every tag this board has on a row or in its own tag registry.
///
/// The bulk paths — a backup, a restore's rescue copy, an archive sweep, a
/// search-index rebuild — reach every row on the board at once, so there is no
/// per-row tag set to check: the unit being read or written is the board. The
/// all-of-tag rule is therefore applied to the UNION, which says a caller who
/// cannot see one tagged row cannot be handed a copy of the whole file either.
/// Without that, the bulk path is the one way to read everything.
///
/// Read from the row tables as well as `tags`, because a row may still carry a
/// tag whose registry entry was removed, and an archived `task_tags` row still
/// records a tag the row carries.
fn board_tag_universe(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT tag FROM task_tags \
         UNION SELECT tag FROM attention_tags \
         UNION SELECT name FROM tags \
         ORDER BY 1",
    )?;
    statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// The REAL tags of the task one deployment attempt names, or none when it
/// names no task or does not exist.
///
/// Takes the connection so a write path can read it inside its own
/// `BEGIN IMMEDIATE`, which is what keeps a concurrent retag from slipping
/// between the check and the write. Read live from `task_tags`, never from
/// anything the immutable attempt row froze at deploy time.
fn deployment_subject_tags_on(connection: &Connection, deployment_id: &str) -> Result<Vec<String>> {
    let task_id: Option<String> = connection
        .query_row(
            "SELECT task_id FROM deployments WHERE id=?",
            [deployment_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    match task_id.as_deref() {
        Some(task_id) => task_tags(connection, task_id),
        None => Ok(Vec::new()),
    }
}

/// The bulk-read gate: board scope, plus every tag on the board.
///
/// See [`board_tag_universe`] for why a whole-board copy is checked against
/// the union rather than per row. Takes the connection so a path holding a
/// `BEGIN IMMEDIATE` can read the universe in the SAME snapshot it is about
/// to sweep — reading it off `Store::connection` while a transaction is open
/// does not even borrow-check, which is a pleasant way for the compiler to
/// insist on the correct snapshot.
fn whole_board_read_on(authz: &AuthzContext, connection: &Connection) -> Result<()> {
    if !authz.is_enforcing() {
        return Ok(());
    }
    authz.check_read(&board_tag_universe(connection)?)
}

/// The bulk-write gate: the same union, at `write`, on both sides — a
/// board-wide mutation leaves every row's tag set where it found it, so the
/// old and the resulting sets are the same set.
fn whole_board_write_on(authz: &AuthzContext, connection: &Connection) -> Result<()> {
    if !authz.is_enforcing() {
        return Ok(());
    }
    let universe = board_tag_universe(connection)?;
    authz.check_write(&universe, &universe)
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

fn parse_subscription_strings(text: String) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(&text)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))
}

fn audit_digest(
    previous: &str,
    seq: i64,
    subject: Option<&str>,
    kind: &str,
    actor: Option<&str>,
    payload: &str,
    created_at: i64,
) -> String {
    fn field(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }

    let mut hash = Sha256::new();
    for value in [
        b"kanban-audit".as_slice(),
        b"1".as_slice(),
        b"board".as_slice(),
        seq.to_string().as_bytes(),
        previous.as_bytes(),
        subject.unwrap_or("").as_bytes(),
        kind.as_bytes(),
        actor.unwrap_or("").as_bytes(),
        payload.as_bytes(),
        created_at.to_string().as_bytes(),
    ] {
        field(&mut hash, value);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

struct EventFilterSpec<'a> {
    task: Option<&'a str>,
    kinds: &'a [String],
    relations: &'a [String],
    prior_statuses: &'a [String],
    current_statuses: &'a [String],
    tags: &'a [String],
    include_archived: bool,
    semantic_payload: &'a str,
}

fn append_event_filters(
    sql: &mut String,
    values: &mut Vec<Box<dyn rusqlite::ToSql>>,
    spec: EventFilterSpec<'_>,
) {
    if let Some(id) = spec.task {
        sql.push_str(" AND task_id=?");
        values.push(Box::new(id.to_owned()));
    }
    if !spec.kinds.is_empty() {
        sql.push_str(" AND kind IN (");
        sql.push_str(
            &std::iter::repeat_n("?", spec.kinds.len())
                .collect::<Vec<_>>()
                .join(","),
        );
        sql.push(')');
        values.extend(
            spec.kinds
                .iter()
                .cloned()
                .map(|kind| Box::new(kind) as Box<dyn rusqlite::ToSql>),
        );
    }
    if !spec.include_archived {
        sql.push_str(" AND archived=0");
    }
    let semantic = !spec.relations.is_empty()
        || !spec.prior_statuses.is_empty()
        || !spec.current_statuses.is_empty()
        || !spec.tags.is_empty();
    if semantic {
        sql.push_str(&format!(
            " AND json_type({},'$._semanticV1')='object'",
            spec.semantic_payload
        ));
    }
    if !spec.relations.is_empty() {
        let semantic_relations = format!(
            "CASE WHEN json_type({},'$._semanticV1.relations')='array' \
             THEN json_extract({},'$._semanticV1.relations') \
             ELSE '[]' END",
            spec.semantic_payload, spec.semantic_payload
        );
        let mut clauses = Vec::new();
        for relation in spec.relations {
            let Some((kind, id)) = relation.split_once(':') else {
                sql.push_str(" AND 0");
                continue;
            };
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM json_each({semantic_relations}) r \
                 WHERE r.type='object' \
                   AND json_extract(CASE WHEN r.type='object' THEN r.value ELSE '{{}}' END,'$.kind')=? \
                   AND json_extract(CASE WHEN r.type='object' THEN r.value ELSE '{{}}' END,'$.id')=?)"
                ));
            values.push(Box::new(kind.to_owned()));
            values.push(Box::new(id.to_owned()));
        }
        if !clauses.is_empty() {
            sql.push_str(" AND (");
            sql.push_str(&clauses.join(" OR "));
            sql.push(')');
        }
    }
    if !spec.prior_statuses.is_empty() {
        sql.push_str(" AND json_extract(CASE WHEN json_valid(payload) THEN payload ELSE '{}' END,'$._semanticV1.priorStatus') IN (");
        sql.push_str(
            &std::iter::repeat_n("?", spec.prior_statuses.len())
                .collect::<Vec<_>>()
                .join(","),
        );
        sql.push(')');
        values.extend(
            spec.prior_statuses
                .iter()
                .cloned()
                .map(|status| Box::new(status) as Box<dyn rusqlite::ToSql>),
        );
    }
    if !spec.current_statuses.is_empty() {
        sql.push_str(&format!(
            " AND json_extract({},'$._semanticV1.currentStatus') IN (",
            spec.semantic_payload
        ));
        sql.push_str(
            &std::iter::repeat_n("?", spec.current_statuses.len())
                .collect::<Vec<_>>()
                .join(","),
        );
        sql.push(')');
        values.extend(
            spec.current_statuses
                .iter()
                .cloned()
                .map(|status| Box::new(status) as Box<dyn rusqlite::ToSql>),
        );
    }
    if !spec.tags.is_empty() {
        let semantic_tags = format!(
            "CASE WHEN json_type({},'$._semanticV1.tags')='array' \
             THEN json_extract({},'$._semanticV1.tags') \
             ELSE '[]' END",
            spec.semantic_payload, spec.semantic_payload
        );
        for tag in spec.tags {
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM json_each({semantic_tags}) tag WHERE tag.value=?)"
            ));
            values.push(Box::new(tag.to_owned()));
        }
    }
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
        priority_level: priority_level(row.get("priority")?).map(str::to_owned),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        completed_at: row.get("completed_at")?,
        archived: row.get::<_, i64>("archived")? != 0,
        archived_at: row.get("archived_at")?,
        metadata: parse_value(row.get("metadata")?),
        // Attached after the row is read: a join per task would be a query per
        // task, and the readers below fill these in one pass.
        tags: Vec::new(),
        allowed_models: Vec::new(),
    })
}

/// The expected identities paired with what the finish observed for each, in
/// the attempt's own expected order.
///
/// Two columns rather than one document the finish rewrites, so what the
/// start claimed stays exactly as it was written (ADR-043 §2). A role with no
/// observation reads as `null` — a started attempt, or a terminal one that
/// did not succeed.
fn artifact_verifications(
    expected: Vec<ArtifactIdentity>,
    observed: Vec<ArtifactIdentity>,
) -> Vec<ArtifactVerification> {
    expected
        .into_iter()
        .map(|artifact| ArtifactVerification {
            observed: observed
                .iter()
                .find(|offered| offered.role == artifact.role)
                .map(|offered| offered.value.clone()),
            role: artifact.role,
            kind: artifact.kind,
            expected: artifact.value,
        })
        .collect()
}

fn deployment_row(row: &Row<'_>) -> rusqlite::Result<DeploymentAttempt> {
    let identity_mode: String = row.get("identity_mode")?;
    let build_commit: String = row.get("commit_sha")?;
    let expected = match row.get::<_, Option<String>>("expected_artifacts")? {
        Some(text) => parse_json_column::<Vec<ArtifactIdentity>>(text)?,
        None => Vec::new(),
    };
    let observed = match row.get::<_, Option<String>>("observed_artifacts")? {
        Some(text) => parse_json_column::<Vec<ArtifactIdentity>>(text)?,
        None => Vec::new(),
    };
    Ok(DeploymentAttempt {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        repo: row.get("repo")?,
        build_commit_label: build_commit_label(&identity_mode, &build_commit),
        identity_mode,
        build_commit,
        deployer_checkout: row.get("deployer_checkout")?,
        branch: row.get("branch")?,
        tier: row.get("tier")?,
        environment: row.get("environment")?,
        host: row.get("host")?,
        url: row.get("url")?,
        mechanism: row.get("mechanism")?,
        operation_id: row.get("operation_id")?,
        retry_of: row.get("retry_of")?,
        status: row.get("status")?,
        phase: row.get("phase")?,
        actor: row.get("actor")?,
        lane: row.get("lane")?,
        receipt: row.get("receipt")?,
        artifact_uri: row.get("artifact_uri")?,
        served_commit: row.get("served_commit")?,
        sprint_id: row.get("sprint_id")?,
        target_version: row.get("target_version")?,
        served_version: row.get("served_version")?,
        artifacts: artifact_verifications(expected, observed),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        completed_at: row.get("completed_at")?,
        archived: row.get::<_, i64>("archived")? != 0,
        archived_at: row.get("archived_at")?,
    })
}

fn sprint_row(row: &Row<'_>) -> rusqlite::Result<Sprint> {
    Ok(Sprint {
        id: row.get("id")?,
        title: row.get("title")?,
        body: row.get("body")?,
        status: row.get("status")?,
        target_version: row.get("target_version")?,
        scheduled_start: row.get("scheduled_start")?,
        scheduled_end: row.get("scheduled_end")?,
        starts_at: row.get("starts_at")?,
        ends_at: row.get("ends_at")?,
        closed_by_deployment: row.get("closed_by_deployment")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        archived: row.get::<_, i64>("archived")? != 0,
    })
}

fn subscription_row(row: &Row<'_>) -> rusqlite::Result<Subscription> {
    Ok(Subscription {
        id: row.get("id")?,
        protocol_version: row.get("protocol_version")?,
        subject_task_id: row.get("subject_task_id")?,
        relations: parse_subscription_strings(row.get("relations")?)?,
        kinds: parse_subscription_strings(row.get("kinds")?)?,
        prior_statuses: parse_subscription_strings(row.get("prior_statuses")?)?,
        current_statuses: parse_subscription_strings(row.get("current_statuses")?)?,
        tags: parse_subscription_strings(row.get("tags")?)?,
        consumer_id: row.get("consumer_id")?,
        action_id: row.get("action_id")?,
        timeout_ms: row.get("timeout_ms")?,
        max_retries: row.get("max_retries")?,
        rate_per_minute: row.get("rate_per_minute")?,
        max_concurrency: row.get("max_concurrency")?,
        start_event_seq: row.get("start_event_seq")?,
        secret_ref: row.get("secret_ref")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        created_by: row.get("created_by")?,
        updated_at: row.get("updated_at")?,
        updated_by: row.get("updated_by")?,
        paused_at: row.get("paused_at")?,
        paused_by: row.get("paused_by")?,
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
        model: row.get("model")?,
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
        tags: parse_strings(row.get("task_tags")?),
        source_board: None,
        source_rule_id: None,
        source_registry_uuid: None,
        source_boards: None,
        source_content_sha256: None,
    })
}

/// A stored JSON document, refused rather than swallowed.
///
/// The columns' CHECKs guarantee valid JSON of the right type, so a parse
/// failure here means the file was edited outside kanban. Reading it as
/// "absent" would present an authored decision as an unauthored row, or an
/// attempt's expected artifact identities as an attempt that expected none.
fn parse_json_column<T: serde::de::DeserializeOwned>(text: String) -> rusqlite::Result<T> {
    serde_json::from_str(&text)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))
}

fn attention_row(row: &Row<'_>) -> rusqlite::Result<Attention> {
    Ok(Attention {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        kind: row.get("kind")?,
        body: row.get("body")?,
        question: row.get("question")?,
        context: row.get("context")?,
        // A row that authored none is SERVED as the default approve/reject
        // pair and STORED as NULL, so a backfilled row stays distinguishable
        // from a never-authored one for as long as any remain (ADR-042 §2).
        choices: match row.get::<_, Option<String>>("choices")? {
            Some(text) => parse_json_column(text)?,
            None => default_choice_pair(),
        },
        raised_by: row.get("raised_by")?,
        created_at: row.get("created_at")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        priority_level: priority_level(row.get("priority")?).map(str::to_owned),
        resolved_at: row.get("resolved_at")?,
        resolved_by: row.get("resolved_by")?,
        resolution: row.get("resolution")?,
        decision: row
            .get::<_, Option<String>>("decision")?
            .map(parse_json_column)
            .transpose()?,
        reopened_at: row.get("reopened_at")?,
        reopened_by: row.get("reopened_by")?,
        reopen_note: row.get("reopen_note")?,
        archived: row.get::<_, i64>("archived")? != 0,
        tags: Vec::new(),
    })
}

fn handoff_row(row: &Row<'_>) -> rusqlite::Result<Handoff> {
    Ok(Handoff {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        checkpoint_seq: row.get("checkpoint_seq")?,
        reason: row.get("reason")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        priority_level: priority_level(row.get("priority")?).map(str::to_owned),
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
        retired_at: row.get("retired_at")?,
        retired_by: row.get("retired_by")?,
        retire_note: row.get("retire_note")?,
        archived: row.get::<_, i64>("archived")? != 0,
    })
}

/// The handoff listing's WHERE clause and its bound values, shared with the
/// pending count so the two cannot disagree about which rows are pending.
///
/// Built up rather than enumerated: three optional filters is eight
/// hand-written queries, and the eighth is the one that gets forgotten.
fn handoff_filter(
    task: Option<&str>,
    status: Option<&str>,
    to_agent: Option<&str>,
    include_archived: bool,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut clauses = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !include_archived {
        clauses.push("archived=0");
    }
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
    (where_clause, values)
}

#[derive(Debug)]
struct BoardEventIdentityRow {
    seq: i64,
    task_id: Option<String>,
    kind: String,
    actor: Option<String>,
    payload: String,
    created_at: i64,
    prev_hash: Option<String>,
    event_hash: Option<String>,
}

fn board_event_identity_row(row: &Row<'_>) -> rusqlite::Result<BoardEventIdentityRow> {
    Ok(BoardEventIdentityRow {
        seq: row.get("seq")?,
        task_id: row.get("task_id")?,
        kind: row.get("kind")?,
        actor: row.get("actor")?,
        payload: row.get("payload")?,
        created_at: row.get("created_at")?,
        prev_hash: row.get("prev_hash")?,
        event_hash: row.get("event_hash")?,
    })
}

#[derive(Debug)]
struct SubscriptionDeliveryRow {
    subscription_id: String,
    event_id: String,
    event_seq: i64,
    event_kind: String,
    event_created_at: i64,
    status: String,
    attempts: i64,
    next_attempt_at: Option<i64>,
    lease_token: Option<String>,
    lease_deadline_at: Option<i64>,
    last_attempt_at: Option<i64>,
    last_error_code: Option<String>,
    acked_at: Option<i64>,
    dead_lettered_at: Option<i64>,
}

fn validate_pending_or_retry_delivery(
    delivery: &SubscriptionDeliveryRow,
    subscription: &Subscription,
) -> Result<()> {
    if delivery.attempts > subscription.max_retries {
        bail!(
            "subscription {} delivery {} has exhausted its retry budget",
            delivery.subscription_id,
            delivery.event_id
        );
    }
    match delivery.status.as_str() {
        "pending" => {
            if delivery.attempts != 0
                || delivery.lease_token.is_some()
                || delivery.last_attempt_at.is_some()
                || delivery.last_error_code.is_some()
                || delivery.acked_at.is_some()
                || delivery.dead_lettered_at.is_some()
            {
                bail!(
                    "subscription {} delivery {} has malformed pending state",
                    delivery.subscription_id,
                    delivery.event_id
                );
            }
        }
        "retry_wait" => {
            if delivery.attempts < 1
                || delivery.attempts > subscription.max_retries
                || delivery.lease_token.is_some()
                || delivery.last_attempt_at.is_none()
                || delivery.last_error_code.is_none()
                || delivery.acked_at.is_some()
                || delivery.dead_lettered_at.is_some()
            {
                bail!(
                    "subscription {} delivery {} has malformed retry_wait state",
                    delivery.subscription_id,
                    delivery.event_id
                );
            }
        }
        _ => bail!(
            "subscription {} delivery {} is not pending or retry_wait",
            delivery.subscription_id,
            delivery.event_id
        ),
    }
    Ok(())
}

fn validate_leased_delivery(delivery: &SubscriptionDeliveryRow) -> Result<()> {
    if delivery.attempts < 1
        || delivery.lease_token.is_none()
        || delivery.lease_deadline_at.is_none()
        || delivery.last_attempt_at.is_none()
        || delivery.last_error_code.is_some()
        || delivery.acked_at.is_some()
        || delivery.dead_lettered_at.is_some()
    {
        bail!(
            "subscription {} delivery {} has malformed leased state",
            delivery.subscription_id,
            delivery.event_id
        );
    }
    Ok(())
}

fn subscription_delivery_row(row: &Row<'_>) -> rusqlite::Result<SubscriptionDeliveryRow> {
    Ok(SubscriptionDeliveryRow {
        subscription_id: row.get("subscription_id")?,
        event_id: row.get("event_id")?,
        event_seq: row.get("event_seq")?,
        event_kind: row.get("event_kind")?,
        event_created_at: row.get("event_created_at")?,
        status: row.get("status")?,
        attempts: row.get("attempts")?,
        next_attempt_at: row.get("next_attempt_at")?,
        lease_token: row.get("lease_token")?,
        lease_deadline_at: row.get("lease_deadline_at")?,
        last_attempt_at: row.get("last_attempt_at")?,
        last_error_code: row.get("last_error_code")?,
        acked_at: row.get("acked_at")?,
        dead_lettered_at: row.get("dead_lettered_at")?,
    })
}

#[cfg(test)]
#[derive(Debug)]
struct SubscriptionDeliveryAttemptRow {
    attempt: i64,
    started_at: i64,
    finished_at: Option<i64>,
    outcome: String,
    error_code: Option<String>,
}

#[cfg(test)]
fn subscription_delivery_attempt_row(
    row: &Row<'_>,
) -> rusqlite::Result<SubscriptionDeliveryAttemptRow> {
    Ok(SubscriptionDeliveryAttemptRow {
        attempt: row.get("attempt")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        outcome: row.get("outcome")?,
        error_code: row.get("error_code")?,
    })
}

fn delivery_retry_delay_ms(attempt_number: i64, timeout_ms: i64) -> i64 {
    let exponential = 1_000_i64
        .checked_shl((attempt_number.saturating_sub(1)) as u32)
        .unwrap_or(i64::MAX);
    exponential.min(timeout_ms)
}

fn require_delivery_event_identity(
    connection: &Connection,
    delivery: &SubscriptionDeliveryRow,
    subscription: &Subscription,
) -> Result<Event> {
    if !is_lower_hex_64(&delivery.event_id) {
        bail!(
            "subscription {} delivery {} has malformed event hash",
            subscription.id,
            delivery.event_id
        );
    }
    if delivery.event_seq <= subscription.start_event_seq {
        bail!(
            "subscription {} delivery {} referenced event seq {} at or before start anchor {}",
            subscription.id,
            delivery.event_id,
            delivery.event_seq,
            subscription.start_event_seq
        );
    }
    let event = connection
        .query_row(
            "SELECT * FROM events WHERE seq=?",
            [delivery.event_seq],
            board_event_row,
        )
        .with_context(|| {
            format!(
                "subscription {} delivery {} expected event seq {} to exist",
                subscription.id, delivery.event_id, delivery.event_seq
            )
        })?;
    if event.event_hash.as_deref() != Some(delivery.event_id.as_str()) {
        bail!(
            "subscription {} delivery {} expected event hash {} at seq {}, found {:?}",
            subscription.id,
            delivery.event_id,
            delivery.event_id,
            delivery.event_seq,
            event.event_hash
        );
    }
    if !event.event_hash.as_deref().is_some_and(is_lower_hex_64) {
        bail!(
            "subscription {} delivery {} expected a well-formed event hash at seq {}",
            subscription.id,
            delivery.event_id,
            delivery.event_seq
        );
    }
    if event.kind != delivery.event_kind {
        bail!(
            "subscription {} delivery {} expected event kind {}, found {}",
            subscription.id,
            delivery.event_id,
            delivery.event_kind,
            event.kind
        );
    }
    if event.created_at != delivery.event_created_at {
        bail!(
            "subscription {} delivery {} expected event created_at {}, found {}",
            subscription.id,
            delivery.event_id,
            delivery.event_created_at,
            event.created_at
        );
    }
    Ok(event)
}

fn subscription_delivery_rate_count(
    connection: &Connection,
    subscription_id: &str,
    now: i64,
) -> Result<i64> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM subscription_delivery_attempts \
             WHERE subscription_id=? AND started_at>=? AND started_at<=?",
            params![subscription_id, now.saturating_sub(60_000), now],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn subscription_delivery_leased_count(
    connection: &Connection,
    subscription_id: &str,
) -> Result<i64> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM subscription_deliveries \
             WHERE subscription_id=? AND status='leased'",
            [subscription_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
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

fn require_active_task(connection: &Connection, id: &str) -> Result<Task> {
    let task = require_task(connection, id)?;
    if task.archived {
        bail!("task {id} is archived history and cannot be changed");
    }
    Ok(task)
}

fn board_event_kind_exists(connection: &Connection, kind: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE kind=?)",
        [kind],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

fn watch_subject_exists_on(connection: &Connection, id: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(\
             SELECT 1 FROM tasks WHERE id=?1 \
             UNION ALL \
             SELECT 1 FROM events WHERE task_id=?1 \
             LIMIT 1\
         )",
        [id],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

fn watch_relation_target_exists_on(connection: &Connection, kind: &str, id: &str) -> Result<bool> {
    let current = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?)",
        [id],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if current {
        return Ok(true);
    }
    Ok(connection.query_row(
        "SELECT EXISTS(\
             SELECT 1 \
             FROM events, \
                  json_each(\
                      CASE WHEN json_valid(events.payload) \
                           THEN CASE WHEN json_type(events.payload,'$._semanticV1.relations')='array' \
                                     THEN json_extract(events.payload,'$._semanticV1.relations') \
                                     ELSE '[]' END \
                           ELSE '[]' END\
                  ) relation \
             WHERE relation.type='object' \
               AND json_extract(CASE WHEN relation.type='object' THEN relation.value ELSE '{}' END,'$.kind')=?1 \
               AND json_extract(CASE WHEN relation.type='object' THEN relation.value ELSE '{}' END,'$.id')=?2\
         )",
        params![kind, id],
        |row| row.get::<_, i64>(0),
    )? != 0)
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

/// Every incomplete prerequisite a row inherits: those declared on the row
/// itself, and those declared on any of its ancestors.
///
/// The recursion walks parent edges only. A prerequisite's own dependencies
/// are deliberately not followed — whether a prerequisite is finished is that
/// row's own business, and walking through it would report blockers nothing is
/// actually waiting on. The `path` guard is what stops a malformed board (a
/// parent cycle written before the nesting rules landed) from spinning here,
/// rather than a recursion limit nobody set.
///
/// Nearest owner first, then prerequisite id, so a refusal and a rendered
/// board name the same gate first. Existence is the caller's to establish,
/// exactly as [`dependencies`] leaves it.
fn blocking_gates(
    connection: &Connection,
    task_id: &str,
    lapsed: &LapsedLeases,
) -> Result<Vec<GateBlocker>> {
    let mut statement = connection.prepare(
        "WITH RECURSIVE owners(id,depth,path) AS (\
             SELECT id,0,json_array(id) FROM tasks WHERE id=?1 \
             UNION ALL \
             SELECT parent.id,owners.depth+1,json_insert(owners.path,'$[#]',parent.id) \
             FROM owners \
             JOIN tasks child ON child.id=owners.id \
             JOIN tasks parent ON parent.id=child.parent_id \
             WHERE NOT EXISTS (SELECT 1 FROM json_each(owners.path) seen WHERE seen.value=parent.id)\
         ) \
         SELECT owners.id,prerequisite.id,prerequisite.title,prerequisite.status \
         FROM owners \
         JOIN task_dependencies dependency ON dependency.task_id=owners.id \
         JOIN tasks prerequisite ON prerequisite.id=dependency.depends_on \
         WHERE prerequisite.status<>'done' \
         ORDER BY owners.depth,prerequisite.id",
    )?;
    let mut blockers = statement
        .query_map([task_id], |row| {
            Ok(GateBlocker {
                source_task_id: row.get(0)?,
                prerequisite_id: row.get(1)?,
                prerequisite_title: row.get(2)?,
                prerequisite_status: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // The projection [`apply_lapsed_leases`] puts on a task row, applied to
    // the prerequisite statuses a blocker carries. Without it a prerequisite
    // whose holder vanished reads `in_progress` here and `todo` in the same
    // command's `dependencies` array — two answers about one row, from one
    // read. Either way the prerequisite is unmet, so this changes what a
    // reader is told and never whether the gate holds.
    for blocker in &mut blockers {
        if blocker.prerequisite_status == "in_progress" && lapsed.contains(&blocker.prerequisite_id)
        {
            blocker.prerequisite_status = "todo".to_owned();
        }
    }
    Ok(blockers)
}

/// Every lease that has run out, read once for a whole command.
///
/// The projection it feeds is per prerequisite, but the read is board-wide,
/// and `task list --with-relations` asks for the gate of every row on the
/// board (`rust/lib.rs`, `list_json`). Reading it inside each
/// [`blocking_gates`] call put an unindexed scan of `task_claims` on that
/// listing path once per row — measured at 0.666s against 0.590s on a
/// 3036-row board the moment one prerequisite was `in_progress`. A caller
/// that reads many gates reads this once and hands it down.
///
/// Empty on a board old enough to lack the claims table, which a read-only
/// open cannot migrate into place.
#[derive(Default)]
struct LapsedLeases(BTreeSet<String>);

impl LapsedLeases {
    fn contains(&self, task_id: &str) -> bool {
        self.0.contains(task_id)
    }
}

fn lapsed_leases(connection: &Connection) -> Result<LapsedLeases> {
    if !has_claims_table(connection)? {
        return Ok(LapsedLeases::default());
    }
    let mut statement =
        connection.prepare("SELECT task_id FROM task_claims WHERE expires_at<=?")?;
    let lapsed = statement
        .query_map([now_ms()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<String>>>()?;
    Ok(LapsedLeases(lapsed))
}

/// Whether the caller being refused is the one holding this row's lease.
///
/// Only a holder can `checkpoint --state blocked` or `release`, so only a
/// holder is told about them: naming that route to `claim`, `task add
/// --status in_progress` or `story advance` sends a caller after a lease it
/// does not have.
#[derive(Clone, Copy)]
enum GateCaller {
    /// `heartbeat`, `checkpoint`, and a `task move` by the current holder.
    Holder,
    /// `claim`, handoff acceptance, `task add`, `story advance`, and a move
    /// by anyone who is not holding the row.
    Unleased,
}

/// Refuse work whose prerequisites are not finished.
///
/// The refusal names every unmet prerequisite, its status, and which row
/// declared it: an inherited gate that said only "blocked" would leave an
/// agent grepping the tree for the epic it came from. A prerequisite counts as
/// met at `done` and at nothing else — `cancelled` is a decision not to do the
/// work, which is not the same as the work being finished — so the way out is
/// named too. The row is named by what it is, as the story-projection refusal
/// in [`Store::move_task`] does, because "task s-2" is wrong about a story.
fn require_no_blocking_gates(connection: &Connection, id: &str, caller: GateCaller) -> Result<()> {
    let blockers = blocking_gates(connection, id, &lapsed_leases(connection)?)?;
    if blockers.is_empty() {
        return Ok(());
    }
    let details = blockers
        .iter()
        .map(|blocker| {
            let source = if blocker.source_task_id == id {
                format!("declared on {id} itself")
            } else {
                format!("declared on ancestor {}", blocker.source_task_id)
            };
            format!(
                "{} is {} ({source})",
                blocker.prerequisite_id, blocker.prerequisite_status
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let kind = require_task(connection, id)?.task_type;
    let escape = match caller {
        GateCaller::Holder => {
            " A holder that must stop here can `checkpoint --state blocked` or `release`."
        }
        GateCaller::Unleased => "",
    };
    bail!(
        "{kind} {id} is gated on work that is not done: {details}. A prerequisite \
         satisfies a gate only at status done: finish it, or drop the edge with \
         `task update <owner> --depends-on ...` or `--clear-dependencies`.{escape}"
    )
}

/// The names the board owner answers to when work is parked on him.
///
/// [`OPERATOR_ACTOR`] is the real source: he is the one actor who may settle
/// any attention row, which makes him the one actor a blocked record can be
/// waiting for. His given name has no source anywhere — measured, not
/// assumed: no table records that `geoyws` is George. `board_meta` holds the
/// board's name and its audit chain and nothing else; the registry's `boards`
/// and `workspace_roots` rows hold paths and timestamps; `principals` are the
/// *caller's* frozen identity (ADR-038), not the board's owner. So the given
/// name is a literal, and a deliberately conservative one: capital-G only, so
/// a lowercase `george` in prose is not a match.
const OWNER_NAMES: [&str; 2] = [OPERATOR_ACTOR, "George"];

/// The first occurrence of `name` in `text` that names the person, or `None`.
///
/// Two exclusions, both measured against how this board spells things.
///
/// A match glued into a longer word is a different word: `Georgetown` and
/// `geoywsMBP` are not the owner, while `George's` and `@geoyws` are.
///
/// A match inside an actor, lane or path compound is a machine address, not
/// the person: the lane spelling `@:geoyws/kanban/driver` names the LANE that
/// will do the work, and refusing a next action that assigns the step to that
/// lane is exactly the false refusal this gate must not produce. A separator
/// touching the match on either side is what distinguishes the two — `:`
/// before it or `/` on either side — because a compound is a path and a
/// person is a word.
fn standalone_match(text: &str, name: &str) -> Option<usize> {
    const COMPOUND_BEFORE: [char; 2] = [':', '/'];
    text.match_indices(name)
        .find(|(at, _)| {
            let before = text[..*at].chars().next_back();
            let after = text[at + name.len()..].chars().next();
            !before.is_some_and(|glyph| glyph.is_alphanumeric() || COMPOUND_BEFORE.contains(&glyph))
                && !after.is_some_and(|glyph| glyph.is_alphanumeric() || glyph == '/')
        })
        .map(|(at, _)| at)
}

/// The clause of `text` that names the board owner, quotable back at the
/// caller, or `None` when he is not in it.
///
/// The clause rather than the whole field: a refusal that echoes a paragraph
/// hides which words tripped it, and rewriting exactly those words is the
/// caller's way out.
fn owner_clause(text: &str) -> Option<&str> {
    const BREAKS: [char; 5] = ['.', ';', '!', '?', '\n'];
    let at = OWNER_NAMES
        .iter()
        .filter_map(|name| standalone_match(text, name))
        .min()?;
    let start = text[..at].rfind(BREAKS).map_or(0, |break_at| break_at + 1);
    let end = text[at..]
        .find(BREAKS)
        .map_or(text.len(), |break_at| at + break_at);
    Some(text[start..end].trim())
}

/// Whether this task is already asking the operator for something.
///
/// The in-transaction twin of [`Store::open_attentions`], holding the same
/// definition of open that [`Store::attention_filter`] builds — this task,
/// status `open`, hot history only — read through the write scope so a card
/// raised earlier in the same batch counts.
fn has_open_attention(connection: &Connection, task_id: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM attention WHERE task_id=? AND status='open' AND archived=0 LIMIT 1",
            [task_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Refuse a record that parks work on the board owner while nothing on the
/// row asks him for it (rule `g-74e8d80c`).
///
/// Measured twice on one board on 2026-09-17: a lane checkpointed `blocked`
/// with "George re-logins to bootstrap" as its next action, raised no card,
/// and the work then waited on a person who was never told while the lane's
/// queue read empty. The attention card is the only surface that reaches
/// him, so the write that would strand the work is where the card is
/// required.
///
/// Only the fields that *are* the assignment are read — a blocked
/// checkpoint's `--next-action` and either record's `--blocker` values: what
/// happens next, and what stops it. The summary is narrative and routinely
/// cites him ("per George's 2026-09-01 decision"); reading it would refuse
/// records that park nothing on anyone, which is the expensive kind of wrong
/// here. The refusal quotes the clause it matched, so a false positive costs
/// one rewrite instead of a hunt.
fn require_owner_has_a_card<'a>(
    connection: &Connection,
    task_id: &str,
    texts: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let Some(phrase) = texts.into_iter().find_map(owner_clause) else {
        return Ok(());
    };
    if has_open_attention(connection, task_id)? {
        return Ok(());
    }
    // A clause carrying its own double quote would close the sentence's
    // quoted span early and leave the refusal reading as two broken halves,
    // so the echoed copy carries single quotes instead. The caller still
    // reads their own words back, and the sentence still holds exactly one
    // quoted span. Nothing is allocated for the ordinary clause.
    let phrase = if phrase.contains('"') {
        Cow::Owned(phrase.replace('"', "'"))
    } else {
        Cow::Borrowed(phrase)
    };
    bail!(
        "parking work on the board owner needs a card he can see it on — \"{phrase}\" hands him \
         the next step while {task_id} has no open attention row — raise it first with \
         `kb attention raise \"<the ask>\" --as <agent> --kind blocking --task {task_id}`, then \
         write this again."
    )
}

pub(crate) fn event(
    connection: &Connection,
    task_id: Option<&str>,
    kind: &str,
    actor: Option<&str>,
    payload: Value,
) -> Result<()> {
    let status = task_id
        .map(|id| {
            connection
                .query_row("SELECT status FROM tasks WHERE id=?", [id], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
        })
        .transpose()?
        .flatten();
    event_with_status(
        connection,
        task_id,
        kind,
        actor,
        payload,
        status.as_deref(),
        status.as_deref(),
    )
}

fn semantic_snapshot(
    connection: &Connection,
    task_id: &str,
    prior_status: Option<&str>,
    current_status: Option<&str>,
) -> Result<Value> {
    let task = require_task(connection, task_id)?;
    let mut tags = connection
        .prepare("SELECT tag FROM task_tags WHERE task_id=? ORDER BY tag")?
        .query_map([task_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tags.sort();

    let mut relations = Vec::new();
    let mut current = task.parent_id.clone();
    let mut first = true;
    while let Some(id) = current {
        let parent = require_task(connection, &id)?;
        relations.push(json!({
            "kind": if first { "parent" } else { "ancestor" },
            "type": parent.task_type,
            "id": parent.id,
        }));
        current = parent.parent_id;
        first = false;
    }
    let mut statement = connection.prepare(
        "SELECT t.type,t.id FROM tasks t JOIN task_dependencies d ON d.depends_on=t.id WHERE d.task_id=? ORDER BY t.type,t.id",
    )?;
    let dependencies = statement
        .query_map([task_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (task_type, id) in dependencies {
        relations.push(json!({ "kind": "depends-on", "type": task_type, "id": id }));
    }
    relations.sort_by_key(|relation| relation.to_string());
    Ok(json!({
        "subject": { "type": task.task_type, "id": task.id },
        "tags": tags,
        "relations": relations,
        "priorStatus": prior_status,
        "currentStatus": current_status,
    }))
}

fn event_with_status(
    connection: &Connection,
    task_id: Option<&str>,
    kind: &str,
    actor: Option<&str>,
    mut payload: Value,
    prior_status: Option<&str>,
    current_status: Option<&str>,
) -> Result<()> {
    if let Some(task_id) = task_id {
        payload["_semanticV1"] =
            semantic_snapshot(connection, task_id, prior_status, current_status)?;
    } else {
        payload["_semanticV1"] = Value::Null;
    }
    event_at(connection, task_id, kind, actor, payload, now_ms())
}

pub(crate) fn event_at(
    connection: &Connection,
    task_id: Option<&str>,
    kind: &str,
    actor: Option<&str>,
    payload: Value,
    created_at: i64,
) -> Result<()> {
    let mut payload = payload;
    if task_id.is_some() {
        if !matches!(payload.get("_semanticV1"), Some(Value::Object(_))) {
            bail!("task events require an object _semanticV1 snapshot");
        }
    } else if payload.get("_semanticV1").is_none() {
        payload["_semanticV1"] = Value::Null;
    }
    // The one funnel every audited board mutation passes through, which is
    // why the batch stamp is applied here rather than per write path: an item
    // that landed inside a `transact` cannot forget to say so (ADR-041 §7).
    // Two keys on the existing payload, beside `_semanticV1`; no new event
    // kind, table or column. The claim sweep on the way into the board runs
    // before any batch sets the stamp, so its events carry none.
    if let Some((batch_id, batch_index)) = current_batch() {
        payload["batchId"] = json!(batch_id);
        payload["batchIndex"] = json!(batch_index);
    }
    let actor = actor.context("actor is required for audited mutation")?;
    crate::audit::append_board_event(
        connection,
        task_id,
        kind,
        actor,
        &payload.to_string(),
        created_at,
    )
}

thread_local! {
    /// The batch this thread's writes belong to while `kanban transact` runs
    /// one item, as `(batchId, batchIndex)`.
    ///
    /// Thread-local rather than threaded through forty write signatures: the
    /// stamp is a property of the invocation, not of any one write, and a
    /// parameter would have to be added to every method for the benefit of
    /// the one caller that sets it — including the methods a future item
    /// reaches. `transact` runs its items on the calling thread, one at a
    /// time, so nothing else can be running under a stamp of its own.
    static BATCH: std::cell::RefCell<Option<(String, usize)>> =
        const { std::cell::RefCell::new(None) };
}

fn current_batch() -> Option<(String, usize)> {
    BATCH.with_borrow(Clone::clone)
}

/// Stamp every event appended on this thread with the batch and the item,
/// until this guard drops.
///
/// A guard rather than a pair of calls, so an item that returns early — every
/// refusal does — cannot leave the next item's events wearing its index.
pub(crate) struct BatchStamp;

impl BatchStamp {
    pub(crate) fn set(batch_id: &str, index: usize) -> Self {
        BATCH.with_borrow_mut(|slot| *slot = Some((batch_id.to_owned(), index)));
        Self
    }
}

impl Drop for BatchStamp {
    fn drop(&mut self) {
        BATCH.with_borrow_mut(|slot| *slot = None);
    }
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

/// Apply the row change [`expire_claims`] STORES, without storing it.
///
/// A lapsed lease has two consequences on a task: it stops being
/// `in_progress`, and it stops naming the vanished holder as assignee. The
/// writable opens materialise both in the `tasks` row, because the retirement
/// is also durable history and the `claim_expired` event has to be written
/// somewhere. A read-only open can write neither — and a read that reported
/// `in_progress · assignee: ghost` would be the exact defect
/// [`Store::sweep_expired_claims`] exists to prevent, arriving through the
/// other door.
///
/// So the rule is applied here instead, on the values, and the condition is
/// character-for-character the one `expire_claims` puts in its `UPDATE`:
/// status `in_progress` becomes `todo`, and an assignee equal to the lapsed
/// holder becomes nobody. Every read goes through this, so the answer does not
/// depend on which constructor opened the board — and on a board a writable
/// open has just swept there is nothing left to find, which is why the probe
/// comes first and costs one indexed lookup.
///
/// `updated_at` is deliberately left alone. It records when the row was last
/// WRITTEN, and nothing wrote it.
fn apply_lapsed_leases<'a>(
    connection: &Connection,
    tasks: impl ExactSizeIterator<Item = &'a mut Task>,
) -> Result<()> {
    if tasks.len() == 0 || !has_claims_table(connection)? {
        return Ok(());
    }
    let mut statement =
        connection.prepare("SELECT task_id,agent_id FROM task_claims WHERE expires_at<=?")?;
    let lapsed = statement
        .query_map([now_ms()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<String, String>>>()?;
    drop(statement);
    if lapsed.is_empty() {
        return Ok(());
    }
    for task in tasks {
        let Some(holder) = lapsed.get(&task.id) else {
            continue;
        };
        if task.status == "in_progress" {
            task.status = "todo".to_owned();
        }
        if task.assignee.as_deref() == Some(holder.as_str()) {
            task.assignee = None;
        }
    }
    Ok(())
}

fn expire_claims(connection: &Connection, now: i64) -> Result<()> {
    // Read the whole row before the DELETE: once the claim is gone, the
    // provenance it carried (who held it, where, and how stale their last
    // checkpoint was) is unrecoverable, and the `claim_expired` event is the
    // only record a successor can read to reconstruct what died.
    let mut statement = connection.prepare("SELECT * FROM task_claims WHERE expires_at<=?")?;
    let expired = statement
        .query_map([now], claim_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for claim in expired {
        // The newest checkpoint written since this claim began. Comparing
        // against `claimed_at` (not just "newest ever") is what stops a
        // checkpoint a *previous* holder wrote from being reported as this
        // holder's freshest work.
        let last_checkpoint: Option<(i64, i64)> = connection
            .query_row(
                "SELECT seq,created_at FROM checkpoints WHERE task_id=? AND created_at>=? ORDER BY seq DESC LIMIT 1",
                params![claim.task_id, claim.claimed_at],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        connection.execute("DELETE FROM task_claims WHERE task_id=?", [&claim.task_id])?;
        connection.execute(
            "UPDATE tasks SET status='todo',assignee=CASE WHEN assignee=? THEN NULL ELSE assignee END,updated_at=? WHERE id=? AND status='in_progress'",
            params![claim.agent_id, now, claim.task_id],
        )?;
        event_with_status(
            connection,
            Some(&claim.task_id),
            "claim_expired",
            Some(&claim.agent_id),
            json!({
                "sessionId": claim.session_id,
                "worktree": claim.worktree,
                "worktreeKind": claim.worktree_kind,
                "branch": claim.branch,
                "headSha": claim.head_sha,
                "rootHead": claim.root_head,
                "claimedAt": claim.claimed_at,
                "heartbeatAt": claim.heartbeat_at,
                "lastCheckpointSeq": last_checkpoint.map(|(seq, _)| seq),
                "lastCheckpointAt": last_checkpoint.map(|(_, at)| at),
            }),
            Some("in_progress"),
            Some("todo"),
        )?;
    }
    Ok(())
}

/// The previous holder whose lease expired, if that expiry is still the reason
/// the task last entered `todo`.
///
/// "Newest ever" would be wrong: a task that was claimed, orphaned, reclaimed
/// and then completed has a `claim_expired` event in its past, but a later
/// holder reaching it after a `task move … todo` should not be told that stale
/// orphan explains the todo. Scoping to `seq >= (newest event whose
/// `_semanticV1.currentStatus` is `todo`)` drops every `claim_expired` that a
/// newer move back into `todo` superseded, while still returning the latest
/// orphan when a task was orphaned, reclaimed and orphaned again.
fn orphaned_from(connection: &Connection, task_id: &str) -> Result<Option<OrphanedFrom>> {
    let last_entered_todo: i64 = connection
        .query_row(
            "SELECT seq FROM events WHERE task_id=? \
             AND json_extract(CASE WHEN json_valid(payload) THEN payload ELSE '{}' END,'$._semanticV1.currentStatus')='todo' \
             ORDER BY seq DESC LIMIT 1",
            params![task_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let row: Option<(String, String, i64)> = connection
        .query_row(
            "SELECT actor,payload,created_at FROM events \
             WHERE task_id=? AND kind='claim_expired' AND seq>=? \
             ORDER BY seq DESC LIMIT 1",
            params![task_id, last_entered_todo],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((agent, payload, expired_at)) = row else {
        return Ok(None);
    };
    let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
    let field = |name: &str| payload.get(name).and_then(Value::as_str).map(str::to_owned);
    Ok(Some(OrphanedFrom {
        agent,
        session_id: field("sessionId"),
        expired_at,
        last_checkpoint_at: payload.get("lastCheckpointAt").and_then(Value::as_i64),
        worktree: field("worktree"),
        branch: field("branch"),
        head_sha: field("headSha"),
    }))
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

/// The first gate this row can never satisfy, as `(owner, prerequisite)`.
///
/// A completion gate is inherited down the parent chain, so a prerequisite can
/// be unsatisfiable with no dependency-only cycle present at all: an epic
/// gated on a task inside its own subtree can never be unblocked, because that
/// descendant inherits the epic's gate and so waits on itself. Re-parenting
/// can build the same deadlock without touching a dependency edge, which is
/// why both changes validate through here and [`depends_transitively`] is kept
/// only for the direct-edge refusal it already worded.
///
/// `owners` is the chain whose gates this row inherits and `edges` their
/// prerequisites; `subtree` is what inherits this row's own gates. `required`
/// is then what finishing each prerequisite actually costs, carried per seed:
/// a row that must be completed drags in its children, because a container is
/// not done while work under it is open, and every row reached contributes the
/// prerequisites of itself and its ancestors. `worked=0` marks a row reached
/// only as a gate owner — its prerequisites still apply, but its other
/// children are nobody's business, so it does not descend.
///
/// `fertile` is what keeps that descent from being the whole board. A
/// prerequisite's children are only ever worth walking into for two reasons:
/// one of them is in this row's subtree, or one of them declares a dependency
/// that leads somewhere that is. So descent is confined to rows that are in
/// `subtree`, or on the `spine` — the ancestors of this row and of every row
/// that declares a dependency at all. A child outside both roots a subtree
/// with no dependency edge in it and no member of `subtree` under it, because
/// an ancestor of a row in `subtree` is itself in `subtree` or an ancestor of
/// this row; all it could still contribute is its own ancestors, which the
/// path that reached it already contributed. Nothing refused before is
/// accepted now, and nothing accepted before is refused. Measured on a
/// 3036-row board: `task update <leaf> --depends-on <epic with 3000
/// descendants>` went from 0.738s to 0.040s.
///
/// Status is deliberately not consulted: a prerequisite that happens to be
/// done today would deadlock the tree the moment it reopened, and a shape that
/// only works while nothing changes is not a shape to store. `UNION`
/// deduplicates, which bounds every walk here on a graph of any shape, a
/// malformed parent cycle included.
fn gate_deadlock(connection: &Connection, id: &str) -> Result<Option<(String, String)>> {
    connection
        .query_row(
            "WITH RECURSIVE subtree(id) AS (\
                 SELECT ?1 \
                 UNION \
                 SELECT child.id FROM subtree JOIN tasks child ON child.parent_id=subtree.id\
             ), \
             spine(id) AS (\
                 SELECT ?1 \
                 UNION \
                 SELECT task_id FROM task_dependencies \
                 UNION \
                 SELECT parent.id FROM spine \
                 JOIN tasks node ON node.id=spine.id \
                 JOIN tasks parent ON parent.id=node.parent_id\
             ), \
             fertile(id) AS (SELECT id FROM subtree UNION SELECT id FROM spine), \
             owners(id) AS (\
                 SELECT ?1 \
                 UNION \
                 SELECT parent.id FROM owners \
                 JOIN tasks owner ON owner.id=owners.id \
                 JOIN tasks parent ON parent.id=owner.parent_id\
             ), \
             edges(owner,prerequisite) AS (\
                 SELECT dependency.task_id,dependency.depends_on FROM owners \
                 JOIN task_dependencies dependency ON dependency.task_id=owners.id\
             ), \
             required(seed,id,worked) AS (\
                 SELECT prerequisite,prerequisite,1 FROM edges \
                 UNION \
                 SELECT required.seed,child.id,1 FROM required \
                 JOIN tasks child ON child.parent_id=required.id \
                 JOIN fertile ON fertile.id=child.id \
                 WHERE required.worked=1 \
                 UNION \
                 SELECT required.seed,parent.id,0 FROM required \
                 JOIN tasks owner ON owner.id=required.id \
                 JOIN tasks parent ON parent.id=owner.parent_id \
                 UNION \
                 SELECT required.seed,dependency.depends_on,1 FROM required \
                 JOIN task_dependencies dependency ON dependency.task_id=required.id\
             ) \
             SELECT edges.owner,edges.prerequisite FROM edges \
             JOIN required ON required.seed=edges.prerequisite \
             JOIN subtree ON subtree.id=required.id \
             WHERE required.worked=1 \
             ORDER BY edges.owner,edges.prerequisite \
             LIMIT 1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
}

/// Refuse a tree whose completion gate could never be satisfied.
///
/// Called after the parent and dependency rows are written and inside the same
/// transaction, so the check sees the shape the caller asked for and the
/// refusal leaves none of it behind.
fn require_no_gate_deadlock(connection: &Connection, id: &str) -> Result<()> {
    let Some((owner, prerequisite)) = gate_deadlock(connection, id)? else {
        return Ok(());
    };
    if owner == id {
        bail!(
            "task {id} cannot depend on {prerequisite}: finishing {prerequisite} needs work \
             inside {id}'s own tree, which inherits this gate and would wait on itself"
        );
    }
    bail!(
        "task {id} cannot sit under {owner}: {owner} depends on {prerequisite}, and finishing \
         {prerequisite} needs work inside {id}'s tree, which inherits that gate and would wait \
         on itself"
    )
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
    /// `None` leaves the model allow-list alone; `Some(list)` replaces it
    /// wholesale, and `Some(vec![])` makes the row unrestricted again.
    pub allowed_models: Option<Vec<String>>,
    /// None leaves scope; Some(Some(id)) attaches; Some(None) audits detach.
    pub sprint: Option<Option<String>>,
}
pub struct AcceptHandoffOptions {
    pub agent: String,
    pub session: Option<String>,
    pub lease_ms: i64,
    pub caller_scope: Option<String>,
    pub sprint_override: Option<String>,
    /// The model the acceptor runs as, checked against the task's allow-list
    /// and recorded on the claim row.
    pub model: Option<String>,
    pub git: Option<crate::gitctx::GitContext>,
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
    /// The sprint-boundary override (ADR-045 §3): `Some("sp-…")` works
    /// another sprint's rows deliberately, `Some("any")` is the escape
    /// hatch that says "I know" in the ledger. `None` scopes the pool to
    /// the board's current sprint when one exists, and changes nothing on a
    /// board without one.
    pub sprint_override: Option<String>,
    /// The model the claimer runs as. A restricted task refuses a claim that
    /// omits it or names one outside its list; an unrestricted task records
    /// it and is otherwise unaffected.
    pub model: Option<String>,
}

/// The outcome of settling a restore rescue source before copying it.
pub(crate) enum PreparedRescueRead {
    Online(Store),
    PhysicalFailure(anyhow::Error),
}

pub struct Store {
    pub connection: Connection,
    /// The one authorization context every board-row surface checks through
    /// (ADR-038 clause 5, t-90903ebe). Resolved once at open — the direct
    /// estate has no principal, so its authority map is empty and the guard
    /// no-ops; the managed broker path injects a minted context via
    /// [`Store::open_with_authz`].
    authz: AuthzContext,
}

/// One named sprint row, from whatever connection holds it.
fn require_sprint_on(connection: &Connection, id: &str) -> Result<Sprint> {
    connection
        .query_row("SELECT * FROM sprints WHERE id=?", [id], sprint_row)
        .optional()?
        .with_context(|| format!("sprint {id} does not exist on this board"))
}

/// The board's current sprint, or none: the boundary a claim is scoped to
/// (ADR-045 §3) and the one line `kb dash` carries.
fn current_sprint_on(connection: &Connection) -> Result<Option<Sprint>> {
    Ok(connection
        .query_row(
            "SELECT * FROM sprints WHERE status='current' AND archived=0",
            [],
            sprint_row,
        )
        .optional()?)
}

/// A sprint rows may be attached to: not history. Closed and abandoned are
/// over — attaching work to either would scope nothing, because no claim is
/// ever bounded by a sprint that ended.
fn require_attachable_sprint_on(connection: &Connection, id: &str) -> Result<Sprint> {
    let sprint = require_sprint_on(connection, id)?;
    if sprint.status == "closed" {
        bail!(
            "sprint {id} is closed history; attach the rows to a planned or current sprint \
             instead"
        );
    }
    if sprint.status == "abandoned" {
        bail!("sprint {id} is abandoned; attach the rows to a planned or current sprint instead");
    }
    Ok(sprint)
}
fn carry_sprint_rows(
    connection: &Connection,
    from: &str,
    to: &str,
    actor: &str,
    now: i64,
    rows: &[String],
) -> Result<()> {
    for id in rows {
        connection.execute(
            concat!("UP", "DATE task_sprints SET sprint_id=?,attached_at=?,attached_by=? WHERE sprint_id=? AND task_id=?"),
            params![to, now, actor, from, id],
        )?;
    }
    Ok(())
}
#[derive(Debug)]
struct SprintMove {
    task_id: String,
    old_sprint_id: Option<String>,
}

fn attach_scope_on(
    connection: &Connection,
    root: &str,
    sprint: &str,
    actor: &str,
    now: i64,
) -> Result<Vec<SprintMove>> {
    require_active_task(connection, root)?;
    let mut statement = connection.prepare(
        "WITH RECURSIVE tree(id) AS (SELECT ?1 UNION SELECT t.id FROM tasks t JOIN tree ON t.parent_id=tree.id) SELECT tree.id,old.sprint_id FROM tree LEFT JOIN task_sprints old ON old.task_id=tree.id ORDER BY tree.id"
    )?;
    let existing = statement
        .query_map([root], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let moves = existing
        .into_iter()
        .filter(|(id, old)| {
            (id == root && old.as_deref() != Some(sprint)) || (id != root && old.is_none())
        })
        .map(|(task_id, old_sprint_id)| SprintMove {
            task_id,
            old_sprint_id,
        })
        .collect::<Vec<_>>();
    for moved in &moves {
        connection.execute(
            "INSERT INTO task_sprints(task_id,sprint_id,attached_at,attached_by) VALUES(?,?,?,?) ON CONFLICT(task_id) DO UPDATE SET sprint_id=excluded.sprint_id,attached_at=excluded.attached_at,attached_by=excluded.attached_by",
            params![moved.task_id, sprint, now, actor],
        )?;
    }
    Ok(moves)
}

/// Resolve a claim's sprint boundary (ADR-045 §3): the filter the candidate
/// pool is bounded by, and the value the `task_claim` payload records when
/// the caller crossed a boundary on purpose.
///
/// One resolver for the read-only inspection and the atomic writer, so
/// `claim --candidates` can never show a row `claim --next` refuses.
fn resolve_claim_sprint(
    connection: &Connection,
    sprint_override: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    match sprint_override {
        // The escape hatch: no filter, and the ledger says "any" out loud.
        Some("any") => Ok((None, Some("any".to_owned()))),
        // A named boundary: refused when unknown or abandoned, because a
        // carry-over needs a live boundary to carry over to. Closed is
        // allowed — landing a fix against the sprint that just shipped is
        // exactly what the override exists for.
        Some(id) => {
            let sprint = require_sprint_on(connection, id)?;
            if sprint.status == "abandoned" {
                bail!("sprint {id} is abandoned");
            }
            Ok((Some(id.to_owned()), Some(id.to_owned())))
        }
        // No override: the board's current sprint, when it has one. On a
        // board without one the filter is a no-op and nothing downstream
        // changes — the byte-identical behaviour ADR-045 §3 promises.
        None => Ok((current_sprint_on(connection)?.map(|sprint| sprint.id), None)),
    }
}

/// The scheduler's single definition of an eligible next task.
///
/// Both read-only inspection and the atomic writer call this function. Keep
/// routing here: duplicating it would let the superbot see work that a claim
/// immediately refuses, or hide work that the scheduler would actually hand
/// out.
///
/// The two `task_claims` joins carry `expires_at` for the same reason: the
/// writer runs after [`Store::sweep_expired_claims`] has retired what lapsed,
/// and read-only inspection never sweeps (see
/// [`Store::open_readonly_as_caller`]). Reading the stored shape directly
/// therefore answered differently on the two paths — a task whose lease had
/// run out was hidden from `claim --candidates`, by its untidied claim row and
/// by the `in_progress` the sweep had not yet reset, while `claim --next`
/// handed the same task straight out.
///
/// So `c` is the LIVE lease, whose absence is what makes a row claimable, and
/// `x` is a LAPSED one, whose presence is what makes an `in_progress` row read
/// as `todo` — character for character the rule [`apply_lapsed_leases`] applies
/// to a listing and `expire_claims` stores. On a swept board no `x` can match,
/// so this reduces to `t.status='todo'` and the writer's answer is unchanged.
/// An `in_progress` row with no claim at all is genuinely in progress and
/// stays out either way.
fn eligible_claim_candidates(
    connection: &Connection,
    agent: &str,
    options: &ClaimOptions,
    sprint_filter: Option<&str>,
) -> Result<Vec<Task>> {
    // The sprint clause is appended only when a boundary exists, so a board
    // with no current sprint runs the exact SQL it always has — the
    // byte-identical behaviour ADR-045 §3 makes the success criterion.
    let sprint_clause = if sprint_filter.is_some() {
        " AND t.id IN (SELECT task_id FROM task_sprints WHERE sprint_id=?)"
    } else {
        ""
    };
    let sql = format!(
        // `gated` is the completion gate as a set, computed once: every row
        // carrying an unfinished prerequisite, plus everything beneath it,
        // because a gate declared on a plan is inherited by the work under it.
        // A correlated per-row walk would answer the same question one
        // candidate at a time; `UNION` also makes a malformed parent cycle
        // terminate instead of recursing.
        "WITH RECURSIVE gated(id) AS (
             SELECT dependency.task_id FROM task_dependencies dependency
             JOIN tasks prerequisite ON prerequisite.id=dependency.depends_on
             WHERE prerequisite.status<>'done'
             UNION
             SELECT child.id FROM gated JOIN tasks child ON child.parent_id=gated.id
         )
         SELECT t.* FROM tasks t
         LEFT JOIN task_claims c ON c.task_id=t.id AND c.expires_at>?
         LEFT JOIN task_claims x ON x.task_id=t.id AND x.expires_at<=?
         WHERE (t.status='todo' OR (t.status='in_progress' AND x.task_id IS NOT NULL))
           AND t.archived=0
           AND t.type=?
           AND c.task_id IS NULL
           AND t.id NOT IN (SELECT id FROM gated){sprint_clause}
         ORDER BY t.priority,t.created_at,t.id"
    );
    // One clock for both lease joins: `c` and `x` must divide the claim rows
    // at the same instant, or a lease expiring between the two reads would
    // count as neither live nor lapsed.
    let now = now_ms();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> =
        vec![Box::new(now), Box::new(now), Box::new(CLAIMABLE_TYPE)];
    if let Some(sprint) = sprint_filter {
        values.push(Box::new(sprint.to_owned()));
    }
    let refs = values.iter().map(|value| value.as_ref());
    let mut statement = connection.prepare(&sql)?;
    let mut candidates = statement
        .query_map(params_from_iter(refs), task_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    // Before the routing below reads `assignee`: a lapsed lease left the
    // vanished holder's name in that column until the sweep cleared it, and
    // routing on the stored value refused the row to every other agent —
    // silently, and only on the path that had not swept.
    apply_lapsed_leases(connection, candidates.iter_mut())?;
    // And before it reads `allowed_models`: the scheduler must not offer work
    // the claim path would immediately refuse, so a restricted row is routable
    // only to a caller whose `--model` is in its list. An unrestricted row
    // carries the empty list and is unaffected either way.
    attach_allowed_models(connection, candidates.iter_mut())?;

    let mut eligible = Vec::new();
    for candidate in candidates {
        let routable = (!candidate.driver_only
            || options.caller_scope.as_deref() == Some("driver"))
            && require_allowed_model(
                &candidate.id,
                &candidate.allowed_models,
                options.model.as_deref(),
            )
            .is_ok()
            && (candidate
                .assignee
                .as_ref()
                .is_none_or(|value| value == agent)
                || options.allow_reassign);
        if routable && draft_ancestor(connection, &candidate.id)?.is_none() {
            eligible.push(candidate);
        }
    }

    Ok(if let Some(role) = options.role_filter.as_deref() {
        eligible
            .into_iter()
            .filter(|candidate| candidate.lane.as_deref() == Some(role))
            .collect()
    } else if let Some(lane) = options.caller_lane.as_deref() {
        let own_lane = eligible
            .iter()
            .filter(|candidate| candidate.lane.as_deref() == Some(lane))
            .cloned()
            .collect::<Vec<_>>();
        if !own_lane.is_empty() || !options.cross_lane {
            own_lane
        } else {
            eligible
                .into_iter()
                .filter(|candidate| candidate.lane.is_none())
                .collect()
        }
    } else {
        eligible
            .into_iter()
            .filter(|candidate| candidate.lane.is_none())
            .collect()
    })
}

/// One write scope on a board connection: `BEGIN IMMEDIATE` at the top level,
/// a `SAVEPOINT` inside a scope that is already open.
///
/// This replaces `Connection::transaction_with_behavior(Immediate)` at every
/// write path in this file, and the difference is the whole reason
/// `kanban transact` can exist (ADR-041 §3). `transaction_with_behavior`
/// takes `&mut` on the connection and hands back a Rust value that owns the
/// scope, so an outer scope held across several write calls does not even
/// borrow-check — `error[E0502]`, measured at `58129a6` — and SQLite refuses
/// the nested `BEGIN` in any case. A scope opened here is raw SQL on
/// `&Connection`: what holds it open is a statement SQLite is remembering,
/// not a value this process is holding, so `transact` can open one, run the
/// unchanged write methods below inside it, and roll all of them back
/// together.
///
/// Whether a scope nests is asked of SQLite rather than tracked here.
/// `Connection::is_autocommit` is false exactly while a transaction is open,
/// so no bookkeeping of ours can disagree with the connection's real state —
/// and the disagreement is the failure that matters: a `SAVEPOINT` issued
/// where no transaction is open commits on `RELEASE`, which would make a
/// rolled-back batch land its items anyway.
///
/// It derefs to `Connection`, so every `&transaction` call site inside the
/// write paths is unchanged. [`WriteScope::commit`] is `COMMIT` or `RELEASE`;
/// dropping without it rolls back, which is the `DropBehavior::Rollback` the
/// read path already relies on (`rust/db.rs:2239`).
pub(crate) struct WriteScope<'a> {
    connection: &'a Connection,
    /// The savepoint this scope is, or `None` when it is the transaction.
    savepoint: Option<String>,
    finished: bool,
}

/// Distinct savepoint names, so `RELEASE` names exactly the scope that opened.
///
/// Reusing one name would work for a single level and silently release two at
/// once the moment a write path grew a nested scope, which is the kind of
/// defect that only shows up later as a batch that half-landed.
static WRITE_SCOPES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl<'a> WriteScope<'a> {
    fn open(connection: &'a Connection) -> Result<Self> {
        if connection.is_autocommit() {
            connection.execute_batch("BEGIN IMMEDIATE")?;
            return Ok(Self {
                connection,
                savepoint: None,
                finished: false,
            });
        }
        let name = format!(
            "kanban_write_{}",
            WRITE_SCOPES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        connection.execute_batch(&format!("SAVEPOINT {name}"))?;
        Ok(Self {
            connection,
            savepoint: Some(name),
            finished: false,
        })
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.finished = true;
        match &self.savepoint {
            Some(name) => self.connection.execute_batch(&format!("RELEASE {name}")),
            None => self.connection.execute_batch("COMMIT"),
        }
        .map_err(Into::into)
    }

    /// Undo this scope and say whether the undo itself worked, for the paths
    /// that decide against their own writes: `archive --dry-run`, and a
    /// deployment start that turns out to be an idempotent replay.
    pub(crate) fn rollback(mut self) -> Result<()> {
        self.finished = true;
        self.undo().map_err(Into::into)
    }

    fn undo(&self) -> rusqlite::Result<()> {
        match &self.savepoint {
            // Inside a batch this undoes THIS item and leaves the batch's own
            // scope open, which is what makes a dry run composable.
            Some(name) => self
                .connection
                .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}")),
            None => self.connection.execute_batch("ROLLBACK"),
        }
    }
}

impl std::ops::Deref for WriteScope<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection
    }
}

impl Drop for WriteScope<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Discarded exactly as `rusqlite::Transaction`'s own drop discards it:
        // there is no caller left to tell, and a scope that will not roll back
        // is a connection this process is about to close.
        let _ = self.undo();
    }
}

impl SnapshotSource for Store {
    fn snapshot_connection(&self) -> &Connection {
        &self.connection
    }
}

/// A coherent read snapshot that is a no-op inside an open scope.
///
/// Readers that must see several rows from one point in time open this. When
/// the connection already holds a transaction — a `transact` batch, or a
/// write path reading back its own work — that scope IS the snapshot, and a
/// nested `BEGIN` would be refused by SQLite. Outside one, a deferred
/// transaction pins the snapshot; dropping it releases the read.
pub(crate) struct ReadSnapshot<'a> {
    connection: Option<&'a Connection>,
}

impl<'a> ReadSnapshot<'a> {
    fn open(connection: &'a Connection) -> Result<Self> {
        if !connection.is_autocommit() {
            return Ok(Self { connection: None });
        }
        connection.execute_batch("BEGIN")?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    fn close(mut self) -> Result<()> {
        // Commit first; a failed COMMIT leaves the transaction open, and Drop
        // must still be able to roll it back.
        if let Some(connection) = self.connection {
            connection.execute_batch("COMMIT")?;
            self.connection = None;
        }
        Ok(())
    }
}

impl Drop for ReadSnapshot<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.execute_batch("ROLLBACK");
        }
    }
}

impl Store {
    /// Open a write scope: the one way a write path in this file takes the
    /// mutation lock.
    ///
    /// `&self` rather than `&mut self`, which is the point: the scope is SQL
    /// SQLite is holding rather than a borrow this process is holding, so a
    /// `&mut self` write method can open one and still read through `&self`
    /// helpers inside it, and `kanban transact` can hold an outer scope open
    /// across several of those methods (ADR-041 §3). See [`WriteScope`].
    pub(crate) fn begin_write(&self) -> Result<WriteScope<'_>> {
        WriteScope::open(&self.connection)
    }

    /// Take the batch's outer scope: `BEGIN IMMEDIATE` once, at the start, so
    /// every item's own [`Store::begin_write`] is a savepoint inside it.
    ///
    /// ADR-041 §3.2: this is strictly stronger than a write per command, not
    /// weaker. Every item's check-then-write runs under a write lock that was
    /// already held when the batch began, which is the property the
    /// `BEGIN IMMEDIATE` at each write path is protecting (see
    /// [`deployment_subject_tags_on`] and [`whole_board_read_on`]). A
    /// savepoint-only batch that skipped this would weaken it.
    ///
    /// Returns nothing to hold, deliberately: a guard held here would be the
    /// live borrow that cannot coexist with the `&mut self` write methods the
    /// items call.
    pub(crate) fn begin_batch(&self) -> Result<()> {
        if !self.connection.is_autocommit() {
            bail!("a write scope is already open on this board");
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    /// Whether a batch's outer scope is open, so a caller that owes work to a
    /// *committed* board can tell "already landed" from "lands later".
    pub(crate) fn in_batch(&self) -> bool {
        !self.connection.is_autocommit()
    }

    /// Land every item of a batch.
    pub(crate) fn commit_batch(&self) -> Result<()> {
        self.connection.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Undo every item of a batch, including their ledger events: the chain
    /// is written on this same connection, so the freed sequence numbers
    /// leave no hole (ADR-041 §3, Probe B; ADR-029).
    pub(crate) fn rollback_batch(&self) -> Result<()> {
        self.connection.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Open a board file DIRECTLY: POSIX file permission is the only
    /// authorization, no policy row is consulted, and the guard no-ops. That
    /// is ADR-038 clause 9's direct open, stated as a constructor.
    ///
    /// This is the constructor for `init` (which creates the file before any
    /// principal could be resolved against it), for the importers, and for
    /// tests. **Every surface that answers a caller uses
    /// [`Store::open_as_caller`] instead**, so that the enforcement read and
    /// the authority mint happen exactly once, at the process entry point,
    /// where `authz.rs` says they belong. Deliberately not folded into this
    /// constructor: a board open would then depend on ambient process state
    /// (`XDG_DATA_HOME`) that has nothing to do with the file being opened.
    pub fn open(path: &Path) -> Result<Self> {
        let mut store = Self {
            connection: open_board(path)?,
            authz: AuthzContext::direct(
                board_id_from_path(&path.to_string_lossy()).unwrap_or_default(),
            ),
        };
        store.sweep_expired_claims()?;
        Ok(store)
    }

    /// Open a board AS THIS PROCESS, under the authorization context
    /// [`crate::routing::board_authz`] resolves from the kernel and the
    /// canonical registry.
    ///
    /// The one constructor every process entry point uses — the CLI, `serve`,
    /// `watch`, and the dispatcher — and the reason none of them can state a
    /// board id, an enforcement state, or an authority of its own.
    pub(crate) fn open_as_caller(path: &Path) -> Result<Self> {
        let mut store = Self {
            connection: open_board(path)?,
            authz: crate::routing::board_authz(path)?,
        };
        store.sweep_expired_claims()?;
        Ok(store)
    }

    /// Open a board under a fully-specified authorization context — the
    /// managed broker path, and the tenancy tests. The context's enforcement
    /// state, authority map, and board UUID are all taken as-is; the store
    /// never re-reads them per surface. The managed broker hop is a separate
    /// slice (t-86eb4fb3), so today only the tenancy tests drive it.
    #[allow(dead_code)]
    pub fn open_with_authz(path: &Path, authz: AuthzContext) -> Result<Self> {
        let mut store = Self {
            connection: open_board(path)?,
            authz,
        };
        store.sweep_expired_claims()?;
        Ok(store)
    }

    /// Wrap an already-open connection with the direct (single-user) context.
    /// For tests that build a connection by hand; the guard no-ops.
    #[cfg(test)]
    pub(crate) fn from_connection(connection: Connection) -> Self {
        Self {
            connection,
            authz: AuthzContext::direct(String::new()),
        }
    }

    pub(crate) fn open_for_dispatcher(path: &Path) -> Result<Self> {
        Ok(Self {
            connection: open_board(path)?,
            // The dispatcher is a separate process, so this is ITS authority,
            // minted from ITS kernel UID — not the subscription's `actor`
            // column, and not the authority of whoever wrote the row that
            // produced the event. A job acts on rows on its own behalf.
            authz: crate::routing::board_authz(path)?,
        })
    }

    /// The read-only open AS THIS PROCESS. **Every read-only command uses
    /// this**, so that a board the caller cannot write is still a board the
    /// caller can read: a backup directory, a read-only mount, or a board
    /// handed to a reviewer all answer here.
    ///
    /// Re-resolved on every call, and `watch` calls it once per poll: that is
    /// what makes a REVOCATION land mid-stream rather than at the next
    /// reconnect (see [`crate::routing::board_authz`]).
    ///
    /// No [`Store::sweep_expired_claims`] and no migration, unlike every
    /// writable constructor above. Both are writes, and a read that writes is
    /// not a read — the sweep is the reason `kanban task list` against a board
    /// at mode 0400 used to die with `attempt to write a readonly database`
    /// instead of answering.
    ///
    /// Skipping the sweep is safe because the sweep is DERIVED, not decided.
    /// What retirement means is `expires_at<=now`, and every read computes it
    /// from that column rather than from the sweep having run:
    /// [`Store::list_tasks`] and [`Store::require_task`] apply
    /// [`apply_lapsed_leases`], [`active_claim`] filters on `expires_at`, and
    /// so does [`eligible_claim_candidates`]. So a lapsed lease reads as
    /// lapsed here whether or not its row has been tidied away, the answer is
    /// identical to the one a writable open gives, and the next writable open
    /// recomputes the sweep from the same column and removes the row then.
    ///
    /// What a read cannot reproduce is the `claim_expired` LEDGER EVENT, which
    /// is history of a state change and so a write by definition. It lands on
    /// the next writable open. Nothing else is lost by not writing, and
    /// nothing is refused for being unable to.
    pub(crate) fn open_readonly_as_caller(path: &Path) -> Result<Self> {
        let authz = crate::routing::board_authz(path)?;
        Self::open_readonly_with_authz(path, authz)
    }

    fn open_readonly_with_authz(path: &Path, authz: AuthzContext) -> Result<Self> {
        // Refuse before SQLite can reveal missing or corrupt board bytes.
        authz.check_read(&[])?;
        let connection = open_board_readonly(path)?;
        Ok(Self { connection, authz })
    }

    /// Open a board for a command that only READS it.
    ///
    /// Two branches, decided by the board's stored schema and nothing else:
    ///
    /// Already at this binary's schema — the overwhelming common case — takes
    /// [`Store::open_readonly_as_caller`]: no sweep, no recency stamp, no
    /// `chmod` re-assert, no migration. That is what lets a read answer from a
    /// board the caller cannot write.
    ///
    /// BEHIND this binary's schema takes the writable open, because such a
    /// board cannot be read at all until it is migrated and a migration is a
    /// write. Keeping that branch is not a loophole in "a read never writes":
    /// `kanban task list` has always brought a board forward, the refusal
    /// `open_board_readonly` would give instead names "any ordinary command"
    /// as the fix while being itself an ordinary command, and a rolling
    /// upgrade leaves exactly this state on every board that has not been
    /// written since.
    ///
    /// Behind AND unwritable is a genuine refusal, and it says both halves.
    /// SQLite's own `attempt to write a readonly database` would name only the
    /// permission and hide the reason a read wanted to write at all.
    pub(crate) fn open_for_read_as_caller(path: &Path) -> Result<Self> {
        let stored = crate::db::stored_schema_version(path);
        if stored == Some(crate::db::BOARD_SCHEMA_VERSION) {
            return Self::open_readonly_as_caller(path);
        }
        if let Some(version) = stored
            && crate::db::refuses_writes(path)
        {
            bail!(
                "board file {} is at schema {version}, and this Kanban reads {}. Bringing it \
                 forward is a write, and this process cannot write the file, so it cannot be \
                 read as it stands.\n\
                 Make the file writable and run any ordinary kanban command once to migrate it, \
                 or copy it somewhere writable and address the copy with --db.",
                path.display(),
                crate::db::BOARD_SCHEMA_VERSION
            );
        }
        Self::open_as_caller(path)
    }

    /// The bulk-read gate on this store's own connection.
    pub(crate) fn require_whole_board_read(&self) -> Result<()> {
        whole_board_read_on(&self.authz, &self.connection)
    }

    /// The bulk-write gate on this store's own connection.
    pub(crate) fn require_whole_board_write(&self) -> Result<()> {
        whole_board_write_on(&self.authz, &self.connection)
    }

    /// A rescue source whose read authority was settled before any rescue
    /// artifact exists. Only a physical open failure stays recoverable.
    pub(crate) fn prepare_rescue_read(path: &Path) -> Result<PreparedRescueRead> {
        let authz = crate::routing::board_authz(path)?;
        authz.check_read(&[])?;
        let connection = match open_board_readonly(path) {
            Ok(connection) => connection,
            Err(error) => return Ok(PreparedRescueRead::PhysicalFailure(error)),
        };
        whole_board_read_on(&authz, &connection)?;
        Ok(PreparedRescueRead::Online(Self { connection, authz }))
    }

    pub(crate) fn require_restore_write(path: &Path) -> Result<()> {
        let authz = crate::routing::board_authz(path)?;
        Self::require_restore_write_with_authz(path, authz)
    }

    fn require_restore_write_with_authz(path: &Path, authz: AuthzContext) -> Result<()> {
        authz.check_write(&[], &[])?;
        // Only physical open failures are recoverable; permission errors propagate.
        if let Ok(connection) = open_board_readonly(path) {
            whole_board_write_on(&authz, &connection)?;
        }
        Ok(())
    }

    /// Keep only the events this caller may see, by each event's REAL tags.
    ///
    /// Board scope is required outright — no board read, no ledger at all —
    /// and then each row is filtered rather than refused, because a refusal
    /// inside an enumeration would say "there is history here you cannot
    /// have". Outside managed enforcement this does no per-row work at all.
    fn visible_events(&self, events: Vec<Event>) -> Result<Vec<Event>> {
        self.authz.check_read(&[])?;
        if !self.authz.is_enforcing() {
            return Ok(events);
        }
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            if self
                .authz
                .permits_read(&event_tags(&self.connection, &event)?)
            {
                out.push(event);
            }
        }
        Ok(out)
    }

    /// Search reaches rows through an INDEX, so the guard cannot be applied
    /// here: `search_documents` carries a projected copy of each row's tags,
    /// and a stale copy is a bypass. The context goes down into
    /// [`crate::search::search`], which re-reads each hit's real tags from the
    /// source tables as it materialises it.
    ///
    /// `board` is a display name for the citation and the receipt. It is a
    /// label the caller supplied — `serve` passes the project's name — and it
    /// is never the authorization subject; the board id inside the context is.
    pub fn search(&self, board: &str, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        crate::search::search(&self.connection, board, options, &self.authz)
    }

    /// Rebuilding the index reads every row on the board and writes a
    /// searchable copy of it, so it takes the bulk-write gate: a caller who
    /// cannot see a tagged row must not be able to (re)write that row's index
    /// entry either.
    pub fn rebuild_search(&mut self, board: &str, actor: &str) -> Result<SearchIndexReport> {
        self.require_whole_board_write()?;
        crate::search::rebuild(&mut self.connection, board, actor)
    }

    /// Index health is counts and a model name, not row content, so board
    /// scope is the whole check.
    pub fn search_health(&self) -> Result<SearchIndexHealth> {
        self.authz.check_read(&[])?;
        crate::search::health(&self.connection)
    }

    pub fn add_subscription(&mut self, input: AddSubscription) -> Result<Subscription> {
        validate_subscription_bounds(&input)?;
        let actor = nonempty(&input.actor, "actor")?.to_owned();
        let consumer_id = subscription_identifier(&input.consumer_id, "consumer id", 64)?;
        let action_id = subscription_identifier(&input.action_id, "action id", 64)?;
        let secret_ref = input
            .secret_ref
            .as_deref()
            .map(|value| subscription_identifier(value, "secret reference", 128))
            .transpose()?;
        let id = match input.id.as_deref() {
            Some(value) => subscription_identifier(value, "subscription id", 64)?,
            None => {
                let random = Uuid::new_v4().simple().to_string();
                format!("sub-{}", &random[..8])
            }
        };
        if !id.starts_with("sub-") || id.len() == 4 {
            bail!("subscription id must start with sub- and include a suffix");
        }

        let transaction = self.begin_write()?;
        // Under the mutation lock: a concurrent write cannot slip between this
        // check and the INSERT below.
        self.authz.check_write(&[], &[])?;
        let subject_task_id = input
            .subject_task_id
            .as_deref()
            .map(|value| nonempty(value, "subscription subject task id").map(str::to_owned))
            .transpose()?;
        if let Some(subject) = subject_task_id.as_deref()
            && !watch_subject_exists_on(&transaction, subject)?
        {
            bail!(
                "subscription subject task {subject} not found in current or historical board state"
            );
        }

        let mut relations = Vec::new();
        for relation in normalized_unique(&input.relations) {
            let (kind, target) = relation
                .split_once(':')
                .context("subscription relation must be KIND:ID")?;
            validate(kind, &RELATION_KINDS, "subscription relation kind")?;
            if target.trim().is_empty() || target != target.trim() {
                bail!("subscription relation target is required");
            }
            if !watch_relation_target_exists_on(&transaction, kind, target)? {
                bail!(
                    "subscription relation target {kind}:{target} not found in current or historical board state"
                );
            }
            relations.push(format!("{kind}:{target}"));
        }

        let kinds = normalized_unique(&input.kinds);
        for kind in &kinds {
            nonempty(kind, "subscription event kind")?;
            if !BOARD_EVENT_KINDS.contains(&kind.as_str())
                && !board_event_kind_exists(&transaction, kind)?
            {
                bail!("subscription event kind {kind} not found in this board's event history");
            }
        }
        let prior_statuses = normalized_unique(&input.prior_statuses);
        for status in &prior_statuses {
            validate(status, &TASK_STATUSES, "subscription prior status")?;
        }
        let current_statuses = normalized_unique(&input.current_statuses);
        for status in &current_statuses {
            validate(status, &TASK_STATUSES, "subscription current status")?;
        }
        let tags = validate_registered_tags(
            &transaction,
            &normalized_unique(&input.tags),
            "subscription",
        )?;
        let now = now_ms();
        let start_event_seq =
            transaction.query_row("SELECT COALESCE(max(seq),0)+1 FROM events", [], |row| {
                row.get::<_, i64>(0)
            })?;
        transaction.execute(
            "INSERT INTO subscriptions(id,protocol_version,subject_task_id,relations,kinds,prior_statuses,current_statuses,tags,consumer_id,action_id,timeout_ms,max_retries,rate_per_minute,max_concurrency,start_event_seq,secret_ref,status,created_at,created_by,updated_at,updated_by,paused_at,paused_by) VALUES(?1,1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,'active',?16,?17,?18,?19,NULL,NULL)",
            params![
                id,
                subject_task_id,
                serde_json::to_string(&relations)?,
                serde_json::to_string(&kinds)?,
                serde_json::to_string(&prior_statuses)?,
                serde_json::to_string(&current_statuses)?,
                serde_json::to_string(&tags)?,
                consumer_id,
                action_id,
                input.timeout_ms,
                input.max_retries,
                input.rate_per_minute,
                input.max_concurrency,
                start_event_seq,
                secret_ref,
                now,
                actor,
                now,
                actor,
            ],
        )?;
        event(
            &transaction,
            None,
            "subscription_added",
            Some(&actor),
            json!({
                "subscriptionID": id,
                "protocolVersion": SUBSCRIPTION_PROTOCOL_VERSION,
                "subjectTaskID": subject_task_id,
                "relations": relations,
                "kinds": kinds,
                "priorStatuses": prior_statuses,
                "currentStatuses": current_statuses,
                "tags": tags,
                "consumerID": consumer_id,
                "actionID": action_id,
                "timeoutMs": input.timeout_ms,
                "maxRetries": input.max_retries,
                "ratePerMinute": input.rate_per_minute,
                "maxConcurrency": input.max_concurrency,
            }),
        )?;
        let tail = transaction.query_row(
            "SELECT seq,kind,payload FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        if tail.0 != start_event_seq {
            bail!(
                "subscription {id} expected subscription_added at seq {start_event_seq}, found seq {}",
                tail.0
            );
        }
        if tail.1 != "subscription_added" {
            bail!(
                "subscription {id} expected subscription_added at seq {start_event_seq}, found {}",
                tail.1
            );
        }
        let payload: Value = serde_json::from_str(&tail.2)
            .context("subscription_added payload is malformed JSON")?;
        if payload.get("subscriptionID").and_then(Value::as_str) != Some(id.as_str()) {
            bail!(
                "subscription {id} expected subscription_added payload to reference subscriptionID {id}"
            );
        }
        let result = transaction.query_row(
            "SELECT * FROM subscriptions WHERE id=?",
            [&id],
            subscription_row,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn require_subscription(&self, id: &str) -> Result<Subscription> {
        self.authz.check_read(&[])?;
        self.connection
            .query_row(
                "SELECT * FROM subscriptions WHERE id=?",
                [id],
                subscription_row,
            )
            .optional()?
            .with_context(|| format!("subscription {id} not found"))
    }

    pub fn subscriptions(
        &self,
        status: Option<&str>,
        consumer_id: Option<&str>,
        include_paused: bool,
    ) -> Result<Vec<Subscription>> {
        if let Some(status) = status {
            validate(status, &SUBSCRIPTION_STATUSES, "subscription status")?;
        }
        if let Some(consumer) = consumer_id {
            subscription_identifier(consumer, "consumer id", 64)?;
        }
        self.authz.check_read(&[])?;
        let mut sql = String::from("SELECT * FROM subscriptions WHERE 1=1");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(status) = status {
            sql.push_str(" AND status=?");
            values.push(Box::new(status.to_owned()));
        } else if !include_paused {
            sql.push_str(" AND status='active'");
        }
        if let Some(consumer) = consumer_id {
            sql.push_str(" AND consumer_id=?");
            values.push(Box::new(consumer.to_owned()));
        }
        sql.push_str(" ORDER BY created_at,id");
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(|value| value.as_ref())),
            subscription_row,
        );
        rows?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Every subscription's derived position on this board — including which
    /// codes its dead letters were refused with — from one grouped query over
    /// the delivery rows, plus one read of the event head.
    ///
    /// Deliberately not per-subscription. The operator page renders a row per
    /// subscription across every registered board, so a position query taking
    /// a subscription id would sit inside that loop and turn one page into one
    /// query per subscription per board. Two statements answer the whole set
    /// however many subscriptions the board carries.
    ///
    /// `max(CASE WHEN status='acked' ...)` is the whole cursor: the ledger
    /// already owns "what have I seen" as `seq`, and there is no cursor column
    /// to read or to keep in step (`docs/ui-pubsub-consumption-seams.md`).
    ///
    /// The extra `GROUP BY` term is what makes the codes trustworthy rather
    /// than merely present. Grouping by subscription *and* dead-letter code
    /// splits each subscription into one row per code plus a null-code row
    /// carrying everything that is not dead-lettered, and the counts are
    /// summed back per subscription here. A second statement would have been
    /// simpler to read and wrong: the dispatcher can dead-letter a delivery
    /// between two statements, and the page would then print an attribution
    /// whose parts do not add up to the total it printed beside them. One
    /// statement is one snapshot.
    pub fn subscription_positions(&self) -> Result<SubscriptionPositions> {
        self.authz.check_read(&[])?;
        let head_event_seq =
            self.connection
                .query_row("SELECT COALESCE(max(seq),0) FROM events", [], |row| {
                    row.get(0)
                })?;
        let mut statement = self.connection.prepare(
            "SELECT subscription_id,\
                    CASE WHEN status='dead_letter' THEN last_error_code END AS dead_letter_code,\
                    max(CASE WHEN status='acked' THEN event_seq END) AS acked_through_seq,\
                    sum(status='pending') AS pending,\
                    sum(status='leased') AS leased,\
                    sum(status='retry_wait') AS retry_wait,\
                    sum(status='dead_letter') AS dead_letter \
             FROM subscription_deliveries GROUP BY subscription_id,dead_letter_code",
        )?;
        let mut by_subscription: BTreeMap<String, SubscriptionPosition> = BTreeMap::new();
        let mut dead_letters: BTreeMap<String, Vec<DeadLetterCode>> = BTreeMap::new();
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>("subscription_id")?,
                // A dead-lettered delivery cannot have a null code — the
                // table's own CHECK says so — so a null here is a row that is
                // not dead-lettered, never an unattributed refusal.
                row.get::<_, Option<String>>("dead_letter_code")?,
                SubscriptionPosition {
                    acked_through_seq: row.get("acked_through_seq")?,
                    pending: row.get("pending")?,
                    leased: row.get("leased")?,
                    retry_wait: row.get("retry_wait")?,
                    dead_letter: row.get("dead_letter")?,
                },
            ))
        })?;
        for row in rows {
            let (subscription_id, dead_letter_code, group) = row?;
            if let Some(code) = dead_letter_code {
                dead_letters
                    .entry(subscription_id.clone())
                    .or_default()
                    .push(DeadLetterCode {
                        code,
                        deliveries: group.dead_letter,
                    });
            }
            let position = by_subscription.entry(subscription_id).or_default();
            // `None` sorts below every `Some`, so this is the highest acked
            // seq across the groups and not an accident of row order.
            position.acked_through_seq = position.acked_through_seq.max(group.acked_through_seq);
            position.pending += group.pending;
            position.leased += group.leased;
            position.retry_wait += group.retry_wait;
            position.dead_letter += group.dead_letter;
        }
        for codes in dead_letters.values_mut() {
            codes.sort_by(|left, right| {
                right
                    .deliveries
                    .cmp(&left.deliveries)
                    .then_with(|| left.code.cmp(&right.code))
            });
        }
        Ok(SubscriptionPositions {
            head_event_seq,
            by_subscription,
            dead_letters,
        })
    }

    fn set_subscription_paused(
        &mut self,
        id: &str,
        paused: bool,
        actor: &str,
    ) -> Result<Subscription> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        // Under the mutation lock: a concurrent write cannot slip between this
        // check and the UPDATE below.
        self.authz.check_write(&[], &[])?;
        let current = transaction
            .query_row(
                "SELECT * FROM subscriptions WHERE id=?",
                [id],
                subscription_row,
            )
            .optional()?
            .with_context(|| format!("subscription {id} not found"))?;
        let desired = if paused { "paused" } else { "active" };
        if current.status == desired {
            transaction.commit()?;
            return Ok(current);
        }
        let now = now_ms();
        transaction.execute(
            "UPDATE subscriptions SET status=?,updated_at=?,updated_by=?,paused_at=?,paused_by=? WHERE id=?",
            params![
                desired,
                now,
                actor,
                paused.then_some(now),
                paused.then_some(actor.as_str()),
                id
            ],
        )?;
        event(
            &transaction,
            None,
            if paused {
                "subscription_paused"
            } else {
                "subscription_resumed"
            },
            Some(&actor),
            json!({
                "subscriptionID": id,
                "consumerID": current.consumer_id,
                "actionID": current.action_id,
            }),
        )?;
        let result = transaction.query_row(
            "SELECT * FROM subscriptions WHERE id=?",
            [id],
            subscription_row,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn pause_subscription(&mut self, id: &str, actor: &str) -> Result<Subscription> {
        self.set_subscription_paused(id, true, actor)
    }

    pub fn resume_subscription(&mut self, id: &str, actor: &str) -> Result<Subscription> {
        self.set_subscription_paused(id, false, actor)
    }

    /// The oldest due delivery candidate, if one is currently eligible.
    ///
    /// The read path does not mutate or lease anything. The dispatcher takes
    /// its own lock after reading the returned candidate, then calls the claim
    /// method that rechecks the same eligibility gates inside one transaction.
    pub(crate) fn next_due_subscription_delivery(
        &self,
        now: i64,
    ) -> Result<Option<SubscriptionDeliveryCandidate>> {
        self.next_due_subscription_delivery_for_consumer(now, None)
    }

    /// The oldest due delivery candidate for one exact consumer, when given.
    ///
    /// Filtering belongs in the eligibility query rather than after `LIMIT 1`:
    /// otherwise an unrelated consumer at the head of the queue can make a
    /// targeted dispatcher report a false idle result.
    ///
    /// Authorization is the JOB's, not the row's. A delivery exists because a
    /// subscription matched an event; that says nothing about whether THIS
    /// dispatcher process may read the row the event is about. Board scope is
    /// required to scan at all, and a candidate whose event the job may not
    /// read is reported as no candidate — never as an error naming it, which
    /// would be an existence oracle for a row on another tenant's board.
    ///
    /// One consequence, stated rather than hidden: this query is `LIMIT 1`, so
    /// an unreadable delivery at the head of the queue leaves that consumer
    /// idle until the authority is granted or the delivery is retired. It
    /// blocks visibly instead of being dead-lettered silently.
    pub(crate) fn next_due_subscription_delivery_for_consumer(
        &self,
        now: i64,
        consumer_id: Option<&str>,
    ) -> Result<Option<SubscriptionDeliveryCandidate>> {
        validate_nonnegative_now(now, "dispatcher now")?;
        self.authz.check_read(&[])?;
        let cutoff = now.saturating_sub(60_000);
        let mut statement = self.connection.prepare(
            "SELECT d.subscription_id,d.event_id,d.event_seq,d.event_kind,d.event_created_at,d.status,d.attempts,d.next_attempt_at,d.lease_token,d.lease_deadline_at,d.last_attempt_at,d.last_error_code,d.acked_at,d.dead_lettered_at,d.created_at,d.updated_at \
             FROM subscription_deliveries d \
             JOIN subscriptions s ON s.id=d.subscription_id \
             WHERE d.status IN ('pending','retry_wait') \
               AND s.status='active' \
               AND d.next_attempt_at IS NOT NULL \
               AND d.next_attempt_at<=?1 \
               AND (?4 IS NULL OR s.consumer_id=?4) \
               AND (SELECT COUNT(*) FROM subscription_delivery_attempts a WHERE a.subscription_id=s.id AND a.started_at>=?2 AND a.started_at<=?3) < s.rate_per_minute \
               AND (SELECT COUNT(*) FROM subscription_deliveries l WHERE l.subscription_id=s.id AND l.status='leased') < s.max_concurrency \
             ORDER BY d.next_attempt_at ASC,d.event_seq ASC,d.subscription_id ASC,d.event_id ASC \
             LIMIT 1",
        )?;
        let delivery = statement
            .query_row(
                params![now, cutoff, now, consumer_id],
                subscription_delivery_row,
            )
            .optional()?;
        let Some(delivery) = delivery else {
            return Ok(None);
        };
        let subscription = self
            .connection
            .query_row(
                "SELECT * FROM subscriptions WHERE id=?",
                [&delivery.subscription_id],
                subscription_row,
            )
            .with_context(|| format!("subscription {} not found", delivery.subscription_id))?;
        validate(
            &subscription.status,
            &SUBSCRIPTION_STATUSES,
            "subscription status",
        )?;
        if subscription.status != "active" {
            return Ok(None);
        }
        validate_pending_or_retry_delivery(&delivery, &subscription)?;
        let event = require_delivery_event_identity(&self.connection, &delivery, &subscription)?;
        if !self
            .authz
            .permits_read(&event_tags(&self.connection, &event)?)
        {
            return Ok(None);
        }
        let next_attempt_at = delivery
            .next_attempt_at
            .context("subscription delivery candidate is missing next_attempt_at")?;
        let result = SubscriptionDeliveryCandidate {
            subscription,
            event_id: delivery.event_id,
            event_seq: delivery.event_seq,
            event_kind: delivery.event_kind,
            delivery_status: delivery.status,
            attempt_number: delivery.attempts + 1,
            next_attempt_at,
        };
        let _ = (
            &result.subscription,
            &result.event_id,
            result.event_seq,
            &result.event_kind,
            &result.delivery_status,
            result.attempt_number,
            result.next_attempt_at,
        );
        Ok(Some(result))
    }

    /// Claim one exact delivery candidate, or return `None` when it lost
    /// eligibility before the transaction could commit it.
    ///
    /// This is where a board row leaves the process: the claim carries the
    /// whole `Event`, and the dispatcher projects it into the request body an
    /// ADAPTER process reads. So the job's authority is checked HERE, under
    /// the mutation lock and before the lease is taken — an unauthorized
    /// delivery is left pending for a dispatcher that may read it, rather than
    /// leased, invoked and then refused after the adapter has already seen the
    /// row.
    pub(crate) fn claim_subscription_delivery(
        &mut self,
        subscription_id: &str,
        event_id: &str,
        now: i64,
        lease_duration_ms: i64,
    ) -> Result<Option<SubscriptionDeliveryClaim>> {
        validate_delivery_lease_duration(lease_duration_ms)?;
        validate_nonnegative_now(now, "dispatcher now")?;
        self.authz.check_read(&[])?;
        let transaction = self.begin_write()?;
        let delivery = transaction
            .query_row(
                "SELECT * FROM subscription_deliveries \
                 WHERE subscription_id=? AND event_id=? \
                   AND status IN ('pending','retry_wait') \
                   AND next_attempt_at IS NOT NULL \
                   AND next_attempt_at<=?",
                params![subscription_id, event_id, now],
                subscription_delivery_row,
            )
            .optional()?;
        let Some(delivery) = delivery else {
            transaction.commit()?;
            return Ok(None);
        };
        let subscription = transaction
            .query_row(
                "SELECT * FROM subscriptions WHERE id=?",
                [subscription_id],
                subscription_row,
            )
            .optional()?
            .with_context(|| format!("subscription {subscription_id} not found"))?;
        validate(
            &subscription.status,
            &SUBSCRIPTION_STATUSES,
            "subscription status",
        )?;
        if subscription.status != "active" {
            transaction.commit()?;
            return Ok(None);
        }
        validate_pending_or_retry_delivery(&delivery, &subscription)?;
        let rate_count = subscription_delivery_rate_count(&transaction, subscription_id, now)?;
        if rate_count >= subscription.rate_per_minute {
            transaction.commit()?;
            return Ok(None);
        }
        let leased_count = subscription_delivery_leased_count(&transaction, subscription_id)?;
        if leased_count >= subscription.max_concurrency {
            transaction.commit()?;
            return Ok(None);
        }
        let event = require_delivery_event_identity(&transaction, &delivery, &subscription)?;
        if !self.authz.permits_read(&event_tags(&transaction, &event)?) {
            transaction.commit()?;
            return Ok(None);
        }
        let attempt_number = delivery.attempts + 1;
        let lease_token = format!("lease-{}", Uuid::new_v4().simple());
        let lease_deadline_at = now
            .checked_add(lease_duration_ms)
            .context("subscription lease deadline overflowed")?;
        let updated = transaction.execute(
            "UPDATE subscription_deliveries SET status='leased',attempts=?,lease_token=?,lease_deadline_at=?,next_attempt_at=NULL,last_attempt_at=?,last_error_code=NULL,acked_at=NULL,dead_lettered_at=NULL,updated_at=? \
             WHERE subscription_id=? AND event_id=? AND status IN ('pending','retry_wait') AND next_attempt_at IS NOT NULL AND next_attempt_at<=? AND lease_token IS NULL",
            params![
                attempt_number,
                lease_token,
                lease_deadline_at,
                now,
                now,
                subscription_id,
                event_id,
                now
            ],
        )?;
        if updated != 1 {
            transaction.commit()?;
            return Ok(None);
        }
        transaction.execute(
            "INSERT INTO subscription_delivery_attempts(subscription_id,event_id,attempt,started_at,finished_at,outcome,error_code) VALUES(?,?,?,?,NULL,'claim',NULL)",
            params![subscription_id, event_id, attempt_number, now],
        )?;
        let result = SubscriptionDeliveryClaim {
            subscription,
            event_id: delivery.event_id,
            event_seq: delivery.event_seq,
            event_kind: delivery.event_kind,
            event_created_at: delivery.event_created_at,
            event,
            delivery_status: "leased".to_owned(),
            attempt_number,
            lease_token,
            lease_deadline_at,
        };
        let _ = (
            &result.subscription,
            &result.event_id,
            result.event_seq,
            &result.event_kind,
            result.event_created_at,
            &result.event,
            &result.delivery_status,
            result.attempt_number,
            &result.lease_token,
            result.lease_deadline_at,
        );
        transaction.commit()?;
        Ok(Some(result))
    }

    /// Release expired leases and either retry them immediately or dead-letter
    /// them once they exhausted their retry budget.
    ///
    /// Lease bookkeeping, not row content: nothing here is returned to a
    /// caller and no event payload is read, so the gate is board-scope write
    /// — a dispatcher with no authority on the board does not get to move its
    /// delivery rows around either.
    pub(crate) fn recover_expired_subscription_deliveries(&mut self, now: i64) -> Result<usize> {
        validate_nonnegative_now(now, "dispatcher now")?;
        self.authz.check_write(&[], &[])?;
        let transaction = self.begin_write()?;
        let deliveries = transaction
            .prepare(
                "SELECT * FROM subscription_deliveries \
                 WHERE status='leased' AND lease_deadline_at<=? \
                 ORDER BY lease_deadline_at,event_seq,subscription_id,event_id",
            )?
            .query_map([now], subscription_delivery_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut recovered = 0_usize;
        for delivery in deliveries {
            validate_leased_delivery(&delivery)?;
            let subscription = transaction
                .query_row(
                    "SELECT * FROM subscriptions WHERE id=?",
                    [&delivery.subscription_id],
                    subscription_row,
                )
                .optional()?
                .with_context(|| format!("subscription {} not found", delivery.subscription_id))?;
            validate(
                &subscription.status,
                &SUBSCRIPTION_STATUSES,
                "subscription status",
            )?;
            let _event = require_delivery_event_identity(&transaction, &delivery, &subscription)?;
            let lease_deadline_at = delivery
                .lease_deadline_at
                .context("leased delivery is missing lease_deadline_at")?;
            let terminal = delivery.attempts > subscription.max_retries;
            let next_attempt_at = if terminal {
                None
            } else {
                Some(
                    lease_deadline_at
                        .checked_add(delivery_retry_delay_ms(
                            delivery.attempts,
                            subscription.timeout_ms,
                        ))
                        .context("subscription retry deadline overflowed")?,
                )
            };
            let finished_at = lease_deadline_at;
            let dead_lettered_at = terminal.then_some(lease_deadline_at);
            let updated = transaction.execute(
                "UPDATE subscription_deliveries SET status=?,lease_token=NULL,lease_deadline_at=NULL,next_attempt_at=?,last_error_code='dispatcher_lease_expired',acked_at=NULL,dead_lettered_at=?,updated_at=? \
                 WHERE subscription_id=? AND event_id=? AND status='leased' AND lease_deadline_at<=?",
                params![
                    if terminal { "dead_letter" } else { "retry_wait" },
                    next_attempt_at,
                    dead_lettered_at,
                    now,
                    delivery.subscription_id,
                    delivery.event_id,
                    now
                ],
            )?;
            if updated != 1 {
                bail!(
                    "subscription {} delivery {} stopped being leased while recovering expiry",
                    delivery.subscription_id,
                    delivery.event_id
                );
            }
            let attempt = transaction.execute(
                "UPDATE subscription_delivery_attempts SET finished_at=?,outcome='lease_expired',error_code='dispatcher_lease_expired' WHERE subscription_id=? AND event_id=? AND attempt=? AND finished_at IS NULL",
                params![finished_at, delivery.subscription_id, delivery.event_id, delivery.attempts],
            )?;
            if attempt != 1 {
                bail!(
                    "subscription {} delivery {} missing open claim attempt during expiry recovery",
                    delivery.subscription_id,
                    delivery.event_id
                );
            }
            recovered += 1;
        }
        transaction.commit()?;
        Ok(recovered)
    }

    /// Ack a leased delivery and close its matching attempt row.
    ///
    /// Board-scope write, deliberately NOT the event's tags. The row was
    /// already authorized at claim time; refusing to record the outcome of
    /// work an adapter has already done would leave the lease to expire and
    /// the same delivery to be attempted again forever. A revocation stops the
    /// NEXT claim, which is where stopping it means something.
    pub(crate) fn finalize_subscription_delivery_success(
        &mut self,
        subscription_id: &str,
        event_id: &str,
        lease_token: &str,
        now: i64,
    ) -> Result<bool> {
        validate_nonnegative_now(now, "dispatcher now")?;
        self.authz.check_write(&[], &[])?;
        let transaction = self.begin_write()?;
        let delivery = transaction
            .query_row(
                "SELECT * FROM subscription_deliveries \
                 WHERE subscription_id=? AND event_id=? AND lease_token=? AND status='leased'",
                params![subscription_id, event_id, lease_token],
                subscription_delivery_row,
            )
            .optional()?;
        let Some(delivery) = delivery else {
            transaction.commit()?;
            return Ok(false);
        };
        validate_leased_delivery(&delivery)?;
        let subscription = transaction
            .query_row(
                "SELECT * FROM subscriptions WHERE id=?",
                [subscription_id],
                subscription_row,
            )
            .optional()?
            .with_context(|| format!("subscription {subscription_id} not found"))?;
        validate(
            &subscription.status,
            &SUBSCRIPTION_STATUSES,
            "subscription status",
        )?;
        let _event = require_delivery_event_identity(&transaction, &delivery, &subscription)?;
        if delivery
            .lease_deadline_at
            .is_some_and(|deadline| now >= deadline)
        {
            transaction.commit()?;
            return Ok(false);
        }
        let updated = transaction.execute(
            "UPDATE subscription_deliveries SET status='acked',lease_token=NULL,lease_deadline_at=NULL,next_attempt_at=NULL,last_error_code=NULL,acked_at=?,dead_lettered_at=NULL,updated_at=? \
             WHERE subscription_id=? AND event_id=? AND lease_token=? AND status='leased'",
            params![now, now, subscription_id, event_id, lease_token],
        )?;
        if updated != 1 {
            transaction.commit()?;
            return Ok(false);
        }
        let attempt = transaction.execute(
            "UPDATE subscription_delivery_attempts SET finished_at=?,outcome='success' WHERE subscription_id=? AND event_id=? AND attempt=? AND finished_at IS NULL",
            params![now, subscription_id, event_id, delivery.attempts],
        )?;
        if attempt != 1 {
            bail!(
                "subscription {} delivery {} missing open claim attempt during success finalization",
                subscription_id,
                event_id
            );
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Record a leased delivery failure and schedule the next deterministic
    /// retry, or dead-letter it once the retry budget is exhausted.
    ///
    /// Retry delays use a fixed exponential schedule with no jitter so a crash
    /// and a restart agree on the same next attempt time:
    /// `now + min(timeout_ms, 1000ms * 2^(attempt_number - 1))`.
    ///
    /// Board-scope write, for the same reason as the success path: this
    /// records what already happened.
    pub(crate) fn finalize_subscription_delivery_failure(
        &mut self,
        subscription_id: &str,
        event_id: &str,
        lease_token: &str,
        now: i64,
        timed_out: bool,
        error_code: &str,
    ) -> Result<bool> {
        let error_code = validate_delivery_error_code(error_code)?;
        validate_nonnegative_now(now, "dispatcher now")?;
        self.authz.check_write(&[], &[])?;
        let transaction = self.begin_write()?;
        let delivery = transaction
            .query_row(
                "SELECT * FROM subscription_deliveries \
                 WHERE subscription_id=? AND event_id=? AND lease_token=? AND status='leased'",
                params![subscription_id, event_id, lease_token],
                subscription_delivery_row,
            )
            .optional()?;
        let Some(delivery) = delivery else {
            transaction.commit()?;
            return Ok(false);
        };
        validate_leased_delivery(&delivery)?;
        let subscription = transaction
            .query_row(
                "SELECT * FROM subscriptions WHERE id=?",
                [subscription_id],
                subscription_row,
            )
            .optional()?
            .with_context(|| format!("subscription {subscription_id} not found"))?;
        validate(
            &subscription.status,
            &SUBSCRIPTION_STATUSES,
            "subscription status",
        )?;
        let _event = require_delivery_event_identity(&transaction, &delivery, &subscription)?;
        if delivery
            .lease_deadline_at
            .is_some_and(|deadline| now >= deadline)
        {
            transaction.commit()?;
            return Ok(false);
        }
        let terminal = delivery.attempts > subscription.max_retries;
        let next_attempt_at = if terminal {
            None
        } else {
            Some(
                now.checked_add(delivery_retry_delay_ms(
                    delivery.attempts,
                    subscription.timeout_ms,
                ))
                .context("subscription retry deadline overflowed")?,
            )
        };
        let updated = transaction.execute(
            "UPDATE subscription_deliveries SET status=?,lease_token=NULL,lease_deadline_at=NULL,next_attempt_at=?,last_error_code=?,acked_at=NULL,dead_lettered_at=?,updated_at=? \
             WHERE subscription_id=? AND event_id=? AND lease_token=? AND status='leased'",
            params![
                if terminal { "dead_letter" } else { "retry_wait" },
                next_attempt_at,
                error_code,
                terminal.then_some(now),
                now,
                subscription_id,
                event_id,
                lease_token
            ],
        )?;
        if updated != 1 {
            transaction.commit()?;
            return Ok(false);
        }
        let outcome = if timed_out {
            "timeout"
        } else if terminal {
            "dead"
        } else {
            "retry"
        };
        let attempt = transaction.execute(
            "UPDATE subscription_delivery_attempts SET finished_at=?,outcome=?,error_code=? WHERE subscription_id=? AND event_id=? AND attempt=? AND finished_at IS NULL",
            params![now, outcome, error_code, subscription_id, event_id, delivery.attempts],
        )?;
        if attempt != 1 {
            bail!(
                "subscription {} delivery {} missing open claim attempt during failure finalization",
                subscription_id,
                event_id
            );
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Retire leases that have run out, on every WRITABLE open of the board.
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
    ///
    /// This is a WRITE, so [`Store::open_readonly_as_caller`] does not call
    /// it and nothing here is best-effort on the paths that do: a writable
    /// open that cannot retire a lapsed lease has a real problem and says so.
    /// The row it tidies is never what a read answers from — see
    /// `open_readonly_as_caller` for why every read derives liveness from
    /// `expires_at` instead.
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
        let transaction = self.begin_write()?;
        expire_claims(&transaction, now_ms())?;
        transaction.commit()?;
        Ok(expired as usize)
    }

    /// Recorded ledger events, newest first. The `events` table is the audit
    /// trail for lease seizures and destructive removals, so it needs a reader.
    pub fn events(
        &self,
        task: Option<&str>,
        kind: Option<&str>,
        limit: i64,
        include_archived: bool,
    ) -> Result<Vec<Event>> {
        self.events_with_bounds(task, kind, None, None, limit, include_archived)
    }

    pub fn events_with_bounds(
        &self,
        task: Option<&str>,
        kind: Option<&str>,
        after: Option<i64>,
        before: Option<i64>,
        limit: i64,
        include_archived: bool,
    ) -> Result<Vec<Event>> {
        validate_event_bounds(after, before)?;
        if let Some(id) = task {
            // A NAMED row: the caller asked about this task's history, so the
            // answer is the single generic denial before the row is opened —
            // `require_task` below would otherwise say `task <id> not found`
            // for a row that exists and is merely invisible.
            self.authz.check_read(&task_tags(&self.connection, id)?)?;
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
        if let Some(after) = after {
            sql.push_str(" AND created_at>=?");
            values.push(Box::new(after));
        }
        if let Some(before) = before {
            sql.push_str(" AND created_at<?");
            values.push(Box::new(before));
        }
        if !include_archived {
            sql.push_str(" AND archived=0");
        }
        sql.push_str(" ORDER BY seq DESC LIMIT ?");
        values.push(Box::new(limit));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(
                params_from_iter(values.iter().map(|value| value.as_ref())),
                board_event_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Filtered by each row's REAL tags AND its own frozen snapshot, so an
        // invisible row is not reconstructible from its trail and a retag
        // event does not name the tag that hid it. See [`event_tags`].
        self.visible_events(rows)
    }

    /// Where this board's ledger currently ends.
    ///
    /// Board scope only, because what leaves this function is a sequence
    /// number and not row content — the same reasoning [`Store::events_since`]
    /// states for its own scope, minus the rows. It discloses that the board
    /// has had `n` events, which board read already tells a caller.
    ///
    /// A live consumer needs this to START at the head: a connection that
    /// began at zero would replay the whole trail at an operator who just
    /// reconnected, and one whose position sat ahead of the head — a board
    /// restored from a backup — would stall silently, which is missing
    /// information that reads as absence of information.
    pub fn event_head_seq(&self) -> Result<i64> {
        self.authz.check_read(&[])?;
        self.connection
            .query_row("SELECT COALESCE(max(seq),0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(Into::into)
    }

    /// Ascending ledger rows after `cursor`, narrowed only by kind and archival.
    ///
    /// Deliberately board-wide: watch reads this as the tail that lets a cursor
    /// move past rows its filtered scan rejected, so it has to see the rows
    /// that do not match. It used to take a `task` parameter the SQL never
    /// bound, so a caller could pass a selector and silently receive board-wide
    /// rows anyway; the parameter is removed rather than honoured, because a
    /// task-scoped board tail would stall the cursor behind other tasks'
    /// traffic. Subject-scoped reads belong in `events_since_filtered`.
    ///
    /// This reasoning is about the board tail only. The registry twin,
    /// `Registry::rule_events_since`, does bind its selector, so a
    /// `--rule R --follow` watch has exactly the rule-scoped tail argued
    /// against here. That asymmetry is deliberate and pre-existing: rule trails
    /// are sparse enough that a stalled cursor costs little, and narrowing a
    /// tail is always safe. Do not "fix" the registry to match this one.
    ///
    /// Authorization: board scope only, and deliberately so. This is the one
    /// event read whose PURPOSE is to see rows the caller's filter rejected,
    /// so per-row filtering here would stall the cursor behind another task's
    /// traffic and re-scan the same window forever. Its only caller —
    /// `watch::poll_once` — takes `.last().seq` from it and nothing else, so
    /// what leaves this function is a sequence number, not row content. The
    /// rows a watcher is actually HANDED come from `events_since_filtered`,
    /// which filters per row. A caller that returned these rows to a consumer
    /// would be a new leak, and would need the filter that this one skips.
    pub fn events_since(
        &self,
        kind: Option<&str>,
        cursor: i64,
        limit: i64,
        include_archived: bool,
    ) -> Result<Vec<Event>> {
        validate_event_limit(limit)?;
        self.authz.check_read(&[])?;
        let mut sql = String::from(
            "SELECT seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash \
             FROM events WHERE seq>?",
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(cursor)];
        if let Some(kind) = kind {
            sql.push_str(" AND kind=?");
            values.push(Box::new(kind.to_owned()));
        }
        if !include_archived {
            sql.push_str(" AND archived=0");
        }
        sql.push_str(" ORDER BY seq ASC LIMIT ?");
        values.push(Box::new(limit));
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(
                params_from_iter(values.iter().map(|value| value.as_ref())),
                board_event_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Ascending watch rows with semantic predicates applied before LIMIT.
    ///
    /// Watch filters must bound delivered events rather than the raw ledger
    /// page: a sparse match at sequence 500 is still the first event in a
    /// `--limit 1` request. The JSON predicates deliberately require the
    /// private semantic snapshot, so legacy rows never match a semantic
    /// filter.
    ///
    /// This is the watch DELIVERY path — every row it returns is handed to a
    /// subscriber — so it filters per row by each event's real tags, through
    /// [`Store::visible_events`]. Because `watch` re-opens its store on every
    /// poll, the authority this filter uses is re-minted every poll: a
    /// revocation stops the very next batch rather than the next reconnect.
    #[allow(clippy::too_many_arguments)]
    pub fn events_since_filtered(
        &self,
        task: Option<&str>,
        kinds: &[String],
        relations: &[String],
        prior_statuses: &[String],
        current_statuses: &[String],
        tags: &[String],
        cursor: i64,
        limit: i64,
        include_archived: bool,
    ) -> Result<Vec<Event>> {
        validate_event_limit(limit)?;
        let mut sql = String::from(
            "SELECT seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash \
             FROM events WHERE seq>?",
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(cursor)];
        append_event_filters(
            &mut sql,
            &mut values,
            EventFilterSpec {
                task,
                kinds,
                relations,
                prior_statuses,
                current_statuses,
                tags,
                include_archived,
                semantic_payload: "CASE WHEN json_valid(payload) THEN payload ELSE '{}' END",
            },
        );
        sql.push_str(" ORDER BY seq ASC LIMIT ?");
        values.push(Box::new(limit));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(
                params_from_iter(values.iter().map(|value| value.as_ref())),
                board_event_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.visible_events(rows)
    }

    /// Fan every new event out to the subscriptions that match it.
    ///
    /// Board-scope write, and deliberately NOT per-event. The
    /// `board_materialization_cursor` is a single board-wide row, so an event
    /// this dispatcher skipped would be skipped for every OTHER dispatcher
    /// too: a narrowly-authorized job could starve a broadly-authorized one by
    /// walking the cursor past rows it could not see. Materialization is
    /// therefore an unfiltered system fan-out, and the job's authority is
    /// checked where it can be checked per job without shared state —
    /// `next_due_subscription_delivery_for_consumer` and
    /// `claim_subscription_delivery`, which are the two places a row actually
    /// reaches a consumer.
    pub(crate) fn materialize_subscriptions(&mut self) -> Result<usize> {
        self.authz.check_write(&[], &[])?;
        let transaction = self.begin_write()?;
        let global_cursor: i64 = transaction.query_row(
            "SELECT event_seq FROM board_materialization_cursor WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        let head: i64 =
            transaction.query_row("SELECT COALESCE(max(seq),0) FROM events", [], |row| {
                row.get(0)
            })?;
        if head < global_cursor {
            bail!(
                "board materialization cursor {global_cursor} is ahead of the current event head {head}"
            );
        }
        let now = now_ms();
        let subscriptions = transaction
            .prepare("SELECT * FROM subscriptions ORDER BY created_at,id")?
            .query_map([], subscription_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut inserted = 0_usize;
        for subscription in subscriptions {
            let lower = global_cursor.max(subscription.start_event_seq);
            if lower >= head {
                continue;
            }
            let mut sql = String::from(
                "SELECT seq,task_id,kind,actor,payload,created_at,prev_hash,event_hash \
                 FROM events WHERE seq>? AND seq<=?",
            );
            let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(lower), Box::new(head)];
            append_event_filters(
                &mut sql,
                &mut values,
                EventFilterSpec {
                    task: subscription.subject_task_id.as_deref(),
                    kinds: &subscription.kinds,
                    relations: &subscription.relations,
                    prior_statuses: &subscription.prior_statuses,
                    current_statuses: &subscription.current_statuses,
                    tags: &subscription.tags,
                    include_archived: true,
                    semantic_payload: "CASE WHEN json_valid(payload) THEN payload ELSE '{}' END",
                },
            );
            sql.push_str(" ORDER BY seq ASC");
            let rows = {
                let mut statement = transaction.prepare(&sql)?;
                statement
                    .query_map(
                        params_from_iter(values.iter().map(|value| value.as_ref())),
                        board_event_identity_row,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for row in rows {
                let prev_hash = row
                    .prev_hash
                    .as_deref()
                    .context(format!("event {} is missing prev_hash", row.seq))?;
                let event_hash = row
                    .event_hash
                    .as_deref()
                    .context(format!("event {} is missing event_hash", row.seq))?;
                if !is_lower_hex_64(prev_hash) {
                    bail!("event {} has malformed prev_hash", row.seq);
                }
                if !is_lower_hex_64(event_hash) {
                    bail!("event {} has malformed event_hash", row.seq);
                }
                let expected = audit_digest(
                    prev_hash,
                    row.seq,
                    row.task_id.as_deref(),
                    &row.kind,
                    row.actor.as_deref(),
                    &row.payload,
                    row.created_at,
                );
                if expected != event_hash {
                    bail!(
                        "event {} has mismatched stored identity: expected {expected}, stored {event_hash}",
                        row.seq
                    );
                }
                inserted += transaction.execute(
                    "INSERT INTO subscription_deliveries(subscription_id,event_id,event_seq,event_kind,event_created_at,status,attempts,next_attempt_at,created_at,updated_at) \
                     VALUES(?,?,?,?,?,'pending',0,?,?,?) \
                     ON CONFLICT(subscription_id,event_id) DO NOTHING",
                    params![
                        subscription.id.as_str(),
                        event_hash,
                        row.seq,
                        row.kind,
                        row.created_at,
                        now,
                        now,
                        now,
                    ],
                )? as usize;
            }
        }
        transaction.execute(
            "UPDATE board_materialization_cursor SET event_seq=?,updated_at=? WHERE id=1",
            params![head, now],
        )?;
        transaction.commit()?;
        Ok(inserted)
    }

    /// Board-scope read, for the surfaces whose answer is a board-wide fact
    /// rather than a row: the ledger head, an index-health count.
    pub(crate) fn require_board_read(&self) -> Result<()> {
        self.authz.check_read(&[])
    }

    /// Whether this board has ever recorded an event of this kind. A kind is
    /// vocabulary, not a row, so board scope is the whole check.
    pub fn event_kind_exists(&self, kind: &str) -> Result<bool> {
        self.authz.check_read(&[])?;
        board_event_kind_exists(&self.connection, kind)
    }

    /// A watch selector may name a task that has since been removed, but a
    /// typo must not read as an authoritative empty stream.
    ///
    /// Checked against the named task's REAL tags, so `watch --task T` on an
    /// invisible row answers `denied or not found` rather than confirming that
    /// T exists. A task that survives only in event history has no live tag
    /// row, so that case falls back to board scope.
    pub fn watch_subject_exists(&self, id: &str) -> Result<bool> {
        self.authz.check_read(&task_tags(&self.connection, id)?)?;
        watch_subject_exists_on(&self.connection, id)
    }

    /// Relation predicates accept current task identities and exact targets
    /// retained in semantic history. Unknown IDs fail closed, while removed
    /// parents and dependencies remain replayable.
    ///
    /// Same rule as [`Store::watch_subject_exists`]: the relation target is a
    /// task id, so it is checked against that task's real tags.
    pub fn watch_relation_target_exists(&self, kind: &str, id: &str) -> Result<bool> {
        self.authz.check_read(&task_tags(&self.connection, id)?)?;
        watch_relation_target_exists_on(&self.connection, kind, id)
    }

    pub fn initialize(&mut self, name: &str, actor: &str) -> Result<()> {
        let name = nonempty(name, "name")?;
        let actor = nonempty(actor, "actor")?;
        let transaction = self.begin_write()?;
        transaction.execute(
            "INSERT INTO board_meta(key,value) VALUES('name',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [name],
        )?;
        event(
            &transaction,
            None,
            "board_initialized",
            Some(actor),
            json!({"name":name}),
        )?;
        transaction.commit()?;
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

    #[cfg(test)]
    pub fn add_task(&mut self, input: AddTask) -> Result<Task> {
        self.add_task_in_sprint(input, None)
    }

    pub fn add_task_in_sprint(&mut self, input: AddTask, sprint_id: Option<&str>) -> Result<Task> {
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
        let transaction = self.begin_write()?;
        // A new row has no old tag set; the resulting set is what the caller
        // asked for. Under the mutation lock.
        self.authz.check_write(&[], &input.tags)?;
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
        require_no_gate_deadlock(&transaction, &id)?;
        // A row may be created straight into work — an import, a task moved in
        // one step — and the gate applies to that exactly as it applies to the
        // move. Checked after the dependency rows exist, so a row declaring its
        // own unfinished prerequisite is refused rather than created mid-work.
        if is_work_bearing_status(&input.status) {
            require_no_blocking_gates(&transaction, &id, GateCaller::Unleased)?;
        }
        // Every other event kind names who did it; creating a task was the one
        // action the trail could not attribute, because there was no `--as` to
        // record. Measured 2026-08-21 across the live boards, 132 of these
        // carried no actor. An absent actor is still recorded as absent —
        // inventing one would be worse than the gap.
        set_tags(&transaction, &id, &input.tags)?;
        // The list is validated inside, before its first INSERT, so a bad
        // name refuses the whole `task add` rather than creating a row with
        // half a restriction.
        set_allowed_models(&transaction, &id, &input.allowed_models)?;
        event_with_status(
            &transaction,
            Some(&id),
            "task_added",
            input.actor.as_deref(),
            json!({ "type": input.task_type, "status": input.status }),
            None,
            Some(&input.status),
        )?;
        if let Some(sprint) = sprint_id {
            require_attachable_sprint_on(&transaction, sprint)?;
            let actor = input.actor.as_deref().unwrap_or("system@cli");
            let moves = attach_scope_on(&transaction, &id, sprint, actor, now)?;
            let moved = moves
                .iter()
                .map(|moved| moved.task_id.clone())
                .collect::<Vec<_>>();
            event(
                &transaction,
                Some(&id),
                "task_sprint_changed",
                Some(actor),
                json!({"taskID":id,"oldSprintID":null,"newSprintID":sprint,"moved":moved}),
            )?;
        }
        transaction.commit()?;
        self.require_task(&id)
    }

    pub fn require_task(&self, id: &str) -> Result<Task> {
        // All-of-tag check BEFORE the row is opened: read the row's tags first,
        // then gate on them. An absent row yields no tags, so a caller without
        // board scope still receives the generic denial, never a difference
        // between "invisible" and "absent".
        let tags = task_tags(&self.connection, id)?;
        self.authz.check_read(&tags)?;
        let mut one = require_task(&self.connection, id)?;
        attach_tags(&self.connection, std::iter::once(&mut one))?;
        attach_allowed_models(&self.connection, std::iter::once(&mut one))?;
        apply_lapsed_leases(&self.connection, std::iter::once(&mut one))?;
        Ok(one)
    }

    /// Rows, optionally narrowed by status, by tag, by lane and by the model
    /// allowed to claim them.
    ///
    /// A tag filter checks the master file first: asking for one that was never
    /// registered returns an empty list otherwise, which reads exactly like
    /// "nothing is tagged that" and is how a typo becomes a wrong answer.
    ///
    /// A lane is not registered anywhere, so the lane filter is an exact match
    /// on the row's own `lane`: a task with no lane matches no lane, and no
    /// lane is inferred for it.
    ///
    /// A model filter is neither: there is no master file of models, so the
    /// name is checked for shape and then matched exactly against the rows'
    /// lists. It selects restricted rows only — an unrestricted row is open to
    /// every model and belongs to none of their listings.
    ///
    /// A row whose tags this caller may not read is not in the answer, on the
    /// same all-of-tag test a named read is refused with, and exactly as
    /// [`Store::sprint_tasks`] filters. The filtering is here rather than in
    /// each caller because every task listing in the product — `task list`,
    /// the dashboard's counts, the JSON projection, the web board page, the
    /// context pack and MCP — reads through this one function, and a count
    /// taken over rows this caller may not see reports their existence as
    /// surely as printing their titles would.
    pub fn list_tasks(
        &self,
        status: Option<&str>,
        tag: Option<&str>,
        lane: Option<&str>,
        allowed_model: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<Task>> {
        self.authz.check_read(&[])?;
        let (where_clause, values) =
            self.task_filter(status, tag, lane, allowed_model, include_archived)?;
        let sql = format!("SELECT * FROM tasks{where_clause} ORDER BY priority,created_at,id");
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement
            .query_map(params_from_iter(refs), task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        attach_tags(&self.connection, rows.iter_mut())?;
        rows.retain(|task| self.authz.permits_read(&task.tags));
        attach_allowed_models(&self.connection, rows.iter_mut())?;
        apply_lapsed_leases(&self.connection, rows.iter_mut())?;
        Ok(rows)
    }

    /// [`Store::list_tasks`] with each row's live lease beside it, filtered by
    /// the same per-row tag read test.
    ///
    /// The lease is read in the same query, one `LEFT JOIN` on the claims
    /// primary key, so a thousand-task board pays one round trip and not a
    /// thousand and one. What comes back is the summary `task show` emits —
    /// the holder and the lease's timing — and never the token. A lease is
    /// live by the same test `get_claim` applies, so a row a long-lived
    /// process lists after the lease lapsed reads as free, not held.
    pub fn list_tasks_with_claims(
        &self,
        status: Option<&str>,
        tag: Option<&str>,
        lane: Option<&str>,
        allowed_model: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<(Task, Option<ClaimSummary>)>> {
        self.authz.check_read(&[])?;
        let (where_clause, filters) =
            self.task_filter(status, tag, lane, allowed_model, include_archived)?;
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(filters.len() + 1);
        values.push(Box::new(now_ms()));
        values.extend(filters);
        let sql = format!(
            "SELECT t.*, c.agent_id AS claim_agent_id, c.session_id AS claim_session_id, \
             c.claimed_at AS claim_claimed_at, c.heartbeat_at AS claim_heartbeat_at, \
             c.expires_at AS claim_expires_at, c.model AS claim_model \
             FROM tasks t LEFT JOIN task_claims c ON c.task_id=t.id AND c.expires_at>?\
             {where_clause} ORDER BY t.priority,t.created_at,t.id"
        );
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement
            .query_map(params_from_iter(refs), |row| {
                let task = task_row(row)?;
                let claim = row
                    .get::<_, Option<String>>("claim_agent_id")?
                    .map(|agent_id| -> rusqlite::Result<ClaimSummary> {
                        Ok(ClaimSummary {
                            task_id: task.id.clone(),
                            agent_id,
                            session_id: row.get("claim_session_id")?,
                            claimed_at: row.get("claim_claimed_at")?,
                            heartbeat_at: row.get("claim_heartbeat_at")?,
                            expires_at: row.get("claim_expires_at")?,
                            model: row.get("claim_model")?,
                        })
                    })
                    .transpose()?;
                Ok((task, claim))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        attach_tags(&self.connection, rows.iter_mut().map(|(task, _)| task))?;
        rows.retain(|(task, _)| self.authz.permits_read(&task.tags));
        attach_allowed_models(&self.connection, rows.iter_mut().map(|(task, _)| task))?;
        apply_lapsed_leases(&self.connection, rows.iter_mut().map(|(task, _)| task))?;
        Ok(rows)
    }

    /// The `WHERE` clause of a task listing and the values it binds, in order.
    ///
    /// Column names are unqualified: the claims table shares none of them, so
    /// the same clause serves the plain listing and the join.
    fn task_filter(
        &self,
        status: Option<&str>,
        tag: Option<&str>,
        lane: Option<&str>,
        allowed_model: Option<&str>,
        include_archived: bool,
    ) -> Result<(String, Vec<Box<dyn rusqlite::ToSql>>)> {
        if let Some(value) = status {
            validate(value, &TASK_STATUSES, "task status")?;
        }
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !include_archived {
            clauses.push("archived=0");
        }
        if let Some(value) = status {
            clauses.push("status=?");
            values.push(Box::new(value.to_owned()));
        }
        if let Some(lane) = lane {
            clauses.push("lane=?");
            values.push(Box::new(lane_filter(lane)?.to_owned()));
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
            clauses.push(if include_archived {
                "id IN (SELECT task_id FROM task_tags WHERE tag=?)"
            } else {
                "id IN (SELECT task_id FROM task_tags WHERE tag=? AND archived=0)"
            });
            values.push(Box::new(tag));
        }
        if let Some(model) = allowed_model {
            // Shape-checked, not registry-checked: there is no master file of
            // models to mistype against, and the refusal a bad name earns is
            // the same one the write side gives.
            let model = crate::model::validate_model_name(model)?;
            clauses.push("id IN (SELECT task_id FROM task_models WHERE model=?)");
            values.push(Box::new(model));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        Ok((where_clause, values))
    }

    /// The prerequisites declared on one row, as rows.
    ///
    /// A listing, so it obeys the listing rule (`t-34f6eed5`): a prerequisite
    /// whose tags this caller may not read is absent, exactly as it is absent
    /// from [`Store::list_tasks`], and for the same reason — an enumeration
    /// that carried it would hand over its title, its tags and its status to
    /// a caller whose named read of it is refused. The EDGE is not hidden
    /// with it: [`Store::blocking_gates`] still reports the gate that
    /// prerequisite holds, by id and status, so a row whose claim is refused
    /// never reads as claimable.
    pub fn dependencies(&self, id: &str) -> Result<Vec<Task>> {
        // The subject row's existence is the caller's to establish; the
        // prerequisites are filtered here, per row, on their own tags.
        self.authz.check_read(&[])?;
        let mut rows = dependencies(&self.connection, id)?;
        attach_tags(&self.connection, rows.iter_mut())?;
        rows.retain(|task| self.authz.permits_read(&task.tags));
        attach_allowed_models(&self.connection, rows.iter_mut())?;
        apply_lapsed_leases(&self.connection, rows.iter_mut())?;
        Ok(rows)
    }

    /// Every unfinished prerequisite standing between this row and work on it,
    /// whether declared here or inherited from a plan above it.
    ///
    /// The one query every surface reads the gate through — the refusals, the
    /// context packet, `task show`, `task list --with-relations` — so a board
    /// view and a refused claim cannot disagree about what is blocking. An
    /// empty list means no dependency gate, which is not the same as claimable:
    /// a draft ancestor, a live lease, routing and authorization are separate
    /// rules with their own answers.
    ///
    /// A prerequisite this caller may not read is REDUCED here, never dropped
    /// (`t-3548303e`): dropping it would leave a readable row reporting an
    /// empty gate while every claim on it is refused by that same gate, which
    /// is a worse answer than a partial one. What survives is the gate's
    /// function — which row holds this one, and whether it is finished — and
    /// what goes is its prose, the title.
    pub fn blocking_gates(&self, id: &str) -> Result<Vec<GateBlocker>> {
        // A named read of the subject row: a gate is read through the row it
        // gates, so that row's own tags gate it.
        self.require_task(id)?;
        let mut blockers = blocking_gates(&self.connection, id, &lapsed_leases(&self.connection)?)?;
        self.redact_denied_gate_titles(&mut blockers)?;
        Ok(blockers)
    }

    /// The same answer for a listing, which asks it of every row.
    ///
    /// The lapsed-lease projection every blocker is read through is
    /// board-wide (see `LapsedLeases`), so a listing reads it once here
    /// rather than once per row.
    pub fn blocking_gates_for(&self, ids: &[String]) -> Result<Vec<Vec<GateBlocker>>> {
        let lapsed = lapsed_leases(&self.connection)?;
        ids.iter()
            .map(|id| {
                self.require_task(id)?;
                let mut blockers = blocking_gates(&self.connection, id, &lapsed)?;
                self.redact_denied_gate_titles(&mut blockers)?;
                Ok(blockers)
            })
            .collect()
    }

    /// Blank the title of every prerequisite this caller may not read.
    ///
    /// Outside `Managed` nothing can be denied, so the whole walk — one tag
    /// read per DISTINCT prerequisite, and a gate names the same prerequisite
    /// once per owner that declared it — is skipped, and the direct estate
    /// pays nothing for a guard that cannot deny.
    fn redact_denied_gate_titles(&self, blockers: &mut [GateBlocker]) -> Result<()> {
        if !self.authz.is_enforcing() {
            return Ok(());
        }
        let mut readable: BTreeMap<String, bool> = BTreeMap::new();
        for blocker in blockers {
            let permitted = match readable.get(&blocker.prerequisite_id) {
                Some(known) => *known,
                None => {
                    let tags = task_tags(&self.connection, &blocker.prerequisite_id)?;
                    let permitted = self.authz.permits_read(&tags);
                    readable.insert(blocker.prerequisite_id.clone(), permitted);
                    permitted
                }
            };
            if !permitted {
                blocker.prerequisite_title = None;
            }
        }
        Ok(())
    }

    /// The chain of plans above this row, outermost first.
    ///
    /// The walk stops at the first ancestor this caller may not read rather
    /// than refusing the whole chain or punching a hole in it (`t-3548303e`):
    /// a chain with a gap misreports which plan owns which, and refusing
    /// outright let one denied epic hide a row the caller IS authorized to
    /// read — `context` answered `denied or not found` for the readable leaf.
    /// What is returned is the contiguous readable run nearest the row, and
    /// it discloses nothing the row does not already carry: the row's own
    /// `parentID` is on the row.
    pub fn ancestors(&self, id: &str) -> Result<Vec<Task>> {
        let mut current = self.require_task(id)?;
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::from([id.to_owned()]);
        while let Some(parent) = current.parent_id.clone() {
            if !seen.insert(parent.clone()) {
                bail!("parent cycle detected at {parent}");
            }
            // The tags are read and decided on first, so a DENIED ancestor
            // truncates the chain while a genuinely broken one — a dangling
            // parent id — still surfaces as the fault it is.
            if !self
                .authz
                .permits_read(&task_tags(&self.connection, &parent)?)
            {
                break;
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
        let transaction = self.begin_write()?;
        // Under the mutation lock, and before the row is opened: a status move
        // does not retag, so the old and resulting tag sets are the row's.
        let old_tags = task_tags(&transaction, id)?;
        self.authz.check_write(&old_tags, &old_tags)?;
        let current = require_active_task(&transaction, id)?;
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
        // A completion gate governs work, not bookkeeping: a move that claims
        // work is under way or finished waits on the prerequisites, while
        // draft, backlog, todo, blocked and cancelled stay available —
        // including to a holder parking a row whose prerequisite reopened.
        // `--force` seizes a live lease; it does not finish a prerequisite,
        // so it is deliberately not consulted here.
        if is_work_bearing_status(status) {
            // A move is the one gate site that may or may not be the holder's:
            // the same refusal reaches a holder pushing their own row to done
            // and a bystander moving a free one, and only the first has a
            // lease to stop.
            let caller = match active_claim(&transaction, id, now_ms())? {
                Some(held) if held.agent_id == actor => GateCaller::Holder,
                _ => GateCaller::Unleased,
            };
            require_no_blocking_gates(&transaction, id, caller)?;
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
        event_with_status(
            &transaction,
            Some(id),
            "task_moved",
            Some(&actor),
            json!({"status": status, "seizedFrom": seized.map(|claim| claim.agent_id), "gateBypassed": gate_bypassed}),
            Some(&current.status),
            Some(status),
        )?;
        transaction.commit()?;
        self.require_task(id)
    }

    pub fn remove_task(&mut self, id: &str, actor: &str, force: bool) -> Result<()> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: removal leaves no
        // row, so the resulting tag set is empty.
        let old_tags = task_tags(&transaction, id)?;
        self.authz.check_write(&old_tags, &[])?;
        let task = require_active_task(&transaction, id)?;
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
        event_with_status(
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
            Some(&task.status),
            None,
        )?;
        transaction.execute("DELETE FROM tasks WHERE id=?", [id])?;
        // The only write whose documents appear AFTER its audit event:
        // search_tasks_ad recreates every document tied to the task, so the
        // embedding append_board_event just did is gone. Embed once more,
        // still inside the transaction.
        crate::search::embed_missing(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn patch_metadata(&mut self, id: &str, patch: Value, actor: &str) -> Result<Task> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: a metadata patch
        // does not retag, so old and resulting tag sets are the row's.
        let old_tags = task_tags(&transaction, id)?;
        self.authz.check_write(&old_tags, &old_tags)?;
        let current = require_active_task(&transaction, id)?;
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
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: a retag is
        // permitted only to a caller who could see the row before AND after,
        // so the old and resulting tag sets are both checked at `write`.
        let old_tags = task_tags(&transaction, id)?;
        let resulting_tags = input.tags.clone().unwrap_or_else(|| old_tags.clone());
        self.authz.check_write(&old_tags, &resulting_tags)?;
        let current = require_active_task(&transaction, id)?;
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
        let dependencies_replaced = input.dependencies.is_some();
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
        // Either change can build a gate nothing can satisfy: a new
        // prerequisite, or a new parent whose gates this row now inherits.
        // Validated once, after both are written, so it sees the shape the
        // caller asked for rather than a half of it.
        if dependencies_replaced || parent != previous_parent {
            require_no_gate_deadlock(&transaction, id)?;
        }
        if let Some(tags) = &input.tags {
            set_tags(&transaction, id, tags)?;
        }
        // Read before the replace, so `allowedModels` joins the changed list
        // only when the SET actually moved: `--allowed-model Astra` on a row
        // already restricted to Astra changed nothing and must not say it did.
        let previous_models = allowed_models_of(&transaction, id)?;
        if let Some(models) = &input.allowed_models {
            set_allowed_models(&transaction, id, models)?;
        }
        let models_moved = input.allowed_models.is_some()
            && allowed_models_of(&transaction, id)? != previous_models;
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
            ("allowedModels", models_moved),
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
        if let Some(sprint) = input.sprint {
            let old: Option<String> = transaction
                .query_row(
                    "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            let moved = if let Some(sprint) = sprint.as_deref() {
                require_attachable_sprint_on(&transaction, sprint)?;
                let moved = attach_scope_on(&transaction, id, sprint, &actor, now_ms())?;
                for change in &moved {
                    let tags = task_tags(&transaction, &change.task_id)?;
                    self.authz.check_write(&tags, &tags)?;
                }
                moved
            } else {
                let Some(old_sprint_id) = old else {
                    bail!("task {id} is not attached to a sprint; there is nothing to clear");
                };
                transaction.execute("DELETE FROM task_sprints WHERE task_id=?", [id])?;
                vec![SprintMove {
                    task_id: id.to_owned(),
                    old_sprint_id: Some(old_sprint_id),
                }]
            };
            for change in &moved {
                event(
                    &transaction,
                    Some(&change.task_id),
                    "task_sprint_changed",
                    Some(&actor),
                    json!({"oldSprintID":change.old_sprint_id, "newSprintID":sprint}),
                )?;
            }
        }
        transaction.commit()?;
        self.require_task(id)
    }

    pub fn claim(&mut self, id: Option<&str>, options: ClaimOptions) -> Result<ClaimReceipt> {
        let agent = nonempty(&options.agent_id, "agent id")?.to_owned();
        if options.lease_ms < 1000 {
            bail!("lease must be at least 1000ms");
        }
        // Before the write lock and before any row is chosen: a malformed
        // `--model` is refused by name rather than silently matching nothing
        // in an allow-list and reading as "this model may not claim it".
        if let Some(model) = &options.model {
            crate::model::validate_model_name(model)?;
        }
        let transaction = self.begin_write()?;
        // Claims carry no tags of their own: board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let now = now_ms();
        expire_claims(&transaction, now)?;
        // Resolved under the write lock, so the filter the pool is bounded
        // by and the override the ledger records are one decision — the
        // boundary exists or it does not, for the pool and the payload alike
        // (ADR-045 §3).
        let (sprint_filter, sprint_recorded) =
            resolve_claim_sprint(&transaction, options.sprint_override.as_deref())?;
        let task = if let Some(id) = id {
            let tags = task_tags(&transaction, id)?;
            self.authz.check_write(&tags, &tags)?;
            let task = require_active_task(&transaction, id)?;
            if let Some(required_sprint) = sprint_filter.as_deref() {
                let attached: Option<String> = transaction
                    .query_row(
                        "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                        [id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if attached.as_deref() != Some(required_sprint) {
                    bail!(
                        "task {id} is not in sprint {required_sprint}; use --sprint with its sprint or --any-sprint to cross the current boundary explicitly"
                    );
                }
            }
            task
        } else {
            let candidates = eligible_claim_candidates(
                &transaction,
                &agent,
                &options,
                sprint_filter.as_deref(),
            )?;
            let mut selected = None;
            for candidate in candidates {
                let tags = task_tags(&transaction, &candidate.id)?;
                if self.authz.check_write(&tags, &tags).is_ok() {
                    selected = Some(candidate);
                    break;
                }
            }
            selected.context("no claimable task")?
        };
        require_claimable_type(&task.id, &task.task_type)?;
        require_no_draft_ancestor(&transaction, &task.id)?;
        if !["todo", "in_progress"].contains(&task.status.as_str()) {
            bail!("task {} is {}, not claimable", task.id, task.status);
        }
        require_no_blocking_gates(&transaction, &task.id, GateCaller::Unleased)?;
        if let Some(held) = active_claim(&transaction, &task.id, now)? {
            // Name the holder and the way out. `claim` has no --force, and
            // --allow-reassign only filters `claim --candidates`, so a caller
            // whose agent died still holding the lease has no route from here
            // and reasonably concludes there is none. There is: --force on
            // `task move` overrides a live lease, and the move is audited.
            bail!(
                "task {} is already claimed by {} until {} (epoch ms) — `claim` has no --force, and --allow-reassign only filters `claim --candidates`. To take it from a holder that is gone: `task move {} todo --as ACTOR --force`, then claim it again",
                task.id,
                held.agent_id,
                held.expires_at,
                task.id
            );
        }
        if task.driver_only && options.caller_scope.as_deref() != Some("driver") {
            bail!("task {} is driver-only", task.id);
        }
        // Right after driver-only, so the refusal order a caller learns stays
        // type → draft → status → gates → held → driver-only → MODEL →
        // assignee. Read from the table rather than from `task`, because the
        // named-id path builds its row without the list attached.
        require_allowed_model(
            &task.id,
            &allowed_models_of(&transaction, &task.id)?,
            options.model.as_deref(),
        )?;
        if task.assignee.as_ref().is_some_and(|value| value != &agent) && !options.allow_reassign {
            bail!("task {} is assigned to {}", task.id, task.assignee.unwrap());
        }
        let token = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at,worktree,worktree_kind,branch,head_sha,root_head,model) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                task.id,agent,options.session_id,token,now,now,now+options.lease_ms,
                options.git.as_ref().map(|g| g.worktree.clone()),
                options.git.as_ref().map(|g| g.worktree_kind.to_owned()),
                options.git.as_ref().and_then(|g| g.branch.clone()),
                options.git.as_ref().map(|g| g.head.clone()),
                options.git.as_ref().and_then(|g| g.root_head.clone()),
                options.model,
            ],
        )?;
        transaction.execute(
            "UPDATE tasks SET status='in_progress',assignee=?,updated_at=? WHERE id=?",
            params![agent, now, task.id],
        )?;
        event_with_status(
            &transaction,
            Some(&task.id),
            "task_claimed",
            Some(&agent),
            // `sprintOverride` rides along only when the caller crossed a
            // boundary on purpose; a default-scoped claim keeps the payload
            // byte-identical to a board without sprints (ADR-045 §3). `model`
            // is conditional for the same reason: a claim that declared none
            // records the same payload it always did.
            {
                let mut payload = json!({"expiresAt": now+options.lease_ms});
                if let Some(recorded) = &sprint_recorded {
                    payload["sprintOverride"] = json!(recorded);
                }
                if let Some(model) = &options.model {
                    payload["model"] = json!(model);
                }
                payload
            },
            Some(&task.status),
            Some("in_progress"),
        )?;
        let result = active_claim(&transaction, &task.id, now)?.context("claim was not created")?;
        let orphaned_from = orphaned_from(&transaction, &task.id)?;
        transaction.commit()?;
        Ok(ClaimReceipt {
            claim: result,
            rules: Vec::new(),
            orphaned_from,
        })
    }

    /// Inspect the tasks the atomic `claim --next` scheduler could choose.
    /// This method accepts `&self` and is reached through `open_readonly`, so
    /// it cannot expire leases, append events, or change task state.
    pub fn claim_candidates(
        &self,
        options: &ClaimOptions,
        tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Task>> {
        let agent = nonempty(&options.agent_id, "agent id")?;
        self.authz.check_read(&[])?;
        if let Some(model) = &options.model {
            crate::model::validate_model_name(model)?;
        }
        let tag = tag
            .map(|value| validate_registered_tags(&self.connection, &[value.to_owned()], "claim"))
            .transpose()?
            .and_then(|mut values| values.pop());
        let (sprint_filter, _) =
            resolve_claim_sprint(&self.connection, options.sprint_override.as_deref())?;
        let mut candidates =
            eligible_claim_candidates(&self.connection, agent, options, sprint_filter.as_deref())?;
        attach_tags(&self.connection, candidates.iter_mut())?;
        attach_allowed_models(&self.connection, candidates.iter_mut())?;
        candidates.retain(|candidate| self.authz.permits_read(&candidate.tags));
        if let Some(tag) = tag {
            candidates.retain(|candidate| candidate.tags.contains(&tag));
        }
        candidates.truncate(limit);
        Ok(candidates)
    }

    pub fn get_claim(&self, id: &str) -> Result<Option<Claim>> {
        self.authz.check_read(&[])?;
        active_claim(&self.connection, id, now_ms())
    }

    pub fn heartbeat(
        &mut self,
        id: &str,
        token: &str,
        lease_ms: i64,
        git: Option<&crate::gitctx::GitContext>,
    ) -> Result<Claim> {
        if lease_ms < 1000 {
            bail!("lease must be at least 1000ms");
        }
        let transaction = self.begin_write()?;
        // Board scope, under the lock: a heartbeat moves a claim row.
        self.authz.check_write(&[], &[])?;
        let now = now_ms();
        let claim = require_lease(&transaction, id, token, now)?;
        // A live lease is never revoked by a gate, but renewing one asserts
        // the work is still going: a prerequisite introduced or reopened
        // underneath the holder stops the renewal here, and the holder's way
        // out is `checkpoint --state blocked` or `release`, both of which stay
        // open.
        require_no_blocking_gates(&transaction, id, GateCaller::Holder)?;
        match git {
            Some(git) => transaction.execute(
                "UPDATE task_claims SET heartbeat_at=?,expires_at=?,worktree=?,worktree_kind=?,branch=?,head_sha=?,root_head=? WHERE task_id=? AND lease_token=?",
                params![
                    now,
                    now + lease_ms,
                    git.worktree,
                    git.worktree_kind,
                    git.branch,
                    git.head,
                    git.root_head,
                    id,
                    token
                ],
            )?,
            None => transaction.execute(
                "UPDATE task_claims SET heartbeat_at=?,expires_at=? WHERE task_id=? AND lease_token=?",
                params![now, now + lease_ms, id, token],
            )?,
        };
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
        let transaction = self.begin_write()?;
        // Board scope, under the lock: a release drops a claim row.
        self.authz.check_write(&[], &[])?;
        let claim = require_lease(&transaction, id, token, now_ms())?;
        transaction.execute("DELETE FROM task_claims WHERE task_id=?", [id])?;
        if !keep_status {
            transaction.execute(
                "UPDATE tasks SET status='todo',updated_at=? WHERE id=? AND status='in_progress'",
                params![now_ms(), id],
            )?;
        }
        let current_status = if keep_status {
            require_task(&transaction, id)?.status
        } else {
            "todo".to_owned()
        };
        event_with_status(
            &transaction,
            Some(id),
            "claim_released",
            Some(&claim.agent_id),
            json!({}),
            Some("in_progress"),
            Some(&current_status),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A note and its ledger event, atomically.
    ///
    /// The scope is not decoration. Until ADR-041 §3.3 this method took no
    /// transaction at all: the `INSERT` and the `append_board_event` beneath
    /// it each ran in their own autocommit, so a failure between them left a
    /// note the ledger has no record of — the one write path on the board
    /// that was not atomic even with itself.
    pub fn add_note(&mut self, id: &str, author: &str, kind: &str, body: &str) -> Result<TaskNote> {
        validate(kind, &NOTE_KINDS, "note kind")?;
        let transaction = self.begin_write()?;
        // Notes carry no tags of their own: board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        require_active_task(&transaction, id)?;
        let now = now_ms();
        transaction.execute(
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
            &transaction,
            Some(id),
            "note_added",
            Some(author),
            json!({"kind":kind}),
        )?;
        transaction.commit()?;
        self.notes(id, 1)?.pop().context("note was not created")
    }

    pub fn notes(&self, id: &str, limit: i64) -> Result<Vec<TaskNote>> {
        self.authz.check_read(&[])?;
        let task = require_task(&self.connection, id)?;
        let cold = if task.archived { "" } else { " AND archived=0" };
        let sql = format!(
            "SELECT * FROM (SELECT * FROM task_notes WHERE task_id=?{cold} ORDER BY seq DESC LIMIT ?) ORDER BY seq ASC"
        );
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(params![id, limit], note_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn checkpoints(&self, id: &str, limit: i64) -> Result<Vec<Checkpoint>> {
        self.authz.check_read(&[])?;
        let task = require_task(&self.connection, id)?;
        let cold = if task.archived { "" } else { " AND archived=0" };
        let sql = format!(
            "SELECT * FROM (SELECT * FROM checkpoints WHERE task_id=?{cold} ORDER BY seq DESC LIMIT ?) ORDER BY seq ASC"
        );
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(params![id, limit], checkpoint_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn checkpoint(&mut self, input: CheckpointInput) -> Result<Checkpoint> {
        validate(&input.state, &CHECKPOINT_STATES, "checkpoint state")?;
        let transaction = self.begin_write()?;
        // Checkpoints carry no tags of their own: board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let now = now_ms();
        let claim = require_lease(&transaction, &input.task_id, &input.lease_token, now)?;
        let prior_status = require_task(&transaction, &input.task_id)?.status;
        if claim.agent_id != input.author {
            bail!("lease belongs to {}, not {}", claim.agent_id, input.author);
        }
        // `continue` and `done` both claim work happened; `blocked` is the
        // holder saying it did not, which is exactly the route out of a
        // prerequisite that appeared or reopened mid-lease, so it stays open.
        if input.state == "blocked" {
            require_owner_has_a_card(
                &transaction,
                &input.task_id,
                input
                    .blockers
                    .iter()
                    .map(String::as_str)
                    .chain(std::iter::once(input.next_action.as_str())),
            )?;
        } else {
            require_no_blocking_gates(&transaction, &input.task_id, GateCaller::Holder)?;
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
        event_with_status(
            &transaction,
            Some(&input.task_id),
            "checkpoint_added",
            Some(&input.author),
            json!({"seq":seq,"state":input.state}),
            Some(&prior_status),
            Some(status),
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
        let transaction = self.begin_write()?;
        // The master file has no tags of its own: board scope, under the lock.
        self.authz.check_write(&[], &[])?;
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
        self.authz.check_read(&[])?;
        let mut statement = self.connection.prepare(
            "SELECT t.name,t.description,t.created_by,t.created_at,t.renamed_from,t.renamed_at,
                    ((SELECT count(*) FROM task_tags x WHERE x.tag=t.name) +
                     (SELECT count(*) FROM attention_tags x WHERE x.tag=t.name)) AS uses
             FROM tags t ORDER BY t.name",
        )?;
        statement
            .query_map([], |row| {
                Ok(Tag {
                    name: row.get("name")?,
                    description: row.get("description")?,
                    created_by: row.get("created_by")?,
                    created_at: row.get("created_at")?,
                    renamed_from: row.get("renamed_from")?,
                    renamed_at: row.get("renamed_at")?,
                    uses: row.get("uses")?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Move every use of one registered tag onto a new name, in one write.
    ///
    /// A rename is not an add plus a remove: `remove_tag` strips rows, and a
    /// two-command spelling would leave a window in which the tag exists under
    /// neither name and every filter misses the rows. So the master row, every
    /// `task_tags` row (archived ones included — history that kept the old
    /// spelling would be readable and unfindable), and every `attention_tags`
    /// row move together inside one transaction, under the mutation lock.
    ///
    /// The new row is inserted before the children move and the old row is
    /// deleted after, because both child tables carry a foreign key to
    /// `tags(name)` with no `ON UPDATE` action: renaming the parent in place
    /// would orphan them mid-statement.
    ///
    /// **The registry is not in this transaction.** Rule tags live in a
    /// separate SQLite database with its own hash chain, and two connections
    /// cannot share one transaction, so the rule rewrite is a second
    /// transaction the caller runs strictly *after* this one commits
    /// (`Registry::rename_rule_tag`, driven from the `tag rename` dispatch in
    /// `rust/lib.rs`). If that second transaction fails, the board is renamed
    /// and the rules still carry the old spelling — which narrows those rules
    /// rather than widening them, and the refusal names
    /// `kanban rule update ID --tag NEW` as the repair. The reverse order was
    /// rejected because a failed board rename would then leave rules pointing
    /// at a tag no board has.
    pub fn rename_tag(&mut self, old: &str, new: &str, actor: Option<&str>) -> Result<TagRename> {
        let new = validate_tag_name(new)?;
        let transaction = self.begin_write()?;
        // The master file has no tags of its own: board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let known: Option<String> = transaction
            .query_row("SELECT name FROM tags WHERE name=?", [old], |row| {
                row.get(0)
            })
            .optional()?;
        if known.is_none() {
            bail!(
                "tag {old} is not in this board's master file, so there is nothing to rename — \
                 `kanban tag list` names the ones that are"
            );
        }
        let taken: Option<String> = transaction
            .query_row("SELECT name FROM tags WHERE name=?", [&new], |row| {
                row.get(0)
            })
            .optional()?;
        if taken.is_some() {
            bail!(
                "tag {new} is already in the master file and a rename does not merge two tags \
                 into one — pick a free name, or retire one of them with `kanban tag remove` first"
            );
        }
        let now = now_ms();
        let (tasks, archived_tasks): (i64, i64) = transaction.query_row(
            "SELECT coalesce(sum(archived=0),0),coalesce(sum(archived=1),0) \
             FROM task_tags WHERE tag=?",
            [old],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        transaction.execute(
            "INSERT INTO tags(name,description,created_by,created_at,renamed_from,renamed_at) \
             SELECT ?,description,created_by,created_at,?,? FROM tags WHERE name=?",
            params![new, old, now, old],
        )?;
        transaction.execute("UPDATE task_tags SET tag=? WHERE tag=?", params![new, old])?;
        let attention = i64::try_from(transaction.execute(
            "UPDATE attention_tags SET tag=? WHERE tag=?",
            params![new, old],
        )?)?;
        transaction.execute("DELETE FROM tags WHERE name=?", [old])?;
        event(
            &transaction,
            None,
            "tag_renamed",
            actor,
            json!({
                "old": old,
                "new": new,
                "tasks": tasks,
                "archivedTasks": archived_tasks,
                "attention": attention,
            }),
        )?;
        transaction.commit()?;
        Ok(TagRename {
            old: old.to_owned(),
            new,
            tasks,
            archived_tasks,
            attention,
            // Filled in by the caller, after this transaction has landed.
            rules: 0,
        })
    }

    /// Retire a tag. One still in use needs `--force`, and says how many rows.
    pub fn remove_tag(&mut self, name: &str, actor: Option<&str>, force: bool) -> Result<()> {
        let transaction = self.begin_write()?;
        // Board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let rule_uses: i64 = transaction.query_row(
            "SELECT count(*) FROM rules r, json_each(r.task_tags) j \
             WHERE r.archived=0 AND j.value=?",
            [name],
            |row| row.get(0),
        )?;
        if rule_uses > 0 {
            bail!(
                "tag {name} scopes {rule_uses} active rule{}; update or retire those rules before \
                 removing the master entry, because stripping it would silently widen their scope",
                if rule_uses == 1 { "" } else { "s" }
            );
        }
        let uses: i64 = transaction.query_row(
            "SELECT (SELECT count(*) FROM task_tags WHERE tag=?) +
                    (SELECT count(*) FROM attention_tags WHERE tag=?)",
            params![name, name],
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
        transaction.execute("DELETE FROM attention_tags WHERE tag=?", [name])?;
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

    /// Active rules are a document, so their order is oldest first.
    pub fn rules(&self, include_archived: bool) -> Result<Vec<Rule>> {
        self.authz.check_read(&[])?;
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

    /// Retire a rule from the active set without erasing it.
    pub fn retire_rule(&mut self, id: &str, actor: &str) -> Result<Rule> {
        let actor = validate_rule_actor(actor)?.to_owned();
        let transaction = self.begin_write()?;
        // Board scope, under the lock.
        self.authz.check_write(&[], &[])?;
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
    /// Provenance is refused rather than stored blank, the same rule the CLI
    /// applies to checkpoints and handoffs — an update that says "tests green"
    /// without saying which checkout is a claim nobody can check. `provenance`
    /// is `None` only for the dashboard fixtures that seed a sitrep with no
    /// checkout at all.
    pub fn post_sitrep(
        &mut self,
        lane: &str,
        body: &str,
        author: &str,
        task_id: Option<&str>,
        provenance: Option<&crate::Provenance>,
    ) -> Result<Sitrep> {
        let lane = nonempty(lane, "lane")?.to_owned();
        let body = nonempty(body, "sitrep body")?.to_owned();
        let author = nonempty(author, "author")?.to_owned();
        let transaction = self.begin_write()?;
        if let Some(id) = task_id {
            require_active_task(&transaction, id)?;
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
                provenance.map(|p| p.repo_path.clone()),
                provenance.map(|p| p.branch.clone()),
                provenance.map(|p| p.head_sha.clone()),
                provenance.as_ref().and_then(|p| p.root_head.clone()),
                provenance.map(|p| p.dirty_summary.clone()),
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
    ///
    /// Board scope is the whole check: a sitrep carries no tags, so there is
    /// no per-row filter to apply and `&[]` is the entire subject. The guard
    /// lives here rather than at each caller because `serve::lane_groups`
    /// reaches this method with a store it opened for a board the caller may
    /// never have been granted (`t-c84850a1`); with the check at the source
    /// both the CLI and `/api/v1/lanes` are refused by construction.
    pub fn sitreps(
        &self,
        lane: Option<&str>,
        include_archived: bool,
        task: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Sitrep>> {
        self.authz.check_read(&[])?;
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

    #[allow(clippy::too_many_arguments)]
    pub fn raise_attention(
        &mut self,
        body: &str,
        kind: &str,
        raised_by: &str,
        task_id: Option<&str>,
        priority: i64,
        tags: &[String],
        card: &DecisionCard,
    ) -> Result<Attention> {
        validate(kind, &ATTENTION_KINDS, "attention kind")?;
        validate_priority(Some(priority))?;
        let body = nonempty(body, "attention body")?.to_owned();
        let raised_by = nonempty(raised_by, "raised by")?.to_owned();
        // Before the write lock: a malformed card is refused without taking
        // it, and the refusal is the same one every other surface reads.
        card.validate()?;
        let transaction = self.begin_write()?;
        // A new attention row has no old tag set; the resulting set is the
        // tags the caller asked for. Under the mutation lock.
        self.authz.check_write(&[], tags)?;
        if let Some(id) = task_id {
            require_active_task(&transaction, id)?;
        }
        let now = now_ms();
        let id = format!("a-{}", &Uuid::new_v4().simple().to_string()[..8]);
        transaction.execute(
            "INSERT INTO attention(id,task_id,kind,body,raised_by,created_at,status,resolved_at,resolved_by,resolution,priority,question,context,choices,decision) VALUES(?,?,?,?,?,?,'open',NULL,NULL,NULL,?,?,?,?,NULL)",
            params![
                id,
                task_id,
                kind,
                body,
                raised_by,
                now,
                priority,
                card.question,
                card.context,
                card.choices_json()
            ],
        )?;
        set_attention_tags(&transaction, &id, tags)?;
        event(
            &transaction,
            task_id,
            "attention_raised",
            Some(&raised_by),
            json!({"attentionID": id, "kind": kind, "priority": priority, "priorityLevel": priority_level(priority), "tags": tags, "choices": card.choices}),
        )?;
        let result =
            transaction.query_row("SELECT * FROM attention WHERE id=?", [&id], attention_row)?;
        transaction.commit()?;
        let mut result = vec![result];
        attach_attention_tags(&self.connection, &mut result)?;
        Ok(result.remove(0))
    }

    /// Open items first, then priority and age within each state.
    ///
    /// An unanswered question does not get less urgent by being ignored, so
    /// age breaks priority ties oldest-first. Explicit priority comes first:
    /// a new P0 must not sit behind an old routine P2.
    ///
    /// `lane` is the kb-att rule: a row belongs to a lane when its raiser is
    /// `<anything>@<lane>` or when the task it is about carries that lane.
    /// Both routes are one SQL clause, so `limit` bounds the lane's rows and
    /// not a page that was filtered after the fact.
    ///
    /// A row whose tags this caller may not read is not in the answer, on the
    /// same all-of-tag test a named read is refused with, and exactly as
    /// [`Store::list_tasks`] filters. The filtering is here rather than in
    /// each caller because every attention listing in the product — `att
    /// list`, `/api/v1/needs-you`, a task's open items, the hover preview,
    /// the mounted deck and MCP — reads through this one function, and a
    /// question, its body and its choices disclose what they are about.
    #[allow(clippy::too_many_arguments)]
    pub fn attention(
        &self,
        status: Option<&str>,
        kind: Option<&str>,
        task: Option<&str>,
        tag: Option<&str>,
        lane: Option<&str>,
        limit: i64,
        include_archived: bool,
    ) -> Result<Vec<Attention>> {
        if let Some(value) = status {
            validate(value, &ATTENTION_STATUSES, "attention status")?;
        }
        if let Some(value) = kind {
            validate(value, &ATTENTION_KINDS, "attention kind")?;
        }
        self.authz.check_read(&[])?;
        if let Some(id) = task {
            require_task(&self.connection, id)?;
        }
        let (where_clause, mut values) =
            self.attention_filter(status, kind, task, tag, lane, include_archived)?;
        // Under managed enforcement the bound is applied to the rows this
        // caller may READ, not to the raw rows: a `LIMIT` bound in SQL runs
        // before the tag test, so a denied row would consume a slot and the
        // page would come back short with nothing to say it had. Every
        // caller's over-fetch arithmetic depends on that — the `+1` probes
        // that detect a next page, `bounded_page`'s `len > limit`, the
        // preview's find over a bounded read, and the dashboard's
        // `limit = 1` urgency read, which a denied row would empty. Outside
        // managed enforcement nothing can be denied, so the direct estate
        // keeps the SQL bound and reads exactly what it asked for.
        let enforcing = self.authz.is_enforcing();
        let bound = if enforcing {
            String::new()
        } else {
            values.push(Box::new(limit));
            " LIMIT ?".to_owned()
        };
        let sql = format!(
            "SELECT * FROM attention{where_clause} ORDER BY status='resolved',priority ASC,created_at ASC,id ASC{bound}"
        );
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement
            .query_map(params_from_iter(refs), attention_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        attach_attention_tags(&self.connection, &mut rows)?;
        if enforcing {
            rows.retain(|row| self.authz.permits_read(&row.tags));
            // A negative bound is SQLite's "no bound", and stays one here.
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        Ok(rows)
    }

    /// How many attention rows nobody has settled: the dashboard's number,
    /// counted rather than fetched, so a board past the listing's page still
    /// reports what it holds. Same filter as `attention(Some("open"), …)`.
    ///
    /// A `COUNT(*)` cannot see tags, and a count taken over rows this caller
    /// may not see reports their existence as surely as printing their
    /// titles would. So when enforcement can deny, the number is taken over
    /// the same rows the listing would hand back — fetched, tagged and
    /// filtered on the one read test. Outside managed enforcement
    /// `permits_read` cannot refuse anything, and the direct estate pays
    /// nothing for a guard that cannot deny: it keeps the SQL count.
    pub fn count_open_attention(&self) -> Result<i64> {
        self.authz.check_read(&[])?;
        let (where_clause, values) =
            self.attention_filter(Some("open"), None, None, None, None, false)?;
        if !self.authz.is_enforcing() {
            let refs = values.iter().map(|value| value.as_ref());
            return self
                .connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM attention{where_clause}"),
                    params_from_iter(refs),
                    |row| row.get(0),
                )
                .map_err(Into::into);
        }
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self
            .connection
            .prepare(&format!("SELECT * FROM attention{where_clause}"))?;
        let mut rows = statement
            .query_map(params_from_iter(refs), attention_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        attach_attention_tags(&self.connection, &mut rows)?;
        rows.retain(|row| self.authz.permits_read(&row.tags));
        Ok(i64::try_from(rows.len()).unwrap_or(i64::MAX))
    }

    /// The newest resolved attention rows, decided-first — the web's Recent
    /// decisions view.
    ///
    /// `attention(Some("resolved"), …)` orders by priority then raised time,
    /// which is the right order for a queue and the wrong end for "what was
    /// decided lately": a board with more resolved rows than the caller's
    /// bound would hand back its oldest decisions. This is the one read that
    /// orders by `resolved_at DESC`.
    ///
    /// A row whose tags this caller may not read is not in the answer, on the
    /// same all-of-tag test a named read is refused with: a decision that was
    /// taken about a row this caller may not see is still about that row. The
    /// bound is the READABLE rows', for the reason [`Store::attention`] gives
    /// — the decisions room over-fetches per board and merges, and a denied
    /// row spending a scan slot would drop a readable decision off the end.
    pub fn recent_resolved_attention(&self, limit: i64) -> Result<Vec<Attention>> {
        self.authz.check_read(&[])?;
        let enforcing = self.authz.is_enforcing();
        let sql = format!(
            "SELECT * FROM attention WHERE status='resolved' AND archived=0 \
             ORDER BY resolved_at DESC,id ASC{}",
            if enforcing { "" } else { " LIMIT ?" }
        );
        let mut statement = self.connection.prepare(&sql)?;
        let bound: &[&dyn rusqlite::ToSql] = if enforcing { &[] } else { &[&limit] };
        let mut rows = statement
            .query_map(bound, attention_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        attach_attention_tags(&self.connection, &mut rows)?;
        if enforcing {
            rows.retain(|row| self.authz.permits_read(&row.tags));
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        Ok(rows)
    }

    /// How many workable rows are waiting on a prerequisite: the dashboard's
    /// number, and a count rather than a filter.
    ///
    /// The status totals beside it stay raw status counts — a `todo` row that
    /// is gated is still `todo`, and quietly renaming or subtracting it would
    /// make the board's own arithmetic stop adding up. This is the separate
    /// number that says how much of the queue nobody can start.
    ///
    /// Tasks only, because a task is the only row a lease is ever granted on,
    /// and `done`/`cancelled` are excluded because a settled row is not
    /// waiting for anything. Archived cold history is out for the same reason
    /// every other dashboard number leaves it out.
    pub fn count_gated_tasks(&self) -> Result<i64> {
        self.authz.check_read(&[])?;
        self.connection
            .query_row(
                "WITH RECURSIVE gated(id) AS (\
                     SELECT dependency.task_id FROM task_dependencies dependency \
                     JOIN tasks prerequisite ON prerequisite.id=dependency.depends_on \
                     WHERE prerequisite.status<>'done' \
                     UNION \
                     SELECT child.id FROM gated JOIN tasks child ON child.parent_id=gated.id\
                 ) \
                 SELECT COUNT(*) FROM tasks t JOIN gated ON gated.id=t.id \
                 WHERE t.archived=0 AND t.type=? AND t.status NOT IN ('done','cancelled')",
                [CLAIMABLE_TYPE],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// The listing's WHERE clause and its bound values, shared with the count
    /// so the two cannot disagree about which rows are open.
    fn attention_filter(
        &self,
        status: Option<&str>,
        kind: Option<&str>,
        task: Option<&str>,
        tag: Option<&str>,
        lane: Option<&str>,
        include_archived: bool,
    ) -> Result<(String, Vec<Box<dyn rusqlite::ToSql>>)> {
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !include_archived {
            clauses.push("archived=0");
        }
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
        if let Some(lane) = lane {
            // `substr` with a negative start counts from the end, so this is
            // an exact suffix test that no `%` or `_` in the lane can widen the
            // way LIKE would. The suffix is bound twice because every other
            // placeholder in this statement is positional.
            clauses.push(
                "(substr(raised_by, -length(?)) = ? \
                 OR task_id IN (SELECT id FROM tasks WHERE lane=?))",
            );
            let lane = lane_filter(lane)?;
            values.push(Box::new(format!("@{lane}")));
            values.push(Box::new(format!("@{lane}")));
            values.push(Box::new(lane.to_owned()));
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
                let names = self
                    .tags()?
                    .into_iter()
                    .map(|item| item.name)
                    .collect::<Vec<_>>();
                let borrowed = names.iter().map(String::as_str).collect::<Vec<_>>();
                let suggestion = crate::nearest(&tag, &borrowed)
                    .map(|near| format!(", did you mean {near}?"))
                    .unwrap_or_default();
                bail!(
                    "tag {tag} is not in this board's master file{suggestion} — \
                     an unregistered tag would filter to nothing and read like an answer"
                );
            }
            clauses.push("id IN (SELECT attention_id FROM attention_tags WHERE tag=?)");
            values.push(Box::new(tag));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        Ok((where_clause, values))
    }

    pub fn open_attentions(&self, task: &str) -> Result<Vec<Attention>> {
        self.attention(Some("open"), None, Some(task), None, None, 1000, false)
    }

    /// Correct an open attention row without settling it. The event retains
    /// the text, tags and card that were superseded; resolved rows are
    /// immutable.
    ///
    /// A card arrives as the halves the caller named: the question/context
    /// pair replaces the row's pair when it is given, and the choices replace
    /// the row's choices when they are given. Nothing is removed implicitly —
    /// `clear_card` is the one way back to the default pair, because
    /// question, context, choices and recommendation are one card and
    /// clearing half of it would leave the pairing rule violated
    /// (ADR-042 §4).
    #[allow(clippy::too_many_arguments)]
    pub fn update_attention(
        &mut self,
        id: &str,
        body: Option<&str>,
        tags: Option<&[String]>,
        card: Option<&DecisionCard>,
        clear_card: bool,
        actor: &str,
    ) -> Result<Attention> {
        let authored = card.is_some_and(|card| !card.is_empty());
        if body.is_none() && tags.is_none() && !authored && !clear_card {
            bail!(
                "attention update requires --body/--body-file, --tag, --clear-tags, \
                 --question and --context, --choice with --consequence and --recommend, \
                 or --clear-card"
            );
        }
        let body = body
            .map(|value| nonempty(value, "attention body"))
            .transpose()?;
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: a retag is
        // permitted only to a caller who could see the row before AND after.
        let old_tags = attention_tags(&transaction, id)?;
        let resulting_tags = tags
            .map(|tags| tags.to_vec())
            .unwrap_or_else(|| old_tags.clone());
        self.authz.check_write(&old_tags, &resulting_tags)?;
        let existing = transaction
            .query_row("SELECT * FROM attention WHERE id=?", [id], attention_row)
            .optional()?
            .with_context(|| format!("attention {id} not found"))?;
        if existing.status != "open" {
            bail!("attention {id} is resolved history; its card cannot be rewritten");
        }
        // The column, not the served row: a row that authored no choices is
        // served as the default pair, and merging that pair in would turn a
        // never-authored row into an authored one on a body-only update.
        let stored_choices: Option<String> =
            transaction.query_row("SELECT choices FROM attention WHERE id=?", [id], |row| {
                row.get(0)
            })?;
        let mut question = existing.question.clone();
        let mut context = existing.context.clone();
        let mut choices = stored_choices.clone();
        if clear_card {
            question = None;
            context = None;
            choices = None;
        }
        if let Some(card) = card {
            if card.question.is_some() {
                question = card.question.clone();
                context = card.context.clone();
            }
            if let Some(json) = card.choices_json() {
                choices = Some(json);
            }
        }
        // What the row will carry, checked by the same validator that checked
        // what the caller typed.
        DecisionCard {
            question: question.clone(),
            context: context.clone(),
            choices: match &choices {
                Some(text) => serde_json::from_str(text)?,
                None => Vec::new(),
            },
        }
        .validate()?;
        let mut previous = vec![existing.clone()];
        attach_attention_tags(&transaction, &mut previous)?;
        transaction.execute(
            "UPDATE attention SET body=?,question=?,context=?,choices=? WHERE id=?",
            params![
                body.unwrap_or(&existing.body),
                question,
                context,
                choices,
                id
            ],
        )?;
        if let Some(tags) = tags {
            set_attention_tags(&transaction, id, tags)?;
        }
        let mut changed = Vec::new();
        if body.is_some() {
            changed.push("body");
        }
        if tags.is_some() {
            changed.push("tags");
        }
        if authored || clear_card {
            changed.push("card");
        }
        event(
            &transaction,
            existing.task_id.as_deref(),
            "attention_updated",
            Some(&actor),
            json!({
                "attentionID": id,
                "changed": changed,
                "previousBody": existing.body,
                "previousTags": previous[0].tags,
                "previousQuestion": existing.question,
                "previousContext": existing.context,
                // The column as it stood: null where the row authored none,
                // so the trail says which of the two it was.
                "previousChoices": stored_choices
                    .as_deref()
                    .map(serde_json::from_str::<Value>)
                    .transpose()?,
            }),
        )?;
        let result =
            transaction.query_row("SELECT * FROM attention WHERE id=?", [id], attention_row)?;
        transaction.commit()?;
        let mut result = vec![result];
        attach_attention_tags(&self.connection, &mut result)?;
        Ok(result.remove(0))
    }

    /// Settle an item. The row stays; only its state moves.
    pub fn resolve_attention(
        &mut self,
        id: &str,
        actor: &str,
        answer: &AttentionAnswer<'_>,
    ) -> Result<Attention> {
        self.resolve_attention_with_authorization(id, actor, actor, answer, false)
    }

    /// Settle an item from the trusted web edge.
    ///
    /// The edge identity is the audit actor, and the web route is the only
    /// caller that may opt into this broader authorization model.
    pub(crate) fn resolve_attention_from_trusted_edge(
        &mut self,
        id: &str,
        actor: &str,
        answer: &AttentionAnswer<'_>,
    ) -> Result<Attention> {
        self.resolve_attention_with_authorization(id, actor, actor, answer, true)
    }

    fn resolve_attention_with_authorization(
        &mut self,
        id: &str,
        authorization_actor: &str,
        audit_actor: &str,
        answer: &AttentionAnswer<'_>,
        trusted_edge: bool,
    ) -> Result<Attention> {
        let authorization_actor = nonempty(authorization_actor, "actor")?.to_owned();
        let audit_actor = nonempty(audit_actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: settling an item
        // does not retag, so old and resulting tag sets are the row's.
        let old_tags = attention_tags(&transaction, id)?;
        self.authz.check_write(&old_tags, &old_tags)?;
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
        if !trusted_edge
            && authorization_actor != OPERATOR_ACTOR
            && authorization_actor != existing.raised_by
        {
            bail!(
                "attention {id} was raised by {}; only {OPERATOR_ACTOR} or that same raiser may resolve it — \
                 use attention update to correct it without closing George's queue",
                existing.raised_by
            );
        }
        let now = now_ms();
        // The composer lives here and nowhere else, so no caller can produce
        // a different trail and `resolution` is derived rather than passed
        // (ADR-042 §3). `existing.choices` is the served card, so a row that
        // authored none is answered through the default pair.
        let (decision, resolution) = answer.decide(id, &existing.choices, &audit_actor, now)?;
        let decision_json = serde_json::to_string(&decision)?;
        transaction.execute(
            "UPDATE attention SET status='resolved',resolved_at=?,resolved_by=?,resolution=?,\
             decision=?,reopened_at=NULL,reopened_by=NULL,reopen_note=NULL WHERE id=?",
            params![now, audit_actor, resolution, decision_json, id],
        )?;
        // `tags` is the row's tag set AT SETTLEMENT, under the same key the
        // raise uses, because `event_tags` reads it as this envelope's own
        // strictest evidence: after an allowed reopen-and-retag the live row
        // no longer carries the tag that hid it, and without this snapshot
        // the historical envelope would fall back to the TASK's tags and
        // reach a caller who could not see the row when it was settled
        // (`t-1de9c707`). The snapshot begins at this change; envelopes
        // written before it fall back to the live row's tags, which is
        // history and accepted as such.
        event(
            &transaction,
            existing.task_id.as_deref(),
            "attention_resolved",
            Some(&audit_actor),
            json!({
                "attentionID": id,
                "kind": existing.kind,
                "tags": old_tags,
                "decision": decision,
                "previousResolvedAt": existing.resolved_at,
                "previousResolvedBy": existing.resolved_by,
                "previousResolution": existing.resolution,
                "previousDecision": existing.decision,
                "reopenedAt": existing.reopened_at,
                "reopenedBy": existing.reopened_by,
                "reopenNote": existing.reopen_note,
            }),
        )?;
        let result =
            transaction.query_row("SELECT * FROM attention WHERE id=?", [id], attention_row)?;
        transaction.commit()?;
        let mut result = vec![result];
        attach_attention_tags(&self.connection, &mut result)?;
        Ok(result.remove(0))
    }

    /// Undo a mistaken resolution without erasing who made it or what they
    /// wrote. Only the operator or that resolver can repair the transition.
    pub fn reopen_attention(&mut self, id: &str, actor: &str, note: &str) -> Result<Attention> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let note = nonempty(note, "reopen note")?.to_owned();
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: reopening does
        // not retag, so old and resulting tag sets are the row's.
        let old_tags = attention_tags(&transaction, id)?;
        self.authz.check_write(&old_tags, &old_tags)?;
        let existing = transaction
            .query_row("SELECT * FROM attention WHERE id=?", [id], attention_row)
            .optional()?
            .with_context(|| format!("attention {id} not found"))?;
        if existing.status != "resolved" {
            bail!("attention {id} is already open; there is no resolution to reopen");
        }
        if actor != OPERATOR_ACTOR && existing.resolved_by.as_deref() != Some(actor.as_str()) {
            bail!(
                "attention {id} was resolved by {}; only {OPERATOR_ACTOR} or that resolver may reopen it",
                existing.resolved_by.as_deref().unwrap_or("someone")
            );
        }
        let now = now_ms();
        // The decision leaves the ROW and stays in the LEDGER: the row's
        // contract is what is true now, and an open item has no decision.
        // The event below is the hash-chained copy, so nothing is lost
        // (ADR-042 §3). `resolved_at`, `resolved_by` and `resolution` stay
        // where BOARD_V17's CHECK requires them.
        transaction.execute(
            "UPDATE attention SET status='open',reopened_at=?,reopened_by=?,reopen_note=?,\
             decision=NULL WHERE id=?",
            params![now, actor, note, id],
        )?;
        // `tags` as at the reopen, on the same key and for the same reason
        // the resolve above records it: the retag that a reopened row then
        // permits must not loosen the envelope that recorded the row before
        // it (`t-1de9c707`). The snapshot begins at this change.
        event(
            &transaction,
            existing.task_id.as_deref(),
            "attention_reopened",
            Some(&actor),
            json!({
                "attentionID": id,
                "tags": old_tags,
                "resolvedAt": existing.resolved_at,
                "resolvedBy": existing.resolved_by,
                "resolution": existing.resolution,
                "decision": existing.decision,
                "note": note,
            }),
        )?;
        let result =
            transaction.query_row("SELECT * FROM attention WHERE id=?", [id], attention_row)?;
        transaction.commit()?;
        let mut result = vec![result];
        attach_attention_tags(&self.connection, &mut result)?;
        Ok(result.remove(0))
    }

    pub fn handoffs(
        &self,
        task: Option<&str>,
        status: Option<&str>,
        to_agent: Option<&str>,
        limit: i64,
        include_archived: bool,
    ) -> Result<Vec<Handoff>> {
        if let Some(value) = status {
            validate(value, &HANDOFF_STATUSES, "handoff status")?;
        }
        self.authz.check_read(&[])?;
        if let Some(id) = task {
            require_task(&self.connection, id)?;
        }
        let (where_clause, mut values) = handoff_filter(task, status, to_agent, include_archived);
        values.push(Box::new(limit));
        let sql = format!(
            "SELECT * FROM handoffs{where_clause} ORDER BY status!='pending',priority ASC,created_at ASC,id ASC LIMIT ?"
        );
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(params_from_iter(refs), handoff_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// How many handoffs nobody has accepted or cancelled: the dashboard's
    /// number, counted rather than fetched, so a board past the listing's
    /// page still reports what it holds. Same filter as
    /// `handoffs(None, Some("pending"), None, …)`.
    pub fn count_pending_handoffs(&self) -> Result<i64> {
        self.authz.check_read(&[])?;
        let (where_clause, values) = handoff_filter(None, Some("pending"), None, false);
        let refs = values.iter().map(|value| value.as_ref());
        self.connection
            .query_row(
                &format!("SELECT COUNT(*) FROM handoffs{where_clause}"),
                params_from_iter(refs),
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn create_handoff(&mut self, input: HandoffInput) -> Result<Handoff> {
        validate(&input.reason, &HANDOFF_REASONS, "handoff reason")?;
        validate_priority(Some(input.priority))?;
        // A session handoff has no task to carry its address, so it must name
        // the lane meant to pick it up: a targetless one wedges `/session
        // cont`, which refuses a lane filter that matches several and a schema
        // that reads null (ADR-008 — the refusal names the fix). A task handoff
        // is addressed by its task, whose queue takes it, so `--to` stays
        // optional there.
        if input.task_id.is_none()
            && input
                .to_agent
                .as_deref()
                .is_none_or(|to| to.trim().is_empty())
        {
            bail!("a session handoff needs an addressee: pass --to LANE (e.g. --to driver-2)");
        }
        let transaction = self.begin_write()?;
        // Handoffs carry no tags of their own: board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let now = now_ms();
        let prior_status = input
            .task_id
            .as_deref()
            .map(|task_id| require_task(&transaction, task_id).map(|task| task.status))
            .transpose()?;
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
        // A handoff has no `blocked` state; its `--blocker` list is where it
        // says what stops the work, so that list is the half the owner gate
        // reads. A session handoff names no task, and there is no row for a
        // card to hang on, so there is nothing to require.
        if let Some(task_id) = &input.task_id {
            require_owner_has_a_card(
                &transaction,
                task_id,
                input.blockers.iter().map(String::as_str),
            )?;
        }
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
            "INSERT INTO handoffs(id,task_id,checkpoint_seq,reason,status,from_agent,from_session,from_model,to_agent,summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at,root_head,accepted_at,accepted_by,accepted_session,priority) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,NULL,NULL,NULL,?)",
            params![id,input.task_id,checkpoint_seq,input.reason,"pending",input.from_agent,input.from_session,input.from_model,input.to_agent,summary,intent,next,blockers,validations,input.repo_path,input.branch,input.head_sha,input.dirty_summary,now,input.root_head,input.priority],
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
        event_with_status(
            &transaction,
            input.task_id.as_deref(),
            "handoff_created",
            Some(&input.from_agent),
            json!({"handoffID":id,"checkpointSeq":checkpoint_seq,"reason":input.reason,"toAgent":input.to_agent,"priority":input.priority,"priorityLevel":priority_level(input.priority)}),
            prior_status.as_deref(),
            Some("todo"),
        )?;
        let result =
            transaction.query_row("SELECT * FROM handoffs WHERE id=?", [&id], handoff_row)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn accept_handoff(
        &mut self,
        id: &str,
        options: AcceptHandoffOptions,
    ) -> Result<(Handoff, Option<Claim>)> {
        let AcceptHandoffOptions {
            agent,
            session,
            lease_ms,
            caller_scope,
            sprint_override,
            model,
            git,
        } = options;
        let agent = nonempty(&agent, "agent id")?.to_owned();
        if lease_ms < 1000 {
            bail!("lease must be at least 1000ms");
        }
        if let Some(model) = &model {
            crate::model::validate_model_name(model)?;
        }
        let transaction = self.begin_write()?;
        // Board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let now = now_ms();
        expire_claims(&transaction, now)?;
        let subject: Option<Option<String>> = transaction
            .query_row("SELECT task_id FROM handoffs WHERE id=?", [id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(Some(task_id)) = subject {
            let tags = task_tags(&transaction, &task_id)?;
            self.authz.check_write(&tags, &tags)?;
        }
        let handoff = transaction
            .query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)
            .optional()?
            .with_context(|| format!("handoff {id} not found"))?;
        if handoff.status == "retired" {
            bail!(
                "handoff {id} was retired by {} at {} (epoch ms); a retired handoff is closed history, not work to accept",
                handoff.retired_by.as_deref().unwrap_or("someone"),
                handoff
                    .retired_at
                    .map_or("an unknown time".into(), |at| at.to_string())
            );
        }
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
        // Once work has been blocked or settled, accepting an older brief is
        // acknowledgement rather than ownership transfer. There is no
        // claimable task to protect, and leaving the handoff pending forever
        // makes every future lane resume rediscover correspondence it cannot
        // clear. Keep the transition atomic and deliberately mint no lease.
        if matches!(task.status.as_str(), "blocked" | "done" | "cancelled") {
            transaction.execute("UPDATE handoffs SET status='accepted',accepted_at=?,accepted_by=?,accepted_session=? WHERE id=? AND status='pending'",params![now,agent,session,id])?;
            event(
                &transaction,
                Some(&task.id),
                "handoff_accepted",
                Some(&agent),
                json!({"handoffID":id,"acknowledged":true,"taskStatus":task.status}),
            )?;
            let updated =
                transaction.query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)?;
            transaction.commit()?;
            return Ok((updated, None));
        }
        require_claimable_type(&task.id, &task.task_type)?;
        require_no_draft_ancestor(&transaction, &task.id)?;
        if task.status != "todo" {
            bail!("task {} is {}, not claimable", task.id, task.status);
        }
        let (sprint_filter, sprint_recorded) =
            resolve_claim_sprint(&transaction, sprint_override.as_deref())?;
        if let Some(required_sprint) = sprint_filter.as_deref() {
            let attached: Option<String> = transaction
                .query_row(
                    "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                    [&task.id],
                    |row| row.get(0),
                )
                .optional()?;
            if attached.as_deref() != Some(required_sprint) {
                bail!(
                    "task {} is not in sprint {required_sprint}; use --sprint with its sprint or --any-sprint to cross the current boundary explicitly",
                    task.id
                );
            }
        }
        if task.driver_only && caller_scope.as_deref() != Some("driver") {
            bail!("task {} is driver-only", task.id);
        }
        // Same place in the order as `claim`: after driver-only, and with the
        // same two sentences, so a caller reads one rule whichever path they
        // arrived on.
        require_allowed_model(
            &task.id,
            &allowed_models_of(&transaction, &task.id)?,
            model.as_deref(),
        )?;
        require_no_blocking_gates(&transaction, &task.id, GateCaller::Unleased)?;
        if let Some(held) = active_claim(&transaction, &task.id, now)? {
            // Name the holder and the way out. `claim` has no --force, and
            // --allow-reassign only filters `claim --candidates`, so a caller
            // whose agent died still holding the lease has no route from here
            // and reasonably concludes there is none. There is: --force on
            // `task move` overrides a live lease, and the move is audited.
            bail!(
                "task {} is already claimed by {} until {} (epoch ms) — `claim` has no --force, and --allow-reassign only filters `claim --candidates`. To take it from a holder that is gone: `task move {} todo --as ACTOR --force`, then claim it again",
                task.id,
                held.agent_id,
                held.expires_at,
                task.id
            );
        }
        let token = Uuid::new_v4().to_string();
        transaction.execute("INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at,worktree,worktree_kind,branch,head_sha,root_head,model) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",params![
            task.id,agent,session,token,now,now,now+lease_ms,
            git.as_ref().map(|g| g.worktree.clone()),
            git.as_ref().map(|g| g.worktree_kind.to_owned()),
            git.as_ref().and_then(|g| g.branch.clone()),
            git.as_ref().map(|g| g.head.clone()),
            git.as_ref().and_then(|g| g.root_head.clone()),
            model,
        ])?;
        transaction.execute(
            "UPDATE tasks SET status='in_progress',assignee=?,updated_at=? WHERE id=?",
            params![agent, now, task.id],
        )?;
        transaction.execute("UPDATE handoffs SET status='accepted',accepted_at=?,accepted_by=?,accepted_session=? WHERE id=? AND status='pending'",params![now,agent,session,id])?;
        event_with_status(
            &transaction,
            Some(&task.id),
            "handoff_accepted",
            Some(&agent),
            {
                let mut payload = json!({"handoffID":id,"expiresAt":now+lease_ms});
                if let Some(value) = sprint_recorded {
                    payload["sprintOverride"] = json!(value);
                }
                if let Some(value) = &model {
                    payload["model"] = json!(value);
                }
                payload
            },
            Some(&task.status),
            Some("in_progress"),
        )?;
        let updated =
            transaction.query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)?;
        let claim =
            active_claim(&transaction, &task.id, now)?.context("accepted claim disappeared")?;
        transaction.commit()?;
        Ok((updated, Some(claim)))
    }

    /// Retire a pending handoff without deleting it, mirroring `rule retire`
    /// and `attention resolve`: a handoff is history, resolved never deleted,
    /// so the row keeps who closed it and why. Only a pending handoff can be
    /// retired — an accepted one already happened, and a retired one is
    /// already closed — and there is no lease to override: a pending session
    /// handoff holds none, and a pending task handoff released its lease at
    /// creation, so `--force` would have nothing to seize.
    pub fn retire_handoff(&mut self, id: &str, actor: &str, note: &str) -> Result<Handoff> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let note = nonempty(note, "retire note")?.to_owned();
        let transaction = self.begin_write()?;
        // Board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let existing = transaction
            .query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)
            .optional()?
            .with_context(|| format!("handoff {id} not found"))?;
        if existing.status == "retired" {
            bail!(
                "handoff {id} is already retired by {}",
                existing.retired_by.as_deref().unwrap_or("someone")
            );
        }
        if existing.status != "pending" {
            bail!(
                "handoff {id} is {}, not pending; only a pending handoff can be retired",
                existing.status
            );
        }
        let now = now_ms();
        transaction.execute(
            "UPDATE handoffs SET status='retired',retired_at=?,retired_by=?,retire_note=? WHERE id=? AND status='pending'",
            params![now, actor, note, id],
        )?;
        event(
            &transaction,
            existing.task_id.as_deref(),
            "handoff_retired",
            Some(&actor),
            json!({"handoffID":id,"note":note,"toAgent":existing.to_agent}),
        )?;
        let result =
            transaction.query_row("SELECT * FROM handoffs WHERE id=?", [id], handoff_row)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn signoff_story(
        &mut self,
        id: &str,
        actor: &str,
        signed: bool,
        note: Option<&str>,
    ) -> Result<Value> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: signoff does not
        // retag, so old and resulting tag sets are the row's.
        let old_tags = task_tags(&transaction, id)?;
        self.authz.check_write(&old_tags, &old_tags)?;
        let story = require_active_task(&transaction, id)?;
        require_story_type(id, &story.task_type, "takes signoff")?;
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
        let transaction = self.begin_write()?;
        // Under the mutation lock, before the row is opened: advancing a story
        // does not retag, so old and resulting tag sets are the row's.
        let old_tags = task_tags(&transaction, id)?;
        self.authz.check_write(&old_tags, &old_tags)?;
        let story = require_active_task(&transaction, id)?;
        require_story_type(id, &story.task_type, "advances")?;
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
        // `planning -> ready` is the story being made ready to act on, which
        // is bookkeeping; every step past it asserts the work itself is
        // moving, so the story's own and inherited prerequisites apply.
        if target != "ready" {
            require_no_blocking_gates(&transaction, id, GateCaller::Unleased)?;
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
                event_with_status(
                    &transaction,
                    Some(&parent.id),
                    "epic_advanced",
                    Some(&actor),
                    json!({"from":"ready","to":"in-progress"}),
                    Some(&parent.status),
                    Some("in_progress"),
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
            event_with_status(
                &transaction,
                Some(&child_id),
                "task_created",
                Some(&actor),
                json!({"storyID":id,"workflowDispatch":target}),
                None,
                Some("in_progress"),
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
        event_with_status(
            &transaction,
            Some(id),
            "story_advanced",
            Some(&actor),
            json!({"from":current,"to":target,"dispatchedTaskID":dispatched}),
            Some(&story.status),
            Some(status),
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
    ///
    /// A task whose lease has LAPSED is not stale, it is unclaimed: the same
    /// rule [`apply_lapsed_leases`] applies to a listing, expressed here as a
    /// `WHERE` clause because this query selects on the status it would have
    /// rewritten. Without it a read-only open — which never sweeps — reported
    /// the vanished holder's task as overdue work in progress, while a
    /// writable open on the same board reported nothing.
    pub fn stale_tasks(&self) -> Result<Vec<StaleTask>> {
        self.authz.check_read(&[])?;
        let now = now_ms();
        let mut statement = self.connection.prepare(
            "SELECT t.*, c.heartbeat_at AS claim_heartbeat FROM tasks t
             LEFT JOIN task_claims c ON c.task_id=t.id
             WHERE t.status='in_progress' AND t.archived=0 AND t.stale_minutes IS NOT NULL
               AND NOT EXISTS (
                 SELECT 1 FROM task_claims x WHERE x.task_id=t.id AND x.expires_at<=?
               )
             ORDER BY t.priority,t.created_at,t.id",
        )?;
        let rows = statement
            .query_map([now], |row| {
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
        // The packet and its applicable rules must use the same task selectors.
        let snapshot = ReadSnapshot::open(&self.connection)?;
        let task = self.require_task(id)?;
        #[cfg(test)]
        tests::run_context_packet_after_task_hook();
        let open_attention = self.open_attentions(id)?;
        // Over-fetch by one so "there is older history" is measured, not
        // assumed. `truncated` was hardcoded false, so a resuming agent was
        // told it held the whole record while notes were being dropped.
        let mut notes = self.notes(id, NOTES as i64 + 1)?;
        let mut checkpoints = self.checkpoints(id, CHECKPOINTS as i64 + 1)?;
        let mut handoffs = self.handoffs(Some(id), None, None, HANDOFFS as i64 + 1, false)?;
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
        let packet = ContextPacket {
            task,
            ancestors: self.ancestors(id)?,
            dependencies: self.dependencies(id)?,
            blocking_gates: self.blocking_gates(id)?,
            claim: self.get_claim(id)?.as_ref().map(ClaimSummary::from),
            orphaned_from: orphaned_from(&self.connection, id)?,
            open_attention,
            notes,
            checkpoints,
            handoffs,
            rules: Vec::new(),
            sitreps,
            // Which release this work is heading toward (ADR-045 §5), read
            // straight off the row's own attachment: the packet is what a
            // resuming agent reads, and the boundary is part of the work.
            sprint: self.context_sprint(id)?,
            generated_at: now_ms(),
            truncated,
        };
        snapshot.close()?;
        Ok(packet)
    }

    /// The sprint summary a context packet carries: id, status, version,
    /// title and the goal headline (ADR-045 §5). `None` when the task is
    /// unattached.
    fn context_sprint(&self, task_id: &str) -> Result<Option<ContextSprint>> {
        let attached: Option<String> = self
            .connection
            .query_row(
                "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                [task_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(id) = attached else {
            return Ok(None);
        };
        // Missing here means a dangling FK, which the schema says cannot
        // happen — surfaced rather than read as "no sprint".
        let sprint = require_sprint_on(&self.connection, &id)?;
        Ok(Some(ContextSprint {
            sprint_id: sprint.id,
            status: sprint.status,
            target_version: sprint.target_version,
            title: sprint.title,
            goal: crate::model::sprint_goal(sprint.body.as_deref()).map(str::to_owned),
        }))
    }

    /// Read the task's authorized rule selectors in one board snapshot.
    /// Claim and handoff call this after their mutation commits.
    pub fn task_rule_selectors(&self, task_id: &str) -> Result<(HashSet<String>, Option<String>)> {
        self.authz.check_read(&[])?;
        let snapshot = ReadSnapshot::open(&self.connection)?;
        let tags = task_tags(&self.connection, task_id)?;
        self.authz.check_read(&tags)?;
        let sprint: Option<String> = self
            .connection
            .query_row(
                "SELECT ts.sprint_id FROM tasks t \
                 LEFT JOIN task_sprints ts ON ts.task_id=t.id WHERE t.id=?",
                [task_id],
                |row| row.get(0),
            )
            .optional()?
            .with_context(|| format!("task {task_id} not found"))?;
        snapshot.close()?;
        Ok((tags.into_iter().collect(), sprint))
    }

    /// Prove a sprint exists through the caller-authorized read-only board.
    pub fn require_rule_sprint(&self, id: &str) -> Result<()> {
        self.authz.check_read(&[])?;
        require_sprint_on(&self.connection, id).map(|_| ())
    }

    /// SQLite's own integrity verdict. Board scope: the strings are page and
    /// index diagnostics, not rows.
    pub fn integrity(&self) -> Result<Vec<String>> {
        self.authz.check_read(&[])?;
        integrity(&self.connection)
    }

    /// The ADR-029 hash-chain verdict: counts, a head hash, and where the
    /// chain broke. Board scope; no row content leaves.
    pub fn audit(&self) -> Result<crate::audit::AuditReport> {
        self.authz.check_read(&[])?;
        crate::audit::verify_board(&self.connection)
    }

    /// Append a board-level event that belongs to no task (a restore marker, a
    /// snapshot note). Untagged by construction, so board-scope write is the
    /// whole check — but it IS a write to the ledger, and an unauthorized
    /// caller must not be able to append to another tenant's audit chain.
    pub fn record_system_event(&self, kind: &str, actor: &str, payload: Value) -> Result<()> {
        self.authz.check_write(&[], &[])?;
        event_at(
            &self.connection,
            None,
            kind,
            Some(nonempty(actor, "actor")?),
            payload,
            now_ms(),
        )
    }

    /// Foreign-key violations, as `doctor` reports them. Board scope: the
    /// descriptions name tables and rowids, which is diagnostic rather than
    /// row content, and a caller with no board read gets none of it.
    pub fn foreign_key_violations(&self) -> Result<Vec<String>> {
        self.authz.check_read(&[])?;
        crate::db::foreign_key_violations(&self.connection)
    }

    /// Tasks stamped after the moment they are read.
    ///
    /// Leases expire by comparing stamps, so a record from the future is not a
    /// cosmetic oddity: it sorts ahead of real work and, on a claim, holds a
    /// lease that no sweep will ever retire. A minute of slack keeps ordinary
    /// clock drift between hosts sharing a board out of the report.
    ///
    /// This one DOES return row identities, so it is filtered per row by each
    /// task's real tags — a `doctor` projection must not become the list of
    /// task ids a caller cannot otherwise see.
    pub fn future_dated_tasks(&self) -> Result<Vec<String>> {
        const SLACK_MS: i64 = 60_000;
        self.authz.check_read(&[])?;
        let mut statement = self
            .connection
            .prepare("SELECT id FROM tasks WHERE created_at>? OR updated_at>? ORDER BY id")?;
        let horizon = now_ms() + SLACK_MS;
        let rows = statement.query_map([horizon, horizon], |row| row.get::<_, String>(0))?;
        let ids = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if !self.authz.is_enforcing() {
            return Ok(ids);
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if self.authz.permits_read(&task_tags(&self.connection, &id)?) {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Copy the whole board file out.
    ///
    /// The bulk-read gate, because this is the purest form of the projection
    /// bypass: every row, every tag, every event and every payload leaves in
    /// one page-level copy that no later read of ours will ever filter. A
    /// caller who may not see one tagged row may not take the file that
    /// contains it.
    pub fn backup(&self, destination: &Path) -> Result<()> {
        self.require_whole_board_read()?;
        let mut target = create_backup_target(destination)?;
        let backup = rusqlite::backup::Backup::new(&self.connection, &mut target)?;
        backup.run_to_completion(64, std::time::Duration::from_millis(1), None)?;
        Ok(())
    }

    /// Refuse a (tier, host) pair the canonical table forbids, naming the tier,
    /// the host, and the row it violates. Per-attempt receipts are immutable,
    /// so this guards only new records; a replay of one already written is
    /// returned before it is reached. Only [`MBP_TIERS`] on a non-MBP host and
    /// non-MBP tiers on an [`MBP_HOSTS`] host are refused, because every other
    /// host is Hetzner by exclusion (see the constants in `model.rs`).
    fn require_deploy_tier_host(tier: &str, host: &str) -> Result<()> {
        let mbp_tier = MBP_TIERS.contains(&tier);
        let mbp_host = MBP_HOSTS.contains(&host);
        if mbp_tier && !mbp_host {
            bail!(
                "tier {tier} is an MBP tier (canonical row \"{tier} -> geoywsMBP\"), but host is {host}; deploy it from geoywsMBP (or geoywsMBA)"
            );
        }
        if !mbp_tier && mbp_host {
            bail!(
                "tier {tier} is a Hetzner tier (canonical row \"{tier} -> Hetzner host\"), but host is {host}; deploy it from a Hetzner host (e.g. hax or hig)"
            );
        }
        Ok(())
    }

    /// Open a deployment attempt.
    ///
    /// A deployment attempt is a board row that may POINT AT a task, so the
    /// check runs under the mutation lock against that task's real tags — a
    /// caller who cannot see the task cannot record a deployment of it, and
    /// cannot use the deployment table to learn that the task exists.
    ///
    /// The identity arrives already resolved to exactly one mode
    /// ([`DeployIdentity`]), so nothing here decides between a build commit
    /// and an artifact identity: the row records what the caller named.
    pub fn start_deployment(&mut self, input: StartDeployment) -> Result<DeploymentStartReceipt> {
        validate(&input.tier, &DEPLOYMENT_TIERS, "deployment tier")?;
        let repo = nonempty(&input.repo, "repo")?.to_owned();
        let identity_mode = input.identity.mode();
        let build_commit = input.identity.build_commit().to_owned();
        let expected = input.identity.artifacts().to_vec();
        let expected_json = (!expected.is_empty())
            .then(|| serde_json::to_string(&expected))
            .transpose()?;
        let deployer_checkout = input
            .deployer_checkout
            .as_deref()
            .map(|value| full_commit(value, "deployer checkout"))
            .transpose()?;
        let environment = nonempty(&input.environment, "environment")?.to_owned();
        let host = nonempty(&input.host, "host")?.to_owned();
        let url = nonempty(&input.url, "url")?.to_owned();
        let actor = nonempty(&input.actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        let subject_tags = match input.task_id.as_deref() {
            Some(task_id) => task_tags(&transaction, task_id)?,
            None => Vec::new(),
        };
        self.authz.check_write(&subject_tags, &subject_tags)?;
        if let Some(task_id) = input.task_id.as_deref() {
            require_task(&transaction, task_id)?;
        }
        let deployment_target_version = if let Some(sprint_id) = input.sprint_id.as_deref() {
            let sprint = require_sprint_on(&transaction, sprint_id)?;
            if sprint.status == "abandoned" {
                bail!("sprint {sprint_id} is abandoned");
            }
            Some(sprint.target_version)
        } else {
            None
        };
        if let Some(retry_of) = input.retry_of.as_deref() {
            let status: Option<String> = transaction
                .query_row(
                    "SELECT status FROM deployments WHERE id=?",
                    [retry_of],
                    |row| row.get(0),
                )
                .optional()?;
            let status = status.with_context(|| format!("deployment {retry_of} not found"))?;
            if status == "started" {
                bail!("deployment {retry_of} is still started and cannot be retried");
            }
        }
        if let Some(operation_id) = input.operation_id.as_deref() {
            let replay: Option<(DeploymentAttempt, String)> = transaction
                .query_row(
                    "SELECT *,capability_token FROM deployments WHERE operation_id=?",
                    [operation_id],
                    |row| Ok((deployment_row(row)?, row.get("capability_token")?)),
                )
                .optional()?;
            if let Some((deployment, capability_token)) = replay {
                let same = deployment.task_id == input.task_id
                    && deployment.repo == repo
                    && deployment.identity_mode == identity_mode
                    && deployment.build_commit == build_commit
                    && deployment.deployer_checkout == deployer_checkout
                    && deployment
                        .artifacts
                        .iter()
                        .map(|artifact| {
                            (
                                artifact.role.as_str(),
                                artifact.kind.as_str(),
                                artifact.expected.as_str(),
                            )
                        })
                        .eq(expected.iter().map(|artifact| {
                            (
                                artifact.role.as_str(),
                                artifact.kind.as_str(),
                                artifact.value.as_str(),
                            )
                        }))
                    && deployment.branch == input.branch
                    && deployment.tier == input.tier
                    && deployment.environment == environment
                    && deployment.host == host
                    && deployment.url == url
                    && deployment.mechanism == input.mechanism
                    && deployment.retry_of == input.retry_of
                    && deployment.actor == actor
                    && deployment.lane == input.lane
                    && deployment.sprint_id == input.sprint_id
                    && deployment.target_version == deployment_target_version;
                if !same {
                    bail!(
                        "operation id {operation_id} already names a different deployment attempt"
                    );
                }
                transaction.rollback()?;
                return Ok(DeploymentStartReceipt {
                    deployment,
                    capability_token,
                    idempotent_replay: true,
                });
            }
        }
        Self::require_deploy_tier_host(&input.tier, &host)?;
        let id = format!("d-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let capability_token = Uuid::new_v4().to_string();
        let now = now_ms();
        transaction.execute(
            "INSERT INTO deployments(id,task_id,repo,identity_mode,commit_sha,deployer_checkout,expected_artifacts,branch,tier,environment,host,url,mechanism,operation_id,retry_of,status,actor,lane,capability_token,created_at,updated_at,sprint_id,target_version) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?, 'started',?,?,?,?,?,?,?)",
            params![id,input.task_id,repo,identity_mode,build_commit,deployer_checkout,expected_json,input.branch,input.tier,environment,host,url,input.mechanism,input.operation_id,input.retry_of,actor,input.lane,capability_token,now,now,input.sprint_id,deployment_target_version],
        )?;
        event(
            &transaction,
            input.task_id.as_deref(),
            "deployment_started",
            Some(&actor),
            json!({"deploymentID":id,"repo":repo,"identityMode":identity_mode,"buildCommit":build_commit,"deployerCheckout":deployer_checkout,"expectedArtifacts":expected,"tier":input.tier,"environment":environment,"host":host,"url":url,"retryOf":input.retry_of,"sprintID":input.sprint_id,"targetVersion":deployment_target_version}),
        )?;
        let deployment = transaction.query_row(
            "SELECT * FROM deployments WHERE id=?",
            [&id],
            deployment_row,
        )?;
        transaction.commit()?;
        Ok(DeploymentStartReceipt {
            deployment,
            capability_token,
            idempotent_replay: false,
        })
    }

    /// One named deployment attempt.
    ///
    /// Board scope is checked BEFORE the row is read, so an unauthorized
    /// caller gets the generic denial rather than `deployment <id> not found`,
    /// which would confirm which ids exist. Then the attempt's subject task is
    /// checked, because a deployment row is a projection of a task: its
    /// `taskID`, `branch`, `commit`, `url` and `receipt` describe work the
    /// caller may have no authority over.
    pub fn require_deployment(&self, id: &str) -> Result<DeploymentAttempt> {
        self.authz.check_read(&[])?;
        let attempt = self
            .connection
            .query_row("SELECT * FROM deployments WHERE id=?", [id], deployment_row)
            .optional()?
            .with_context(|| format!("deployment {id} not found"))?;
        self.authz
            .check_read(&self.deployment_subject_tags(&attempt)?)?;
        Ok(attempt)
    }

    /// The subject task's REAL tags for one deployment attempt, or none when
    /// the attempt names no task.
    ///
    /// Read live from `task_tags` rather than from anything stored on the
    /// deployment row: the attempt is immutable and its task can be retagged
    /// after the fact, so a copy taken at deploy time would authorize against
    /// a tag set that no longer exists.
    fn deployment_subject_tags(&self, attempt: &DeploymentAttempt) -> Result<Vec<String>> {
        if !self.authz.is_enforcing() {
            return Ok(Vec::new());
        }
        match attempt.task_id.as_deref() {
            Some(task_id) => task_tags(&self.connection, task_id),
            None => Ok(Vec::new()),
        }
    }

    /// Keep only the attempts this caller may see, by each attempt's subject
    /// task. Filtered rather than refused: this is an enumeration.
    fn visible_deployments(
        &self,
        attempts: Vec<DeploymentAttempt>,
    ) -> Result<Vec<DeploymentAttempt>> {
        self.authz.check_read(&[])?;
        if !self.authz.is_enforcing() {
            return Ok(attempts);
        }
        let mut out = Vec::with_capacity(attempts.len());
        for attempt in attempts {
            if self
                .authz
                .permits_read(&self.deployment_subject_tags(&attempt)?)
            {
                out.push(attempt);
            }
        }
        Ok(out)
    }

    pub fn deployments(
        &self,
        status: Option<&str>,
        tier: Option<&str>,
        include_archived: bool,
        limit: i64,
    ) -> Result<Vec<DeploymentAttempt>> {
        if let Some(value) = status {
            validate(value, &DEPLOYMENT_STATUSES, "deployment status")?;
        }
        if let Some(value) = tier {
            validate(value, &DEPLOYMENT_TIERS, "deployment tier")?;
        }
        let mut sql = String::from("SELECT * FROM deployments WHERE 1=1");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !include_archived {
            sql.push_str(" AND archived=0");
        }
        if let Some(value) = status {
            sql.push_str(" AND status=?");
            values.push(Box::new(value.to_owned()));
        }
        if let Some(value) = tier {
            sql.push_str(" AND tier=?");
            values.push(Box::new(value.to_owned()));
        }
        sql.push_str(" ORDER BY created_at DESC,id DESC LIMIT ?");
        values.push(Box::new(limit));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(
                params_from_iter(values.iter().map(|value| value.as_ref())),
                deployment_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.visible_deployments(rows)
    }

    /// The newest succeeded attempt per (repo, tier, environment) — the
    /// "what is live where" view, and the most attractive projection on the
    /// board, because it is a short list of exactly the interesting rows.
    pub fn current_deployments(&self) -> Result<Vec<DeploymentAttempt>> {
        let mut statement = self.connection.prepare(
            "SELECT d.* FROM deployments d WHERE d.status='succeeded' AND d.archived=0 AND NOT EXISTS (SELECT 1 FROM deployments newer WHERE newer.status='succeeded' AND newer.repo=d.repo AND newer.tier=d.tier AND newer.environment=d.environment AND (newer.created_at>d.created_at OR (newer.created_at=d.created_at AND newer.id>d.id))) ORDER BY d.repo,d.tier,d.environment",
        )?;
        let rows = statement
            .query_map([], deployment_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.visible_deployments(rows)
    }

    /// Close a deployment attempt with its verdict.
    ///
    /// Two-step, under the mutation lock: board-scope write BEFORE the row is
    /// read, so `deployment <id> not found` cannot become an existence
    /// oracle, and then the attempt's subject task. The capability token
    /// proves ownership of the ATTEMPT; it is not authority over the board and
    /// never substitutes for the check.
    ///
    /// The verdict is proved in the mode the attempt was STARTED in, which
    /// the row already carries: a Git attempt against its served commit, an
    /// artifact attempt against every expected role. Neither mode's flag is
    /// accepted for the other, so a digest-only success can never be recorded
    /// as a verified Git commit (ADR-043 §3).
    pub fn finish_deployment(&mut self, input: FinishDeployment) -> Result<DeploymentAttempt> {
        validate(&input.result, &DEPLOYMENT_RESULTS, "deployment result")?;
        if input.result == "abandoned" {
            bail!("use deploy abandon for an abandoned attempt");
        }
        let phase = input
            .phase
            .as_deref()
            .context("deployment phase is required")?;
        validate(phase, &DEPLOYMENT_PHASES, "deployment phase")?;
        let actor = nonempty(&input.actor, "actor")?.to_owned();
        nonempty(input.receipt.as_deref().unwrap_or(""), "deployment receipt")?;
        let served_commit = input
            .served_commit
            .as_deref()
            .map(|value| full_commit(value, "served commit"))
            .transpose()?;
        let served_version = input
            .served_version
            .as_deref()
            .map(target_version)
            .transpose()?;
        if input.result == "succeeded" && phase != "verification" {
            bail!("a succeeded deployment requires --phase verification");
        }
        let transaction = self.begin_write()?;
        self.authz.check_write(&[], &[])?;
        let subject_tags = deployment_subject_tags_on(&transaction, &input.id)?;
        self.authz.check_write(&subject_tags, &subject_tags)?;
        let current: (String, String, String, String, Option<String>, Option<String>, Option<String>) = transaction
            .query_row(
                "SELECT status,capability_token,identity_mode,commit_sha,expected_artifacts,sprint_id,target_version FROM deployments WHERE id=?",
                [&input.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
            )
            .optional()?
            .with_context(|| format!("deployment {} not found", input.id))?;
        let (
            status,
            token,
            identity_mode,
            build_commit,
            expected_json,
            sprint_id,
            deployment_target_version,
        ) = current;
        if status != "started" {
            bail!("deployment {} is already {}", input.id, status);
        }
        if token != input.capability_token {
            bail!("capability token does not own deployment {}", input.id);
        }
        let artifact_mode = identity_mode == IDENTITY_MODE_ARTIFACT;
        if artifact_mode && served_commit.is_some() {
            bail!(
                "deployment {} was started in artifact-identity mode, where no Git commit was \
                 proved; --served-commit cannot be recorded for it — verify it with --observed \
                 ROLE=KIND:VALUE for every expected role",
                input.id
            );
        }
        if !artifact_mode && !input.observed.is_empty() {
            bail!(
                "deployment {} was started in Git mode; --observed belongs to artifact-identity \
                 mode — verify it with --served-commit FULL_SHA",
                input.id
            );
        }
        let observed_json = if artifact_mode && input.result == "succeeded" {
            let expected: Vec<ArtifactIdentity> = match expected_json.as_deref() {
                Some(text) => serde_json::from_str(text)?,
                None => Vec::new(),
            };
            let verified = verify_artifacts(&input.id, &expected, &input.observed)?;
            Some(serde_json::to_string(&verified)?)
        } else if artifact_mode && !input.observed.is_empty() {
            // An attempt that did not succeed still records what it measured:
            // the mismatch is the evidence, and the row's status already says
            // that nothing here was verified against the expectation.
            Some(serde_json::to_string(&input.observed)?)
        } else {
            None
        };
        if !artifact_mode
            && input.result == "succeeded"
            && served_commit.as_deref() != Some(build_commit.as_str())
        {
            bail!("served commit must exactly match the requested deployment commit");
        }
        if input.result == "succeeded" {
            match (
                sprint_id.as_deref(),
                deployment_target_version.as_deref(),
                served_version.as_deref(),
            ) {
                (Some(_), Some(expected), Some(served)) if expected == served => {}
                (Some(sprint_id), Some(expected), Some(served)) => bail!(
                    "deployment {} is bound to sprint {sprint_id} target version {expected}, but served version {served} was observed",
                    input.id
                ),
                (Some(sprint_id), Some(expected), None) => bail!(
                    "deployment {} is bound to sprint {sprint_id} target version {expected}; successful verification requires --served-version {expected}",
                    input.id
                ),
                (None, _, Some(_)) => {
                    bail!("--served-version requires a deployment started with --sprint")
                }
                _ => {}
            }
        } else if served_version.is_some() {
            bail!("--served-version is proof for a succeeded verification deployment only");
        }
        let now = now_ms();
        transaction.execute(
            "UPDATE deployments SET status=?,phase=?,receipt=?,artifact_uri=?,served_commit=?,served_version=?,observed_artifacts=?,updated_at=?,completed_at=? WHERE id=?",
            params![input.result,input.phase,input.receipt,input.artifact_uri,served_commit,served_version,observed_json,now,now,input.id],
        )?;
        let task_id: Option<String> = transaction.query_row(
            "SELECT task_id FROM deployments WHERE id=?",
            [&input.id],
            |row| row.get(0),
        )?;
        event(
            &transaction,
            task_id.as_deref(),
            "deployment_finished",
            Some(&actor),
            json!({"deploymentID":input.id,"result":input.result,"phase":input.phase,"identityMode":identity_mode,"servedCommit":served_commit,"observedArtifacts":input.observed,"receipt":input.receipt,"artifactURI":input.artifact_uri}),
        )?;
        let deployment = transaction.query_row(
            "SELECT * FROM deployments WHERE id=?",
            [&input.id],
            deployment_row,
        )?;
        transaction.commit()?;
        Ok(deployment)
    }

    /// Abandon a started attempt.
    ///
    /// `--force` bypasses the capability token, so the authorization check is
    /// what stops it becoming a way to write a deployment row on a task the
    /// caller cannot see. Same two-step under the mutation lock.
    pub fn abandon_deployment(
        &mut self,
        id: &str,
        token: Option<&str>,
        force: bool,
        note: &str,
        actor: &str,
    ) -> Result<DeploymentAttempt> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let note = nonempty(note, "abandon note")?.to_owned();
        let transaction = self.begin_write()?;
        self.authz.check_write(&[], &[])?;
        let subject_tags = deployment_subject_tags_on(&transaction, id)?;
        self.authz.check_write(&subject_tags, &subject_tags)?;
        let (status, capability, task_id, updated_at): (String, String, Option<String>, i64) =
            transaction
                .query_row(
                    "SELECT status,capability_token,task_id,updated_at FROM deployments WHERE id=?",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?
                .with_context(|| format!("deployment {id} not found"))?;
        if status != "started" {
            bail!("deployment {id} is already {status}");
        }
        if !force && token != Some(capability.as_str()) {
            bail!(
                "capability token does not own deployment {id}; use --force only for explicit recovery"
            );
        }
        let now = now_ms();
        if force && now.saturating_sub(updated_at) < 60 * 60 * 1000 {
            bail!(
                "deployment {id} is not stale; wait 60 minutes or finish it with its capability token"
            );
        }
        transaction.execute("UPDATE deployments SET status='abandoned',receipt=?,updated_at=?,completed_at=? WHERE id=?", params![note,now,now,id])?;
        event(
            &transaction,
            task_id.as_deref(),
            "deployment_abandoned",
            Some(&actor),
            json!({"deploymentID":id,"note":note,"forced":force}),
        )?;
        let deployment =
            transaction.query_row("SELECT * FROM deployments WHERE id=?", [id], deployment_row)?;
        transaction.commit()?;
        Ok(deployment)
    }

    /// Open a sprint. The row starts `planned`; `start` makes it the
    /// boundary, `close` ends it on proof, `abandon` ends it on a note
    /// (ADR-045 §2).
    pub fn create_sprint(&mut self, input: NewSprint) -> Result<Sprint> {
        let title = nonempty(&input.title, "sprint title")?.to_owned();
        let target_version = target_version(&input.target_version)?;
        let actor = nonempty(&input.actor, "actor")?.to_owned();
        if input.scheduled_start < 0 || input.scheduled_end < input.scheduled_start {
            bail!("sprint schedule requires non-negative --start and --end at or after --start");
        }
        if let Some(body) = input.body.as_deref() {
            nonempty(body, "sprint body")?;
        }
        let transaction = self.begin_write()?;
        // A sprint carries no tags of its own: board scope, under the lock.
        self.authz.check_write(&[], &[])?;
        let now = now_ms();
        let id = match input.id {
            Some(value) => sprint_id(&value)?,
            None => format!("sp-{}", &Uuid::new_v4().simple().to_string()[..8]),
        };
        transaction.execute(
            "INSERT INTO sprints(id,title,body,status,target_version,scheduled_start,scheduled_end,starts_at,ends_at,closed_by_deployment,created_at,updated_at) \
             VALUES(?,?,?,'planned',?,?,?,0,NULL,NULL,?,?)",
            params![id, title, input.body, target_version, input.scheduled_start, input.scheduled_end, now, now],
        )?;
        event(
            &transaction,
            None,
            "sprint_created",
            Some(&actor),
            json!({"sprintID": id, "title": title, "targetVersion": target_version, "scheduledStart": input.scheduled_start, "scheduledEnd": input.scheduled_end}),
        )?;
        let result =
            transaction.query_row("SELECT * FROM sprints WHERE id=?", [&id], sprint_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Author the sprint's goal and success criteria. Status stays
    /// `planned` — planning writes the card, not the boundary (ADR-045 §2).
    pub fn plan_sprint(
        &mut self,
        id: &str,
        body: &str,
        candidates: &[String],
        parent_epic: Option<&str>,
        empty_scope: bool,
        actor: &str,
    ) -> Result<Sprint> {
        let body = nonempty(body, "sprint body")?.to_owned();
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        self.authz.check_write(&[], &[])?;
        let existing = require_sprint_on(&transaction, id)?;
        if existing.status == "closed" {
            bail!("sprint {id} is closed history; its card cannot be rewritten");
        }
        if existing.status == "abandoned" {
            bail!(
                "sprint {id} is abandoned; open a new sprint instead of rewriting this one — \
                 the goal it never met is part of the record"
            );
        }
        if existing.status == "current" {
            bail!(
                "sprint {id} is current and cannot be replanned; close or abandon it before planning another sprint"
            );
        }
        let mut scope = candidates.to_vec();
        if let Some(parent) = parent_epic {
            let parent_tags = task_tags(&transaction, parent)?;
            self.authz.check_write(&parent_tags, &parent_tags)?;
            let task = require_active_task(&transaction, parent)?;
            if task.task_type != "epic" {
                bail!(
                    "sprint plan --parent-epic requires an epic, but {parent} is a {}",
                    task.task_type
                );
            }
            scope.push(parent.to_owned());
        }
        scope.sort();
        scope.dedup();
        let existing_scope: i64 = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_sprints WHERE sprint_id=?)",
            [id],
            |row| row.get(0),
        )?;
        if empty_scope && (!scope.is_empty() || existing_scope != 0) {
            bail!(
                "--empty-scope cannot be combined with candidate, parent epic, or existing sprint scope"
            );
        }
        if !empty_scope && scope.is_empty() && existing_scope == 0 {
            bail!(
                "sprint plan requires --candidate, --parent-epic, existing explicit scope, or --empty-scope"
            );
        }
        let now = now_ms();
        let mut attached = Vec::new();
        for root in &scope {
            let root_tags = task_tags(&transaction, root)?;
            self.authz.check_write(&root_tags, &root_tags)?;
            attached.extend(attach_scope_on(&transaction, root, id, &actor, now)?);
        }
        for change in &attached {
            let tags = task_tags(&transaction, &change.task_id)?;
            self.authz.check_write(&tags, &tags)?;
            event(
                &transaction,
                Some(&change.task_id),
                "task_sprint_changed",
                Some(&actor),
                json!({"oldSprintID":change.old_sprint_id,"newSprintID":id,"planned":true}),
            )?;
        }
        transaction.execute(
            "UPDATE sprints SET body=?,updated_at=? WHERE id=?",
            params![body, now, id],
        )?;
        event(
            &transaction,
            None,
            "sprint_planned",
            Some(&actor),
            json!({"sprintID": id, "previousBody": existing.body, "emptyScope": empty_scope}),
        )?;
        let result = transaction.query_row("SELECT * FROM sprints WHERE id=?", [id], sprint_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// planned -> current: this sprint becomes the boundary every claim is
    /// scoped to. Refused while another sprint is current, because the
    /// boundary must be unambiguous — and the `one_current_sprint` index
    /// holds the same rule even against a bug here (ADR-045 §1).
    pub fn start_sprint(&mut self, id: &str, actor: &str) -> Result<Sprint> {
        let transaction = self.begin_write()?;
        self.authz.check_write(&[], &[])?;
        let existing = require_sprint_on(&transaction, id)?;
        if existing.status == "closed" {
            bail!("sprint {id} is closed history; its card cannot be rewritten");
        }
        if existing.status == "abandoned" {
            bail!("sprint {id} is abandoned; open a new sprint instead of starting this one");
        }
        if existing.status == "current" {
            bail!("sprint {id} is already current");
        }
        if crate::model::sprint_goal(existing.body.as_deref()).is_none() {
            bail!("sprint {id} has no recorded goal and criteria; run sprint plan first");
        }
        let has_plan: i64 = transaction.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='sprint_planned' AND json_extract(payload,'$.sprintID')=?)", [id], |row| row.get(0))?;
        if has_plan == 0 {
            bail!("sprint {id} has not been planned; run sprint plan first");
        }
        let has_scope: i64 = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_sprints WHERE sprint_id=?)",
            [id],
            |row| row.get(0),
        )?;
        let deliberately_empty: i64 = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE kind='sprint_planned' AND json_extract(payload,'$.sprintID')=? AND json_extract(payload,'$.emptyScope')=1)", [id], |row| row.get(0)
        )?;
        if has_scope == 0 && deliberately_empty == 0 {
            bail!(
                "sprint {id} has no deliberate scope; plan candidates, a parent epic, or --empty-scope before starting"
            );
        }
        // Under the write lock this check is authoritative; the index below
        // is the invariant, so the rule cannot fail open either way.
        if let Some(current) = current_sprint_on(&transaction)? {
            bail!(
                "sprint {} is current; close or abandon it before starting {id}",
                current.id
            );
        }
        let now = now_ms();
        match transaction.execute(
            "UPDATE sprints SET status='current',starts_at=?,updated_at=? WHERE id=?",
            params![now, now, id],
        ) {
            Ok(_) => {}
            // The partial unique index firing here means the check above
            // raced something it cannot race under BEGIN IMMEDIATE; answer
            // with the same refusal anyway, naming the holder it names.
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation
                    && error.to_string().contains("one_current_sprint") =>
            {
                let current = current_sprint_on(&transaction)?
                    .expect("the index fired, so a current sprint exists");
                bail!(
                    "sprint {} is current; close or abandon it before starting {id}",
                    current.id
                );
            }
            Err(error) => return Err(error.into()),
        }
        event(
            &transaction,
            None,
            "sprint_started",
            Some(actor),
            json!({"sprintID": id, "startsAt": now, "previousStatus": existing.status}),
        )?;
        let result = transaction.query_row("SELECT * FROM sprints WHERE id=?", [id], sprint_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Close the current sprint on typed, version-matched served proof. Any
    /// unfinished attached rows must be moved atomically to one named sprint
    /// with an audit note; close never rolls work over implicitly.
    pub fn close_sprint(
        &mut self,
        id: &str,
        deployment_id: Option<&str>,
        carry_to: Option<&str>,
        carry_note: Option<&str>,
        actor: &str,
    ) -> Result<Sprint> {
        let deployment_id = deployment_id.context(
            "sprint close requires --deployment d-…: the version is served or the sprint is not done",
        )?;
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        self.authz.check_write(&[], &[])?;
        let existing = require_sprint_on(&transaction, id)?;
        match existing.status.as_str() {
            "closed" => bail!("sprint {id} is closed history; its card cannot be rewritten"),
            "abandoned" => bail!("sprint {id} is abandoned; it cannot be closed"),
            "planned" => bail!("sprint {id} is planned, not current; start it before closing it"),
            _ => {}
        }
        let proof_tags = deployment_subject_tags_on(&transaction, deployment_id)?;
        self.authz.check_write(&proof_tags, &proof_tags)?;
        let proof = transaction
            .query_row(
                "SELECT * FROM deployments WHERE id=?",
                [deployment_id],
                deployment_row,
            )
            .optional()?
            .with_context(|| format!("deployment {deployment_id} not found"))?;
        if proof.status != "succeeded" || proof.phase.as_deref() != Some("verification") {
            bail!(
                "sprint {id} cannot close on {deployment_id}: that attempt is {} ({}); \
                 a sprint closes only on a succeeded verification-phase deployment",
                proof.status,
                proof.phase.as_deref().unwrap_or("no phase"),
            );
        }
        if proof.sprint_id.as_deref() != Some(id)
            || proof.target_version.as_deref() != Some(existing.target_version.as_str())
            || proof.served_version.as_deref() != Some(existing.target_version.as_str())
        {
            bail!(
                "sprint {id} cannot close on {deployment_id}: proof must be bound to this sprint and served version {}",
                existing.target_version
            );
        }
        let mut statement = transaction.prepare(
            "SELECT t.id,t.status FROM tasks t JOIN task_sprints ts ON ts.task_id=t.id WHERE ts.sprint_id=? AND t.archived=0 ORDER BY t.id",
        )?;
        let sprint_rows = statement
            .query_map([id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (row_id, _) in &sprint_rows {
            let tags = task_tags(&transaction, row_id)?;
            self.authz.check_write(&tags, &tags)?;
        }
        let open_rows = sprint_rows
            .into_iter()
            .filter_map(|(id, status)| {
                (!matches!(status.as_str(), "done" | "cancelled")).then_some(id)
            })
            .collect::<Vec<_>>();
        let carry = if open_rows.is_empty() {
            if carry_to.is_some() || carry_note.is_some() {
                bail!("sprint {id} has no unfinished rows to carry over");
            }
            None
        } else {
            let destination = carry_to.context(format!("sprint {id} has {} unfinished row(s); close requires --carry-to sp-… and --carry-note TEXT", open_rows.len()))?;
            let note = nonempty(carry_note.unwrap_or(""), "carry-over note")?;
            if destination == id {
                bail!("carry-over destination must be a different sprint");
            }
            require_attachable_sprint_on(&transaction, destination)?;
            Some((destination, note))
        };
        let now = now_ms();
        if let Some((destination, note)) = carry {
            carry_sprint_rows(&transaction, id, destination, &actor, now, &open_rows)?;
            for row_id in &open_rows {
                event(
                    &transaction,
                    Some(row_id),
                    "task_sprint_changed",
                    Some(&actor),
                    json!({"oldSprintID":id,"newSprintID":destination,"carryNote":note}),
                )?;
            }
        }
        transaction.execute(
            "UPDATE sprints SET status='closed',ends_at=?,closed_by_deployment=?,updated_at=? \
             WHERE id=?",
            params![now, deployment_id, now, id],
        )?;
        event(
            &transaction,
            None,
            "sprint_closed",
            Some(&actor),
            json!({
                "sprintID": id,
                "deploymentID": deployment_id,
                "targetVersion": existing.target_version,
                "previousStatus": existing.status,
            }),
        )?;
        let result = transaction.query_row("SELECT * FROM sprints WHERE id=?", [id], sprint_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Any non-closed status -> abandoned, with a note. "Later" with no
    /// stated reason is how a boundary silently stops existing — the same
    /// rule a defer consequence answers (ADR-045 §2, ADR-042 §7).
    pub fn abandon_sprint(&mut self, id: &str, note: &str, actor: &str) -> Result<Sprint> {
        let note = nonempty(note, "abandon note")?.to_owned();
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        self.authz.check_write(&[], &[])?;
        let existing = require_sprint_on(&transaction, id)?;
        if existing.status == "closed" {
            bail!("sprint {id} is closed history; its card cannot be rewritten");
        }
        if existing.status == "abandoned" {
            bail!("sprint {id} is already abandoned");
        }
        let now = now_ms();
        transaction.execute(
            "UPDATE sprints SET status='abandoned',ends_at=?,updated_at=? WHERE id=?",
            params![now, now, id],
        )?;
        event(
            &transaction,
            None,
            "sprint_abandoned",
            Some(&actor),
            json!({"sprintID": id, "note": note, "previousStatus": existing.status}),
        )?;
        let result = transaction.query_row("SELECT * FROM sprints WHERE id=?", [id], sprint_row)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Sprint rows, newest first. With no `--status` the closed and
    /// abandoned ones stay hidden — history, not a queue — unless `all`
    /// asks for them (ADR-045 §2).
    pub fn sprints(&self, status: Option<&str>, all: bool, limit: i64) -> Result<Vec<Sprint>> {
        if let Some(value) = status {
            validate(value, &SPRINT_STATUSES, "sprint status")?;
        }
        self.authz.check_read(&[])?;
        let mut sql = String::from("SELECT * FROM sprints WHERE 1=1");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !all {
            sql.push_str(" AND archived=0");
        }
        match status {
            Some(value) => {
                sql.push_str(" AND status=?");
                values.push(Box::new(value.to_owned()));
            }
            // The default view answers "what is live or coming": a closed
            // sprint is a receipt, not a plan.
            None if !all => sql.push_str(" AND status IN ('planned','current')"),
            None => {}
        }
        sql.push_str(" ORDER BY created_at DESC,id DESC LIMIT ?");
        values.push(Box::new(limit));
        let refs = values.iter().map(|value| value.as_ref());
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(refs), sprint_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One named sprint row. Board scope: the row carries no tags of its own.
    pub fn require_sprint(&self, id: &str) -> Result<Sprint> {
        self.authz.check_read(&[])?;
        require_sprint_on(&self.connection, id)
    }

    /// The board's current sprint — the boundary a claim is scoped to and
    /// `kb dash` reports. `None` on a board that has not opened one, where
    /// every claim behaviour is byte-identical to a board without sprints
    /// (ADR-045 §3).
    pub fn current_sprint(&self) -> Result<Option<Sprint>> {
        self.authz.check_read(&[])?;
        current_sprint_on(&self.connection)
    }

    /// The task rows attached to a sprint — `sprint cat`'s "its rows", in
    /// the listing's own order.
    pub fn sprint_tasks(&self, sprint_id: &str) -> Result<Vec<Task>> {
        self.authz.check_read(&[])?;
        require_sprint_on(&self.connection, sprint_id)?;
        let mut statement = self.connection.prepare(
            "SELECT t.* FROM tasks t JOIN task_sprints s ON s.task_id=t.id \
             WHERE s.sprint_id=? AND t.archived=0 \
             ORDER BY t.priority,t.created_at,t.id",
        )?;
        let mut rows = statement
            .query_map([sprint_id], task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        attach_tags(&self.connection, rows.iter_mut())?;
        rows.retain(|task| self.authz.permits_read(&task.tags));
        attach_allowed_models(&self.connection, rows.iter_mut())?;
        apply_lapsed_leases(&self.connection, rows.iter_mut())?;
        Ok(rows)
    }

    /// Count only rows visible to this caller; hidden tagged scope contributes
    /// neither content nor aggregate existence.
    pub fn sprint_task_counts(&self, sprint_id: &str) -> Result<(i64, i64)> {
        self.authz.check_read(&[])?;
        require_sprint_on(&self.connection, sprint_id)?;
        let mut statement = self.connection.prepare(
            "SELECT t.id,t.status FROM tasks t JOIN task_sprints s ON s.task_id=t.id WHERE s.sprint_id=? AND t.archived=0"
        )?;
        let rows = statement
            .query_map([sprint_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut open = 0;
        let mut done = 0;
        for (id, status) in rows {
            if !self.authz.permits_read(&task_tags(&self.connection, &id)?) {
                continue;
            }
            if status == "done" {
                done += 1;
            } else if status != "cancelled" {
                open += 1;
            }
        }
        Ok((open, done))
    }

    /// Attach a task to a sprint. Attaching a container carries its subtree:
    /// every descendant not already inside another sprint joins, and the one
    /// event names the full set, so the audit trail is the scope (ADR-045
    /// §2). The named row itself always moves — a carry-over after close is
    /// exactly this.
    #[cfg(test)]
    pub fn attach_sprint(
        &mut self,
        task_id: &str,
        sprint_id: &str,
        actor: &str,
    ) -> Result<TaskSprintReceipt> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        let root_tags = task_tags(&transaction, task_id)?;
        self.authz.check_write(&root_tags, &root_tags)?;
        require_active_task(&transaction, task_id)?;
        require_attachable_sprint_on(&transaction, sprint_id)?;
        // The subtree, breadth-first so the receipt reads parent-first; the
        // seen-set terminates a malformed parent cycle the way the gate's
        // recursive CTE does.
        let mut subtree = vec![task_id.to_owned()];
        let mut seen = std::collections::BTreeSet::from([task_id.to_owned()]);
        let mut cursor = 0;
        while cursor < subtree.len() {
            let parent = subtree[cursor].clone();
            cursor += 1;
            let mut statement =
                transaction.prepare("SELECT id FROM tasks WHERE parent_id=? ORDER BY id")?;
            let children: Vec<String> = statement
                .query_map([&parent], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(statement);
            for child in children {
                if seen.insert(child.clone()) {
                    subtree.push(child);
                }
            }
        }
        // The named row moves; a descendant joins only when unattached — a
        // row that moved itself to another sprint stays where it put itself.
        let old_sprint_id: Option<String> = transaction
            .query_row(
                "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                [task_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let mut moved = Vec::new();
        if old_sprint_id.as_deref() != Some(sprint_id) {
            moved.push(task_id.to_owned());
        }
        for id in &subtree[1..] {
            let attached: Option<String> = transaction
                .query_row(
                    "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                    [id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            if attached.is_none() {
                moved.push(id.clone());
            }
        }
        if moved.is_empty() {
            bail!(
                "task {task_id} and its subtree are already attached to sprint {sprint_id}; \
                 there is nothing to attach"
            );
        }
        // Attachment moves rows between scopes, so each moved row is checked
        // against its own tags — old set equals new set, because attaching
        // does not retag (the reopen_attention precedent).
        for id in &moved {
            let tags = task_tags(&transaction, id)?;
            self.authz.check_write(&tags, &tags)?;
        }
        let now = now_ms();
        for id in &moved {
            // An upsert rather than a bare insert: the named row may be
            // carrying over from another sprint, which is a replace, while a
            // joining descendant is a plain insert — the PK on task_id makes
            // one statement hold both (ADR-045 §2 carry-over).
            transaction.execute(
                "INSERT INTO task_sprints(task_id,sprint_id,attached_at,attached_by) \
                 VALUES(?,?,?,?) \
                 ON CONFLICT(task_id) DO UPDATE SET \
                 sprint_id=excluded.sprint_id,attached_at=excluded.attached_at,\
                 attached_by=excluded.attached_by",
                params![id, sprint_id, now, actor],
            )?;
        }
        for moved_id in &moved {
            event(
                &transaction,
                Some(moved_id),
                "task_sprint_changed",
                Some(&actor),
                json!({"oldSprintID": if moved_id == task_id { old_sprint_id.as_ref() } else { None }, "newSprintID": sprint_id}),
            )?;
        }
        transaction.commit()?;
        Ok(TaskSprintReceipt {
            task_id: task_id.to_owned(),
            old_sprint_id,
            new_sprint_id: Some(sprint_id.to_owned()),
            moved,
        })
    }

    /// Clear one row's sprint attachment. Explicit, audited, never
    /// recursive, never a side effect — a subtree detaches row by row, by
    /// the operator's hand (ADR-045 §2).
    #[cfg(test)]
    pub fn detach_sprint(&mut self, task_id: &str, actor: &str) -> Result<TaskSprintReceipt> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        let root_tags = task_tags(&transaction, task_id)?;
        self.authz.check_write(&root_tags, &root_tags)?;
        require_active_task(&transaction, task_id)?;
        let old_sprint_id: Option<String> = transaction
            .query_row(
                "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                [task_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(old_sprint_id) = old_sprint_id else {
            bail!("task {task_id} is not attached to a sprint; there is nothing to clear");
        };
        // Detaching does not retag: old set equals new set, under the lock.
        let tags = task_tags(&transaction, task_id)?;
        self.authz.check_write(&tags, &tags)?;
        transaction.execute("DELETE FROM task_sprints WHERE task_id=?", [task_id])?;
        event(
            &transaction,
            Some(task_id),
            "task_sprint_changed",
            Some(&actor),
            json!({
                "taskID": task_id,
                "oldSprintID": old_sprint_id,
                "newSprintID": null,
                "moved": [task_id],
            }),
        )?;
        transaction.commit()?;
        Ok(TaskSprintReceipt {
            task_id: task_id.to_owned(),
            old_sprint_id: Some(old_sprint_id),
            new_sprint_id: None,
            moved: vec![task_id.to_owned()],
        })
    }

    /// Move settled history out of operational views and secondary indexes.
    /// Rows stay in the same SQLite file and remain readable through `--all`.
    /// This is intentionally an explicit sweep: opening or reading a board must
    /// never mutate it merely because wall-clock time passed.
    ///
    /// The bulk-write gate. Each `UPDATE ... WHERE archived=0 AND ...` below
    /// sweeps every table on the board in one statement, so there is no per-row
    /// decision to make: the sweep either runs over rows the caller cannot see
    /// or it does not run. Narrowing each statement by tag instead would let a
    /// partially-authorized sweep archive half a board and report the other
    /// half as untouched, which is a worse answer than a refusal. Taken under
    /// the mutation lock, so a concurrent retag cannot widen the sweep after
    /// the check.
    pub fn archive_settled(
        &mut self,
        cutoff_at: i64,
        actor: &str,
        dry_run: bool,
    ) -> Result<ArchiveReport> {
        let actor = nonempty(actor, "actor")?.to_owned();
        let transaction = self.begin_write()?;
        whole_board_write_on(&self.authz, &transaction)?;
        let archived_at = now_ms();

        let tasks = transaction.execute(
            "UPDATE tasks SET archived=1,archived_at=? \
             WHERE archived=0 AND status IN ('done','cancelled') \
             AND completed_at IS NOT NULL AND completed_at<=? \
             AND id NOT IN (SELECT task_id FROM task_claims)",
            params![archived_at, cutoff_at],
        )? as i64;
        let notes = transaction.execute(
            "UPDATE task_notes SET archived=1 WHERE archived=0 \
             AND task_id IN (SELECT id FROM tasks WHERE archived=1)",
            [],
        )? as i64;
        let checkpoints = transaction.execute(
            "UPDATE checkpoints SET archived=1 WHERE archived=0 \
             AND task_id IN (SELECT id FROM tasks WHERE archived=1)",
            [],
        )? as i64;
        let task_tags = transaction.execute(
            "UPDATE task_tags SET archived=1 WHERE archived=0 \
             AND task_id IN (SELECT id FROM tasks WHERE archived=1)",
            [],
        )? as i64;
        let handoffs = transaction.execute(
            "UPDATE handoffs SET archived=1 WHERE archived=0 AND status<>'pending' AND ( \
               task_id IN (SELECT id FROM tasks WHERE archived=1) OR \
               (task_id IS NULL AND COALESCE(accepted_at,created_at)<=?) \
             )",
            [cutoff_at],
        )? as i64;
        let attention = transaction.execute(
            "UPDATE attention SET archived=1 WHERE archived=0 AND status='resolved' AND ( \
               task_id IN (SELECT id FROM tasks WHERE archived=1) OR resolved_at<=? \
             )",
            [cutoff_at],
        )? as i64;
        let sitreps = transaction.execute(
            "UPDATE sitreps SET archived=1 WHERE archived=0 AND ( \
               task_id IN (SELECT id FROM tasks WHERE archived=1) OR created_at<=? \
             )",
            [cutoff_at],
        )? as i64;
        let deployments = transaction.execute(
            "UPDATE deployments SET archived=1,archived_at=? \
             WHERE archived=0 AND status IN ('succeeded','failed','cancelled','abandoned') \
             AND completed_at IS NOT NULL AND completed_at<=? \
             AND id NOT IN ( \
               SELECT d.id FROM deployments d WHERE d.status='succeeded' AND NOT EXISTS ( \
                 SELECT 1 FROM deployments newer WHERE newer.status='succeeded' \
                 AND newer.repo=d.repo AND newer.tier=d.tier AND newer.environment=d.environment \
                 AND (newer.created_at>d.created_at OR (newer.created_at=d.created_at AND newer.id>d.id)) \
               ) \
             )",
            params![archived_at, cutoff_at],
        )? as i64;
        let events = transaction.execute(
            "UPDATE events SET archived=1 WHERE archived=0 AND ( \
               task_id IN (SELECT id FROM tasks WHERE archived=1) OR \
               (task_id IS NULL AND created_at<=?) \
             )",
            [cutoff_at],
        )? as i64;

        let report = ArchiveReport {
            cutoff_at,
            dry_run,
            tasks,
            notes,
            checkpoints,
            events,
            handoffs,
            attention,
            sitreps,
            task_tags,
            deployments,
        };
        if dry_run {
            transaction.rollback()?;
            return Ok(report);
        }
        if tasks
            + notes
            + checkpoints
            + events
            + handoffs
            + attention
            + sitreps
            + task_tags
            + deployments
            > 0
        {
            event(
                &transaction,
                None,
                "archive_swept",
                Some(&actor),
                serde_json::to_value(&report)?,
            )?;
        }
        transaction.commit()?;
        Ok(report)
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
    use std::fs;
    use std::path::PathBuf;

    std::thread_local! {
        static CONTEXT_PACKET_AFTER_TASK_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn run_context_packet_after_task_hook() {
        let hook = CONTEXT_PACKET_AFTER_TASK_HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    struct ContextPacketAfterTaskHookReset;

    impl Drop for ContextPacketAfterTaskHookReset {
        fn drop(&mut self) {
            let hook = CONTEXT_PACKET_AFTER_TASK_HOOK.with(|slot| slot.borrow_mut().take());
            drop(hook);
        }
    }

    #[test]
    fn readonly_open_denies_before_observing_board_bytes() {
        use crate::policy::{Capability, ScopeTuple};

        let corrupt_path = board_db_path("readonly-open-authz-before-bytes");
        let parent = corrupt_path.parent().unwrap().to_path_buf();
        let absent_path = parent.join("absent.db");
        fs::write(&corrupt_path, vec![0xA5_u8; 4096]).unwrap();
        assert!(!absent_path.exists());

        let board_id = Uuid::new_v4().to_string();
        let denied_authz = || {
            AuthzContext::new(
                crate::routing::Enforcement::Managed,
                crate::policy::authority([]),
                board_id.to_owned(),
            )
        };

        assert_denied(
            Store::open_readonly_with_authz(&corrupt_path, denied_authz()).map(|_| ()),
            "unauthorized corrupt board",
        );
        assert_denied(
            Store::open_readonly_with_authz(&absent_path, denied_authz()).map(|_| ()),
            "unauthorized absent board",
        );
        assert!(!absent_path.exists());

        let allowed_authz = AuthzContext::new(
            crate::routing::Enforcement::Managed,
            crate::policy::authority([(
                ScopeTuple::Board {
                    board_id: board_id.clone(),
                },
                Capability::Read,
            )]),
            board_id.to_owned(),
        );
        let error = Store::open_readonly_with_authz(&corrupt_path, allowed_authz)
            .err()
            .expect("authorized opening of corrupt board must report an actual error");
        assert!(
            !error.to_string().contains("denied or not found"),
            "authorized corrupt-board error must not be an authorization denial: {error}"
        );
        drop(error);

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn context_packet_task_and_sprint_share_one_read_snapshot() {
        let path = board_db_path("context-packet-task-sprint-snapshot");
        let parent = path.parent().unwrap().to_path_buf();
        let mut store = Store::open(&path).unwrap();
        let actor = "codex@driver";
        store.initialize("snapshot board", actor).unwrap();

        let journal_mode: String = store
            .connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

        store.add_tag("old", None, Some(actor)).unwrap();
        store.add_tag("new", None, Some(actor)).unwrap();
        let task_id = Uuid::new_v4().to_string();
        store
            .add_task(task_input(
                &task_id,
                "snapshot task",
                vec!["old".to_owned()],
                vec![],
            ))
            .unwrap();

        for sprint_id in ["sp-old", "sp-new"] {
            store
                .create_sprint(NewSprint {
                    id: Some(sprint_id.to_owned()),
                    title: sprint_id.to_owned(),
                    body: None,
                    target_version: "1.0.0".to_owned(),
                    scheduled_start: 1_800_000_000,
                    scheduled_end: 1_800_086_400,
                    actor: actor.to_owned(),
                })
                .unwrap();
        }
        store.attach_sprint(&task_id, "sp-old", actor).unwrap();

        let _hook_reset = ContextPacketAfterTaskHookReset;
        let writer_path = path.clone();
        let writer_task_id = task_id.clone();
        CONTEXT_PACKET_AFTER_TASK_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "unexpected preexisting thread-local hook");
            *slot = Some(Box::new(move || {
                let mut writer = Connection::open(&writer_path).unwrap();
                let transaction = writer.transaction().unwrap();
                assert_eq!(
                    transaction
                        .execute(
                            "UPDATE task_tags SET tag = ?1 WHERE task_id = ?2 AND tag = ?3",
                            params!["new", &writer_task_id, "old"],
                        )
                        .unwrap(),
                    1,
                    "writer must replace exactly the old tag"
                );
                assert_eq!(
                    transaction
                        .execute(
                            "UPDATE task_sprints SET sprint_id = ?1 \
                             WHERE task_id = ?2 AND sprint_id = ?3",
                            params!["sp-new", &writer_task_id, "sp-old"],
                        )
                        .unwrap(),
                    1,
                    "writer must replace exactly the old sprint attachment"
                );
                transaction.commit().unwrap();
            }));
        });

        // No external transaction: context_packet must establish its own snapshot.
        let packet = store.context_packet(&task_id).unwrap();
        CONTEXT_PACKET_AFTER_TASK_HOOK.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "context_packet did not consume the hook"
            );
        });
        assert_eq!(packet.task.tags, vec!["old".to_owned()]);
        assert_eq!(
            packet
                .sprint
                .as_ref()
                .map(|sprint| sprint.sprint_id.as_str()),
            Some("sp-old"),
            "task tags and sprint must come from the same pre-write snapshot"
        );

        let (tags, sprint_id) = store.task_rule_selectors(&task_id).unwrap();
        let expected_tags: HashSet<String> = ["new".to_owned()].into_iter().collect();
        assert_eq!(tags, expected_tags);
        assert_eq!(sprint_id.as_deref(), Some("sp-new"));

        drop(packet);
        drop(store);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn require_restore_write_never_swallows_authority_denial() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement::Managed;

        let path = board_db_path("restore-preflight");
        let mut store = Store::open(&path).unwrap();
        store.initialize("RESTORE-PREFLIGHT", "test").unwrap();
        store.add_tag("private", None, Some("test")).unwrap();
        store
            .add_task(task_input(
                "t-restore",
                "Private task",
                vec!["private".to_owned()],
                vec![],
            ))
            .unwrap();

        let parent = path.parent().unwrap();
        let corrupt = parent.join("corrupt.db");
        fs::write(&corrupt, b"not a database").unwrap();
        let board_id = Uuid::new_v4().to_string();

        // A valid database must not hide a denial from the authorization constructor.
        assert_denied(
            Store::require_restore_write_with_authz(
                &path,
                AuthzContext::new(Managed, authority([]), board_id.clone()),
            ),
            "valid database with no grants",
        );

        // Corruption is not a reason to bypass the base WRITE requirement.
        assert_denied(
            Store::require_restore_write_with_authz(
                &corrupt,
                AuthzContext::new(
                    Managed,
                    authority([(
                        ScopeTuple::Board {
                            board_id: board_id.clone(),
                        },
                        Capability::Read,
                    )]),
                    board_id.clone(),
                ),
            ),
            "corrupt database with board READ only",
        );

        // An openable board also requires write authority over its tagged content.
        assert_denied(
            Store::require_restore_write_with_authz(
                &path,
                AuthzContext::new(
                    Managed,
                    authority([(
                        ScopeTuple::Board {
                            board_id: board_id.clone(),
                        },
                        Capability::Write,
                    )]),
                    board_id.clone(),
                ),
            ),
            "valid tagged database with board WRITE but no tag WRITE",
        );

        Store::require_restore_write_with_authz(
            &path,
            AuthzContext::new(
                Managed,
                authority([
                    (
                        ScopeTuple::Board {
                            board_id: board_id.clone(),
                        },
                        Capability::Write,
                    ),
                    (
                        ScopeTuple::BoardWildcard {
                            board_id: board_id.clone(),
                        },
                        Capability::Write,
                    ),
                ]),
                board_id.clone(),
            ),
        )
        .unwrap();

        // Full write authority permits recovery preflight despite corrupt bytes.
        Store::require_restore_write_with_authz(
            &corrupt,
            AuthzContext::new(
                Managed,
                authority([
                    (
                        ScopeTuple::Board {
                            board_id: board_id.clone(),
                        },
                        Capability::Write,
                    ),
                    (
                        ScopeTuple::BoardWildcard {
                            board_id: board_id.clone(),
                        },
                        Capability::Write,
                    ),
                ]),
                board_id,
            ),
        )
        .unwrap();

        drop(store);
        fs::remove_dir_all(parent).unwrap();
    }

    fn query_plan_details<P: rusqlite::Params>(
        connection: &Connection,
        sql: &str,
        params: P,
    ) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare explain query plan");
        statement
            .query_map(params, |row| row.get::<_, String>(3))
            .expect("run explain query plan")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect query plan details")
    }

    fn board_db_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("kanban-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp board dir");
        path.join("board.db")
    }

    fn test_store(name: &str) -> Store {
        Store::open(&board_db_path(name)).expect("open test store")
    }

    fn expired_claim_board(name: &str) -> PathBuf {
        let path = board_db_path(name);
        let mut store = Store::open(&path).expect("open expired-claim fixture");
        store.initialize(name, "test@driver").unwrap();
        insert_task(&store, "t-expired");
        store
            .connection
            .execute(
                "UPDATE tasks SET status='in_progress',assignee='ghost' WHERE id='t-expired'",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO task_claims(task_id,agent_id,lease_token,claimed_at,heartbeat_at,expires_at) \
                 VALUES('t-expired','ghost','expired-token',1,1,1)",
                [],
            )
            .unwrap();
        drop(store);
        path
    }

    fn insert_task(store: &Store, id: &str) {
        store
            .connection
            .execute(
                "INSERT INTO tasks(id,type,parent_id,title,body,status,priority,created_at,updated_at,completed_at,metadata) \
                 VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    id,
                    "task",
                    Option::<String>::None,
                    "fixture task",
                    Option::<String>::None,
                    "todo",
                    3,
                    1_i64,
                    1_i64,
                    Option::<i64>::None,
                    "{}",
                ],
            )
            .expect("insert task");
    }

    fn assert_denied<T: std::fmt::Debug>(result: anyhow::Result<T>, surface: &str) {
        match result {
            Ok(value) => panic!("{surface} reached the other board: {value:?}"),
            Err(error) => assert_eq!(
                error.to_string(),
                "denied or not found",
                "{surface} must deny with the generic string"
            ),
        }
    }

    fn task_input(id: &str, title: &str, tags: Vec<String>, dependencies: Vec<String>) -> AddTask {
        AddTask {
            id: Some(id.to_owned()),
            task_type: "task".to_owned(),
            parent_id: None,
            title: title.to_owned(),
            body: None,
            assignee: None,
            lane: None,
            deliverable: None,
            stale_minutes: None,
            driver_only: false,
            status: "todo".to_owned(),
            priority: 3,
            dependencies,
            metadata: serde_json::json!({}),
            actor: Some("seed".to_owned()),
            allowed_models: Vec::new(),
            tags,
        }
    }

    fn claim_options(agent: &str) -> ClaimOptions {
        ClaimOptions {
            agent_id: agent.to_owned(),
            session_id: None,
            lease_ms: 60_000,
            caller_lane: None,
            role_filter: None,
            caller_scope: None,
            cross_lane: false,
            allow_reassign: false,
            sprint_override: None,
            model: None,
            git: None,
        }
    }

    fn checkpoint_input(task_id: &str, author: &str) -> CheckpointInput {
        CheckpointInput {
            task_id: task_id.to_owned(),
            lease_token: "token".to_owned(),
            author: author.to_owned(),
            session_id: None,
            model: None,
            state: "continue".to_owned(),
            summary: "s".to_owned(),
            intent: "i".to_owned(),
            next_action: "n".to_owned(),
            blockers: vec![],
            validations: vec![],
            repo_path: None,
            branch: None,
            head_sha: None,
            dirty_summary: None,
            root_head: None,
        }
    }

    /// A typed, parented row for the completion-gate fixtures, created
    /// through `add_task` so the nesting and gate rules see it as a caller
    /// would build it.
    fn gate_row(id: &str, task_type: &str, parent: Option<&str>) -> AddTask {
        AddTask {
            id: Some(id.to_owned()),
            task_type: task_type.to_owned(),
            parent_id: parent.map(str::to_owned),
            title: format!("{id} fixture"),
            body: None,
            assignee: None,
            lane: None,
            deliverable: None,
            stale_minutes: None,
            driver_only: false,
            status: "todo".to_owned(),
            priority: 3,
            dependencies: Vec::new(),
            metadata: serde_json::json!({}),
            actor: Some("seed".to_owned()),
            allowed_models: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// A dependency-set replacement, leaving every other field alone.
    fn dependency_update(prerequisites: &[&str]) -> UpdateTask {
        UpdateTask {
            dependencies: Some(prerequisites.iter().map(|id| (*id).to_owned()).collect()),
            ..Default::default()
        }
    }

    /// The blockers as `(owner, prerequisite, status)`, which is what every
    /// caller of the gate actually reads.
    fn gate_triples(store: &Store, id: &str) -> Vec<(String, String, String)> {
        store
            .blocking_gates(id)
            .expect("read blocking gates")
            .into_iter()
            .map(|blocker| {
                (
                    blocker.source_task_id,
                    blocker.prerequisite_id,
                    blocker.prerequisite_status,
                )
            })
            .collect()
    }

    fn handoff_input() -> HandoffInput {
        HandoffInput {
            task_id: None,
            lease_token: None,
            from_agent: "actor".to_owned(),
            from_session: None,
            from_model: None,
            to_agent: Some("driver-2".to_owned()),
            reason: "manual".to_owned(),
            priority: 3,
            summary: "s".to_owned(),
            intent: "i".to_owned(),
            next_action: "n".to_owned(),
            blockers: vec![],
            validations: vec![],
            repo_path: None,
            branch: None,
            head_sha: None,
            dirty_summary: None,
            root_head: None,
        }
    }

    fn tenant_subscription_input() -> AddSubscription {
        AddSubscription {
            id: None,
            subject_task_id: None,
            relations: vec![],
            kinds: vec![],
            prior_statuses: vec![],
            current_statuses: vec![],
            tags: vec![],
            consumer_id: "c1".to_owned(),
            action_id: "a1".to_owned(),
            timeout_ms: 1_000,
            max_retries: 3,
            rate_per_minute: 60,
            max_concurrency: 1,
            secret_ref: None,
            actor: "actor".to_owned(),
        }
    }

    /// Two boards in one registry, a caller whose authority covers exactly one
    /// of them, and the assertion that NO row of the other board is reachable
    /// through ANY surface in scope. Removing any single surface's guard makes
    /// the matching assertion fire.
    #[test]
    fn managed_tenancy_hides_every_surface_of_the_other_board() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;

        let board_a = "aaaaaaaa-1111-4111-8111-111111111111";
        let board_b = "bbbbbbbb-2222-4222-8222-222222222222";
        let b_dir = std::env::temp_dir().join(format!("kanban-tenant-b-{}", Uuid::new_v4()));
        fs::create_dir_all(&b_dir).expect("create board b dir");
        let b_path = b_dir.join(format!("{board_b}.db"));

        // Seed the OTHER board (B) with real rows, via the direct estate.
        {
            let mut b = Store::open(&b_path).expect("open seed board");
            b.initialize("board-b", "seed").expect("init board b");
            b.add_tag("beta", None, Some("seed")).expect("seed tag");
            b.add_task(task_input("t-b2", "dependency target", vec![], vec![]))
                .expect("seed dependency target");
            b.add_task(task_input(
                "t-b",
                "the other board's row",
                vec!["beta".to_owned()],
                vec!["t-b2".to_owned()],
            ))
            .expect("seed task");
            b.raise_attention(
                "needs eyes",
                "blocking",
                "seed",
                Some("t-b"),
                3,
                &["beta".to_owned()],
                &DecisionCard::default(),
            )
            .expect("seed attention");
            b.add_note("t-b", "seed", "progress", "a note on the other board")
                .expect("seed note");
            b.create_handoff(handoff_input()).expect("seed handoff");
            drop(b);
        }

        // The caller's authority covers exactly board A.
        let authority = authority([
            (
                ScopeTuple::Board {
                    board_id: board_a.to_owned(),
                },
                Capability::Read,
            ),
            (
                ScopeTuple::Board {
                    board_id: board_a.to_owned(),
                },
                Capability::Write,
            ),
            (
                ScopeTuple::BoardWildcard {
                    board_id: board_a.to_owned(),
                },
                Capability::Read,
            ),
            (
                ScopeTuple::BoardWildcard {
                    board_id: board_a.to_owned(),
                },
                Capability::Write,
            ),
        ]);

        let mut store = Store::open_with_authz(
            &b_path,
            AuthzContext::new(Enforcement::Managed, authority, board_b.to_owned()),
        )
        .expect("open the other board under A-only authority");

        // Reads: no row of B is reachable.
        assert_denied(store.require_task("t-b"), "task show");
        assert_denied(store.list_tasks(None, None, None, None, false), "task list");
        assert_denied(
            store.list_tasks_with_claims(None, None, None, None, false),
            "task list --with-claims",
        );
        assert_denied(store.dependencies("t-b"), "relations dependencies");
        assert_denied(store.ancestors("t-b"), "relations ancestors");
        assert_denied(store.notes("t-b", 10), "note list");
        assert_denied(store.checkpoints("t-b", 10), "checkpoint list");
        assert_denied(store.sitreps(None, false, None, 10), "sitrep list");
        assert_denied(store.handoffs(None, None, None, 10, false), "handoff list");
        assert_denied(store.count_pending_handoffs(), "handoff count");
        assert_denied(
            store.attention(None, None, None, None, None, 10, false),
            "attention list",
        );
        assert_denied(store.count_open_attention(), "attention count");
        assert_denied(store.open_attentions("t-b"), "attention open-by-task");
        assert_denied(
            store.claim_candidates(&claim_options("agent"), None, 10),
            "claim candidates",
        );
        assert_denied(store.get_claim("t-b"), "claim show");
        assert_denied(store.rules(false), "rule list");
        assert_denied(store.tags(), "tag list");
        assert_denied(store.subscriptions(None, None, false), "subscription list");
        assert_denied(store.require_subscription("sub-x"), "subscription show");
        assert_denied(store.stale_tasks(), "stale task list");

        // Writes: no write to B succeeds.
        assert_denied(
            store.add_task(task_input("t-new", "new", vec![], vec![])),
            "task add",
        );
        assert_denied(
            store.move_task("t-b", "todo", "actor", serde_json::json!({}), false),
            "task move",
        );
        assert_denied(store.remove_task("t-b", "actor", false), "task remove");
        assert_denied(
            store.patch_metadata("t-b", serde_json::json!({"k": "v"}), "actor"),
            "task metadata",
        );
        assert_denied(
            store.update_task(
                "t-b",
                UpdateTask {
                    tags: Some(vec!["beta".to_owned()]),
                    ..Default::default()
                },
                "actor",
            ),
            "task update",
        );
        assert_denied(
            store.signoff_story("t-b", "actor", true, None),
            "story signoff",
        );
        assert_denied(
            store.advance_story("t-b", "actor", None, None, None),
            "story advance",
        );
        assert_denied(
            store.raise_attention(
                "body",
                "blocking",
                "actor",
                None,
                3,
                &[],
                &DecisionCard::default(),
            ),
            "attention raise",
        );
        assert_denied(
            store.update_attention(
                "a-x",
                None,
                Some(&["beta".to_owned()]),
                None,
                false,
                "actor",
            ),
            "attention update",
        );
        assert_denied(
            store.resolve_attention("a-x", "actor", &AttentionAnswer::custom("other", "done")),
            "attention resolve",
        );
        assert_denied(
            store.reopen_attention("a-x", "actor", "oops"),
            "attention reopen",
        );
        assert_denied(store.claim(Some("t-b"), claim_options("agent")), "claim");
        assert_denied(store.heartbeat("t-b", "token", 60_000, None), "heartbeat");
        assert_denied(store.release("t-b", "token", false), "release");
        assert_denied(
            store.add_note("t-b", "actor", "progress", "body"),
            "note add",
        );
        assert_denied(
            store.checkpoint(checkpoint_input("t-b", "actor")),
            "checkpoint add",
        );
        assert_denied(store.create_handoff(handoff_input()), "handoff create");
        assert_denied(
            store.accept_handoff(
                "h-x",
                AcceptHandoffOptions {
                    agent: "agent".into(),
                    session: None,
                    lease_ms: 60_000,
                    caller_scope: None,
                    sprint_override: None,
                    model: None,
                    git: None,
                },
            ),
            "handoff accept",
        );
        assert_denied(
            store.retire_handoff("h-x", "actor", "note"),
            "handoff retire",
        );
        assert_denied(store.add_tag("gamma", None, Some("actor")), "tag add");
        assert_denied(store.remove_tag("beta", Some("actor"), false), "tag remove");
        assert_denied(store.retire_rule("r-x", "actor"), "rule retire");
        assert_denied(
            store.add_subscription(tenant_subscription_input()),
            "subscription add",
        );
        assert_denied(
            store.pause_subscription("sub-x", "actor"),
            "subscription pause",
        );
        assert_denied(
            store.resume_subscription("sub-x", "actor"),
            "subscription resume",
        );
    }

    /// A readable board whose rows are not all readable: the task listings and
    /// the aggregates taken over them count only what this caller may see.
    ///
    /// Board read plus one tag's read is the authority; `secret` is held on
    /// neither capability. The row carrying BOTH tags is in the fixture on
    /// purpose: the read test is all-of-tag, so holding one of a row's two
    /// tags is not authority over the row, and a filter written as any-of
    /// would hand it over.
    ///
    /// The counts asserted are the dashboard's own arithmetic — a per-status
    /// tally and `len()` over `list_tasks` (`rust/lib.rs`, `taskCounts` and
    /// `totalTasks`) — so a listing that leaked a row would be caught here as
    /// a wrong aggregate as well as a wrong set.
    #[test]
    fn managed_task_listings_and_their_counts_exclude_a_tag_denied_row() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;

        let board = "dddddddd-6666-4666-8666-666666666666";
        let dir = std::env::temp_dir().join(format!("kanban-tag-listing-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create board dir");
        let path = dir.join(format!("{board}.db"));

        {
            let mut seed = Store::open(&path).expect("open seed board");
            seed.initialize("board", "seed").expect("init");
            seed.add_tag("visible", None, Some("seed")).expect("tag");
            seed.add_tag("secret", None, Some("seed")).expect("tag");
            seed.add_task(task_input(
                "t-visible",
                "visible row",
                vec!["visible".to_owned()],
                vec![],
            ))
            .expect("seed visible");
            seed.add_task(task_input(
                "t-visible-done",
                "visible finished row",
                vec!["visible".to_owned()],
                vec![],
            ))
            .expect("seed visible done");
            seed.move_task(
                "t-visible-done",
                "done",
                "seed",
                serde_json::json!({}),
                true,
            )
            .expect("finish the visible row");
            seed.add_task(task_input(
                "t-secret",
                "secret row",
                vec!["secret".to_owned()],
                vec![],
            ))
            .expect("seed secret");
            seed.add_task(task_input(
                "t-both",
                "row carrying both tags",
                vec!["visible".to_owned(), "secret".to_owned()],
                vec![],
            ))
            .expect("seed both");
        }

        let grants = authority([
            (
                ScopeTuple::Board {
                    board_id: board.to_owned(),
                },
                Capability::Read,
            ),
            (
                ScopeTuple::BoardTag {
                    board_id: board.to_owned(),
                    tag: "visible".to_owned(),
                },
                Capability::Read,
            ),
        ]);
        let store = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Managed, grants, board.to_owned()),
        )
        .expect("open under partial tag authority");

        // The control: the board itself is readable and the hidden row is not.
        assert_denied(store.require_task("t-secret"), "task show t-secret");
        assert_denied(store.require_task("t-both"), "task show t-both");
        store
            .require_task("t-visible")
            .expect("task show t-visible");

        let listed = store
            .list_tasks(None, None, None, None, false)
            .expect("task list");
        assert_eq!(
            listed
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["t-visible", "t-visible-done"],
            "the listing handed over a row this caller may not read"
        );
        let with_claims = store
            .list_tasks_with_claims(None, None, None, None, false)
            .expect("task list --with-claims");
        assert_eq!(
            with_claims
                .iter()
                .map(|(task, _)| task.id.as_str())
                .collect::<Vec<_>>(),
            ["t-visible", "t-visible-done"],
            "the claims listing handed over a row this caller may not read"
        );

        // Asking for the hidden tag by name is not a way round it either, and
        // the answer is the same empty listing a tag with no rows would give.
        assert!(
            store
                .list_tasks(None, Some("secret"), None, None, false)
                .expect("task list --tag secret")
                .is_empty(),
            "the tag filter named the hidden rows"
        );

        // The dashboard's own arithmetic over that listing.
        let count = |status: &str| listed.iter().filter(|task| task.status == status).count();
        assert_eq!(count("todo"), 1, "the visible open row is not counted");
        assert_eq!(count("done"), 1, "the visible finished row is not counted");
        assert_eq!(
            listed.len(),
            2,
            "the total counts rows this caller may not read"
        );
    }

    /// The same finding one surface along: the attention listings, the
    /// decisions view and the open count hand back only what this caller may
    /// read (`t-8efced5b`).
    ///
    /// Board read plus one tag's read is the authority; `secret` is held on
    /// neither capability. The row carrying BOTH tags is in the fixture on
    /// purpose: the read test is all-of-tag, so holding one of a row's two
    /// tags is not authority over the row, and a filter written as any-of
    /// would hand it over — question, body and choices together.
    ///
    /// The resolved secret row is there for `recent_resolved_attention`: a
    /// decision taken about a row this caller may not see is still about
    /// that row. `count_open_attention` is asserted beside the listing
    /// because it is a `COUNT(*)` that cannot see tags, and a number is a
    /// disclosure too.
    ///
    /// The direct estate is asserted unchanged in the same fixture: outside
    /// managed enforcement every row is still listed and still counted.
    #[test]
    fn managed_attention_listings_and_their_count_exclude_a_tag_denied_row() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;

        let board = "eeeeeeee-7777-4777-8777-777777777777";
        let dir = std::env::temp_dir().join(format!("kanban-tag-attention-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create board dir");
        let path = dir.join(format!("{board}.db"));

        let secret_open;
        let secret_resolved;
        {
            let mut seed = Store::open(&path).expect("open seed board");
            seed.initialize("board", "seed").expect("init");
            seed.add_tag("visible", None, Some("seed")).expect("tag");
            seed.add_tag("secret", None, Some("seed")).expect("tag");
            seed.add_task(task_input(
                "t-visible",
                "visible row",
                vec!["visible".to_owned()],
                vec![],
            ))
            .expect("seed visible");
            seed.raise_attention(
                "the visible question",
                "decision",
                "seed",
                Some("t-visible"),
                1,
                &["visible".to_owned()],
                &DecisionCard::default(),
            )
            .expect("raise visible");
            // More urgent than every readable row, so a SQL `LIMIT` bound
            // before the tag test would spend the whole page on it.
            secret_open = seed
                .raise_attention(
                    "the secret question",
                    "decision",
                    "seed",
                    None,
                    0,
                    &["secret".to_owned()],
                    &DecisionCard::default(),
                )
                .expect("raise secret")
                .id;
            for (body, priority) in [
                ("the second visible question", 2),
                ("the third visible question", 3),
            ] {
                seed.raise_attention(
                    body,
                    "decision",
                    "seed",
                    None,
                    priority,
                    &["visible".to_owned()],
                    &DecisionCard::default(),
                )
                .expect("raise another visible");
            }
            seed.raise_attention(
                "the question carrying both tags",
                "decision",
                "seed",
                None,
                1,
                &["visible".to_owned(), "secret".to_owned()],
                &DecisionCard::default(),
            )
            .expect("raise both");
            secret_resolved = seed
                .raise_attention(
                    "the secret decision",
                    "decision",
                    "seed",
                    None,
                    1,
                    &["secret".to_owned()],
                    &DecisionCard::default(),
                )
                .expect("raise secret decision")
                .id;
            // Decided FIRST, so the unreadable decision below is the newest
            // one and would take the whole page of a bound applied in SQL.
            let visible_resolved = seed
                .raise_attention(
                    "the visible decision",
                    "decision",
                    "seed",
                    None,
                    1,
                    &["visible".to_owned()],
                    &DecisionCard::default(),
                )
                .expect("raise visible decision")
                .id;
            seed.resolve_attention(
                &visible_resolved,
                "seed",
                &AttentionAnswer::custom("other", "settled"),
            )
            .expect("resolve the visible decision");
            std::thread::sleep(std::time::Duration::from_millis(2));
            seed.resolve_attention(
                &secret_resolved,
                "seed",
                &AttentionAnswer::custom("other", "settled"),
            )
            .expect("resolve the secret decision");
        }

        let grants = || {
            authority([
                (
                    ScopeTuple::Board {
                        board_id: board.to_owned(),
                    },
                    Capability::Read,
                ),
                (
                    ScopeTuple::BoardTag {
                        board_id: board.to_owned(),
                        tag: "visible".to_owned(),
                    },
                    Capability::Read,
                ),
            ])
        };
        let store = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Managed, grants(), board.to_owned()),
        )
        .expect("open under partial tag authority");

        let open = store
            .attention(Some("open"), None, None, None, None, 100, false)
            .expect("att list");
        assert_eq!(
            open.iter().map(|row| row.body.as_str()).collect::<Vec<_>>(),
            [
                "the visible question",
                "the second visible question",
                "the third visible question"
            ],
            "the attention listing handed over a row this caller may not read"
        );
        assert_eq!(
            store.count_open_attention().expect("count open attention"),
            3,
            "the open count counts rows this caller may not read"
        );

        // The bound is the READABLE rows'. The denied row is the most urgent
        // one on the board, so a `LIMIT` bound in SQL — before the tag test —
        // would spend this page on it and answer nothing, which is what the
        // dashboard's own `limit = 1` urgency read would then report.
        assert_eq!(
            store
                .attention(Some("open"), None, None, None, None, 1, false)
                .expect("the urgency read")
                .iter()
                .map(|row| row.body.as_str())
                .collect::<Vec<_>>(),
            ["the visible question"],
            "a denied row spent this caller's whole page"
        );
        assert_eq!(
            store
                .attention(Some("open"), None, None, None, None, 2, false)
                .expect("a bounded page")
                .len(),
            2,
            "a denied row spent a slot of this caller's page"
        );
        let decided = store
            .recent_resolved_attention(100)
            .expect("recent decisions");
        assert_eq!(
            decided
                .iter()
                .map(|row| row.body.as_str())
                .collect::<Vec<_>>(),
            ["the visible decision"],
            "the decisions view handed over a decision about a row this caller may not read"
        );
        // The decisions room's scan is bounded per board before the merge, so
        // the newest decision being unreadable must not cost the readable one
        // its place in the scan.
        assert_eq!(
            store
                .recent_resolved_attention(1)
                .expect("the bounded scan")
                .iter()
                .map(|row| row.body.as_str())
                .collect::<Vec<_>>(),
            ["the visible decision"],
            "an unreadable decision spent this caller's whole scan"
        );

        // Naming the row is not a way round the listing: the hover preview's
        // named read is a `find` over this same bounded listing
        // (`projection::preview`), so the id it would resolve is absent, and
        // the tag filter answers the empty listing an unused tag would.
        let bounded = store
            .attention(None, None, None, None, None, 500, false)
            .expect("the preview's bounded listing");
        assert!(
            !bounded
                .iter()
                .any(|row| row.id == secret_open || row.id == secret_resolved),
            "a named read of the hidden rows would have resolved"
        );
        assert!(
            store
                .attention(None, None, None, Some("secret"), None, 100, false)
                .expect("att list --tag secret")
                .is_empty(),
            "the tag filter named the hidden rows"
        );

        // The direct estate is untouched: the same partial authority sees
        // every row, because `permits_read` cannot refuse outside managed.
        let relaxed = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Direct, grants(), board.to_owned()),
        )
        .expect("open the direct estate");
        assert_eq!(
            relaxed
                .attention(Some("open"), None, None, None, None, 100, false)
                .expect("att list")
                .len(),
            5,
            "the direct estate stopped listing its own rows"
        );
        assert_eq!(
            relaxed
                .count_open_attention()
                .expect("count open attention"),
            5,
            "the direct estate stopped counting its own rows"
        );
        // Its bound is still the raw one SQL applies: the most urgent row on
        // the board is the one no managed caller may read, and the direct
        // estate reads exactly what it asked for.
        assert_eq!(
            relaxed
                .attention(Some("open"), None, None, None, None, 1, false)
                .expect("the urgency read")
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [secret_open.as_str()],
            "the direct estate's own bound changed"
        );
        assert_eq!(
            relaxed
                .recent_resolved_attention(100)
                .expect("recent decisions")
                .len(),
            2,
            "the direct estate stopped reporting its own decisions"
        );
        assert_eq!(
            relaxed
                .recent_resolved_attention(1)
                .expect("the bounded scan")
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [secret_resolved.as_str()],
            "the direct estate's own scan bound changed"
        );
    }

    /// The same finding one surface further along again: the EVENT TAIL of a
    /// readable task withholds the envelopes of an attention row this caller
    /// may not read (`t-1de9c707`).
    ///
    /// The leak was that `event_tags` read the TASK's tags and the event's
    /// own `_semanticV1` snapshot, and an attention event's subject is
    /// neither: a `secret` question raised against a `visible` task produced
    /// `attention_raised` and `attention_resolved` envelopes carrying the
    /// row's id, kind, tags and choices, and those sailed through
    /// `visible_events` on the task's `visible` tag alone. Every event tail
    /// in the product — `kb ev`, MCP, `/api/v1/task` and watch — reads
    /// through that one filter.
    ///
    /// The control is in the same fixture and on the same task: a `visible`
    /// attention row's envelopes are still there, so a filter that had simply
    /// dropped every attention event would fail here. The direct estate is
    /// asserted unchanged for the same reason it is in the listings fixture.
    ///
    /// The secret row is then REOPENED and RETAGGED to `visible` only, which
    /// the store permits a full-authority caller, so its live `attention_tags`
    /// no longer carry the tag that hid it. That is the case the live read
    /// alone cannot answer: an all-of-tag test over a union is the
    /// intersection of the two rights only while both sets are there, so the
    /// resolve and the reopen envelopes have to carry their own `tags`
    /// snapshot or they fall back to the TASK's `visible` and hand over a row
    /// this caller could not see when those events were written.
    #[test]
    fn managed_event_tails_exclude_a_tag_denied_attention_rows_envelopes() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;

        let board = "eeeeeeee-8888-4888-8888-888888888888";
        let dir = std::env::temp_dir().join(format!("kanban-tag-attention-ev-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create board dir");
        let path = dir.join(format!("{board}.db"));

        let secret;
        let visible;
        {
            let mut seed = Store::open(&path).expect("open seed board");
            seed.initialize("board", "seed").expect("init");
            seed.add_tag("visible", None, Some("seed")).expect("tag");
            seed.add_tag("secret", None, Some("seed")).expect("tag");
            seed.add_task(task_input(
                "t-visible",
                "visible row",
                vec!["visible".to_owned()],
                vec![],
            ))
            .expect("seed visible");
            let mut raise = |body: &str, tag: &str| {
                seed.raise_attention(
                    body,
                    "decision",
                    "seed",
                    Some("t-visible"),
                    1,
                    &[tag.to_owned()],
                    &DecisionCard::default(),
                )
                .expect("raise")
                .id
            };
            visible = raise("the visible question", "visible");
            secret = raise("the secret question", "secret");
            for id in [&visible, &secret] {
                seed.resolve_attention(id, "seed", &AttentionAnswer::custom("other", "settled"))
                    .expect("resolve");
            }
            // The retag the fix has to survive: a full-authority caller
            // reopens the settled secret row and moves it onto `visible`,
            // so nothing live carries `secret` for it any more. Only the
            // envelopes' own snapshots still say what it was.
            seed.reopen_attention(&secret, "seed", "reopened to retag")
                .expect("reopen the secret row");
            seed.update_attention(
                &secret,
                None,
                Some(&["visible".to_owned()]),
                None,
                false,
                "seed",
            )
            .expect("retag the secret row onto visible");
            assert_eq!(
                attention_tags(&seed.connection, &secret).expect("the retagged row's live tags"),
                vec!["visible".to_owned()],
                "the fixture did not actually strip the tag that hid the row"
            );
        }

        let grants = || {
            authority([
                (
                    ScopeTuple::Board {
                        board_id: board.to_owned(),
                    },
                    Capability::Read,
                ),
                (
                    ScopeTuple::BoardTag {
                        board_id: board.to_owned(),
                        tag: "visible".to_owned(),
                    },
                    Capability::Read,
                ),
            ])
        };
        // The subject ids each envelope names, per event kind.
        let subjects = |store: &Store, kind: &str| -> Vec<String> {
            store
                .events(Some("t-visible"), Some(kind), 100, true)
                .unwrap_or_else(|error| panic!("events {kind}: {error}"))
                .iter()
                .filter_map(|event| {
                    event
                        .payload
                        .pointer("/attentionID")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        };

        let store = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Managed, grants(), board.to_owned()),
        )
        .expect("open under partial tag authority");
        // The task itself is readable, so this is the tail of a row this
        // caller MAY see — the denial has to be per event, not per task.
        store
            .require_task("t-visible")
            .expect("task show t-visible");
        for kind in ["attention_raised", "attention_resolved"] {
            assert_eq!(
                subjects(&store, kind),
                vec![visible.clone()],
                "the {kind} tail named an attention row this caller may not read"
            );
        }
        // Only the retagged secret row was reopened and updated, and its
        // live tags are `visible` now — these two are the envelopes that
        // survive on their payload snapshot alone.
        for kind in ["attention_reopened", "attention_updated"] {
            assert!(
                subjects(&store, kind).is_empty(),
                "the {kind} tail survived the retag and named the row this caller may not read"
            );
        }
        // And nothing the hidden row carries is anywhere in the whole tail:
        // an envelope's body, tags and choices name the row as surely as its
        // id does.
        let tail = store
            .events(Some("t-visible"), None, 100, true)
            .expect("the task's whole tail")
            .iter()
            .map(|event| event.payload.to_string())
            .collect::<String>();
        assert!(
            !tail.contains(secret.as_str()) && !tail.contains("secret"),
            "the event tail carried the hidden row: {tail}"
        );
        assert!(
            tail.contains(visible.as_str()),
            "the event tail lost the attention row this caller MAY read: {tail}"
        );

        // The direct estate is untouched: the same partial authority still
        // reads every envelope, because `permits_read` cannot refuse outside
        // managed enforcement.
        let relaxed = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Direct, grants(), board.to_owned()),
        )
        .expect("open the direct estate");
        for kind in ["attention_raised", "attention_resolved"] {
            assert_eq!(
                subjects(&relaxed, kind),
                vec![secret.clone(), visible.clone()],
                "the direct estate stopped replaying its own {kind} envelopes"
            );
        }
        for kind in ["attention_reopened", "attention_updated"] {
            assert_eq!(
                subjects(&relaxed, kind),
                vec![secret.clone()],
                "the direct estate stopped replaying its own {kind} envelopes"
            );
        }
    }

    /// The relations of a READABLE row: a prerequisite the caller may not read
    /// leaves the dependency listing, keeps its gate, and loses its title
    /// there; an ancestor it may not read truncates the chain instead of
    /// refusing the whole row (`t-3548303e`).
    ///
    /// The three answers are deliberately different because the three
    /// questions are:
    ///
    /// * `dependencies` ENUMERATES rows, so it obeys the listing rule
    ///   `t-34f6eed5` set — the denied row is absent, exactly as it is absent
    ///   from `list_tasks`.
    /// * `blocking_gates` is the one read every refusal is taken through, so
    ///   the denied prerequisite must stay: dropped, a row whose every claim
    ///   the gate refuses would report itself ungated. Its id and status are
    ///   its function and stay; its title is prose and goes.
    /// * `ancestors` is a CHAIN, so neither dropping (a hole misreports which
    ///   plan owns which) nor refusing (one denied epic hides a readable leaf,
    ///   which is what `context` did) is right: it stops at the boundary.
    ///
    /// Every assertion is paired with its positive control — the readable
    /// prerequisite `t-open` is present in full, with its title, in all three
    /// answers — so a reader that had simply broken would fail here too.
    /// Remove either `retain` line or the redaction and this test fails.
    #[test]
    fn managed_relations_drop_a_tag_denied_prerequisite_but_keep_its_gate() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;

        let board = "dddddddd-7777-4777-8777-777777777777";
        let dir = std::env::temp_dir().join(format!("kanban-tag-relations-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create board dir");
        let path = dir.join(format!("{board}.db"));

        {
            let mut seed = Store::open(&path).expect("open seed board");
            seed.initialize("board", "seed").expect("init");
            seed.add_tag("visible", None, Some("seed")).expect("tag");
            seed.add_tag("secret", None, Some("seed")).expect("tag");

            let mut epic = task_input("t-epic", "secret epic", vec!["secret".to_owned()], vec![]);
            epic.task_type = "epic".to_owned();
            seed.add_task(epic).expect("seed epic");
            seed.add_task(task_input(
                "t-secret",
                "secret prerequisite",
                vec!["secret".to_owned()],
                vec![],
            ))
            .expect("seed secret prerequisite");
            seed.add_task(task_input(
                "t-open",
                "open prerequisite",
                vec!["visible".to_owned()],
                vec![],
            ))
            .expect("seed open prerequisite");

            let mut story = task_input(
                "t-visible",
                "visible row",
                vec!["visible".to_owned()],
                vec!["t-secret".to_owned(), "t-open".to_owned()],
            );
            story.task_type = "story".to_owned();
            story.parent_id = Some("t-epic".to_owned());
            seed.add_task(story).expect("seed visible row");

            let mut child =
                task_input("t-child", "secret child", vec!["secret".to_owned()], vec![]);
            child.parent_id = Some("t-visible".to_owned());
            seed.add_task(child).expect("seed secret child");
            let mut leaf = task_input("t-leaf", "visible leaf", vec!["visible".to_owned()], vec![]);
            leaf.parent_id = Some("t-visible".to_owned());
            seed.add_task(leaf).expect("seed visible leaf");
        }

        let grants = authority([
            (
                ScopeTuple::Board {
                    board_id: board.to_owned(),
                },
                Capability::Read,
            ),
            (
                ScopeTuple::BoardTag {
                    board_id: board.to_owned(),
                    tag: "visible".to_owned(),
                },
                Capability::Read,
            ),
        ]);
        let store = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Managed, grants, board.to_owned()),
        )
        .expect("open under partial tag authority");

        // The control: the row whose relations are read IS readable, and the
        // rows hanging off it are not.
        store
            .require_task("t-visible")
            .expect("task show t-visible");
        for hidden in ["t-secret", "t-epic", "t-child"] {
            assert_denied(store.require_task(hidden), &format!("task show {hidden}"));
        }

        // 1. The dependency listing: the denied prerequisite is absent, the
        //    readable one is there in full.
        let dependencies = store.dependencies("t-visible").expect("dependencies");
        assert_eq!(
            dependencies
                .iter()
                .map(|task| (task.id.as_str(), task.title.as_str()))
                .collect::<Vec<_>>(),
            [("t-open", "open prerequisite")],
            "the dependency listing handed over a prerequisite this caller may not read"
        );

        // 2. The gate: BOTH prerequisites, because the gate is what refuses
        //    the claim — with the denied one's title blanked and nothing else.
        let gates = store.blocking_gates("t-visible").expect("blocking gates");
        assert_eq!(
            gates
                .iter()
                .map(|gate| (
                    gate.source_task_id.as_str(),
                    gate.prerequisite_id.as_str(),
                    gate.prerequisite_title.as_deref(),
                    gate.prerequisite_status.as_str(),
                ))
                .collect::<Vec<_>>(),
            [
                ("t-visible", "t-open", Some("open prerequisite"), "todo"),
                ("t-visible", "t-secret", None, "todo"),
            ],
            "the gate must keep the denied prerequisite and lose only its title"
        );
        // The listing form answers identically, including for a row that
        // INHERITS the gate from the readable story above it.
        let listed_gates = store
            .blocking_gates_for(&["t-visible".to_owned(), "t-leaf".to_owned()])
            .expect("blocking gates for a listing");
        assert_eq!(
            listed_gates[0], gates,
            "the listing gate differs from the row's"
        );
        assert_eq!(
            listed_gates[1]
                .iter()
                .map(|gate| (
                    gate.source_task_id.as_str(),
                    gate.prerequisite_id.as_str(),
                    gate.prerequisite_title.as_deref(),
                ))
                .collect::<Vec<_>>(),
            [
                ("t-visible", "t-open", Some("open prerequisite")),
                ("t-visible", "t-secret", None),
            ],
            "the inherited gate must read the same way"
        );

        // 3. The chain: truncated at the denied epic, never refused, and the
        //    readable run nearest the row is intact.
        assert!(
            store.ancestors("t-visible").expect("ancestors").is_empty(),
            "a denied ancestor must not be in the chain"
        );
        assert_eq!(
            store
                .ancestors("t-leaf")
                .expect("the chain of a readable leaf must answer, not refuse")
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["t-visible"],
            "the chain must keep the readable run and stop at the boundary"
        );

        // 4. The context packet reads all three through the store, so it is
        //    true by construction — asserted because it is the surface the
        //    finding was filed against.
        let packet = store.context_packet("t-visible").expect("context packet");
        assert!(
            packet.ancestors.is_empty(),
            "the packet leaked the denied epic"
        );
        assert_eq!(
            packet
                .dependencies
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["t-open"],
            "the packet leaked the denied prerequisite"
        );
        assert_eq!(
            packet.blocking_gates, gates,
            "the packet's gate differs from the store's"
        );

        // 5. The same board read with full authority: every title is there.
        //    Outside `Managed` nothing is filtered and nothing is blanked, so
        //    the direct estate's answer is unchanged.
        let direct = Store::open(&path).expect("open direct");
        assert_eq!(
            direct
                .dependencies("t-visible")
                .expect("direct dependencies")
                .len(),
            2,
            "the direct estate must still see both prerequisites"
        );
        assert_eq!(
            direct
                .blocking_gates("t-visible")
                .expect("direct gate")
                .iter()
                .map(|gate| gate.prerequisite_title.as_deref())
                .collect::<Vec<_>>(),
            [Some("open prerequisite"), Some("secret prerequisite")],
            "the direct estate must still see both titles"
        );
        assert_eq!(
            direct
                .ancestors("t-visible")
                .expect("direct ancestors")
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["t-epic"],
            "the direct estate must still see the whole chain"
        );
    }

    /// A caller who can see a task's old tag set but not the resulting one is
    /// denied a retag, and the row is byte-unchanged afterwards — the write did
    /// not partially apply.
    #[test]
    fn retag_denied_when_only_the_old_tags_are_visible_and_the_row_is_unchanged() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;

        let board = "cccccccc-3333-4333-8333-333333333333";
        let dir = std::env::temp_dir().join(format!("kanban-retag-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create retag dir");
        let path = dir.join(format!("{board}.db"));

        {
            let mut s = Store::open(&path).expect("open seed board");
            s.initialize("retag", "seed").expect("init");
            s.add_tag("alpha", None, Some("seed")).expect("seed alpha");
            s.add_tag("beta", None, Some("seed")).expect("seed beta");
            s.add_task(task_input(
                "t-r",
                "the row",
                vec!["alpha".to_owned()],
                vec![],
            ))
            .expect("seed task");
            drop(s);
        }

        // The caller holds write on the board and on `alpha` (the old tag),
        // but NOT on `beta` (the resulting tag).
        let authority = authority([
            (
                ScopeTuple::Board {
                    board_id: board.to_owned(),
                },
                Capability::Write,
            ),
            (
                ScopeTuple::BoardTag {
                    board_id: board.to_owned(),
                    tag: "alpha".to_owned(),
                },
                Capability::Write,
            ),
        ]);

        let mut store = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Managed, authority, board.to_owned()),
        )
        .expect("open under managed authority");

        let error = store
            .update_task(
                "t-r",
                UpdateTask {
                    tags: Some(vec!["beta".to_owned()]),
                    ..Default::default()
                },
                "actor",
            )
            .expect_err("a retag to an unseen tag must be denied");
        assert_eq!(error.to_string(), "denied or not found");

        // The row is unchanged: still tagged alpha, still todo, same title.
        let direct = Store::open(&path).expect("reopen directly");
        let task = direct.require_task("t-r").expect("row still exists");
        assert_eq!(task.tags, vec!["alpha".to_owned()]);
        assert_eq!(task.status, "todo");
        assert_eq!(task.title, "the row");
    }

    /// The same both-scopes rule at the other tagged surface: `attention_tags`
    /// is a separate table from `task_tags`, so a retag there is a separate
    /// call site with its own old/resulting computation.
    #[test]
    fn attention_retag_denied_when_only_the_old_tags_are_visible_and_the_row_is_unchanged() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;

        let board = "dddddddd-4444-4444-8444-444444444444";
        let dir = std::env::temp_dir().join(format!("kanban-att-retag-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create attention retag dir");
        let path = dir.join(format!("{board}.db"));

        let raised = {
            let mut s = Store::open(&path).expect("open seed board");
            s.initialize("attention-retag", "seed").expect("init");
            s.add_tag("alpha", None, Some("seed")).expect("seed alpha");
            s.add_tag("beta", None, Some("seed")).expect("seed beta");
            s.raise_attention(
                "needs eyes",
                "decision",
                "seed",
                None,
                3,
                &["alpha".to_owned()],
                &DecisionCard::default(),
            )
            .expect("seed attention")
        };
        assert_eq!(raised.tags, vec!["alpha".to_owned()]);

        // Write on the board and on `alpha` (the old tag), never on `beta`.
        let authority = authority([
            (
                ScopeTuple::Board {
                    board_id: board.to_owned(),
                },
                Capability::Write,
            ),
            (
                ScopeTuple::BoardTag {
                    board_id: board.to_owned(),
                    tag: "alpha".to_owned(),
                },
                Capability::Write,
            ),
        ]);

        let mut store = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Managed, authority, board.to_owned()),
        )
        .expect("open under managed authority");

        let error = store
            .update_attention(
                &raised.id,
                Some("rewritten"),
                Some(&["beta".to_owned()]),
                None,
                false,
                "actor",
            )
            .expect_err("a retag to an unseen tag must be denied");
        assert_eq!(error.to_string(), "denied or not found");

        // Neither the tags nor the body moved: the write did not partly apply.
        let direct = Store::open(&path).expect("reopen directly");
        let after = direct
            .attention(None, None, None, None, None, 10, false)
            .expect("read attention directly");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].tags, vec!["alpha".to_owned()]);
        assert_eq!(after[0].body, "needs eyes");
        assert_eq!(after[0].status, "open");
    }

    #[test]
    fn events_with_bounds_respect_half_open_bounds_limit_and_archive_visibility() {
        let store = test_store("events-with-bounds");
        insert_task(&store, "t-events");

        for (kind, created_at) in [
            ("task_created", 10_i64),
            ("task_updated", 20_i64),
            ("task_moved", 30_i64),
            ("task_finished", 40_i64),
            ("task_removed", 100_i64),
        ] {
            crate::audit::append_board_event(
                &store.connection,
                Some("t-events"),
                kind,
                "codex",
                "{}",
                created_at,
            )
            .expect("append event");
        }
        store
            .connection
            .execute("UPDATE events SET archived=1 WHERE seq=5", [])
            .unwrap();

        let exact_bounds = store
            .events_with_bounds(None, None, Some(20), Some(40), 10, false)
            .expect("read half-open bounded events");
        assert_eq!(board_event_seqs(&exact_bounds), vec![3, 2]);

        let equal_bounds = store
            .events_with_bounds(None, None, Some(30), Some(30), 10, false)
            .expect("read equal bounded events");
        assert!(board_event_seqs(&equal_bounds).is_empty());

        let after_only = store
            .events_with_bounds(None, None, Some(30), None, 10, true)
            .expect("read after-only bounded events");
        assert_eq!(board_event_seqs(&after_only), vec![5, 4, 3]);

        let before_only = store
            .events_with_bounds(None, None, None, Some(40), 10, true)
            .expect("read before-only bounded events");
        assert_eq!(board_event_seqs(&before_only), vec![3, 2, 1]);

        let bounded_limit = store
            .events_with_bounds(None, None, Some(15), Some(45), 1, false)
            .expect("read limit-bounded events");
        assert_eq!(board_event_seqs(&bounded_limit), vec![4]);

        let active = store
            .events_with_bounds(None, None, None, None, 10, false)
            .expect("read active events");
        assert_eq!(board_event_seqs(&active), vec![4, 3, 2, 1]);

        let all = store.events(None, None, 10, true).expect("read all events");
        assert_eq!(board_event_seqs(&all), vec![5, 4, 3, 2, 1]);
    }

    #[test]
    fn events_with_bounds_rejects_negative_and_reversed_bounds() {
        let store = test_store("events-with-bounds-validation");

        let negative_after = store
            .events_with_bounds(None, None, Some(-1), None, 10, false)
            .expect_err("negative after must be rejected")
            .to_string();
        assert_eq!(negative_after, "--after must be non-negative");

        let negative_before = store
            .events_with_bounds(None, None, None, Some(-1), 10, false)
            .expect_err("negative before must be rejected")
            .to_string();
        assert_eq!(negative_before, "--before must be non-negative");

        let reversed = store
            .events_with_bounds(None, None, Some(40), Some(30), 10, false)
            .expect_err("reversed bounds must be rejected")
            .to_string();
        assert_eq!(reversed, "--after must not be later than --before");
    }

    #[test]
    fn trusted_edge_resolution_records_the_edge_actor_without_changing_cli_rules() {
        let mut store = test_store("trusted-edge-resolution");
        store.initialize("TRUSTED", "geoyws").unwrap();
        insert_task(&store, "t-web");
        insert_task(&store, "t-cli");
        insert_task(&store, "t-forbidden");

        let web_attention = store
            .raise_attention(
                "Resolve from the trusted edge.",
                "decision",
                "geoyws",
                Some("t-web"),
                0,
                &[],
                &DecisionCard::default(),
            )
            .expect("raise web attention");
        let cli_attention = store
            .raise_attention(
                "CLI still owns this one.",
                "decision",
                "ifca-sso",
                Some("t-cli"),
                0,
                &[],
                &DecisionCard::default(),
            )
            .expect("raise cli attention");
        let forbidden_attention = store
            .raise_attention(
                "Geo still owns this other one.",
                "decision",
                "geoyws",
                Some("t-forbidden"),
                0,
                &[],
                &DecisionCard::default(),
            )
            .expect("raise forbidden attention");

        let resolved = store
            .resolve_attention_from_trusted_edge(
                &web_attention.id,
                "ifca-sso",
                &AttentionAnswer::custom("other", "done"),
            )
            .expect("trusted edge resolution");
        assert_eq!(resolved.resolved_by.as_deref(), Some("ifca-sso"));

        let resolved_events = store
            .events(Some("t-web"), Some("attention_resolved"), 10, true)
            .expect("read resolved web event");
        assert_eq!(resolved_events.len(), 1);
        assert_eq!(resolved_events[0].actor.as_deref(), Some("ifca-sso"));

        let cli_resolved = store
            .resolve_attention(
                &cli_attention.id,
                "ifca-sso",
                &AttentionAnswer::custom("other", "done"),
            )
            .expect("ordinary cli resolution");
        assert_eq!(cli_resolved.resolved_by.as_deref(), Some("ifca-sso"));

        let cli_events = store
            .events(Some("t-cli"), Some("attention_resolved"), 10, true)
            .expect("read resolved cli event");
        assert_eq!(cli_events.len(), 1);
        assert_eq!(cli_events[0].actor.as_deref(), Some("ifca-sso"));

        let forbidden = store
            .resolve_attention(
                &forbidden_attention.id,
                "ifca-sso",
                &AttentionAnswer::custom("other", "not allowed"),
            )
            .expect_err("CLI resolve must still reject a non-geoyws, non-raiser actor")
            .to_string();
        assert!(
            forbidden.contains("only geoyws or that same raiser may resolve"),
            "{forbidden}"
        );
    }

    #[test]
    fn production_query_shapes_use_their_covering_indexes_without_force_hints() {
        let store = test_store("query-plan-shapes");

        for index in 0_i64..32 {
            let id = format!("task-{index:02}");
            insert_task(&store, &id);
            store
                .connection
                .execute(
                    "UPDATE tasks SET priority=?,created_at=?,updated_at=? WHERE id=?",
                    params![index % 10, 10_000 + index, 10_000 + index, id],
                )
                .unwrap();
        }

        for (kind, created_at) in [
            ("task_created", 10_i64),
            ("task_updated", 20_i64),
            ("task_moved", 30_i64),
            ("task_finished", 40_i64),
            ("task_removed", 100_i64),
        ] {
            crate::audit::append_board_event(
                &store.connection,
                Some("task-00"),
                kind,
                "codex",
                "{}",
                created_at,
            )
            .expect("append planning event");
        }
        for offset in 0_i64..256 {
            crate::audit::append_board_event(
                &store.connection,
                Some("task-00"),
                "task_updated",
                "codex",
                "{}",
                1_000 + offset,
            )
            .expect("append planning tail event");
        }

        store.connection.execute_batch("ANALYZE").unwrap();

        let task_plan = query_plan_details(
            &store.connection,
            "SELECT * FROM tasks WHERE 1=1 ORDER BY priority,created_at,id",
            params![],
        );
        let task_plan_text = task_plan.join("\n");
        assert!(
            task_plan_text.contains("idx_tasks_priority_created_id"),
            "{task_plan_text}"
        );
        assert!(
            !task_plan_text.contains("USE TEMP B-TREE"),
            "{task_plan_text}"
        );

        let event_plan = query_plan_details(
            &store.connection,
            "SELECT * FROM events WHERE 1=1 AND created_at>=? AND created_at<? AND archived=0 ORDER BY seq DESC LIMIT ?",
            params![1_240_i64, 1_248_i64, 1_i64],
        );
        let event_plan_text = event_plan.join("\n");
        assert!(
            event_plan_text.contains("idx_events_created_seq"),
            "{event_plan_text}"
        );
        assert!(
            event_plan_text.contains("created_at>") || event_plan_text.contains("created_at<"),
            "{event_plan_text}"
        );
    }

    #[test]
    fn a_completion_gate_is_inherited_from_every_ancestor_and_only_done_clears_it() {
        let mut store = test_store("gate-inheritance");
        for (id, task_type, parent) in [
            ("e-root", "epic", None),
            ("e-phase", "epic", Some("e-root")),
            ("t-leaf", "task", Some("e-phase")),
            ("t-root-prereq", "task", None),
            ("t-phase-prereq", "task", None),
            ("t-own-prereq", "task", None),
            ("t-archived-prereq", "task", None),
            ("t-far", "task", None),
            ("t-independent", "task", None),
        ] {
            store.add_task(gate_row(id, task_type, parent)).unwrap();
        }
        store
            .update_task("e-root", dependency_update(&["t-root-prereq"]), "seed")
            .unwrap();
        store
            .update_task("e-phase", dependency_update(&["t-phase-prereq"]), "seed")
            .unwrap();
        store
            .update_task(
                "t-leaf",
                dependency_update(&["t-own-prereq", "t-archived-prereq"]),
                "seed",
            )
            .unwrap();
        // A prerequisite with a prerequisite of its own: whether `t-far` is
        // finished is `t-own-prereq`'s business, and reporting it as a blocker
        // on the leaf would name a row the leaf is not waiting for.
        store
            .update_task("t-own-prereq", dependency_update(&["t-far"]), "seed")
            .unwrap();

        assert_eq!(
            gate_triples(&store, "t-leaf"),
            [
                ("t-leaf", "t-archived-prereq", "todo"),
                ("t-leaf", "t-own-prereq", "todo"),
                ("e-phase", "t-phase-prereq", "todo"),
                ("e-root", "t-root-prereq", "todo"),
            ]
            .map(|(owner, prerequisite, status)| (
                owner.to_owned(),
                prerequisite.to_owned(),
                status.to_owned()
            ))
        );
        assert!(gate_triples(&store, "t-independent").is_empty());
        // The gate is the only thing standing in the way, and an unrelated row
        // is still claimable while the gated tree is not.
        store
            .claim(Some("t-independent"), claim_options("driver-2"))
            .unwrap();
        // The queue's set-shaped gate and the per-row one are two expressions
        // of the same rule, so they have to agree: anything offered has no
        // blockers, and the gated tree is never offered.
        let candidates = store
            .claim_candidates(&claim_options("driver-9"), None, 50)
            .unwrap();
        assert!(!candidates.is_empty());
        for candidate in &candidates {
            assert!(
                gate_triples(&store, &candidate.id).is_empty(),
                "{} was offered while gated",
                candidate.id
            );
        }
        assert!(!candidates.iter().any(|row| row.id == "t-leaf"));

        // A done prerequisite clears; a cancelled one does not, because a
        // decision not to do the work is not the work being finished; and a
        // done row archived into cold history still counts as finished.
        store
            .move_task("t-phase-prereq", "done", "seed", json!({}), false)
            .unwrap();
        store
            .move_task("t-root-prereq", "cancelled", "seed", json!({}), false)
            .unwrap();
        store
            .move_task("t-archived-prereq", "done", "seed", json!({}), false)
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE tasks SET archived=1,archived_at=1 WHERE id='t-archived-prereq'",
                [],
            )
            .unwrap();
        assert_eq!(
            gate_triples(&store, "t-leaf"),
            [
                (
                    "t-leaf".to_owned(),
                    "t-own-prereq".to_owned(),
                    "todo".to_owned()
                ),
                (
                    "e-root".to_owned(),
                    "t-root-prereq".to_owned(),
                    "cancelled".to_owned()
                ),
            ]
        );
        let refusal = store
            .claim(Some("t-leaf"), claim_options("driver-3"))
            .unwrap_err()
            .to_string();
        for needle in [
            "t-leaf",
            "t-own-prereq",
            "t-root-prereq",
            "cancelled",
            "ancestor e-root",
        ] {
            assert!(refusal.contains(needle), "{needle} missing from {refusal}");
        }

        // A reopened prerequisite gates future work again.
        store
            .move_task("t-phase-prereq", "todo", "seed", json!({}), false)
            .unwrap();
        assert!(gate_triples(&store, "t-leaf").contains(&(
            "e-phase".to_owned(),
            "t-phase-prereq".to_owned(),
            "todo".to_owned()
        )));
    }

    #[test]
    fn a_gate_nothing_could_satisfy_is_refused_on_dependency_and_on_parent_changes() {
        let mut store = test_store("gate-deadlock");
        for (id, task_type, parent) in [
            ("e-parent", "epic", None),
            ("t-inside", "task", Some("e-parent")),
            ("e-gated", "epic", None),
            ("t-external", "task", None),
            ("e-one", "epic", None),
            ("t-a", "task", Some("e-one")),
            ("e-two", "epic", None),
            ("t-b", "task", Some("e-two")),
        ] {
            store.add_task(gate_row(id, task_type, parent)).unwrap();
        }

        // An epic gated on a task inside its own subtree: the descendant
        // inherits the gate, so finishing the prerequisite requires work the
        // gate forbids. No dependency-only cycle exists here at all.
        let own = store
            .update_task("e-parent", dependency_update(&["t-inside"]), "seed")
            .unwrap_err()
            .to_string();
        assert!(
            own.contains("e-parent") && own.contains("t-inside"),
            "{own}"
        );
        assert!(store.dependencies("e-parent").unwrap().is_empty());

        // The same deadlock built from the parent side, with the dependency
        // edge already legal when it was declared.
        store
            .update_task("e-gated", dependency_update(&["t-external"]), "seed")
            .unwrap();
        let reparented = store
            .update_task(
                "t-external",
                UpdateTask {
                    parent_id: Some(Some("e-gated".to_owned())),
                    ..Default::default()
                },
                "seed",
            )
            .unwrap_err()
            .to_string();
        assert!(
            reparented.contains("e-gated") && reparented.contains("t-external"),
            "{reparented}"
        );
        assert_eq!(store.require_task("t-external").unwrap().parent_id, None);

        // A dependency across two trees is ordinary sequencing, not a
        // deadlock, and stays accepted.
        store
            .update_task("t-a", dependency_update(&["t-b"]), "seed")
            .unwrap();
        assert_eq!(
            store
                .dependencies("t-a")
                .unwrap()
                .into_iter()
                .map(|task| task.id)
                .collect::<Vec<_>>(),
            ["t-b".to_owned()]
        );
    }

    #[test]
    fn the_gated_task_count_is_unfinished_unarchived_leaf_rows_only() {
        let mut store = test_store("gate-count");
        for (id, task_type, parent) in [
            ("e-plan", "epic", None),
            ("t-waiting", "task", Some("e-plan")),
            ("t-settled", "task", Some("e-plan")),
            ("t-direct", "task", None),
            ("t-plan-prereq", "task", None),
        ] {
            store.add_task(gate_row(id, task_type, parent)).unwrap();
        }
        // Finished before the gate exists, because finishing under a live gate
        // is precisely what the gate refuses.
        store
            .move_task("t-settled", "done", "seed", json!({}), false)
            .unwrap();
        store
            .update_task("e-plan", dependency_update(&["t-plan-prereq"]), "seed")
            .unwrap();
        store
            .update_task("t-direct", dependency_update(&["t-plan-prereq"]), "seed")
            .unwrap();

        // `t-waiting` inherits the plan's gate and `t-direct` declared its
        // own. The gated epic itself is not counted — no lease is ever granted
        // on a container — and neither is the settled row or the prerequisite.
        assert_eq!(store.count_gated_tasks().unwrap(), 2);

        store
            .connection
            .execute(
                "UPDATE tasks SET archived=1,archived_at=1 WHERE id='t-waiting'",
                [],
            )
            .unwrap();
        assert_eq!(store.count_gated_tasks().unwrap(), 1);

        store
            .move_task("t-direct", "cancelled", "seed", json!({}), false)
            .unwrap();
        assert_eq!(store.count_gated_tasks().unwrap(), 0);
    }

    #[test]
    fn dispatcher_open_does_not_sweep_unrelated_expired_task_claims() {
        let dispatcher_path = expired_claim_board("dispatcher-open-preserves-expired-claim");
        let dispatcher = Store::open_for_dispatcher(&dispatcher_path).unwrap();
        let dispatcher_claims: i64 = dispatcher
            .connection
            .query_row(
                "SELECT COUNT(*) FROM task_claims WHERE task_id='t-expired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let dispatcher_expiry_events: i64 = dispatcher
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE task_id='t-expired' AND kind='claim_expired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let dispatcher_task: (String, Option<String>) = dispatcher
            .connection
            .query_row(
                "SELECT status,assignee FROM tasks WHERE id='t-expired'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(dispatcher_claims, 1);
        assert_eq!(dispatcher_expiry_events, 0);
        assert_eq!(
            dispatcher_task,
            ("in_progress".into(), Some("ghost".into()))
        );
        drop(dispatcher);

        let ordinary_path = expired_claim_board("ordinary-open-sweeps-expired-claim");
        let ordinary = Store::open(&ordinary_path).unwrap();
        let ordinary_claims: i64 = ordinary
            .connection
            .query_row(
                "SELECT COUNT(*) FROM task_claims WHERE task_id='t-expired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let ordinary_expiry_events: i64 = ordinary
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE task_id='t-expired' AND kind='claim_expired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let ordinary_task: (String, Option<String>) = ordinary
            .connection
            .query_row(
                "SELECT status,assignee FROM tasks WHERE id='t-expired'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ordinary_claims, 0);
        assert_eq!(ordinary_expiry_events, 1);
        assert_eq!(ordinary_task, ("todo".into(), None));
    }

    fn board_event_seqs(events: &[Event]) -> Vec<i64> {
        events.iter().map(|event| event.seq).collect()
    }

    fn subscription_input(id: &str) -> AddSubscription {
        AddSubscription {
            id: Some(id.into()),
            subject_task_id: Some("t-subject".into()),
            relations: vec!["parent:t-parent".into()],
            kinds: vec!["board_initialized".into()],
            prior_statuses: vec!["todo".into()],
            current_statuses: vec!["in_progress".into()],
            tags: vec!["pubsub".into()],
            consumer_id: "codex.queue".into(),
            action_id: "enqueue-turn".into(),
            timeout_ms: 30_000,
            max_retries: 3,
            rate_per_minute: 60,
            max_concurrency: 1,
            secret_ref: Some("codex_queue_token".into()),
            actor: "test@driver".into(),
        }
    }

    fn delivery_subscription_input(id: &str) -> AddSubscription {
        let mut input = subscription_input(id);
        input.subject_task_id = None;
        input.relations.clear();
        input.kinds = vec!["checkpoint_added".into()];
        input.prior_statuses.clear();
        input.current_statuses.clear();
        input.tags.clear();
        input.secret_ref = None;
        input
    }

    fn subscription_store(name: &str) -> Store {
        let mut store = test_store(name);
        store.initialize(name, "test@driver").unwrap();
        store.add_tag("pubsub", None, Some("test@driver")).unwrap();
        insert_task(&store, "t-subject");
        insert_task(&store, "t-parent");
        store
    }

    fn semantic_event_payload(
        subject: &str,
        relation: (&str, &str),
        prior_status: &str,
        current_status: &str,
        tags: &[&str],
    ) -> Value {
        json!({
            "_semanticV1": {
                "subject": { "type": "task", "id": subject },
                "relations": [
                    { "kind": relation.0, "type": "task", "id": relation.1 }
                ],
                "priorStatus": prior_status,
                "currentStatus": current_status,
                "tags": tags,
            }
        })
    }

    fn delivery_event_ids(store: &Store, subscription_id: &str) -> Vec<String> {
        let mut statement = store
            .connection
            .prepare(
                "SELECT event_id FROM subscription_deliveries \
                 WHERE subscription_id=? ORDER BY event_seq,event_id",
            )
            .unwrap();
        statement
            .query_map([subscription_id], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn board_event_count(store: &Store) -> i64 {
        store
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap()
    }

    fn delivery_count(store: &Store) -> i64 {
        store
            .connection
            .query_row("SELECT COUNT(*) FROM subscription_deliveries", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn delivery_row(
        store: &Store,
        subscription_id: &str,
        event_id: &str,
    ) -> SubscriptionDeliveryRow {
        store
            .connection
            .query_row(
                "SELECT * FROM subscription_deliveries WHERE subscription_id=? AND event_id=?",
                params![subscription_id, event_id],
                subscription_delivery_row,
            )
            .unwrap()
    }

    fn delivery_attempt_rows(
        store: &Store,
        subscription_id: &str,
        event_id: &str,
    ) -> Vec<SubscriptionDeliveryAttemptRow> {
        store
            .connection
            .prepare(
                "SELECT * FROM subscription_delivery_attempts \
                 WHERE subscription_id=? AND event_id=? ORDER BY attempt",
            )
            .unwrap()
            .query_map(
                params![subscription_id, event_id],
                subscription_delivery_attempt_row,
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn materialization_cursor(store: &Store) -> i64 {
        store
            .connection
            .query_row(
                "SELECT event_seq FROM board_materialization_cursor WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn board_event_by_seq(store: &Store, seq: i64) -> Event {
        store
            .connection
            .query_row("SELECT * FROM events WHERE seq=?", [seq], board_event_row)
            .unwrap()
    }

    fn error_string<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.to_string(),
        }
    }

    fn flip_lower_hex_prefix(hash: &str) -> String {
        let mut chars = hash.chars().collect::<Vec<_>>();
        if let Some(first) = chars.first_mut() {
            *first = if *first == '0' { '1' } else { '0' };
        }
        chars.into_iter().collect()
    }

    fn event_hashes(events: Vec<Event>) -> Vec<String> {
        events
            .into_iter()
            .map(|event| event.event_hash.expect("board event hash is required"))
            .collect()
    }

    #[test]
    fn subscriptions_are_normalized_audited_and_secret_values_never_enter_events() {
        let mut store = subscription_store("subscriptions-lifecycle");
        let mut input = subscription_input("sub-unit");
        input.relations.push("parent:t-parent".into());
        input.kinds = vec!["subscription_resumed".into(), "checkpoint_added".into()];
        input.tags.push("pubsub".into());
        let added = store.add_subscription(input).unwrap();
        assert_eq!(added.protocol_version, SUBSCRIPTION_PROTOCOL_VERSION);
        assert_eq!(added.relations, vec!["parent:t-parent"]);
        assert_eq!(
            added.kinds,
            vec!["checkpoint_added", "subscription_resumed"]
        );
        assert_eq!(added.tags, vec!["pubsub"]);
        assert_eq!(added.secret_ref.as_deref(), Some("codex_queue_token"));
        assert_eq!(
            store.subscriptions(None, None, false).unwrap(),
            vec![added.clone()]
        );
        assert_eq!(
            store
                .subscriptions(Some("active"), Some("codex.queue"), true)
                .unwrap(),
            vec![added.clone()]
        );
        assert!(store.subscriptions(Some("unknown"), None, true).is_err());
        assert!(store.subscriptions(None, Some("../../bad"), true).is_err());

        let add_event = store
            .events(None, Some("subscription_added"), 1, true)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(added.start_event_seq, add_event.seq);
        assert_eq!(
            store
                .require_subscription("sub-unit")
                .unwrap()
                .start_event_seq,
            add_event.seq
        );
        let encoded = add_event.payload.to_string();
        assert!(!encoded.contains("secretRef"), "{encoded}");
        assert!(!encoded.contains("codex_queue_token"), "{encoded}");
        assert_eq!(add_event.payload["_semanticV1"], Value::Null);

        let mut unfiltered_store = subscription_store("subscriptions-lifecycle-unfiltered");
        let mut unfiltered_input = subscription_input("sub-open");
        unfiltered_input.subject_task_id = None;
        unfiltered_input.relations.clear();
        unfiltered_input.kinds.clear();
        unfiltered_input.prior_statuses.clear();
        unfiltered_input.current_statuses.clear();
        unfiltered_input.tags.clear();
        unfiltered_input.secret_ref = None;
        let unfiltered = unfiltered_store.add_subscription(unfiltered_input).unwrap();
        assert_eq!(
            unfiltered_store
                .require_subscription("sub-open")
                .unwrap()
                .start_event_seq,
            unfiltered.start_event_seq
        );
        assert_eq!(unfiltered_store.materialize_subscriptions().unwrap(), 0);
        assert!(delivery_event_ids(&unfiltered_store, "sub-open").is_empty());

        let paused = store.pause_subscription("sub-unit", "test@driver").unwrap();
        assert_eq!(paused.status, "paused");
        assert!(store.subscriptions(None, None, false).unwrap().is_empty());
        assert_eq!(store.subscriptions(None, None, true).unwrap().len(), 1);
        let paused_again = store.pause_subscription("sub-unit", "test@driver").unwrap();
        assert_eq!(paused_again, paused);
        assert_eq!(
            store
                .events(None, Some("subscription_paused"), 10, true)
                .unwrap()
                .len(),
            1
        );

        let resumed = store
            .resume_subscription("sub-unit", "test@driver")
            .unwrap();
        assert_eq!(resumed.status, "active");
        assert!(resumed.paused_at.is_none());
        assert!(resumed.paused_by.is_none());
        assert!(store.audit().unwrap().healthy);
    }

    #[test]
    fn subscription_validation_fails_closed_for_every_untrusted_field_family() {
        let mut store = subscription_store("subscriptions-invalid");
        type InvalidCase = (&'static str, fn(&mut AddSubscription));
        let cases: Vec<InvalidCase> = vec![
            ("subject", |input| {
                input.subject_task_id = Some("missing".into())
            }),
            ("relation kind", |input| {
                input.relations = vec!["child:t-parent".into()]
            }),
            ("relation target", |input| {
                input.relations = vec!["parent:missing".into()]
            }),
            ("empty relation target", |input| {
                input.relations = vec!["parent:".into()]
            }),
            ("event kind", |input| {
                input.kinds = vec!["never_happened".into()]
            }),
            ("prior status", |input| {
                input.prior_statuses = vec!["running".into()]
            }),
            ("current status", |input| {
                input.current_statuses = vec!["running".into()]
            }),
            ("tag", |input| input.tags = vec!["unknown".into()]),
            ("consumer", |input| {
                input.consumer_id = "../../bin/sh".into()
            }),
            ("action", |input| input.action_id = "run command".into()),
            ("secret", |input| {
                input.secret_ref = Some("env:TOKEN".into())
            }),
            ("id prefix", |input| input.id = Some("bad-id".into())),
            ("id suffix", |input| input.id = Some("sub-".into())),
            ("timeout", |input| input.timeout_ms = 0),
            ("retries", |input| input.max_retries = 21),
            ("rate", |input| input.rate_per_minute = 0),
            ("concurrency", |input| input.max_concurrency = 65),
        ];
        for (index, (label, mutate)) in cases.into_iter().enumerate() {
            let mut input = subscription_input(&format!("sub-invalid-{index}"));
            mutate(&mut input);
            assert!(
                store.add_subscription(input).is_err(),
                "{label} was accepted"
            );
        }
        assert!(store.subscriptions(None, None, true).unwrap().is_empty());
    }

    #[test]
    fn subscription_identity_is_immutable_and_collisions_fail() {
        let mut store = subscription_store("subscriptions-identity");
        store
            .add_subscription(subscription_input("sub-stable"))
            .unwrap();
        let error = store
            .add_subscription(subscription_input("sub-stable"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("UNIQUE") || error.contains("unique"),
            "{error}"
        );
        assert_eq!(
            store.require_subscription("sub-stable").unwrap().id,
            "sub-stable"
        );
        assert!(store.require_subscription("sub-missing").is_err());

        let mut generated_input = subscription_input("sub-unused");
        generated_input.id = None;
        let generated = store.add_subscription(generated_input).unwrap();
        assert!(generated.id.starts_with("sub-"), "{}", generated.id);
    }

    #[test]
    fn subscription_storage_faults_fail_closed_and_surface_the_database_error() {
        let mut kind_store = subscription_store("subscriptions-broken-kind-ledger");
        kind_store
            .connection
            .execute_batch("DROP TABLE events;")
            .unwrap();
        let mut kind_input = subscription_input("sub-broken-kind");
        kind_input.subject_task_id = None;
        kind_input.relations.clear();
        kind_input.kinds = vec!["extension_kind".into()];
        assert!(kind_store.add_subscription(kind_input).is_err());

        let mut subject_store = subscription_store("subscriptions-broken-subject-ledger");
        subject_store
            .connection
            .execute_batch("DROP TABLE tasks;")
            .unwrap();
        assert!(
            subject_store
                .add_subscription(subscription_input("sub-broken-subject"))
                .is_err()
        );

        let mut current_relation_store =
            subscription_store("subscriptions-broken-current-relation");
        current_relation_store
            .connection
            .execute_batch("DROP TABLE tasks;")
            .unwrap();
        let mut current_relation = subscription_input("sub-broken-current-relation");
        current_relation.subject_task_id = None;
        assert!(
            current_relation_store
                .add_subscription(current_relation)
                .is_err()
        );

        let mut historical_relation_store =
            subscription_store("subscriptions-broken-historical-relation");
        historical_relation_store
            .connection
            .execute_batch("DELETE FROM tasks WHERE id='t-parent'; DROP TABLE events;")
            .unwrap();
        let mut historical_relation = subscription_input("sub-broken-historical-relation");
        historical_relation.subject_task_id = None;
        assert!(
            historical_relation_store
                .add_subscription(historical_relation)
                .is_err()
        );

        let mut add_event_store = subscription_store("subscriptions-broken-add-event");
        add_event_store
            .connection
            .execute_batch("DROP TABLE events;")
            .unwrap();
        let mut add_event = subscription_input("sub-broken-add-event");
        add_event.subject_task_id = None;
        add_event.kinds.clear();
        assert!(add_event_store.add_subscription(add_event).is_err());

        let mut pause_event_store = subscription_store("subscriptions-broken-pause-event");
        pause_event_store
            .add_subscription(subscription_input("sub-broken-pause-event"))
            .unwrap();
        pause_event_store
            .connection
            .execute_batch("DROP TABLE events;")
            .unwrap();
        assert!(
            pause_event_store
                .pause_subscription("sub-broken-pause-event", "test@driver")
                .is_err()
        );

        let mut malformed_row_store = subscription_store("subscriptions-malformed-row");
        malformed_row_store
            .add_subscription(subscription_input("sub-malformed-row"))
            .unwrap();
        malformed_row_store
            .connection
            .execute_batch(
                "PRAGMA ignore_check_constraints=ON;\
                 UPDATE subscriptions SET relations='not-json' WHERE id='sub-malformed-row';",
            )
            .unwrap();
        assert!(malformed_row_store.subscriptions(None, None, true).is_err());
    }

    #[test]
    fn subscription_materialization_skips_history_and_self_add_and_dedupes() {
        let mut store = subscription_store("subscriptions-materialization-history");
        let prehistory = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &prehistory.to_string(),
            10,
        )
        .unwrap();
        let mut input = subscription_input("sub-history");
        input.kinds = vec!["checkpoint_added".into()];
        let added = store.add_subscription(input).unwrap();
        let matching = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &matching.to_string(),
            20,
        )
        .unwrap();
        let unmatched = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "review",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &unmatched.to_string(),
            30,
        )
        .unwrap();

        let expected = event_hashes(
            store
                .events_since_filtered(
                    Some("t-subject"),
                    &["checkpoint_added".to_owned()],
                    &["parent:t-parent".to_owned()],
                    &["todo".to_owned()],
                    &["in_progress".to_owned()],
                    &["pubsub".to_owned()],
                    added.start_event_seq,
                    10,
                    false,
                )
                .unwrap(),
        );

        let events_before = board_event_count(&store);
        let inserted = store.materialize_subscriptions().unwrap();
        assert_eq!(inserted, expected.len());
        assert_eq!(delivery_event_ids(&store, "sub-history"), expected);
        assert_eq!(delivery_count(&store), expected.len() as i64);
        assert_eq!(materialization_cursor(&store), board_event_count(&store));
        assert_eq!(board_event_count(&store), events_before);

        let repeated = store.materialize_subscriptions().unwrap();
        assert_eq!(repeated, 0);
        assert_eq!(delivery_event_ids(&store, "sub-history"), expected);
        assert_eq!(delivery_count(&store), expected.len() as i64);
    }

    #[test]
    fn subscription_materialization_includes_archived_events_after_the_anchor() {
        let mut store = subscription_store("subscriptions-materialization-archived");
        let mut input = delivery_subscription_input("sub-archived");
        input.kinds = vec!["checkpoint_added".into()];
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        let archived_seq = board_event_count(&store);
        store
            .connection
            .execute(
                "UPDATE events SET archived=1 WHERE seq=?",
                params![archived_seq],
            )
            .unwrap();

        let inserted = store.materialize_subscriptions().unwrap();
        let expected = event_hashes(
            store
                .events_since_filtered(
                    Some("t-subject"),
                    &["checkpoint_added".to_owned()],
                    &[],
                    &["todo".to_owned()],
                    &["in_progress".to_owned()],
                    &["pubsub".to_owned()],
                    added.start_event_seq,
                    10,
                    true,
                )
                .unwrap(),
        );
        assert_eq!(inserted, 1);
        assert_eq!(delivery_event_ids(&store, &added.id), expected);
        assert_eq!(delivery_count(&store), 1);
    }

    #[test]
    fn subscription_materialization_matches_filtered_watch_rows_for_active_and_paused_subscriptions()
     {
        let mut store = subscription_store("subscriptions-materialization-parity");
        store.add_tag("ops", None, Some("test@driver")).unwrap();
        insert_task(&store, "t-other");
        insert_task(&store, "t-other-parent");

        let mut active_input = subscription_input("sub-active");
        active_input.kinds = vec!["checkpoint_added".into()];
        let active = store.add_subscription(active_input).unwrap();
        let mut paused_input = subscription_input("sub-paused");
        paused_input.subject_task_id = Some("t-other".into());
        paused_input.relations = vec!["parent:t-other-parent".into()];
        paused_input.kinds = vec!["task_moved".into()];
        paused_input.prior_statuses = vec!["todo".into()];
        paused_input.current_statuses = vec!["done".into()];
        paused_input.tags = vec!["ops".into()];
        let paused = store.add_subscription(paused_input).unwrap();
        store
            .pause_subscription("sub-paused", "test@driver")
            .unwrap();

        let active_payload = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &active_payload.to_string(),
            20,
        )
        .unwrap();

        let paused_payload = semantic_event_payload(
            "t-other",
            ("parent", "t-other-parent"),
            "todo",
            "done",
            &["ops"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-other"),
            "task_moved",
            "test",
            &paused_payload.to_string(),
            30,
        )
        .unwrap();

        let inserted = store.materialize_subscriptions().unwrap();
        assert_eq!(inserted, 2);
        assert_eq!(store.subscriptions(None, None, false).unwrap().len(), 1);
        assert_eq!(store.subscriptions(None, None, true).unwrap().len(), 2);

        let active_expected = event_hashes(
            store
                .events_since_filtered(
                    Some("t-subject"),
                    &["checkpoint_added".to_owned()],
                    &["parent:t-parent".to_owned()],
                    &["todo".to_owned()],
                    &["in_progress".to_owned()],
                    &["pubsub".to_owned()],
                    active.start_event_seq,
                    10,
                    false,
                )
                .unwrap(),
        );
        let paused_expected = event_hashes(
            store
                .events_since_filtered(
                    Some("t-other"),
                    &["task_moved".to_owned()],
                    &["parent:t-other-parent".to_owned()],
                    &["todo".to_owned()],
                    &["done".to_owned()],
                    &["ops".to_owned()],
                    paused.start_event_seq,
                    10,
                    false,
                )
                .unwrap(),
        );

        assert_eq!(delivery_event_ids(&store, "sub-active"), active_expected);
        assert_eq!(delivery_event_ids(&store, "sub-paused"), paused_expected);
        assert_eq!(materialization_cursor(&store), board_event_count(&store));
    }

    #[test]
    fn subscription_materialization_fails_closed_on_non_duplicate_constraint_errors() {
        let mut store = subscription_store("subscriptions-materialization-constraints");
        let mut input = subscription_input("sub-constraint");
        input.kinds = vec!["checkpoint_added".into()];
        let added = store.add_subscription(input).unwrap();
        let first = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &first.to_string(),
            20,
        )
        .unwrap();
        assert_eq!(store.materialize_subscriptions().unwrap(), 1);
        assert_eq!(delivery_count(&store), 1);
        store
            .connection
            .execute(
                "CREATE UNIQUE INDEX idx_subscription_deliveries_test_kind ON subscription_deliveries(subscription_id,event_kind)",
                [],
            )
            .unwrap();
        let second = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &second.to_string(),
            30,
        )
        .unwrap();
        let error = store.materialize_subscriptions().unwrap_err().to_string();
        assert!(
            error.contains("UNIQUE") || error.contains("unique"),
            "{error}"
        );
        assert_eq!(delivery_count(&store), 1);
        assert_eq!(store.require_subscription(&added.id).unwrap().id, added.id);
    }

    #[test]
    fn subscription_materialization_rolls_back_on_bad_hash_and_advances_over_unmatched_tail() {
        let mut store = subscription_store("subscriptions-materialization-failures");
        let mut fail_input = subscription_input("sub-fail");
        fail_input.kinds = vec!["checkpoint_added".into()];
        let _added = store.add_subscription(fail_input).unwrap();
        let matching = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &matching.to_string(),
            20,
        )
        .unwrap();
        let good = store
            .connection
            .query_row(
                "SELECT seq,event_hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        let bad_payload = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        store
            .connection
            .execute(
                "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) \
                 VALUES(?,?,?,?,?, ?,0,?,?)",
                params![
                    good.0 + 1,
                    "t-subject",
                    "checkpoint_added",
                    "test",
                    bad_payload.to_string(),
                    30_i64,
                    good.1,
                    "not-a-hash",
                ],
            )
            .unwrap();
        let cursor_before = materialization_cursor(&store);
        let events_before = board_event_count(&store);
        let error = store.materialize_subscriptions().unwrap_err().to_string();
        assert!(error.contains("malformed event_hash"), "{error}");
        assert_eq!(delivery_count(&store), 0);
        assert_eq!(materialization_cursor(&store), cursor_before);
        assert_eq!(board_event_count(&store), events_before);

        let mut tail = subscription_store("subscriptions-materialization-tail");
        let mut tail_input = subscription_input("sub-tail");
        tail_input.kinds = vec!["checkpoint_added".into()];
        let tail_added = tail.add_subscription(tail_input).unwrap();
        let unmatched = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "review",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &tail.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &unmatched.to_string(),
            20,
        )
        .unwrap();
        let first = tail.materialize_subscriptions().unwrap();
        assert_eq!(first, 0);
        let cursor_after_unmatched = materialization_cursor(&tail);
        let expected_after_unmatched = event_hashes(
            tail.events_since_filtered(
                Some("t-subject"),
                &["checkpoint_added".to_owned()],
                &["parent:t-parent".to_owned()],
                &["todo".to_owned()],
                &["review".to_owned()],
                &["pubsub".to_owned()],
                cursor_after_unmatched,
                10,
                false,
            )
            .unwrap(),
        );
        assert!(expected_after_unmatched.is_empty());

        let matching_tail = semantic_event_payload(
            "t-subject",
            ("parent", "t-parent"),
            "todo",
            "in_progress",
            &["pubsub"],
        );
        crate::audit::append_board_event(
            &tail.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &matching_tail.to_string(),
            30,
        )
        .unwrap();
        let inserted = tail.materialize_subscriptions().unwrap();
        assert_eq!(inserted, 1);
        let expected = event_hashes(
            tail.events_since_filtered(
                Some("t-subject"),
                &["checkpoint_added".to_owned()],
                &["parent:t-parent".to_owned()],
                &["todo".to_owned()],
                &["in_progress".to_owned()],
                &["pubsub".to_owned()],
                cursor_after_unmatched,
                10,
                false,
            )
            .unwrap(),
        );
        assert_eq!(delivery_event_ids(&tail, "sub-tail"), expected);
        assert_eq!(materialization_cursor(&tail), board_event_count(&tail));
        assert_eq!(tail.subscriptions(None, None, true).unwrap().len(), 1);
        assert_eq!(
            tail.require_subscription("sub-tail")
                .unwrap()
                .start_event_seq,
            tail_added.start_event_seq
        );
    }

    #[test]
    fn subscription_delivery_candidate_filters_in_sql_by_exact_consumer() {
        let mut store = subscription_store("subscriptions-delivery-consumer-filter");
        let mut first = delivery_subscription_input("sub-consumer-a");
        first.consumer_id = "consumer.a".into();
        let mut second = delivery_subscription_input("sub-consumer-b");
        second.consumer_id = "consumer.b".into();
        store.add_subscription(first).unwrap();
        store.add_subscription(second).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        assert_eq!(store.materialize_subscriptions().unwrap(), 2);

        let due_at = delivery_row(
            &store,
            "sub-consumer-a",
            &delivery_event_ids(&store, "sub-consumer-a")[0],
        )
        .next_attempt_at
        .unwrap();
        assert_eq!(
            store
                .next_due_subscription_delivery_for_consumer(due_at, Some("consumer.b"))
                .unwrap()
                .unwrap()
                .subscription
                .id,
            "sub-consumer-b"
        );
        assert!(
            store
                .next_due_subscription_delivery_for_consumer(due_at, Some("consumer.missing"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .next_due_subscription_delivery(due_at)
                .unwrap()
                .unwrap()
                .subscription
                .id,
            "sub-consumer-a"
        );
    }

    #[test]
    fn subscription_delivery_claim_finalize_and_wrong_lease_are_atomic_and_side_effect_free() {
        let mut store = subscription_store("subscriptions-delivery-claim");
        let mut input = delivery_subscription_input("sub-delivery-claim");
        input.max_retries = 1;
        input.rate_per_minute = 60;
        input.max_concurrency = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let board_events_before = board_event_count(&store);
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let candidate = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        assert_eq!(candidate.subscription.id, added.id);
        assert_eq!(candidate.subscription.secret_ref, None);
        assert_eq!(candidate.delivery_status, "pending");
        assert_eq!(candidate.attempt_number, 1);
        let candidate_event = board_event_by_seq(&store, candidate.event_seq);
        assert_eq!(
            candidate.event_id,
            candidate_event.event_hash.clone().unwrap()
        );
        assert_eq!(candidate.event_seq, candidate_event.seq);
        assert_eq!(candidate.event_kind, candidate_event.kind);
        assert_eq!(candidate.next_attempt_at, due_at);
        assert_eq!(board_event_count(&store), board_events_before);

        let claimed = store
            .claim_subscription_delivery(&added.id, &candidate.event_id, due_at, 5_000)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.delivery_status, "leased");
        assert_eq!(claimed.attempt_number, 1);
        assert_eq!(claimed.lease_deadline_at, due_at + 5_000);
        assert!(!claimed.lease_token.is_empty());
        assert_eq!(board_event_count(&store), board_events_before);
        assert!(
            store
                .claim_subscription_delivery(&added.id, &candidate.event_id, due_at + 1, 5_000,)
                .unwrap()
                .is_none()
        );

        let leased = delivery_row(&store, &added.id, &candidate.event_id);
        assert_eq!(leased.status, "leased");
        assert_eq!(leased.attempts, 1);
        assert_eq!(
            leased.lease_token.as_deref(),
            Some(claimed.lease_token.as_str())
        );
        assert_eq!(leased.next_attempt_at, None);
        assert_eq!(leased.last_attempt_at, Some(candidate.next_attempt_at));

        let attempts = delivery_attempt_rows(&store, &added.id, &candidate.event_id);
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].attempt, 1);
        assert_eq!(attempts[0].outcome, "claim");
        assert_eq!(attempts[0].started_at, due_at);
        assert_eq!(attempts[0].finished_at, None);
        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO subscription_delivery_attempts(subscription_id,event_id,attempt,started_at,finished_at,outcome,error_code) VALUES(?,?,?,?,?,?,?)",
                    params![
                        &added.id,
                        &candidate.event_id,
                        2,
                        due_at + 1,
                        due_at + 2,
                        "success",
                        Option::<String>::None,
                    ],
                )
                .is_err(),
            "subscription_delivery_attempts accepted a terminal insert"
        );

        assert!(
            !store
                .finalize_subscription_delivery_success(
                    &added.id,
                    &candidate.event_id,
                    "wrong-lease",
                    due_at + 2,
                )
                .unwrap()
        );
        assert_eq!(
            delivery_row(&store, &added.id, &candidate.event_id).status,
            "leased"
        );

        assert!(
            store
                .finalize_subscription_delivery_success(
                    &added.id,
                    &candidate.event_id,
                    &claimed.lease_token,
                    due_at + 3,
                )
                .unwrap()
        );
        let acked = delivery_row(&store, &added.id, &candidate.event_id);
        assert_eq!(acked.status, "acked");
        assert_eq!(acked.acked_at, Some(due_at + 3));
        assert!(acked.lease_token.is_none());
        assert!(acked.dead_lettered_at.is_none());
        let attempts = delivery_attempt_rows(&store, &added.id, &candidate.event_id);
        assert_eq!(attempts[0].outcome, "success");
        assert_eq!(attempts[0].finished_at, Some(due_at + 3));
        assert!(
            store
                .connection
                .execute(
                    "UPDATE subscription_delivery_attempts SET outcome='retry',finished_at=? WHERE subscription_id=? AND event_id=? AND attempt=?",
                    params![due_at + 4, &added.id, &candidate.event_id, 1],
                )
                .is_err(),
            "subscription_delivery_attempts accepted a second update"
        );
        assert!(
            store
                .connection
                .execute(
                    "DELETE FROM subscription_delivery_attempts WHERE subscription_id=? AND event_id=? AND attempt=?",
                    params![&added.id, &candidate.event_id, 1],
                )
                .is_err(),
            "subscription_delivery_attempts accepted a direct delete"
        );
        assert_eq!(board_event_count(&store), board_events_before);
        assert!(
            !store
                .finalize_subscription_delivery_success(
                    &added.id,
                    &candidate.event_id,
                    &claimed.lease_token,
                    due_at + 4,
                )
                .unwrap()
        );
        assert!(
            store
                .next_due_subscription_delivery(due_at + 4)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn subscription_positions_answer_every_row_from_one_grouped_query() {
        let mut store = subscription_store("subscriptions-positions");
        let mut ids = Vec::new();
        for (id, max_retries) in [
            ("sub-position-acked", 1),
            ("sub-position-retry", 3),
            ("sub-position-dead", 0),
            ("sub-position-leased", 1),
        ] {
            let mut input = delivery_subscription_input(id);
            input.max_retries = max_retries;
            ids.push(store.add_subscription(input).unwrap().id);
        }
        for created_at in [20, 30] {
            crate::audit::append_board_event(
                &store.connection,
                Some("t-subject"),
                "checkpoint_added",
                "test",
                "{}",
                created_at,
            )
            .unwrap();
        }
        // Added after those events, so its anchor is past them: a
        // subscription with no delivery rows at all.
        let idle = store
            .add_subscription(delivery_subscription_input("sub-position-idle"))
            .unwrap()
            .id;
        store.materialize_subscriptions().unwrap();

        let mut first_event = std::collections::BTreeMap::new();
        for id in &ids {
            let event_ids = delivery_event_ids(&store, id);
            assert_eq!(event_ids.len(), 2, "{id} should have both events queued");
            first_event.insert(id.clone(), event_ids[0].clone());
        }
        let acked_seq = {
            let event = &first_event["sub-position-acked"];
            let due_at = delivery_row(&store, "sub-position-acked", event)
                .next_attempt_at
                .unwrap();
            let claimed = store
                .claim_subscription_delivery("sub-position-acked", event, due_at, 5_000)
                .unwrap()
                .unwrap();
            assert!(
                store
                    .finalize_subscription_delivery_success(
                        "sub-position-acked",
                        event,
                        &claimed.lease_token,
                        due_at + 1,
                    )
                    .unwrap()
            );
            claimed.event_seq
        };
        for id in ["sub-position-retry", "sub-position-dead"] {
            let event = &first_event[id];
            let due_at = delivery_row(&store, id, event).next_attempt_at.unwrap();
            let claimed = store
                .claim_subscription_delivery(id, event, due_at, 5_000)
                .unwrap()
                .unwrap();
            assert!(
                store
                    .finalize_subscription_delivery_failure(
                        id,
                        event,
                        &claimed.lease_token,
                        due_at + 1,
                        false,
                        "consumer_refused",
                    )
                    .unwrap()
            );
        }
        {
            let event = &first_event["sub-position-leased"];
            let due_at = delivery_row(&store, "sub-position-leased", event)
                .next_attempt_at
                .unwrap();
            store
                .claim_subscription_delivery("sub-position-leased", event, due_at, 5_000)
                .unwrap()
                .unwrap();
        }

        let positions = store.subscription_positions().unwrap();
        assert_eq!(positions.head_event_seq, board_event_count(&store));
        assert_eq!(
            positions.position("sub-position-acked"),
            SubscriptionPosition {
                acked_through_seq: Some(acked_seq),
                pending: 1,
                leased: 0,
                retry_wait: 0,
                dead_letter: 0,
            }
        );
        assert_eq!(
            positions.position("sub-position-retry"),
            SubscriptionPosition {
                acked_through_seq: None,
                pending: 1,
                leased: 0,
                retry_wait: 1,
                dead_letter: 0,
            }
        );
        assert_eq!(
            positions.position("sub-position-dead"),
            SubscriptionPosition {
                acked_through_seq: None,
                pending: 1,
                leased: 0,
                retry_wait: 0,
                dead_letter: 1,
            }
        );
        assert_eq!(
            positions.position("sub-position-leased"),
            SubscriptionPosition {
                acked_through_seq: None,
                pending: 1,
                leased: 1,
                retry_wait: 0,
                dead_letter: 0,
            }
        );
        // No delivery rows is a real position, not a missing one.
        assert!(!positions.by_subscription.contains_key(&idle));
        assert_eq!(
            positions.position(&idle),
            SubscriptionPosition::default(),
            "a subscription with nothing queued has a position, not an absence"
        );
        assert_eq!(positions.by_subscription.len(), ids.len());

        // Not N+1, and provably so: the projection is one grouped statement
        // over the delivery rows plus one head read, and it takes no
        // subscription id — there is no shape in which a caller could put it
        // inside a per-row loop and still get an answer. A reviewer can read
        // that off the two statements below; this asserts it stays true.
        const SOURCE: &str = include_str!("store.rs");
        let body = SOURCE
            .split_once("pub fn subscription_positions(")
            .expect("the projection is defined in this file")
            .1
            .split_once("\n    fn ")
            .expect("the next private method ends the body")
            .0;
        assert_eq!(body.matches("SELECT").count(), 2, "{body}");
        assert_eq!(
            body.matches("FROM subscription_deliveries").count(),
            1,
            "{body}"
        );
        assert_eq!(
            body.matches("GROUP BY subscription_id").count(),
            1,
            "{body}"
        );
        assert!(!body.contains("WHERE subscription_id"), "{body}");
    }

    #[test]
    fn dead_letters_are_attributed_to_their_codes_and_retries_are_not() {
        let mut store = subscription_store("subscriptions-dead-letter-codes");
        let mut dead_input = delivery_subscription_input("sub-codes-dead");
        dead_input.max_retries = 0;
        let dead = store.add_subscription(dead_input).unwrap().id;
        let mut retry_input = delivery_subscription_input("sub-codes-retry");
        retry_input.max_retries = 3;
        let retry = store.add_subscription(retry_input).unwrap().id;
        for created_at in [20, 30, 40] {
            crate::audit::append_board_event(
                &store.connection,
                Some("t-subject"),
                "checkpoint_added",
                "test",
                "{}",
                created_at,
            )
            .unwrap();
        }
        store.materialize_subscriptions().unwrap();

        // Two refusals of one kind and one of another, so the projection has
        // to both group them and rank them.
        let events = delivery_event_ids(&store, &dead);
        for (event, code) in events.iter().zip([
            "opencode_endpoint_unreachable",
            "kimi_frame_oversized",
            "opencode_endpoint_unreachable",
        ]) {
            let due_at = delivery_row(&store, &dead, event).next_attempt_at.unwrap();
            let claimed = store
                .claim_subscription_delivery(&dead, event, due_at, 5_000)
                .unwrap()
                .unwrap();
            assert!(
                store
                    .finalize_subscription_delivery_failure(
                        &dead,
                        event,
                        &claimed.lease_token,
                        due_at + 1,
                        false,
                        code,
                    )
                    .unwrap()
            );
        }
        // A retry carries a `last_error_code` too, and it is not a dead
        // letter: attributing it would report a delivery that is still
        // going to happen as one that has stopped.
        let retry_event = &delivery_event_ids(&store, &retry)[0];
        let due_at = delivery_row(&store, &retry, retry_event)
            .next_attempt_at
            .unwrap();
        let claimed = store
            .claim_subscription_delivery(&retry, retry_event, due_at, 5_000)
            .unwrap()
            .unwrap();
        assert!(
            store
                .finalize_subscription_delivery_failure(
                    &retry,
                    retry_event,
                    &claimed.lease_token,
                    due_at + 1,
                    true,
                    "opencode_deadline_exceeded",
                )
                .unwrap()
        );

        let positions = store.subscription_positions().unwrap();
        assert_eq!(
            positions.position(&dead),
            SubscriptionPosition {
                acked_through_seq: None,
                pending: 0,
                leased: 0,
                retry_wait: 0,
                dead_letter: 3,
            }
        );
        // Ranked by how many deliveries carry each code, and the counts add
        // up to the total beside them.
        assert_eq!(
            positions.dead_letter_codes(&dead),
            [
                DeadLetterCode {
                    code: "opencode_endpoint_unreachable".to_owned(),
                    deliveries: 2,
                },
                DeadLetterCode {
                    code: "kimi_frame_oversized".to_owned(),
                    deliveries: 1,
                },
            ]
        );
        assert_eq!(
            positions
                .dead_letter_codes(&dead)
                .iter()
                .map(|code| code.deliveries)
                .sum::<i64>(),
            positions.position(&dead).dead_letter
        );
        assert_eq!(positions.position(&retry).retry_wait, 1);
        assert_eq!(positions.position(&retry).dead_letter, 0);
        assert!(
            positions.dead_letter_codes(&retry).is_empty(),
            "a retrying delivery is not a dead letter: {:?}",
            positions.dead_letters
        );
        assert!(
            positions.dead_letter_codes("sub-not-here").is_empty(),
            "an unknown subscription has no codes, not a missing answer"
        );
    }

    #[test]
    fn subscription_delivery_lease_duration_has_30_second_cleanup_headroom() {
        assert!(validate_delivery_lease_duration(330_000).is_ok());
        assert!(validate_delivery_lease_duration(330_001).is_err());
        assert!(validate_delivery_lease_duration(0).is_err());
    }

    #[test]
    fn subscription_delivery_rejects_schema_retargeting_and_negative_now() {
        let mut store = subscription_store("subscriptions-delivery-anchor");
        let mut input = delivery_subscription_input("sub-delivery-anchor");
        input.max_retries = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let _claimed = store
            .claim_subscription_delivery(&added.id, &event_ids[0], due_at, 5_000)
            .unwrap()
            .unwrap();
        let current = delivery_row(&store, &added.id, &event_ids[0]);
        let (created_at, updated_at): (i64, i64) = store
            .connection
            .query_row(
                "SELECT created_at,updated_at FROM subscription_deliveries WHERE subscription_id=? AND event_id=?",
                params![&added.id, &event_ids[0]],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let anchor_event = board_event_by_seq(&store, added.start_event_seq);
        let anchor_event_id = anchor_event.event_hash.clone().unwrap();
        let update_error = store
            .connection
            .execute(
                "UPDATE subscription_deliveries SET status=?,attempts=?,event_seq=?,event_id=?,event_kind=?,event_created_at=?,lease_token=?,lease_deadline_at=?,next_attempt_at=?,last_attempt_at=?,last_error_code=?,acked_at=?,dead_lettered_at=?,created_at=?,updated_at=? WHERE subscription_id=? AND event_id=?",
                params![
                    "retry_wait",
                    current.attempts,
                    anchor_event.seq,
                    anchor_event_id.clone(),
                    anchor_event.kind,
                    anchor_event.created_at,
                    Option::<String>::None,
                    Option::<i64>::None,
                    current.next_attempt_at.unwrap_or(anchor_event.created_at + 1),
                    current.last_attempt_at,
                    "adapter_failed",
                    Option::<i64>::None,
                    Option::<i64>::None,
                    created_at,
                    updated_at + 1,
                    added.id,
                    event_ids[0],
                ],
            )
            .unwrap_err()
            .to_string();
        assert!(
            update_error.contains("immutable")
                || update_error.contains("identity")
                || update_error.contains("event_id must match"),
            "{update_error}"
        );
        assert_eq!(
            delivery_row(&store, &added.id, &event_ids[0]).event_seq,
            anchor_event.seq + 1
        );
        assert!(store.next_due_subscription_delivery(-1).is_err());
        assert!(
            store
                .claim_subscription_delivery(&added.id, &event_ids[0], -1, 5_000)
                .is_err()
        );
    }

    #[test]
    fn subscription_delivery_rejects_attempt_tampering_and_keeps_retry_rows_eligible() {
        let mut store = subscription_store("subscriptions-delivery-exhausted");
        let mut input = delivery_subscription_input("sub-delivery-exhausted");
        input.max_retries = 1;
        input.rate_per_minute = 60;
        input.max_concurrency = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let first = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        let first_claim = store
            .claim_subscription_delivery(&added.id, &first.event_id, due_at, 5_000)
            .unwrap()
            .unwrap();
        assert!(
            store
                .finalize_subscription_delivery_failure(
                    &added.id,
                    &first.event_id,
                    &first_claim.lease_token,
                    due_at + 1,
                    false,
                    "adapter_failed",
                )
                .unwrap()
        );
        let retry_due_at = delivery_row(&store, &added.id, &first.event_id)
            .next_attempt_at
            .unwrap();
        let tamper_error = store
            .connection
            .execute(
                "UPDATE subscription_deliveries SET attempts=2 WHERE subscription_id=? AND event_id=?",
                params![&added.id, &first.event_id],
            )
            .unwrap_err()
            .to_string();
        assert!(
            tamper_error.contains("state transition is invalid")
                || tamper_error.contains("immutable"),
            "{tamper_error}"
        );
        assert!(
            store
                .next_due_subscription_delivery(retry_due_at)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .claim_subscription_delivery(&added.id, &first.event_id, retry_due_at, 5_000)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn subscription_delivery_pause_resume_rechecks_and_concurrency_blocks_when_one_lease_is_live() {
        let mut store = subscription_store("subscriptions-delivery-pause-resume");
        let mut input = delivery_subscription_input("sub-delivery-pause-resume");
        input.max_concurrency = 1;
        input.rate_per_minute = 60;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            30,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let board_events_after_materialization = board_event_count(&store);
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let due = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        assert_eq!(due.event_id, event_ids[0]);
        let paused = store.pause_subscription(&added.id, "test@driver").unwrap();
        assert_eq!(paused.status, "paused");
        assert!(
            store
                .next_due_subscription_delivery(due_at)
                .unwrap()
                .is_none()
        );
        let resumed = store.resume_subscription(&added.id, "test@driver").unwrap();
        assert_eq!(resumed.status, "active");
        let due_again = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        assert_eq!(due_again.event_id, event_ids[0]);
        let claimed = store
            .claim_subscription_delivery(&added.id, &due_again.event_id, due_at, 5_000)
            .unwrap()
            .unwrap();
        assert_eq!(
            board_event_count(&store),
            board_events_after_materialization + 2
        );
        assert!(
            store
                .next_due_subscription_delivery(due_at)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .finalize_subscription_delivery_success(
                    &added.id,
                    &due_again.event_id,
                    &claimed.lease_token,
                    due_at + 1,
                )
                .unwrap()
        );
        let second = store
            .next_due_subscription_delivery(due_at + 1)
            .unwrap()
            .unwrap();
        assert_eq!(second.event_id, event_ids[1]);
        assert_eq!(
            board_event_count(&store),
            board_events_after_materialization + 2
        );
    }

    #[test]
    fn subscription_delivery_rate_limit_counts_durable_attempt_rows_and_blocks_retry_until_window_clears()
     {
        let mut store = subscription_store("subscriptions-delivery-rate");
        let mut input = delivery_subscription_input("sub-delivery-rate");
        input.rate_per_minute = 1;
        input.max_concurrency = 2;
        input.max_retries = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let board_events_before = board_event_count(&store);
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let candidate = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        let claimed = store
            .claim_subscription_delivery(&added.id, &candidate.event_id, due_at, 5_000)
            .unwrap()
            .unwrap();
        assert!(
            store
                .finalize_subscription_delivery_failure(
                    &added.id,
                    &candidate.event_id,
                    &claimed.lease_token,
                    due_at + 1,
                    false,
                    "adapter_failed",
                )
                .unwrap()
        );
        let retry_due_at = due_at + 1 + 1_000;
        let waiting = delivery_row(&store, &added.id, &candidate.event_id);
        assert_eq!(waiting.status, "retry_wait");
        assert_eq!(waiting.next_attempt_at, Some(retry_due_at));
        assert!(
            store
                .next_due_subscription_delivery(retry_due_at)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .next_due_subscription_delivery(retry_due_at + 60_001)
                .unwrap()
                .is_some()
        );
        assert_eq!(board_event_count(&store), board_events_before);
    }

    #[test]
    fn subscription_delivery_retry_schedule_is_deterministic_and_stops_after_max_retries() {
        let mut store = subscription_store("subscriptions-delivery-retry");
        let mut input = delivery_subscription_input("sub-delivery-retry");
        input.rate_per_minute = 60;
        input.max_concurrency = 1;
        input.max_retries = 2;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let board_events_before = board_event_count(&store);
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let first = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        let first_claim = store
            .claim_subscription_delivery(&added.id, &first.event_id, due_at, 5_000)
            .unwrap()
            .unwrap();
        let first_failure_at = due_at + 1;
        assert!(
            store
                .finalize_subscription_delivery_failure(
                    &added.id,
                    &first.event_id,
                    &first_claim.lease_token,
                    first_failure_at,
                    false,
                    "adapter_failed",
                )
                .unwrap()
        );
        let retry1 = delivery_row(&store, &added.id, &first.event_id)
            .next_attempt_at
            .unwrap();
        assert_eq!(retry1, first_failure_at + 1_000);
        let second = store
            .next_due_subscription_delivery(retry1)
            .unwrap()
            .unwrap();
        assert_eq!(second.attempt_number, 2);
        let second_claim = store
            .claim_subscription_delivery(&added.id, &second.event_id, second.next_attempt_at, 5_000)
            .unwrap()
            .unwrap();
        let second_failure_at = second.next_attempt_at + 1;
        assert!(
            store
                .finalize_subscription_delivery_failure(
                    &added.id,
                    &second.event_id,
                    &second_claim.lease_token,
                    second_failure_at,
                    true,
                    "adapter_timeout",
                )
                .unwrap()
        );
        let retry2 = delivery_row(&store, &added.id, &first.event_id)
            .next_attempt_at
            .unwrap();
        assert_eq!(retry2, second_failure_at + 2_000);
        let third = store
            .next_due_subscription_delivery(retry2)
            .unwrap()
            .unwrap();
        assert_eq!(third.attempt_number, 3);
        let third_claim = store
            .claim_subscription_delivery(&added.id, &third.event_id, third.next_attempt_at, 5_000)
            .unwrap()
            .unwrap();
        let terminal_failure_at = third.next_attempt_at + 1;
        assert!(
            store
                .finalize_subscription_delivery_failure(
                    &added.id,
                    &third.event_id,
                    &third_claim.lease_token,
                    terminal_failure_at,
                    true,
                    "adapter_failed",
                )
                .unwrap()
        );
        let finished = delivery_row(&store, &added.id, &first.event_id);
        assert_eq!(finished.status, "dead_letter");
        assert_eq!(finished.attempts, 3);
        assert_eq!(finished.dead_lettered_at, Some(terminal_failure_at));
        assert!(finished.next_attempt_at.is_none());
        let attempts = delivery_attempt_rows(&store, &added.id, &first.event_id);
        assert_eq!(
            attempts
                .iter()
                .map(|row| row.outcome.as_str())
                .collect::<Vec<_>>(),
            vec!["retry", "timeout", "timeout"]
        );
        assert_eq!(
            attempts
                .iter()
                .map(|row| row.finished_at.is_some())
                .collect::<Vec<_>>(),
            vec![true, true, true]
        );
        assert_eq!(board_event_count(&store), board_events_before);
    }

    #[test]
    fn subscription_delivery_expired_leases_recover_to_retry_wait_with_stable_error_code_and_late_retry_is_due_immediately()
     {
        let mut store = subscription_store("subscriptions-delivery-expired");
        let mut input = delivery_subscription_input("sub-delivery-expired");
        input.max_retries = 1;
        input.rate_per_minute = 60;
        input.max_concurrency = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let board_events_before = board_event_count(&store);
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let candidate = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        let claimed = store
            .claim_subscription_delivery(&added.id, &candidate.event_id, due_at, 1)
            .unwrap()
            .unwrap();
        assert!(
            !store
                .finalize_subscription_delivery_success(
                    &added.id,
                    &candidate.event_id,
                    &claimed.lease_token,
                    claimed.lease_deadline_at,
                )
                .unwrap()
        );
        assert!(
            !store
                .finalize_subscription_delivery_failure(
                    &added.id,
                    &candidate.event_id,
                    &claimed.lease_token,
                    claimed.lease_deadline_at,
                    true,
                    "adapter_timeout",
                )
                .unwrap()
        );
        let still_leased = delivery_row(&store, &added.id, &candidate.event_id);
        assert_eq!(still_leased.status, "leased");
        let recovery_now = claimed.lease_deadline_at + 10_000;
        let recovered = store
            .recover_expired_subscription_deliveries(recovery_now)
            .unwrap();
        assert_eq!(recovered, 1);
        let finished = delivery_row(&store, &added.id, &candidate.event_id);
        assert_eq!(finished.status, "retry_wait");
        let retry_at = claimed.lease_deadline_at + 1_000;
        assert_eq!(finished.next_attempt_at, Some(retry_at));
        assert!(finished.dead_lettered_at.is_none());
        assert_eq!(
            finished.last_error_code.as_deref(),
            Some("dispatcher_lease_expired")
        );
        assert!(finished.lease_token.is_none());
        let attempts = delivery_attempt_rows(&store, &added.id, &candidate.event_id);
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, "lease_expired");
        assert_eq!(
            attempts[0].error_code.as_deref(),
            Some("dispatcher_lease_expired")
        );
        assert_eq!(attempts[0].finished_at, Some(claimed.lease_deadline_at));
        assert_eq!(board_event_count(&store), board_events_before);
        assert!(
            store
                .next_due_subscription_delivery(recovery_now)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .next_due_subscription_delivery(claimed.lease_deadline_at)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn subscription_delivery_expired_leases_dead_letter_at_the_stored_deadline_when_recovered_late()
    {
        let mut store = subscription_store("subscriptions-delivery-expired-terminal");
        let mut input = delivery_subscription_input("sub-delivery-expired-terminal");
        input.max_retries = 0;
        input.rate_per_minute = 60;
        input.max_concurrency = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let candidate = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        let claimed = store
            .claim_subscription_delivery(&added.id, &candidate.event_id, due_at, 1)
            .unwrap()
            .unwrap();
        let recovery_now = claimed.lease_deadline_at + 10_000;
        let recovered = store
            .recover_expired_subscription_deliveries(recovery_now)
            .unwrap();
        assert_eq!(recovered, 1);
        let finished = delivery_row(&store, &added.id, &candidate.event_id);
        assert_eq!(finished.status, "dead_letter");
        assert_eq!(finished.dead_lettered_at, Some(claimed.lease_deadline_at));
        assert!(finished.next_attempt_at.is_none());
        let attempts = delivery_attempt_rows(&store, &added.id, &candidate.event_id);
        assert_eq!(attempts[0].finished_at, Some(claimed.lease_deadline_at));
        assert!(
            store
                .next_due_subscription_delivery(recovery_now)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn subscription_delivery_detects_malformed_event_identity_and_fails_closed() {
        let mut store = subscription_store("subscriptions-delivery-malformed");
        let mut input = delivery_subscription_input("sub-delivery-malformed");
        input.rate_per_minute = 60;
        input.max_concurrency = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();
        let event_ids = delivery_event_ids(&store, &added.id);
        let due_at = delivery_row(&store, &added.id, &event_ids[0])
            .next_attempt_at
            .unwrap();
        let candidate = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        let board_events_before = board_event_count(&store);
        store
            .connection
            .execute(
                "UPDATE events SET event_hash=? WHERE seq=?",
                params![
                    "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                    candidate.event_seq
                ],
            )
            .unwrap();
        let next_error = match store.next_due_subscription_delivery(due_at) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected a malformed event identity rejection"),
        };
        assert!(next_error.contains("expected event hash"), "{next_error}");
        assert_eq!(
            delivery_row(&store, &added.id, &candidate.event_id).status,
            "pending"
        );
        let claim_error = match store.claim_subscription_delivery(
            &added.id,
            &candidate.event_id,
            due_at,
            5_000,
        ) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected a malformed event identity rejection"),
        };
        assert!(claim_error.contains("expected event hash"), "{claim_error}");
        assert_eq!(board_event_count(&store), board_events_before);
    }

    #[test]
    fn subscription_delivery_validation_helpers_reject_mutated_states_and_event_identity_drift() {
        let mut store = subscription_store("subscriptions-delivery-validation-helpers");
        let mut input = delivery_subscription_input("sub-delivery-validation-helpers");
        input.max_retries = 1;
        input.rate_per_minute = 60;
        input.max_concurrency = 1;
        let added = store.add_subscription(input).unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-subject"),
            "checkpoint_added",
            "test",
            &semantic_event_payload(
                "t-subject",
                ("parent", "t-parent"),
                "todo",
                "in_progress",
                &["pubsub"],
            )
            .to_string(),
            20,
        )
        .unwrap();
        store.materialize_subscriptions().unwrap();

        let subscription = store.require_subscription(&added.id).unwrap();
        let due_at = delivery_row(&store, &added.id, &delivery_event_ids(&store, &added.id)[0])
            .next_attempt_at
            .unwrap();
        let candidate = store
            .next_due_subscription_delivery(due_at)
            .unwrap()
            .unwrap();
        let candidate_event = board_event_by_seq(&store, candidate.event_seq);

        let mut malformed_pending = delivery_row(&store, &added.id, &candidate.event_id);
        malformed_pending.attempts = 1;
        let pending_error = error_string(validate_pending_or_retry_delivery(
            &malformed_pending,
            &subscription,
        ));
        assert!(
            pending_error.contains("malformed pending state"),
            "{pending_error}"
        );

        let leased = store
            .claim_subscription_delivery(&added.id, &candidate.event_id, due_at, 5_000)
            .unwrap()
            .unwrap();
        let mut malformed_leased = delivery_row(&store, &added.id, &candidate.event_id);
        malformed_leased.attempts = 0;
        let leased_error = error_string(validate_leased_delivery(&malformed_leased));
        assert!(
            leased_error.contains("malformed leased state"),
            "{leased_error}"
        );

        let mut unexpected_status = delivery_row(&store, &added.id, &candidate.event_id);
        unexpected_status.status = "leased".to_owned();
        let unexpected_error = error_string(validate_pending_or_retry_delivery(
            &unexpected_status,
            &subscription,
        ));
        assert!(
            unexpected_error.contains("not pending or retry_wait"),
            "{unexpected_error}"
        );

        let mut malformed_retry_wait = delivery_row(&store, &added.id, &candidate.event_id);
        malformed_retry_wait.status = "retry_wait".to_owned();
        malformed_retry_wait.attempts = 0;
        malformed_retry_wait.last_attempt_at = Some(candidate.next_attempt_at);
        malformed_retry_wait.last_error_code = Some("adapter_failed".to_owned());
        let retry_wait_error = error_string(validate_pending_or_retry_delivery(
            &malformed_retry_wait,
            &subscription,
        ));
        assert!(
            retry_wait_error.contains("malformed retry_wait state"),
            "{retry_wait_error}"
        );

        let mut malformed_hash = delivery_row(&store, &added.id, &candidate.event_id);
        malformed_hash.event_id = "not-a-hash".to_owned();
        let hash_error = error_string(require_delivery_event_identity(
            &store.connection,
            &malformed_hash,
            &subscription,
        ));
        assert!(hash_error.contains("malformed event hash"), "{hash_error}");

        let mut anchored = delivery_row(&store, &added.id, &candidate.event_id);
        anchored.event_seq = subscription.start_event_seq;
        let anchor_error = error_string(require_delivery_event_identity(
            &store.connection,
            &anchored,
            &subscription,
        ));
        assert!(
            anchor_error.contains("at or before start anchor"),
            "{anchor_error}"
        );

        let mut missing_seq = delivery_row(&store, &added.id, &candidate.event_id);
        missing_seq.event_seq = candidate_event.seq + 1_000;
        let missing_seq_error = error_string(require_delivery_event_identity(
            &store.connection,
            &missing_seq,
            &subscription,
        ));
        assert!(
            missing_seq_error.contains("expected event seq"),
            "{missing_seq_error}"
        );

        let mut mismatched_hash = delivery_row(&store, &added.id, &candidate.event_id);
        mismatched_hash.event_id = flip_lower_hex_prefix(
            candidate_event
                .event_hash
                .as_deref()
                .expect("board event hash is required"),
        );
        let mismatch_hash_error = error_string(require_delivery_event_identity(
            &store.connection,
            &mismatched_hash,
            &subscription,
        ));
        assert!(
            mismatch_hash_error.contains("expected event hash"),
            "{mismatch_hash_error}"
        );

        let mut mismatched_kind = delivery_row(&store, &added.id, &candidate.event_id);
        mismatched_kind.event_kind = "task_moved".to_owned();
        let mismatch_kind_error = error_string(require_delivery_event_identity(
            &store.connection,
            &mismatched_kind,
            &subscription,
        ));
        assert!(
            mismatch_kind_error.contains("expected event kind"),
            "{mismatch_kind_error}"
        );

        let mut mismatched_created_at = delivery_row(&store, &added.id, &candidate.event_id);
        mismatched_created_at.event_created_at = candidate_event.created_at + 1;
        let mismatch_created_at_error = error_string(require_delivery_event_identity(
            &store.connection,
            &mismatched_created_at,
            &subscription,
        ));
        assert!(
            mismatch_created_at_error.contains("expected event created_at"),
            "{mismatch_created_at_error}"
        );

        assert_eq!(leased.delivery_status, "leased");
        assert_eq!(leased.attempt_number, 1);
    }

    #[test]
    fn task_added_event_contains_semantic_snapshot() {
        let mut store = test_store("semantic-add");
        store.add_tag("zeta", None, Some("test")).unwrap();
        store.add_tag("alpha", None, Some("test")).unwrap();
        store
            .add_task(AddTask {
                id: Some("t-semantic".into()),
                task_type: "task".into(),
                parent_id: None,
                title: "semantic".into(),
                body: None,
                assignee: None,
                lane: None,
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "todo".into(),
                priority: 3,
                dependencies: Vec::new(),
                metadata: json!({}),
                actor: Some("test".into()),
                tags: vec!["zeta".into(), "alpha".into()],
                allowed_models: Vec::new(),
            })
            .unwrap();
        let event = store
            .events(Some("t-semantic"), Some("task_added"), 1, false)
            .unwrap()
            .pop()
            .unwrap();
        let snapshot = &event.payload["_semanticV1"];
        assert_eq!(
            snapshot["subject"],
            json!({"type":"task","id":"t-semantic"})
        );
        assert_eq!(snapshot["tags"], json!(["alpha", "zeta"]));
        assert_eq!(snapshot["priorStatus"], Value::Null);
        assert_eq!(snapshot["currentStatus"], "todo");
    }

    #[test]
    fn task_moved_event_records_status_transition() {
        let mut store = test_store("semantic-move");
        store
            .add_task(AddTask {
                id: Some("t-move".into()),
                task_type: "task".into(),
                parent_id: None,
                title: "move".into(),
                body: None,
                assignee: None,
                lane: None,
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "todo".into(),
                priority: 3,
                dependencies: Vec::new(),
                metadata: json!({}),
                actor: Some("test".into()),
                tags: Vec::new(),
                allowed_models: Vec::new(),
            })
            .unwrap();
        store
            .move_task("t-move", "done", "test", json!({}), false)
            .unwrap();
        let event = store
            .events(Some("t-move"), Some("task_moved"), 1, false)
            .unwrap()
            .pop()
            .unwrap();
        let snapshot = &event.payload["_semanticV1"];
        assert_eq!(snapshot["priorStatus"], "todo");
        assert_eq!(snapshot["currentStatus"], "done");
    }

    #[test]
    fn semantic_snapshot_sorts_typed_parent_ancestor_and_dependency_relations() {
        let store = test_store("semantic-relations");
        for id in ["e-root", "e-parent", "s-parent", "d-one", "t-child"] {
            insert_task(&store, id);
        }
        store
            .connection
            .execute(
                "UPDATE tasks SET type='epic',parent_id=? WHERE id=?",
                params![Option::<String>::None, "e-root"],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE tasks SET type='epic',parent_id=? WHERE id=?",
                params!["e-root", "e-parent"],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE tasks SET type='story',parent_id=? WHERE id=?",
                params!["e-parent", "s-parent"],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE tasks SET parent_id=? WHERE id=?",
                params!["s-parent", "t-child"],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO task_dependencies(task_id,depends_on) VALUES(?,?)",
                params!["t-child", "d-one"],
            )
            .unwrap();
        let snapshot =
            semantic_snapshot(&store.connection, "t-child", Some("todo"), Some("todo")).unwrap();
        let relations = snapshot["relations"].as_array().unwrap();
        let encoded = relations.iter().map(Value::to_string).collect::<Vec<_>>();
        assert!(encoded.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(relations.contains(&json!({"kind":"parent","type":"story","id":"s-parent"})));
        assert!(relations.contains(&json!({"kind":"ancestor","type":"epic","id":"e-parent"})));
        assert!(relations.contains(&json!({"kind":"ancestor","type":"epic","id":"e-root"})));
        assert!(relations.contains(&json!({"kind":"depends-on","type":"task","id":"d-one"})));
    }

    #[test]
    fn events_since_replays_removed_task_subjects() {
        let mut store = test_store("events-since-removed");
        store
            .add_task(AddTask {
                id: Some("t-removed".into()),
                task_type: "task".into(),
                parent_id: None,
                title: "remove".into(),
                body: None,
                assignee: None,
                lane: None,
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "todo".into(),
                priority: 3,
                dependencies: Vec::new(),
                metadata: json!({}),
                actor: Some("test".into()),
                tags: Vec::new(),
                allowed_models: Vec::new(),
            })
            .unwrap();
        store.remove_task("t-removed", "test", false).unwrap();
        let events = store
            .events_since(Some("task_removed"), 0, 10, true)
            .unwrap();
        assert_eq!(events.len(), 1);
        let filtered = store
            .events_since_filtered(
                Some("t-removed"),
                &["task_removed".to_owned()],
                &[],
                &[],
                &[],
                &[],
                0,
                10,
                true,
            )
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert!(store.watch_subject_exists("t-removed").unwrap());
        assert!(!store.watch_subject_exists("t-never-existed").unwrap());
    }

    #[test]
    fn watch_relation_targets_accept_current_and_historical_ids_but_reject_unknown_ids() {
        let store = test_store("watch-relation-targets");
        insert_task(&store, "s-current");
        assert!(
            store
                .watch_relation_target_exists("parent", "s-current")
                .unwrap()
        );
        crate::audit::append_board_event(
            &store.connection,
            Some("t-invalid-relations"),
            "task_removed",
            "test",
            r#"{"_semanticV1":{"relations":["oops",42,null]}}"#,
            1,
        )
        .unwrap();
        crate::audit::append_board_event(
            &store.connection,
            Some("t-historical"),
            "task_removed",
            "test",
            r#"{"_semanticV1":{"relations":[{"kind":"parent","type":"story","id":"s-1"}]}}"#,
            1,
        )
        .unwrap();
        assert!(store.watch_relation_target_exists("parent", "s-1").unwrap());
        assert!(
            !store
                .watch_relation_target_exists("parent", "not-history")
                .unwrap()
        );
        assert!(
            !store
                .watch_relation_target_exists("parent", "s-never-existed")
                .unwrap()
        );
    }

    #[test]
    fn filtered_watch_rows_apply_semantics_before_limit_and_fail_closed_on_legacy_rows() {
        let store = test_store("filtered-watch-sparse");
        let append = |kind: &str, payload: &str| {
            crate::audit::append_board_event(
                &store.connection,
                Some("gone-task"),
                kind,
                "codex",
                payload,
                1,
            )
            .unwrap();
        };
        append("legacy", r#"{"note":"no snapshot"}"#);
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        append("malformed", "not json");
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=OFF")
            .unwrap();
        append(
            "wrong-relations",
            r#"{"_semanticV1":{"subject":{"type":"task","id":"gone-task"},"relations":{"kind":"parent","type":"story","id":"s-1"},"priorStatus":"todo","currentStatus":"done","tags":["infra"]}}"#,
        );
        append(
            "wrong-tags",
            r#"{"_semanticV1":{"subject":{"type":"task","id":"gone-task"},"relations":[{"kind":"parent","type":"story","id":"s-1"}],"priorStatus":"todo","currentStatus":"done","tags":{"name":"infra"}}}"#,
        );
        append(
            "wrong-relation-elements",
            r#"{"_semanticV1":{"subject":{"type":"task","id":"gone-task"},"relations":["oops",42,null],"priorStatus":"todo","currentStatus":"done","tags":["infra"]}}"#,
        );
        append(
            "wanted",
            r#"{"_semanticV1":{"subject":{"type":"task","id":"gone-task"},"relations":[{"kind":"parent","type":"story","id":"s-1"}],"priorStatus":"todo","currentStatus":"done","tags":["infra"]}}"#,
        );
        append(
            "other",
            r#"{"_semanticV1":{"subject":{"type":"task","id":"gone-task"},"relations":[],"priorStatus":"todo","currentStatus":"review","tags":["docs"]}}"#,
        );
        let relations = vec!["parent:s-1".to_owned()];
        let prior = vec!["todo".to_owned()];
        let current = vec!["done".to_owned()];
        let tags = vec!["infra".to_owned()];
        let rows = store
            .events_since_filtered(
                Some("gone-task"),
                &[
                    "malformed".to_owned(),
                    "wrong-relations".to_owned(),
                    "wrong-tags".to_owned(),
                    "wrong-relation-elements".to_owned(),
                    "wanted".to_owned(),
                ],
                &relations,
                &prior,
                &current,
                &tags,
                0,
                1,
                true,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "wanted");
        assert_eq!(
            store
                .events_since_filtered(
                    Some("gone-task"),
                    &[
                        "wrong-relations".to_owned(),
                        "wrong-relation-elements".to_owned(),
                        "wanted".to_owned(),
                    ],
                    &relations,
                    &prior,
                    &current,
                    &[],
                    0,
                    1,
                    true,
                )
                .unwrap()
                .into_iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec!["wanted".to_owned()]
        );
        assert_eq!(
            store
                .events_since_filtered(
                    Some("gone-task"),
                    &["wrong-tags".to_owned(), "wanted".to_owned()],
                    &[],
                    &prior,
                    &current,
                    &tags,
                    0,
                    1,
                    true,
                )
                .unwrap()
                .into_iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec!["wanted".to_owned()]
        );
        assert!(
            store
                .events_since_filtered(
                    Some("gone-task"),
                    &["wrong-relations".to_owned(), "wanted".to_owned()],
                    &relations,
                    &[],
                    &[],
                    &[],
                    0,
                    10,
                    true,
                )
                .unwrap()
                .iter()
                .all(|event| event.kind == "wanted")
        );
        assert!(
            store
                .events_since_filtered(
                    Some("gone-task"),
                    &[],
                    &[],
                    &[],
                    &[],
                    &["unknown".to_owned()],
                    0,
                    10,
                    true,
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn checkpoint_events_capture_each_projected_status() {
        let mut store = test_store("checkpoint-status");
        for (id, state, expected) in [
            ("t-continue", "continue", "in_progress"),
            ("t-blocked", "blocked", "blocked"),
            ("t-done", "done", "done"),
        ] {
            insert_task(&store, id);
            let claim = store
                .claim(
                    Some(id),
                    ClaimOptions {
                        agent_id: "test".into(),
                        session_id: None,
                        lease_ms: 60_000,
                        caller_lane: None,
                        role_filter: None,
                        caller_scope: None,
                        cross_lane: false,
                        allow_reassign: false,
                        sprint_override: None,
                        model: None,
                        git: None,
                    },
                )
                .unwrap();
            store
                .checkpoint(CheckpointInput {
                    task_id: id.into(),
                    lease_token: claim.claim.lease_token,
                    author: "test".into(),
                    session_id: None,
                    model: None,
                    state: state.into(),
                    summary: "summary".into(),
                    intent: "intent".into(),
                    next_action: "next".into(),
                    blockers: Vec::new(),
                    validations: Vec::new(),
                    repo_path: None,
                    branch: None,
                    head_sha: None,
                    dirty_summary: None,
                    root_head: None,
                })
                .unwrap();
            let event = store
                .events(Some(id), Some("checkpoint_added"), 1, false)
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(event.payload["_semanticV1"]["priorStatus"], "in_progress");
            assert_eq!(event.payload["_semanticV1"]["currentStatus"], expected);
        }
    }

    #[test]
    fn incremental_writes_embed_every_source_kind_so_health_stays_clean() {
        let mut store = test_store("incremental-embed");
        store
            .add_task(AddTask {
                id: Some("t-embed".into()),
                task_type: "task".into(),
                parent_id: None,
                title: "Incremental embedding".into(),
                body: Some("A task whose vector must be written on add.".into()),
                assignee: None,
                lane: Some("driver".into()),
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "todo".into(),
                priority: 3,
                dependencies: Vec::new(),
                metadata: json!({}),
                actor: Some("test".into()),
                tags: Vec::new(),
                allowed_models: Vec::new(),
            })
            .unwrap();

        store
            .add_note("t-embed", "test", "progress", "A note written inline.")
            .unwrap();

        let claim = store
            .claim(
                Some("t-embed"),
                ClaimOptions {
                    agent_id: "test".into(),
                    session_id: None,
                    lease_ms: 60_000,
                    caller_lane: None,
                    role_filter: None,
                    caller_scope: None,
                    cross_lane: false,
                    allow_reassign: false,
                    sprint_override: None,
                    model: None,
                    git: None,
                },
            )
            .unwrap();
        store
            .checkpoint(CheckpointInput {
                task_id: "t-embed".into(),
                lease_token: claim.claim.lease_token,
                author: "test".into(),
                session_id: None,
                model: None,
                state: "continue".into(),
                summary: "checkpoint summary".into(),
                intent: "intent".into(),
                next_action: "next".into(),
                blockers: Vec::new(),
                validations: Vec::new(),
                repo_path: None,
                branch: None,
                head_sha: None,
                dirty_summary: None,
                root_head: None,
            })
            .unwrap();

        store
            .raise_attention(
                "An attention row must be searchable.",
                "decision",
                "test",
                Some("t-embed"),
                3,
                &[],
                &DecisionCard::default(),
            )
            .unwrap();

        store
            .post_sitrep(
                "driver",
                "A sitrep for the driver lane.",
                "test",
                Some("t-embed"),
                None,
            )
            .unwrap();

        let health = store.search_health().unwrap();
        assert_eq!(
            health.missing_embeddings, 0,
            "incremental writes left documents without embeddings: {:?}",
            health.unhealthy_because
        );
        assert_eq!(health.stale_embeddings, 0, "{:?}", health.unhealthy_because);
        assert!(health.healthy, "{:?}", health.unhealthy_because);
    }

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
            priority_level: Some("P1".into()),
            created_at: 0,
            updated_at: 0,
            completed_at: None,
            archived: false,
            archived_at: None,
            metadata: json!({}),
            tags: Vec::new(),
            allowed_models: Vec::new(),
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

        // A namespaced name is the same alphabet, one level deeper, and the
        // estate half is checked because `ifac/aix-chat` beside
        // `ifca/aix-chat` is the master file's own collision one level up.
        // One table, because every row here is the same question.
        for (name, accepted) in [
            ("ifca/aix-chat", true),
            ("geoyws/infra", true),
            ("unum/a/b", true),
            ("infra", true),
            ("Ifca/x", false),
            ("ifca//x", false),
            ("/ifca", false),
            ("ifca/", false),
            ("ifca_x", false),
            ("acme/x", false),
        ] {
            let outcome = validate_tag_name(name);
            assert_eq!(
                outcome.is_ok(),
                accepted,
                "tag {name}: {:?}",
                outcome.as_ref().err().map(ToString::to_string)
            );
            if accepted {
                assert_eq!(outcome.expect("accepted"), name);
            }
        }

        // An unregistered estate is refused by name, not by shape: the fix is
        // a different first segment, and the sentence has to say so.
        let estate = validate_tag_name("acme/x")
            .expect_err("acme is not a registered estate")
            .to_string();
        assert!(estate.contains("acme"), "{estate}");
        assert!(estate.contains("ifca, unum, geoyws"), "{estate}");
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

    #[test]
    fn ascending_board_events_resume_without_skips_and_respect_archives() {
        let store = test_store("board-events");
        insert_task(&store, "task-1");
        insert_task(&store, "task-2");

        crate::audit::append_board_event(
            &store.connection,
            Some("task-1"),
            "task_started",
            "codex",
            r#"{"step":1}"#,
            10,
        )
        .expect("append first event");
        crate::audit::append_board_event(
            &store.connection,
            Some("task-2"),
            "task_started",
            "codex",
            r#"{"step":2}"#,
            11,
        )
        .expect("append second event");
        store
            .connection
            .execute(
                "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) \
                 VALUES(?,?,?,?,?,?,?,?,?)",
                params![
                    3_i64,
                    "task-1",
                    "task_archived",
                    "codex",
                    r#"{"step":3}"#,
                    12_i64,
                    1_i64,
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "hash-3",
                ],
            )
            .expect("insert archived event");
        crate::audit::append_board_event(
            &store.connection,
            Some("task-1"),
            "task_finished",
            "codex",
            r#"{"step":4}"#,
            13,
        )
        .expect("append fourth event");
        crate::audit::append_board_event(
            &store.connection,
            Some("task-2"),
            "task_finished",
            "codex",
            r#"{"step":5}"#,
            14,
        )
        .expect("append fifth event");

        let all = store
            .events_since(None, 0, 10, true)
            .expect("read all board events");
        assert_eq!(board_event_seqs(&all), vec![1, 2, 3, 4, 5]);

        let active = store
            .events_since(None, 0, 10, false)
            .expect("read active board events");
        assert_eq!(board_event_seqs(&active), vec![1, 2, 4, 5]);

        let empty = store
            .events_since(None, 0, 0, true)
            .expect("zero limit is allowed");
        assert!(empty.is_empty());

        let task_events = store
            .events_since_filtered(Some("task-1"), &[], &[], &[], &[], &[], 1, 10, true)
            .expect("resume task events");
        assert_eq!(board_event_seqs(&task_events), vec![3, 4]);

        let kind_events = store
            .events_since_filtered(
                Some("task-1"),
                &["task_finished".to_owned()],
                &[],
                &[],
                &[],
                &[],
                0,
                10,
                true,
            )
            .expect("filter by task and kind");
        assert_eq!(board_event_seqs(&kind_events), vec![4]);

        let first_batch = store
            .events_since_filtered(Some("task-1"), &[], &[], &[], &[], &[], 0, 1, true)
            .expect("first batch");
        assert_eq!(board_event_seqs(&first_batch), vec![1]);

        let second_batch = store
            .events_since_filtered(
                Some("task-1"),
                &[],
                &[],
                &[],
                &[],
                &[],
                first_batch.last().unwrap().seq,
                1,
                true,
            )
            .expect("second batch");
        assert_eq!(board_event_seqs(&second_batch), vec![3]);

        let third_batch = store
            .events_since_filtered(
                Some("task-1"),
                &[],
                &[],
                &[],
                &[],
                &[],
                second_batch.last().unwrap().seq,
                1,
                true,
            )
            .expect("third batch");
        assert_eq!(board_event_seqs(&third_batch), vec![4]);

        let negative = store
            .events_since(None, 0, -1, true)
            .expect_err("negative limits must be rejected")
            .to_string();
        assert!(negative.contains("1000000"), "{negative}");

        // The ceiling itself is a legal page, and only the row past it is not.
        store
            .events_since(None, 0, crate::LIMIT_CEILING, true)
            .expect("the ceiling is a limit, not a refusal");
        let over = store
            .events_since(None, 0, crate::LIMIT_CEILING + 1, true)
            .expect_err("over-ceiling limits must be rejected")
            .to_string();
        assert!(over.contains("1000000"), "{over}");
    }

    fn insert_lane_task(store: &Store, id: &str, lane: Option<&str>) {
        insert_task(store, id);
        store
            .connection
            .execute("UPDATE tasks SET lane=? WHERE id=?", params![lane, id])
            .expect("set lane");
    }

    #[test]
    fn list_tasks_lane_filter_matches_the_row_lane_exactly() {
        let store = test_store("list-tasks-lane");
        insert_lane_task(&store, "t-two", Some("driver-2"));
        insert_lane_task(&store, "t-three", Some("driver-3"));
        insert_lane_task(&store, "t-none", None);

        let ids = |lane: Option<&str>| {
            store
                .list_tasks(None, None, lane, None, false)
                .unwrap()
                .into_iter()
                .map(|task| task.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Some("driver-2")), ["t-two"]);
        assert_eq!(ids(Some("driver-3")), ["t-three"]);
        // A lane nobody is in is empty, and a task without a lane is in none.
        assert_eq!(ids(Some("driver")), [] as [String; 0]);
        assert_eq!(ids(None).len(), 3, "no filter is the whole board");
        let error = store
            .list_tasks(None, None, Some("  "), None, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--lane must name a lane"), "{error}");
    }

    #[test]
    fn a_listing_reads_a_lapsed_lease_as_free_without_waiting_for_the_sweep() {
        // `Store::open` sweeps expired leases before anything reads, so the
        // CLI never sees one; a long-lived process that opened an hour ago
        // does. The join must apply the same expiry test `get_claim` does,
        // or `claimed: true` outlives the lease it reports.
        let store = test_store("list-tasks-lapsed-lease");
        insert_lane_task(&store, "t-live", None);
        insert_lane_task(&store, "t-lapsed", None);
        insert_lane_task(&store, "t-free", None);
        let now = now_ms();
        for (id, agent, expires_at) in [
            ("t-live", "driver-2", now + 60_000),
            ("t-lapsed", "ghost", now - 1),
        ] {
            store
                .connection
                .execute(
                    "INSERT INTO task_claims(task_id,agent_id,lease_token,claimed_at,heartbeat_at,expires_at) \
                     VALUES(?,?,?,?,?,?)",
                    params![id, agent, format!("{id}-token"), now - 10, now - 10, expires_at],
                )
                .expect("insert claim");
        }
        let listed = store
            .list_tasks_with_claims(None, None, None, None, false)
            .unwrap();
        let claim = |id: &str| {
            listed
                .iter()
                .find(|(task, _)| task.id == id)
                .map(|(_, claim)| claim.clone())
                .unwrap()
        };
        assert_eq!(
            claim("t-live").map(|claim| claim.agent_id).as_deref(),
            Some("driver-2")
        );
        assert!(claim("t-lapsed").is_none(), "a lapsed lease read as held");
        assert!(claim("t-free").is_none());
        assert_eq!(listed.len(), 3, "the join must not drop or duplicate rows");
        // What the join reports agrees with the per-task read `task show` uses.
        for id in ["t-live", "t-lapsed", "t-free"] {
            assert_eq!(
                store.get_claim(id).unwrap().map(|claim| claim.agent_id),
                claim(id).map(|claim| claim.agent_id),
                "{id}"
            );
        }
    }

    #[test]
    fn attention_lane_matches_the_raiser_suffix_or_the_task_lane() {
        let mut store = test_store("attention-lane");
        store.initialize("LANES", "geoyws").unwrap();
        insert_lane_task(&store, "t-lane", Some("driver-2"));
        insert_lane_task(&store, "t-other", Some("driver-3"));
        let raise = |store: &mut Store, body: &str, raiser: &str, task: Option<&str>| {
            store
                .raise_attention(
                    body,
                    "decision",
                    raiser,
                    task,
                    6,
                    &[],
                    &DecisionCard::default(),
                )
                .expect("raise")
                .id
        };
        // Route one: the raiser is `<name>@driver-2`, about no task.
        let by_raiser = raise(&mut store, "raiser route", "worker@driver-2", None);
        // Route two: raised by someone else, about a task in driver-2.
        let by_task = raise(&mut store, "task route", "geoyws", Some("t-lane"));
        // Neither: the raiser's lane is another, and so is the task's.
        raise(&mut store, "elsewhere", "worker@driver-3", Some("t-other"));
        // `driver-2` is a suffix of `@driver-2` only after the `@`: a raiser
        // in a lane that merely ends the same way must not match.
        raise(&mut store, "near miss", "worker@xdriver-2", None);

        let ids = |lane: Option<&str>, limit: i64| {
            store
                .attention(None, None, None, None, lane, limit, false)
                .unwrap()
                .into_iter()
                .map(|row| row.id)
                .collect::<Vec<_>>()
        };
        let mut matched = ids(Some("driver-2"), 100);
        matched.sort();
        let mut expected = vec![by_raiser.clone(), by_task.clone()];
        expected.sort();
        assert_eq!(matched, expected);
        // The filter is applied before the limit, not to a page after it.
        assert_eq!(ids(Some("driver-2"), 1).len(), 1);
        assert!(expected.contains(&ids(Some("driver-2"), 1)[0]));
        assert_eq!(ids(Some("driver-3"), 100).len(), 1);
        assert_eq!(ids(None, 100).len(), 4, "no filter is every row");
    }

    /// One artifact-identity attempt and one Git attempt on the same board,
    /// so the mode refusals below are read off real rows.
    fn deploy_identity_fixture(
        name: &str,
    ) -> (Store, DeploymentStartReceipt, DeploymentStartReceipt) {
        let mut store = test_store(name);
        store.initialize(name, "test@driver").unwrap();
        let start = |store: &mut Store, identity: DeployIdentity, environment: &str| {
            store
                .start_deployment(StartDeployment {
                    task_id: None,
                    repo: "geoyws/legacy-stack".to_owned(),
                    identity,
                    deployer_checkout: None,
                    branch: None,
                    tier: "@_p".to_owned(),
                    environment: environment.to_owned(),
                    host: "hax".to_owned(),
                    url: "https://legacy.geoy.ws".to_owned(),
                    mechanism: None,
                    operation_id: None,
                    retry_of: None,
                    actor: "geoyws".to_owned(),
                    lane: None,
                    sprint_id: None,
                })
                .expect("start the attempt")
        };
        let artifact = start(
            &mut store,
            DeployIdentity::Artifact(
                ArtifactIdentity::parse(
                    &[format!(
                        "api={ARTIFACT_KIND_IMAGE_ID}:sha256:{}",
                        "1".repeat(64)
                    )],
                    "--artifact",
                )
                .unwrap(),
            ),
            "recovery",
        );
        let git = start(
            &mut store,
            DeployIdentity::Git("a".repeat(40)),
            "production",
        );
        (store, artifact, git)
    }

    fn finish_input(receipt: &DeploymentStartReceipt) -> FinishDeployment {
        FinishDeployment {
            id: receipt.deployment.id.clone(),
            capability_token: receipt.capability_token.clone(),
            result: "succeeded".to_owned(),
            phase: Some("verification".to_owned()),
            receipt: Some("read both identities off the running tier".to_owned()),
            artifact_uri: None,
            served_commit: None,
            observed: Vec::new(),
            actor: "geoyws".to_owned(),
            served_version: None,
        }
    }

    /// The refusal ADR-043 exists for: a success proved by a digest may not be
    /// recorded as a verified Git commit, and the Git path may not be proved
    /// by a digest either.
    #[test]
    fn a_finish_refuses_the_other_identity_mode_s_proof_by_name() {
        let (mut store, artifact, git) = deploy_identity_fixture("deploy-identity-modes");

        let mut served = finish_input(&artifact);
        served.served_commit = Some("a".repeat(40));
        assert_eq!(
            error_string(store.finish_deployment(served)),
            format!(
                "deployment {} was started in artifact-identity mode, where no Git commit \
                 was proved; --served-commit cannot be recorded for it — verify it with \
                 --observed ROLE=KIND:VALUE for every expected role",
                artifact.deployment.id
            )
        );

        let mut observed = finish_input(&git);
        observed.served_commit = Some("a".repeat(40));
        observed.observed = ArtifactIdentity::parse(
            &[format!(
                "api={ARTIFACT_KIND_IMAGE_ID}:sha256:{}",
                "1".repeat(64)
            )],
            "--observed",
        )
        .unwrap();
        assert_eq!(
            error_string(store.finish_deployment(observed)),
            format!(
                "deployment {} was started in Git mode; --observed belongs to \
                 artifact-identity mode — verify it with --served-commit FULL_SHA",
                git.deployment.id
            )
        );

        // Both attempts are still open: a refused finish writes nothing.
        for receipt in [&artifact, &git] {
            assert_eq!(
                store
                    .require_deployment(&receipt.deployment.id)
                    .unwrap()
                    .status,
                "started"
            );
        }
    }

    /// A recovery attempt's row says `unknown` where a build SHA would be, in
    /// storage and in every projection, and records what was observed per
    /// role once it succeeds.
    #[test]
    fn a_succeeded_artifact_attempt_records_unknown_provenance_and_its_observations() {
        let (mut store, artifact, _) = deploy_identity_fixture("deploy-identity-recovery");
        let mut finish = finish_input(&artifact);
        finish.observed = ArtifactIdentity::parse(
            &[format!(
                "api={ARTIFACT_KIND_IMAGE_ID}:sha256:{}",
                "1".repeat(64)
            )],
            "--observed",
        )
        .unwrap();
        let done = store
            .finish_deployment(finish)
            .expect("the identities match");
        assert_eq!(done.identity_mode, IDENTITY_MODE_ARTIFACT);
        assert_eq!(done.build_commit, UNKNOWN_BUILD_COMMIT);
        assert_eq!(done.build_commit_label, UNKNOWN_BUILD_COMMIT_WORDS);
        assert_eq!(done.served_commit, None);
        assert_eq!(
            done.artifacts
                .iter()
                .map(|artifact| (
                    artifact.role.as_str(),
                    artifact.expected.as_str(),
                    artifact.observed.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![(
                "api",
                format!("sha256:{}", "1".repeat(64)).as_str(),
                Some(format!("sha256:{}", "1".repeat(64)).as_str())
            )]
        );
        // The projection every "what is live where" reader takes carries the
        // words, so nothing downstream has to know what `unknown` means.
        let live = store.current_deployments().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].build_commit_label, UNKNOWN_BUILD_COMMIT_WORDS);
    }

    /// A board with sprints enabled, ready for sprint work.
    fn sprint_store(name: &str) -> Store {
        let mut store = test_store(name);
        store.initialize(name, "test@driver").unwrap();
        store
    }

    fn new_sprint(id: &str, version: &str) -> NewSprint {
        NewSprint {
            id: Some(id.to_owned()),
            title: format!("{id} fixture"),
            body: Some("Fixture goal\nFixture acceptance criteria".to_owned()),
            target_version: version.to_owned(),
            scheduled_start: 0,
            scheduled_end: i64::MAX,
            actor: "geoyws".to_owned(),
        }
    }
    fn plan_empty(store: &mut Store, id: &str) {
        store
            .plan_sprint(
                id,
                "Fixture goal\nFixture acceptance criteria",
                &[],
                None,
                true,
                "geoyws",
            )
            .unwrap();
    }

    fn task_sprint_id(store: &Store, id: &str) -> Option<String> {
        store
            .connection
            .query_row(
                "SELECT sprint_id FROM task_sprints WHERE task_id=?",
                [id],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
    }

    fn last_event_payload(store: &Store, kind: &str) -> Value {
        store
            .connection
            .query_row(
                "SELECT payload FROM events WHERE kind=? ORDER BY seq DESC LIMIT 1",
                [kind],
                |row| {
                    let text: String = row.get(0)?;
                    Ok(serde_json::from_str(&text)
                        .expect("the events table stores valid JSON payloads"))
                },
            )
            .unwrap()
    }

    #[test]
    fn starting_a_second_current_sprint_is_refused_naming_the_holder() {
        let mut store = sprint_store("sprint-one-current");
        store
            .create_sprint(new_sprint("sp-first", "0.1.0"))
            .unwrap();
        plan_empty(&mut store, "sp-first");
        store.start_sprint("sp-first", "geoyws").unwrap();
        let before_events: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        let before_body = store.require_sprint("sp-first").unwrap().body;
        let refusal = error_string(store.plan_sprint(
            "sp-first",
            "replacement goal\ncriteria",
            &[],
            None,
            true,
            "geoyws",
        ));
        assert!(
            refusal.contains("current and cannot be replanned"),
            "{refusal}"
        );
        assert_eq!(store.require_sprint("sp-first").unwrap().body, before_body);
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            before_events
        );
        store
            .create_sprint(new_sprint("sp-second", "0.2.0"))
            .unwrap();
        plan_empty(&mut store, "sp-second");
        assert_eq!(
            error_string(store.start_sprint("sp-second", "geoyws")),
            "sprint sp-first is current; close or abandon it before starting sp-second"
        );
        // The refused start left the row planned, and closing out the first
        // opens the way.
        assert_eq!(store.require_sprint("sp-second").unwrap().status, "planned");
        store
            .abandon_sprint("sp-first", "the boundary moved", "geoyws")
            .unwrap();
        store.start_sprint("sp-second", "geoyws").unwrap();
        assert_eq!(store.require_sprint("sp-second").unwrap().status, "current");
        assert!(store.require_sprint("sp-second").unwrap().starts_at > 0);
    }

    #[test]
    fn a_sprint_closes_only_on_a_succeeded_verification_deployment() {
        let mut store = sprint_store("sprint-close-gate");
        store
            .create_sprint(new_sprint("sp-close", "0.4.0"))
            .unwrap();
        plan_empty(&mut store, "sp-close");
        store.start_sprint("sp-close", "geoyws").unwrap();

        // ADR-045 §2 refusal 3: no proof offered at all.
        assert_eq!(
            error_string(store.close_sprint("sp-close", None, None, None, "geoyws")),
            "sprint close requires --deployment d-…: the version is served or the sprint is not done"
        );

        // A started attempt is not proof: refusal 2, verbatim.
        let started = store
            .start_deployment(StartDeployment {
                task_id: None,
                repo: "geoyws/legacy-stack".to_owned(),
                identity: DeployIdentity::Git("a".repeat(40)),
                deployer_checkout: None,
                branch: None,
                tier: "@_p".to_owned(),
                environment: "production".to_owned(),
                host: "hax".to_owned(),
                url: "https://legacy.geoy.ws".to_owned(),
                mechanism: None,
                operation_id: None,
                retry_of: None,
                actor: "geoyws".to_owned(),
                lane: None,
                sprint_id: Some("sp-close".to_owned()),
            })
            .unwrap();
        assert_eq!(
            error_string(store.close_sprint(
                "sp-close",
                Some(&started.deployment.id),
                None,
                None,
                "geoyws"
            )),
            format!(
                "sprint sp-close cannot close on {}: that attempt is started (no phase); \
                 a sprint closes only on a succeeded verification-phase deployment",
                started.deployment.id
            )
        );

        // A succeeded verification attempt closes it and stamps the proof.
        let mut finish = finish_input(&started);
        finish.served_commit = Some("a".repeat(40));
        finish.served_version = Some("0.4.0".to_owned());
        store.finish_deployment(finish).unwrap();
        let closed = store
            .close_sprint(
                "sp-close",
                Some(&started.deployment.id),
                None,
                None,
                "geoyws",
            )
            .unwrap();
        assert_eq!(closed.status, "closed");
        assert_eq!(
            closed.closed_by_deployment.as_deref(),
            Some(started.deployment.id.as_str())
        );
        assert!(closed.ends_at.is_some());
        assert_eq!(
            last_event_payload(&store, "sprint_closed")["deploymentID"],
            json!(started.deployment.id)
        );
        assert_eq!(
            last_event_payload(&store, "sprint_closed")["targetVersion"],
            json!("0.4.0")
        );

        // ADR-045 §2 refusal 6: closed is history.
        assert_eq!(
            error_string(store.plan_sprint("sp-close", "rewrite", &[], None, false, "geoyws")),
            "sprint sp-close is closed history; its card cannot be rewritten"
        );
        assert_eq!(
            error_string(store.close_sprint(
                "sp-close",
                Some(&started.deployment.id),
                None,
                None,
                "geoyws"
            )),
            "sprint sp-close is closed history; its card cannot be rewritten"
        );
    }

    #[test]
    fn a_planned_sprint_cannot_close_and_abandon_needs_a_note() {
        let mut store = sprint_store("sprint-lifecycle-refusals");
        store.create_sprint(new_sprint("sp-plan", "0.1.0")).unwrap();
        assert_eq!(
            error_string(store.close_sprint("sp-plan", Some("d-none"), None, None, "geoyws")),
            "sprint sp-plan is planned, not current; start it before closing it"
        );
        assert_eq!(
            error_string(store.abandon_sprint("sp-plan", "  ", "geoyws")),
            "abandon note is required"
        );
        let abandoned = store
            .abandon_sprint("sp-plan", "the release is pulled", "geoyws")
            .unwrap();
        assert_eq!(abandoned.status, "abandoned");
        assert!(abandoned.ends_at.is_some());
        assert_eq!(
            last_event_payload(&store, "sprint_abandoned")["note"],
            json!("the release is pulled")
        );
        assert_eq!(
            error_string(store.start_sprint("sp-plan", "geoyws")),
            "sprint sp-plan is abandoned; open a new sprint instead of starting this one"
        );
    }

    #[test]
    fn a_version_must_be_semver_shaped_before_a_sprint_exists() {
        assert_eq!(target_version("0.4.0").unwrap(), "0.4.0");
        assert_eq!(target_version(" 1.2.3 ").unwrap(), "1.2.3");
        assert_eq!(target_version("0.4.0-rc.1").unwrap(), "0.4.0-rc.1");
        assert_eq!(
            target_version("10.20.30-build.1").unwrap(),
            "10.20.30-build.1"
        );
        for bad in ["", "v0.4", "0.4", "0.4.0.1", "0.4.x", "latest", "0..3"] {
            assert!(
                target_version(bad).is_err(),
                "{bad} was accepted as a version"
            );
        }
        let mut store = sprint_store("sprint-version-shape");
        let error = error_string(store.create_sprint(NewSprint {
            id: None,
            title: "Bad version".into(),
            body: None,
            target_version: "v1".into(),
            scheduled_start: 0,
            scheduled_end: 1,
            actor: "geoyws".into(),
        }));
        assert!(error.contains("X.Y.Z"), "{error}");
        assert!(error.contains("\"v1\""), "{error}");
    }

    #[test]
    fn claims_are_scoped_to_the_current_sprint_and_overrides_are_recorded() {
        let mut store = sprint_store("sprint-claim-scope");
        insert_task(&store, "t-in");
        insert_task(&store, "t-out");
        insert_task(&store, "t-free");
        store.create_sprint(new_sprint("sp-live", "0.4.0")).unwrap();
        store.create_sprint(new_sprint("sp-next", "0.5.0")).unwrap();
        store.attach_sprint("t-in", "sp-live", "geoyws").unwrap();
        store.attach_sprint("t-out", "sp-next", "geoyws").unwrap();
        store
            .plan_sprint(
                "sp-live",
                "Fixture goal\nFixture acceptance criteria",
                &[],
                None,
                false,
                "geoyws",
            )
            .unwrap();
        store.start_sprint("sp-live", "geoyws").unwrap();

        let ids = |options: &ClaimOptions| {
            store
                .claim_candidates(options, None, 10)
                .unwrap()
                .into_iter()
                .map(|task| task.id)
                .collect::<Vec<_>>()
        };
        // The default pool is the current sprint's rows — unattached is
        // outside the boundary, not inside it (ADR-045 §3).
        assert_eq!(ids(&claim_options("agent")), vec!["t-in"]);
        // The escape hatch sees everything again.
        let mut any = claim_options("agent");
        any.sprint_override = Some("any".into());
        let mut any_ids = ids(&any);
        any_ids.sort();
        assert_eq!(any_ids, vec!["t-free", "t-in", "t-out"]);
        // A named boundary sees only its own rows.
        let mut named = claim_options("agent");
        named.sprint_override = Some("sp-next".into());
        assert_eq!(ids(&named), vec!["t-out"]);
        // ADR-045 §2 refusal 4: unknown and abandoned names.
        named.sprint_override = Some("sp-nope".into());
        assert_eq!(
            error_string(store.claim_candidates(&named, None, 10)),
            "sprint sp-nope does not exist on this board"
        );
        store
            .abandon_sprint("sp-next", "superseded", "geoyws")
            .unwrap();
        named.sprint_override = Some("sp-next".into());
        assert_eq!(
            error_string(store.claim_candidates(&named, None, 10)),
            "sprint sp-next is abandoned"
        );

        // A default-scoped claim records no override and lands inside the
        // boundary; a crossing one records the "I know".
        let receipt = store.claim(None, claim_options("agent")).unwrap();
        assert_eq!(receipt.claim.task_id, "t-in");
        let payload = last_event_payload(&store, "task_claimed");
        assert!(
            payload.get("sprintOverride").is_none(),
            "a default-scoped claim must not grow a payload key: {payload}"
        );
        let mut crossing = claim_options("agent");
        crossing.sprint_override = Some("any".into());
        let receipt = store.claim(Some("t-free"), crossing).unwrap();
        assert_eq!(receipt.claim.task_id, "t-free");
        assert_eq!(
            last_event_payload(&store, "task_claimed")["sprintOverride"],
            json!("any")
        );
    }

    #[test]
    fn a_board_without_a_current_sprint_claims_exactly_as_before() {
        let mut store = sprint_store("sprint-no-current");
        insert_task(&store, "t-a");
        insert_task(&store, "t-b");
        // A planned sprint exists — planned is not the boundary, so the pool
        // is untouched until one is current.
        store.create_sprint(new_sprint("sp-idle", "0.1.0")).unwrap();
        let ids: Vec<String> = store
            .claim_candidates(&claim_options("agent"), None, 10)
            .unwrap()
            .into_iter()
            .map(|task| task.id)
            .collect();
        assert_eq!(ids, vec!["t-a", "t-b"]);
        let receipt = store.claim(None, claim_options("agent")).unwrap();
        assert_eq!(receipt.claim.task_id, "t-a");
        assert!(
            last_event_payload(&store, "task_claimed")
                .get("sprintOverride")
                .is_none(),
            "a board with no current sprint must not grow a payload key"
        );
    }

    #[test]
    fn attaching_an_epic_carries_its_subtree_skipping_other_sprints_rows() {
        let mut store = sprint_store("sprint-subtree");
        store.add_task(gate_row("e-root", "epic", None)).unwrap();
        store
            .add_task(gate_row("s-kid", "story", Some("e-root")))
            .unwrap();
        store
            .add_task(gate_row("t-kid", "task", Some("s-kid")))
            .unwrap();
        store
            .add_task(gate_row("t-late", "task", Some("e-root")))
            .unwrap();
        store.create_sprint(new_sprint("sp-one", "0.1.0")).unwrap();
        store.create_sprint(new_sprint("sp-two", "0.2.0")).unwrap();
        // A descendant that already moved itself stays where it put itself.
        store.attach_sprint("t-late", "sp-two", "geoyws").unwrap();

        let receipt = store.attach_sprint("e-root", "sp-one", "geoyws").unwrap();
        assert_eq!(receipt.moved, vec!["e-root", "s-kid", "t-kid"]);
        assert_eq!(receipt.old_sprint_id, None);
        assert_eq!(receipt.new_sprint_id.as_deref(), Some("sp-one"));
        assert_eq!(task_sprint_id(&store, "s-kid").as_deref(), Some("sp-one"));
        assert_eq!(task_sprint_id(&store, "t-kid").as_deref(), Some("sp-one"));
        assert_eq!(task_sprint_id(&store, "t-late").as_deref(), Some("sp-two"));
        let audited: i64 = store.connection.query_row(
            "SELECT COUNT(DISTINCT task_id) FROM events WHERE kind='task_sprint_changed' AND task_id IN ('e-root','s-kid','t-kid')",
            [], |row| row.get(0)
        ).unwrap();
        assert_eq!(audited, 3);

        // Attaching to history is refused; attaching what is already
        // attached is a no-op refusal.
        store
            .plan_sprint(
                "sp-one",
                "Fixture goal\nFixture acceptance criteria",
                &[],
                None,
                false,
                "geoyws",
            )
            .unwrap();
        store.start_sprint("sp-one", "geoyws").unwrap();
        store.create_sprint(new_sprint("sp-old", "0.0.9")).unwrap();
        store
            .abandon_sprint("sp-old", "never happened", "geoyws")
            .unwrap();
        assert_eq!(
            error_string(store.attach_sprint("t-kid", "sp-old", "geoyws")),
            "sprint sp-old is abandoned; attach the rows to a planned or current sprint instead"
        );
        assert_eq!(
            error_string(store.attach_sprint("t-late", "sp-two", "geoyws")),
            "task t-late and its subtree are already attached to sprint sp-two; \
             there is nothing to attach"
        );

        // Detach is one row, never recursive.
        let detach = store.detach_sprint("s-kid", "geoyws").unwrap();
        assert_eq!(detach.moved, vec!["s-kid"]);
        assert_eq!(detach.old_sprint_id.as_deref(), Some("sp-one"));
        assert_eq!(detach.new_sprint_id, None);
        assert_eq!(task_sprint_id(&store, "s-kid"), None);
        assert_eq!(task_sprint_id(&store, "t-kid").as_deref(), Some("sp-one"));
        assert_eq!(
            error_string(store.detach_sprint("s-kid", "geoyws")),
            "task s-kid is not attached to a sprint; there is nothing to clear"
        );
    }

    #[test]
    fn a_context_packet_carries_the_tasks_sprint_and_the_dash_counts_it() {
        let mut store = sprint_store("sprint-projections");
        insert_task(&store, "t-ctx");
        insert_task(&store, "t-ctx-done");
        store
            .connection
            .execute(
                "UPDATE tasks SET status='done',completed_at=1 WHERE id='t-ctx-done'",
                [],
            )
            .unwrap();
        store.create_sprint(new_sprint("sp-ctx", "0.4.0")).unwrap();
        store
            .plan_sprint(
                "sp-ctx",
                "\n   \n  Ship the decisions room  \n- criteria one",
                &[],
                None,
                true,
                "geoyws",
            )
            .unwrap();
        store.attach_sprint("t-ctx", "sp-ctx", "geoyws").unwrap();
        store
            .attach_sprint("t-ctx-done", "sp-ctx", "geoyws")
            .unwrap();
        store.start_sprint("sp-ctx", "geoyws").unwrap();

        // The packet's sprint is the boundary the resuming agent works toward.
        let packet = store.context_packet("t-ctx").unwrap();
        let sprint = packet.sprint.expect("the task is attached");
        assert_eq!(sprint.sprint_id, "sp-ctx");
        assert_eq!(sprint.target_version, "0.4.0");
        assert_eq!(sprint.goal.as_deref(), Some("Ship the decisions room"));
        // An unattached task reads no sprint at all.
        insert_task(&store, "t-loose");
        assert!(store.context_packet("t-loose").unwrap().sprint.is_none());

        // Dashboard counts unfinished and actually done rows independently.
        let (open, done) = store.sprint_task_counts("sp-ctx").unwrap();
        assert_eq!((open, done), (1, 1));
    }

    #[test]
    fn sprint_plan_records_only_real_scope_moves_with_previous_assignment() {
        let mut store = sprint_store("sprint-actual-moves");
        store.add_task(gate_row("e-move", "epic", None)).unwrap();
        store
            .add_task(gate_row("t-stays", "task", Some("e-move")))
            .unwrap();
        store.create_sprint(new_sprint("sp-from", "1.0.0")).unwrap();
        store.create_sprint(new_sprint("sp-to", "1.1.0")).unwrap();
        store.attach_sprint("e-move", "sp-from", "actor").unwrap();
        store
            .plan_sprint(
                "sp-to",
                "Move root\nKeep assigned descendant",
                &["e-move".into()],
                None,
                false,
                "actor",
            )
            .unwrap();
        assert_eq!(task_sprint_id(&store, "e-move").as_deref(), Some("sp-to"));
        assert_eq!(
            task_sprint_id(&store, "t-stays").as_deref(),
            Some("sp-from")
        );
        let payload: String = store.connection.query_row("SELECT payload FROM events WHERE kind='task_sprint_changed' AND task_id='e-move' ORDER BY seq DESC LIMIT 1", [], |row| row.get(0)).unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["oldSprintID"], "sp-from");
        assert_eq!(payload["newSprintID"], "sp-to");
        let before: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='task_sprint_changed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        store
            .plan_sprint(
                "sp-to",
                "Reworded goal\nSame scope",
                &[],
                None,
                false,
                "actor",
            )
            .unwrap();
        let after: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='task_sprint_changed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            after, before,
            "replanning unchanged scope emitted phantom moves"
        );
    }

    #[test]
    fn managed_sprint_projections_hide_restricted_rows_counts_and_event_ids() {
        use crate::policy::{Capability, ScopeTuple, authority};
        use crate::routing::Enforcement;
        let board = "eeeeeeee-5555-4555-8555-555555555555";
        let dir = std::env::temp_dir().join(format!("kanban-sprint-visibility-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{board}.db"));
        let (secret_handoff, proof_id) = {
            let mut seed = Store::open(&path).unwrap();
            seed.initialize("visibility", "seed").unwrap();
            seed.add_tag("visible", None, Some("seed")).unwrap();
            seed.add_tag("secret", None, Some("seed")).unwrap();
            seed.add_task(task_input(
                "t-visible",
                "visible content",
                vec!["visible".into()],
                vec![],
            ))
            .unwrap();
            seed.add_task(task_input(
                "t-secret",
                "secret content",
                vec!["secret".into()],
                vec![],
            ))
            .unwrap();
            seed.add_task(task_input(
                "t-secret-lease",
                "secret lease",
                vec!["secret".into()],
                vec![],
            ))
            .unwrap();
            seed.create_sprint(new_sprint("sp-visible", "1.0.0"))
                .unwrap();
            seed.create_sprint(new_sprint("sp-plan", "1.1.0")).unwrap();
            seed.attach_sprint("t-visible", "sp-visible", "seed")
                .unwrap();
            seed.attach_sprint("t-secret", "sp-visible", "seed")
                .unwrap();
            seed.attach_sprint("t-secret-lease", "sp-visible", "seed")
                .unwrap();
            let claimed = seed
                .claim(Some("t-secret-lease"), claim_options("seed"))
                .unwrap();
            let mut input = handoff_input();
            input.task_id = Some("t-secret-lease".into());
            input.lease_token = Some(claimed.claim.lease_token);
            input.from_agent = "seed".into();
            let handoff_id = seed.create_handoff(input).unwrap().id;
            seed.move_task("t-secret", "done", "seed", json!({}), false)
                .unwrap();
            seed.plan_sprint(
                "sp-visible",
                "Visible release\nAcceptance",
                &[],
                None,
                false,
                "seed",
            )
            .unwrap();
            seed.start_sprint("sp-visible", "seed").unwrap();
            let started = seed
                .start_deployment(StartDeployment {
                    task_id: None,
                    repo: "geoyws/kanban".into(),
                    identity: DeployIdentity::Git("a".repeat(40)),
                    deployer_checkout: None,
                    branch: None,
                    tier: "@_p".into(),
                    environment: "production".into(),
                    host: "hax".into(),
                    url: "https://kb.invalid".into(),
                    mechanism: None,
                    operation_id: None,
                    retry_of: None,
                    actor: "seed".into(),
                    lane: None,
                    sprint_id: Some("sp-visible".into()),
                })
                .unwrap();
            let mut finish = finish_input(&started);
            finish.served_commit = Some("a".repeat(40));
            finish.served_version = Some("1.0.0".into());
            seed.finish_deployment(finish).unwrap();
            (handoff_id, started.deployment.id)
        };
        let grants = authority([
            (
                ScopeTuple::Board {
                    board_id: board.into(),
                },
                Capability::Read,
            ),
            (
                ScopeTuple::BoardTag {
                    board_id: board.into(),
                    tag: "visible".into(),
                },
                Capability::Read,
            ),
            (
                ScopeTuple::Board {
                    board_id: board.into(),
                },
                Capability::Write,
            ),
            (
                ScopeTuple::BoardTag {
                    board_id: board.into(),
                    tag: "visible".into(),
                },
                Capability::Write,
            ),
        ]);
        let mut managed = Store::open_with_authz(
            &path,
            AuthzContext::new(Enforcement::Managed, grants, board.into()),
        )
        .unwrap();
        assert_denied(
            managed.close_sprint("sp-visible", Some(&proof_id), None, None, "actor"),
            "close hidden sprint scope",
        );
        let listing_options = claim_options("managed");
        let candidates = managed
            .claim_candidates(&listing_options, None, 10)
            .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["t-visible"]
        );
        assert_denied(
            managed.claim(Some("t-secret-lease"), claim_options("managed")),
            "direct hidden claim",
        );
        assert_denied(
            managed.accept_handoff(
                &secret_handoff,
                AcceptHandoffOptions {
                    agent: "managed".into(),
                    session: None,
                    lease_ms: 60_000,
                    caller_scope: None,
                    sprint_override: None,
                    model: None,
                    git: None,
                },
            ),
            "hidden handoff accept",
        );
        let tasks = managed.sprint_tasks("sp-visible").unwrap();
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["t-visible"]
        );
        assert!(tasks.iter().all(|task| !task.title.contains("secret")));
        assert_eq!(managed.sprint_task_counts("sp-visible").unwrap(), (1, 0));
        let events = managed
            .events_since_filtered(
                None,
                &["task_sprint_changed".into()],
                &[],
                &[],
                &[],
                &[],
                0,
                100,
                true,
            )
            .unwrap();
        assert!(events.iter().all(|event| {
            !matches!(
                event.task_id.as_deref(),
                Some("t-secret" | "t-secret-lease")
            ) && !event.payload.to_string().contains("t-secret")
        }));
        assert_eq!(
            managed
                .claim(None, claim_options("managed"))
                .unwrap()
                .claim
                .task_id,
            "t-visible"
        );
        let direct = Store::open(&path).unwrap();
        assert_eq!(
            direct
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM task_claims WHERE task_id='t-secret-lease'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            direct
                .connection
                .query_row(
                    "SELECT status FROM handoffs WHERE id=?",
                    [&secret_handoff],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "pending"
        );
        for error in [
            managed
                .plan_sprint(
                    "sp-plan",
                    "replacement\ncriteria",
                    &["t-secret".into()],
                    None,
                    false,
                    "actor",
                )
                .unwrap_err(),
            managed
                .plan_sprint(
                    "sp-plan",
                    "replacement\ncriteria",
                    &[],
                    Some("t-secret"),
                    false,
                    "actor",
                )
                .unwrap_err(),
            managed
                .attach_sprint("t-secret", "sp-missing", "actor")
                .unwrap_err(),
            managed.detach_sprint("t-secret", "actor").unwrap_err(),
        ] {
            assert_eq!(error.to_string(), "denied or not found");
        }
    }
}
