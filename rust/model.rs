use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The immutable per-board UUID ADR-032 mints as the board file's name, read
/// back from a published board path: `<root>/boards/<uuid>.db` -> `<uuid>`.
///
/// This is the one place the path-to-identity mapping lives. Scope atoms key
/// on this value, never on the board's display name, and the store's
/// authorization context carries it so no surface re-parses a path. A path
/// with no file stem (a scratch `--db` path, which only the direct estate
/// opens) yields `None`, and callers fall back to the empty string — the
/// value is never consulted because the guard no-ops outside
/// [`crate::routing::Enforcement::Managed`].
pub fn board_id_from_path(board_path: &str) -> Option<String> {
    std::path::Path::new(board_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_owned)
}

/// Every status a task row may hold.
///
/// `draft` leads because it precedes the rest: a row still being written, whose
/// title, body or scope may yet be wrong. `backlog` already meant real work
/// that is simply unscheduled, and there was nothing for the state before that
/// — so an unfinished row read as a specification, and agents decomposed,
/// depended on and worked it as though it were settled.
pub const TASK_STATUSES: [&str; 8] = [
    "draft",
    "backlog",
    "todo",
    "in_progress",
    "blocked",
    "review",
    "done",
    "cancelled",
];
/// A registered tag: an entry in the board's master file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub name: String,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: i64,
    /// How many rows currently carry it, so a listing answers "is this used".
    pub uses: i64,
}

/// One operator rule in the registry-owned, tag-scoped document.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub body: String,
    pub author: String,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Selector tags (`ALL`, `ONLY:<board>`, `EXCEPT:<board>`) and lowercase
    /// subsystem tags share one ordered, fail-closed vocabulary.
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_board: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_registry_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_boards: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_content_sha256: Option<String>,
}

pub const SUBSCRIPTION_STATUSES: [&str; 2] = ["active", "paused"];
pub const SUBSCRIPTION_PROTOCOL_VERSION: i64 = 1;

/// Event kinds emitted by the compiled board ledger.
///
/// Watch and durable subscriptions share this source of truth so a consumer
/// can select a built-in kind before that kind has occurred on a new board.
pub const BOARD_EVENT_KINDS: &[&str] = &[
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
    "subscription_added",
    "subscription_paused",
    "subscription_resumed",
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

/// One durable, declarative consumer selection for exactly one board.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub protocol_version: i64,
    #[serde(rename = "subjectTaskID")]
    pub subject_task_id: Option<String>,
    pub relations: Vec<String>,
    pub kinds: Vec<String>,
    pub prior_statuses: Vec<String>,
    pub current_statuses: Vec<String>,
    pub tags: Vec<String>,
    #[serde(rename = "consumerID")]
    pub consumer_id: String,
    #[serde(rename = "actionID")]
    pub action_id: String,
    pub timeout_ms: i64,
    pub max_retries: i64,
    pub rate_per_minute: i64,
    pub max_concurrency: i64,
    pub start_event_seq: i64,
    /// Opaque host-local lookup name, never a credential value.
    pub secret_ref: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub created_by: String,
    pub updated_at: i64,
    pub updated_by: String,
    pub paused_at: Option<i64>,
    pub paused_by: Option<String>,
}

/// A due delivery candidate selected from the durable dispatcher queue.
///
/// The nested subscription carries the immutable consumer/action capability
/// lookup plus the selection policy. The dispatcher resolves the raw event
/// row itself when it needs to claim the delivery.
#[derive(Clone)]
pub(crate) struct SubscriptionDeliveryCandidate {
    pub(crate) subscription: Subscription,
    pub(crate) event_id: String,
    pub(crate) event_seq: i64,
    pub(crate) event_kind: String,
    pub(crate) delivery_status: String,
    pub(crate) attempt_number: i64,
    pub(crate) next_attempt_at: i64,
}

/// A claimed delivery with a live lease token and deadline.
///
/// The raw event stays crate-private so the dispatcher can project it
/// through the canonical watch redaction path before anything outside the
/// crate sees it.
#[derive(Clone)]
pub(crate) struct SubscriptionDeliveryClaim {
    pub(crate) subscription: Subscription,
    pub(crate) event_id: String,
    pub(crate) event_seq: i64,
    pub(crate) event_kind: String,
    pub(crate) event_created_at: i64,
    pub(crate) event: Event,
    pub(crate) delivery_status: String,
    pub(crate) attempt_number: i64,
    pub(crate) lease_token: String,
    pub(crate) lease_deadline_at: i64,
}

#[derive(Debug, Clone)]
pub struct AddSubscription {
    pub id: Option<String>,
    pub subject_task_id: Option<String>,
    pub relations: Vec<String>,
    pub kinds: Vec<String>,
    pub prior_statuses: Vec<String>,
    pub current_statuses: Vec<String>,
    pub tags: Vec<String>,
    pub consumer_id: String,
    pub action_id: String,
    pub timeout_ms: i64,
    pub max_retries: i64,
    pub rate_per_minute: i64,
    pub max_concurrency: i64,
    pub secret_ref: Option<String>,
    pub actor: String,
}

/// The always-carried table-of-contents entry for a rule.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSummary {
    pub id: String,
    pub headline: String,
    pub has_more: bool,
    pub bytes: usize,
    pub tags: Vec<String>,
}

/// Receipt for the one-time, idempotent consolidation of board-local rules
/// into ADR-027's registry-owned rules document.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleMigrationReport {
    pub legacy_registry_migrated: bool,
    pub legacy_registry_already_migrated: bool,
    pub legacy_rules_imported: usize,
    pub legacy_rules_updated: usize,
    pub legacy_events_imported: usize,
    pub legacy_rules_retired: usize,
    pub boards_migrated: usize,
    pub boards_already_migrated: usize,
    pub rules_imported: usize,
    pub rules_already_imported: usize,
    pub source_rules_retired: usize,
}

/// One rule entry in a source-to-destination transfer bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTransferItem {
    pub source_board: Option<String>,
    pub source_registry_uuid: String,
    pub source_rule_id: String,
    pub source_boards: Vec<String>,
    pub source_content_sha256: String,
    pub body: String,
    pub author: String,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub tags: Vec<String>,
}

/// A deterministic, auditable export bundle for allowlisted rule transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTransferBundle {
    pub format_version: u32,
    pub exported_by: String,
    pub exported_at: i64,
    pub source_registry_uuid: String,
    pub source_registry_audit: crate::audit::AuditReport,
    pub source_boards: Vec<String>,
    pub rules: Vec<RuleTransferItem>,
}

/// Receipt for a registry-to-registry rule import.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTransferReport {
    pub imported_rules: usize,
    pub already_imported_rules: usize,
    pub destination_boards_verified: usize,
    pub source_registry_uuid: String,
    pub source_registry_audit_head: String,
}

/// A registered root that no longer names the directory it was registered for.
///
/// Registration canonicalises, so a stored root is correct the moment it is
/// written and can only become wrong afterwards — the directory is deleted, or
/// moved and replaced by a symlink to its new home. Resolution canonicalises
/// the caller's cwd, so once the two spellings differ **no cwd inside that tree
/// resolves to the board at all**: the project is reachable only by name, and
/// nothing said so.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreachableRoot {
    pub name: String,
    pub root_path: String,
    pub board_path: String,
    /// Where the stored path leads today, or `None` when nothing is there.
    pub resolves_to: Option<String>,
}

/// Where a lane stands, written by whoever is working it.
///
/// The low-ceremony sibling of a [`Handoff`]. A handoff is deliberate — it says
/// *I am leaving, here is everything you need*, releases a lease, and names a
/// successor. A sitrep says only *here is where this stands right now*,
/// costs one command, needs no lease and no task, and can be written twenty
/// times a day. The handoff stays the thing you write when you go; this is the
/// thing that means the handoff, or a successor without one, has something to
/// stand on.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sitrep {
    pub id: String,
    /// The lane this describes — `driver-2`, `solo`. Lane-keyed, not
    /// task-keyed, because the work an agent does between and across tasks is
    /// exactly what had nowhere to go.
    pub lane: String,
    #[serde(rename = "taskID")]
    pub task_id: Option<String>,
    pub author: String,
    pub body: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub root_head: Option<String>,
    pub dirty_summary: Option<String>,
    /// Superseded by newer updates in the same lane. Hidden from the default
    /// read; never deleted by archiving.
    pub archived: bool,
    pub created_at: i64,
}

pub const TASK_TYPES: [&str; 3] = ["epic", "story", "task"];
pub const NOTE_KINDS: [&str; 6] = [
    "plan", "progress", "blocker", "decision", "evidence", "done",
];
pub const HANDOFF_REASONS: [&str; 4] =
    ["token_pressure", "provider_limit", "session_end", "manual"];

/// The states a checkpoint may record, and nothing else. `continue` keeps the
/// lease and the row `in_progress`; `blocked` and `done` are terminal.
pub const CHECKPOINT_STATES: [&str; 3] = ["continue", "blocked", "done"];

/// The statuses a pending handoff may hold. `retired` is history — resolved,
/// never deleted — so a retired handoff is closed rather than advanced.
pub const HANDOFF_STATUSES: [&str; 4] = ["pending", "accepted", "cancelled", "retired"];

/// The relation kinds a watch or subscription filter may select on. One
/// vocabulary for both surfaces, so a filter cannot say a relation here that
/// the other side refuses.
pub const RELATION_KINDS: [&str; 3] = ["parent", "ancestor", "depends-on"];

/// The story gate, in order. A story moves one step at a time along this list.
pub const STORY_FLOW: [&str; 7] = [
    "planning",
    "ready",
    "in-progress",
    "testing",
    "review",
    "merging",
    "done",
];

/// Operator-facing projection of the durable 0-9 queue key.
pub fn priority_level(priority: i64) -> Option<&'static str> {
    match priority {
        0..=2 => Some("P0"),
        3..=5 => Some("P1"),
        6..=9 => Some("P2"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    #[serde(rename = "type")]
    pub task_type: String,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub assignee: Option<String>,
    pub lane: Option<String>,
    pub deliverable: Option<String>,
    pub stale_minutes: Option<i64>,
    pub driver_only: bool,
    pub status: String,
    pub priority: i64,
    #[serde(default)]
    pub priority_level: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    /// Settled cold history. Hidden from default lists but retained in SQLite.
    pub archived: bool,
    pub archived_at: Option<i64>,
    pub metadata: Value,
    /// Registered tags carried by this row, sorted. What the row is *about*,
    /// as opposed to `lane`, which is what kind of work it is.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    #[serde(rename = "taskID")]
    pub task_id: String,
    #[serde(rename = "agentID")]
    pub agent_id: String,
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    pub lease_token: String,
    pub claimed_at: i64,
    pub heartbeat_at: i64,
    pub expires_at: i64,
    /// Where the claim was taken, when the claimer was standing in a
    /// repository. A lane is a `linked` worktree; an ordinary checkout is
    /// `main`.
    pub worktree: Option<String>,
    pub worktree_kind: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    /// The outermost superproject's commit, for a nested checkout.
    pub root_head: Option<String>,
}

/// A newly granted lease plus the active rules that frame its work.
///
/// Flattening preserves the existing top-level claim wire shape. This is not a
/// field on [`Claim`]: `get_claim` must not serialize an empty rules array that
/// falsely reads as proof that the project has no rules.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimReceipt {
    #[serde(flatten)]
    pub claim: Claim,
    pub rules: Vec<RuleSummary>,
    /// Who last died holding this task, when, and how stale their last
    /// checkpoint was — derived from the newest `claim_expired` event since
    /// the task last entered `todo`. Absent when the task was never orphaned,
    /// or when it was reclaimed and completed since, so a later holder is not
    /// told a stale predecessor explains why the task sits in `todo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_from: Option<OrphanedFrom>,
}

/// The previous holder whose lease expired, surfaced to a successor so it can
/// see who died, when, and how stale their last checkpoint is.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanedFrom {
    pub agent: String,
    pub session_id: Option<String>,
    pub expired_at: i64,
    pub last_checkpoint_at: Option<i64>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimSummary {
    #[serde(rename = "taskID")]
    pub task_id: String,
    #[serde(rename = "agentID")]
    pub agent_id: String,
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    pub claimed_at: i64,
    pub heartbeat_at: i64,
    pub expires_at: i64,
}

impl From<&Claim> for ClaimSummary {
    fn from(value: &Claim) -> Self {
        Self {
            task_id: value.task_id.clone(),
            agent_id: value.agent_id.clone(),
            session_id: value.session_id.clone(),
            claimed_at: value.claimed_at,
            heartbeat_at: value.heartbeat_at,
            expires_at: value.expires_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskNote {
    pub seq: i64,
    #[serde(rename = "taskID")]
    pub task_id: String,
    pub author: String,
    pub kind: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub seq: i64,
    #[serde(rename = "taskID")]
    pub task_id: String,
    pub author: String,
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub state: String,
    pub summary: String,
    pub intent: String,
    pub next_action: String,
    pub blockers: Vec<String>,
    pub validations: Vec<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty_summary: Option<String>,
    pub created_at: i64,
    /// The outermost superproject's commit, for a nested checkout.
    pub root_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub id: String,
    /// Absent when the handoff is about the session rather than one task.
    #[serde(rename = "taskID")]
    pub task_id: Option<String>,
    /// The checkpoint that closed the task, and so absent for the same reason.
    pub checkpoint_seq: Option<i64>,
    pub reason: String,
    pub status: String,
    pub priority: i64,
    #[serde(default)]
    pub priority_level: Option<String>,
    pub from_agent: String,
    pub from_session: Option<String>,
    pub from_model: Option<String>,
    pub to_agent: Option<String>,
    pub summary: String,
    pub intent: String,
    pub next_action: String,
    pub blockers: Vec<String>,
    pub validations: Vec<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty_summary: Option<String>,
    pub created_at: i64,
    pub accepted_at: Option<i64>,
    pub accepted_by: Option<String>,
    pub accepted_session: Option<String>,
    /// Set when a pending handoff was retired instead of accepted: a handoff
    /// is history, resolved never deleted, so the row keeps who closed it and
    /// why rather than vanishing.
    pub retired_at: Option<i64>,
    pub retired_by: Option<String>,
    pub retire_note: Option<String>,
    pub archived: bool,
    /// The outermost superproject's commit, for a nested checkout.
    pub root_head: Option<String>,
}

/// One row of the durable audit trail. `lease_seized` and `task_removed`
/// carry who overrode what, so this is the record an operator reviews after a
/// forced override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: i64,
    #[serde(rename = "taskID")]
    pub task_id: Option<String>,
    pub kind: String,
    pub actor: Option<String>,
    pub payload: Value,
    pub created_at: i64,
    pub archived: bool,
    pub prev_hash: Option<String>,
    pub event_hash: Option<String>,
}

/// A task that has been in progress longer than its own `stale_minutes`
/// budget allows. The column was accepted, stored and imported from atmux, but
/// nothing read it, so a task could be configured stale-aware and never
/// reported.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleTask {
    #[serde(flatten)]
    pub task: Task,
    /// Minutes since the last heartbeat, or since the last update when the
    /// task carries no claim.
    pub idle_minutes: i64,
    pub overdue_minutes: i64,
    pub last_signal: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub root_path: String,
    pub name: String,
    /// The immutable per-board UUID (ADR-032), derived from the board file's
    /// stem. `workspace list --json` carries it as `boardID`.
    #[serde(rename = "boardID")]
    pub board_id: String,
    pub board_path: String,
    pub created_at: i64,
    pub last_used_at: i64,
    pub archived: bool,
    pub archived_at: Option<i64>,
    pub archived_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_note: Option<String>,
    pub rootless: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub name: String,
    pub board_path: String,
    pub workspace_roots: Vec<String>,
    pub last_used_at: i64,
    pub archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_note: Option<String>,
}

/// Receipt for adopting an existing board file into the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceAdoptReceipt {
    #[serde(flatten)]
    pub project: ProjectRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,
    pub source_board_path: String,
    /// SHA-256 of the exact migrated snapshot inode published to the registry.
    pub source_sha256: String,
    /// Byte count of the exact migrated snapshot inode published to the registry.
    pub source_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPacket {
    pub task: Task,
    pub ancestors: Vec<Task>,
    pub dependencies: Vec<Task>,
    pub claim: Option<ClaimSummary>,
    /// The previous holder whose lease expired, for a successor reading the
    /// packet cold. See [`ClaimReceipt::orphaned_from`]; absent when nothing
    /// is reportable so existing consumers see no change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_from: Option<OrphanedFrom>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_attention: Vec<Attention>,
    pub notes: Vec<TaskNote>,
    pub checkpoints: Vec<Checkpoint>,
    pub handoffs: Vec<Handoff>,
    /// Applicable rules, as an untruncated table of contents.
    pub rules: Vec<RuleSummary>,
    /// Sitreps mentioning this task, newest first.
    ///
    /// A resuming agent reads the packet and nothing else, so an update that
    /// only `sitrep list` could see would be an update the reader it was
    /// written for never gets.
    pub sitreps: Vec<Sitrep>,
    pub generated_at: i64,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct AddTask {
    pub id: Option<String>,
    pub task_type: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub assignee: Option<String>,
    pub lane: Option<String>,
    pub deliverable: Option<String>,
    pub stale_minutes: Option<i64>,
    pub driver_only: bool,
    pub status: String,
    pub priority: i64,
    pub dependencies: Vec<String>,
    pub metadata: Value,
    /// Who created the row. Compatibility callers may omit it; the CLI then
    /// supplies the explicit `system@cli` actor before the store writes.
    pub actor: Option<String>,
    /// Registered tags to apply. Unregistered ones are refused.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CheckpointInput {
    pub task_id: String,
    pub lease_token: String,
    pub author: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub state: String,
    pub summary: String,
    pub intent: String,
    pub next_action: String,
    pub blockers: Vec<String>,
    pub validations: Vec<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty_summary: Option<String>,
    pub root_head: Option<String>,
}

/// The one actor who may settle or reopen anyone's attention item and whom
/// the browser acts as when no actor header is configured. Rows resolved
/// before 2026-09-05 carry the historical spelling `geo`; that is a record,
/// not an alias, and `geo` is refused like any other non-raiser today.
pub const OPERATOR_ACTOR: &str = "geoyws";

/// The kinds of thing that can need the operator, and nothing else.
///
/// Deliberately no `info`: a note that does not need anyone is a note, and
/// `task note` already holds those. Everything here is something only the
/// operator can retire.
pub const ATTENTION_KINDS: [&str; 5] = ["blocking", "decision", "approval", "review", "risk"];

/// The statuses an attention row may hold. `resolved` is history — reopened
/// rather than deleted — so a resolved row is closed until reopened.
pub const ATTENTION_STATUSES: [&str; 2] = ["open", "resolved"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attention {
    pub id: String,
    #[serde(rename = "taskID")]
    pub task_id: Option<String>,
    pub kind: String,
    pub body: String,
    pub raised_by: String,
    pub created_at: i64,
    pub status: String,
    pub priority: i64,
    pub priority_level: Option<String>,
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
    pub reopened_at: Option<i64>,
    pub reopened_by: Option<String>,
    pub reopen_note: Option<String>,
    pub archived: bool,
    /// Registered subsystem tags carried directly by this attention row.
    pub tags: Vec<String>,
}

/// Receipt from one retention sweep.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveReport {
    pub cutoff_at: i64,
    pub dry_run: bool,
    pub tasks: i64,
    pub notes: i64,
    pub checkpoints: i64,
    pub events: i64,
    pub handoffs: i64,
    pub attention: i64,
    pub sitreps: i64,
    pub task_tags: i64,
    pub deployments: i64,
}

pub const DEPLOYMENT_TIERS: [&str; 7] = ["@_bdt", "@_bd", "@_bst", "@_bs", "@_s", "@_uat", "@_p"];
pub const DEPLOYMENT_STATUSES: [&str; 5] =
    ["started", "succeeded", "failed", "cancelled", "abandoned"];
/// The results a finished deployment may record. `started` is the initial
/// state and can never be a result, so this is [`DEPLOYMENT_STATUSES`] without
/// its first entry.
pub const DEPLOYMENT_RESULTS: [&str; 4] = ["succeeded", "failed", "cancelled", "abandoned"];
pub const DEPLOYMENT_PHASES: [&str; 4] = ["build", "publish", "start", "verification"];

/// The canonical seven-tier deployment table, quoted from the estate CLAUDE.md
/// ("Deployment tiers" section): `@_bdt` and `@_bd` are MBP tiers, hosted on
/// `geoywsMBP` (or the thin client `geoywsMBA`); `@_bst`, `@_bs`, `@_s`,
/// `@_uat` and `@_p` are Hetzner tiers. Every other canonical tier is Hetzner
/// by exclusion, so this list and [`MBP_HOSTS`] are the whole table — no other
/// pairing is hard-coded anywhere.
pub const MBP_TIERS: [&str; 2] = ["@_bdt", "@_bd"];

/// The only hostnames that are MBP. Everything else is treated as a Hetzner
/// host, which is the load-bearing half: an MBP tier stamped with a Hetzner
/// host (`@_bdt` on `hig`, measured in the field) is refused, while a Hetzner
/// tier on `geoywsMBP` is refused as the mirror image.
pub const MBP_HOSTS: [&str; 2] = ["geoywsMBP", "geoywsMBA"];

/// One immutable deployment attempt. Terminal completion only fills the
/// result columns; a retry is always a new row linked through `retry_of`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentAttempt {
    pub id: String,
    #[serde(rename = "taskID")]
    pub task_id: Option<String>,
    pub repo: String,
    pub commit_sha: String,
    pub branch: Option<String>,
    pub tier: String,
    pub environment: String,
    pub host: String,
    pub url: String,
    pub mechanism: Option<String>,
    pub operation_id: Option<String>,
    pub retry_of: Option<String>,
    pub status: String,
    pub phase: Option<String>,
    pub actor: String,
    pub lane: Option<String>,
    pub receipt: Option<String>,
    pub artifact_uri: Option<String>,
    pub served_commit: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub archived: bool,
    pub archived_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStartReceipt {
    #[serde(flatten)]
    pub deployment: DeploymentAttempt,
    pub capability_token: String,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone)]
pub struct StartDeployment {
    pub task_id: Option<String>,
    pub repo: String,
    pub commit_sha: String,
    pub branch: Option<String>,
    pub tier: String,
    pub environment: String,
    pub host: String,
    pub url: String,
    pub mechanism: Option<String>,
    pub operation_id: Option<String>,
    pub retry_of: Option<String>,
    pub actor: String,
    pub lane: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FinishDeployment {
    pub id: String,
    pub capability_token: String,
    pub result: String,
    pub phase: Option<String>,
    pub receipt: Option<String>,
    pub artifact_uri: Option<String>,
    pub served_commit: Option<String>,
    pub actor: String,
}

/// One bounded retrieval request over Kanban's derived search corpus.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub source: Option<String>,
    pub status: Option<String>,
    pub tags: Vec<String>,
    pub lane: Option<String>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub include_archived: bool,
    pub limit: usize,
    pub max_chars: usize,
}

/// A source-backed retrieval result. The citation is stable and sufficient to
/// retrieve the authoritative row; the snippet is deliberately bounded.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub board: String,
    pub source_kind: String,
    pub source_id: String,
    #[serde(rename = "taskID", skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub title: String,
    pub snippet: String,
    pub status: Option<String>,
    pub lane: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived: bool,
    pub exact_score: f64,
    pub lexical_score: f64,
    pub semantic_score: f64,
    pub score: f64,
    pub citation: String,
}

/// A registered board a survey could not open, and the reason it could not.
///
/// Reported separately from the `missingBoards` list beside it because the two
/// call for opposite responses. A missing board is recovered by restoring a
/// snapshot over its path; doing that to an unreadable one overwrites intact
/// data with older data. Boards are created `0600`, so one written by another
/// user is unreadable and perfectly healthy at the same time, and a survey that
/// prints `missing` at an operator has pointed them at the destructive move.
///
/// Carries the path as well as the name, because the fix is on the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadableBoard {
    pub name: String,
    pub board_path: String,
    /// What stopped the read, verbatim — `Permission denied (os error 13)` and
    /// a locked-database failure are different problems with different fixes.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchReceipt {
    pub query: String,
    pub embedding_model: String,
    pub boards: Vec<String>,
    pub missing_boards: Vec<String>,
    /// Boards that exist and would not open. Never folded into
    /// `missing_boards`: see [`UnreadableBoard`].
    pub unreadable_boards: Vec<UnreadableBoard>,
    pub results: Vec<SearchResult>,
    pub result_chars: usize,
    pub truncated: bool,
    pub generated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchIndexReport {
    pub board: String,
    pub documents: i64,
    pub embedded: i64,
    pub embedding_model: String,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchIndexHealth {
    pub healthy: bool,
    pub source_rows: i64,
    pub documents: i64,
    pub fts_rows: i64,
    pub missing_embeddings: i64,
    pub stale_embeddings: i64,
    pub embedding_model: String,
    /// Why `healthy` is false, each naming the measured gap and its fix
    /// (`kb search-rebuild`). Empty when the index is healthy.
    pub unhealthy_because: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HandoffInput {
    /// The task being handed over, or `None` for a session handoff that is
    /// about the work as a whole rather than one row of it.
    pub task_id: Option<String>,
    /// The lease authorizing the handover. Travels with `task_id`: a lease
    /// exists only over a task, and a task cannot be handed over without one.
    pub lease_token: Option<String>,
    pub from_agent: String,
    pub from_session: Option<String>,
    pub from_model: Option<String>,
    pub to_agent: Option<String>,
    pub reason: String,
    pub priority: i64,
    pub summary: String,
    pub intent: String,
    pub next_action: String,
    pub blockers: Vec<String>,
    pub validations: Vec<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty_summary: Option<String>,
    pub root_head: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::board_id_from_path;

    #[test]
    fn board_id_from_path_reads_the_uuid_stem_and_ignores_extension() {
        assert_eq!(
            board_id_from_path("/root/boards/b1e2c2d9-b9e8-4c67-923d-153f7faed19a.db"),
            Some("b1e2c2d9-b9e8-4c67-923d-153f7faed19a".to_owned())
        );
        // A path with no file component (the root, or the empty path) yields
        // nothing, which the direct estate falls back to the empty string and
        // never consults.
        assert_eq!(board_id_from_path("/"), None);
        assert_eq!(board_id_from_path(""), None);
    }
}
