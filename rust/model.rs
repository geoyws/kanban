use anyhow::{Result, bail};
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

/// Where one subscription has actually got to, derived and never stored.
///
/// There is no cursor column and there must not be one: `start_event_seq` is
/// where a subscription began, and progress is the state of its
/// `subscription_deliveries` rows. This is that state summarised for a
/// reader — the highest acked seq plus how much is queued, retrying or
/// dead-lettered.
///
/// `Copy` on purpose: the operator page looks one of these up per rendered
/// row, and a lookup that allocates is a lookup that shows up in a page
/// serving thirteen boards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionPosition {
    /// The highest `acked` delivery seq, or `None` when nothing has been
    /// acked yet — which is not the same fact as "acked through seq 0".
    pub acked_through_seq: Option<i64>,
    pub pending: i64,
    pub leased: i64,
    pub retry_wait: i64,
    pub dead_letter: i64,
}

/// One code a dead-lettered delivery was refused with, and how many of this
/// subscription's dead letters carry it.
///
/// The code is the adapter's own classification, stored on the delivery row
/// as `last_error_code`: `opencode_endpoint_unreachable` tells an operator to
/// check a port, `kimi_frame_oversized` tells them to check a payload, and a
/// count alone tells them to guess. Not `Copy` and deliberately owned — a
/// code is text out of the ledger, and the alternative is a reader holding a
/// borrow of the whole projection while it renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetterCode {
    pub code: String,
    pub deliveries: i64,
}

/// Every subscription's position on one board, against that board's head.
///
/// The head travels with the positions because the two are only meaningful
/// together: "acked through seq 8" says nothing until you know whether the
/// board is at seq 8 or seq 800, and reading them apart invites a page that
/// pairs one board's head with another board's acks.
///
/// The dead-letter codes travel with them for the same reason: `dead_letter`
/// is how many refusals are waiting and the codes are what refused, and a
/// reader that has one without the other either reports a count nobody can
/// act on or an attribution that does not add up.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionPositions {
    pub head_event_seq: i64,
    pub by_subscription: std::collections::BTreeMap<String, SubscriptionPosition>,
    /// Only subscriptions with dead letters appear here, each with its codes
    /// ordered by how many deliveries carry them and then by code, so equal
    /// counts keep one order across reads.
    pub dead_letters: std::collections::BTreeMap<String, Vec<DeadLetterCode>>,
}

impl SubscriptionPositions {
    /// A subscription with no delivery rows has a real position — nothing
    /// acked, nothing queued — rather than a missing one, so this never
    /// returns an absence the caller has to interpret.
    pub fn position(&self, subscription_id: &str) -> SubscriptionPosition {
        self.by_subscription
            .get(subscription_id)
            .copied()
            .unwrap_or_default()
    }

    /// The codes behind one subscription's dead letters, empty when it has
    /// none — same contract as `position`: no absence to interpret.
    pub fn dead_letter_codes(&self, subscription_id: &str) -> &[DeadLetterCode] {
        self.dead_letters
            .get(subscription_id)
            .map_or(&[], Vec::as_slice)
    }
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

/// The machine-readable verdicts a choice can carry (ADR-042 §1).
///
/// Beside [`ATTENTION_KINDS`] and [`ATTENTION_STATUSES`] and for the same
/// reason: a closed set the schema publishes rather than a convention. The
/// label is what geoyws reads; this is what a lane branches on.
pub const ATTENTION_OUTCOMES: [&str; 4] = ["approve", "reject", "defer", "other"];

/// The reserved key of the free-text answer every item offers. Never stored
/// in `choices`, and refused as an authored key (ADR-042 §4 refusal 9).
pub const CUSTOM_CHOICE: &str = "custom";

const QUESTION_MAX: usize = 160;
const CONTEXT_MAX: usize = 800;
const LABEL_MAX: usize = 60;
const CONSEQUENCE_MAX: usize = 200;
const KEY_MAX: usize = 32;

/// "approve, reject, defer, or other" — the four values, for a refusal that
/// names them rather than leaving the caller to guess.
fn outcome_list() -> String {
    let (last, rest) = ATTENTION_OUTCOMES.split_last().expect("four outcomes");
    format!("{}, or {last}", rest.join(", "))
}

/// One authored answer to an attention item's question.
///
/// `key` is what the CLI, the MCP tool, the form POST and the ledger all
/// name, so it is a slug rather than prose; `label` is the button text and
/// `consequence` is what happens if it is picked, including its cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionChoice {
    pub key: String,
    pub label: String,
    pub consequence: String,
    pub outcome: String,
    #[serde(default)]
    pub recommended: bool,
}

/// What resolving recorded: the key that was picked, its verdict, any note,
/// and who settled it when (ADR-042 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionDecision {
    /// An authored key, or the literal [`CUSTOM_CHOICE`].
    pub choice: String,
    pub outcome: String,
    pub note: Option<String>,
    pub by: String,
    pub at: i64,
}

/// The pair a row with no authored choices is served as (ADR-042 §2).
///
/// Nobody authored it, so nothing is recommended — the one place in the model
/// where zero recommendations is legal. Labels are ASCII because they travel
/// through argv, a form POST and a terminal.
pub fn default_choice_pair() -> Vec<AttentionChoice> {
    vec![
        AttentionChoice {
            key: "approve".to_owned(),
            label: "Approve - proceed".to_owned(),
            consequence: "The work the body describes goes ahead as written.".to_owned(),
            outcome: "approve".to_owned(),
            recommended: false,
        },
        AttentionChoice {
            key: "reject".to_owned(),
            label: "Reject - do not proceed".to_owned(),
            consequence:
                "The work the body describes does not happen; whoever raised it needs a new plan."
                    .to_owned(),
            outcome: "reject".to_owned(),
            recommended: false,
        },
    ]
}

/// The authored card an `attention raise` or `attention update` carries: the
/// question, the context needed to answer it, and two to four choices with
/// exactly one recommendation.
///
/// Every ADR-042 §4 refusal that is about the card lives here and nowhere
/// else, so the CLI, the MCP tool and the web edge share one wording. The
/// schema cannot express these — a `CHECK constraint failed` names no fix
/// (ADR-008) — so the columns bound lengths and this bounds the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionCard {
    pub question: Option<String>,
    pub context: Option<String>,
    /// Empty when the raiser authored none; such a row reads as
    /// [`default_choice_pair`].
    pub choices: Vec<AttentionChoice>,
}

impl DecisionCard {
    /// Whether this card asks for nothing to be written.
    pub fn is_empty(&self) -> bool {
        self.question.is_none() && self.context.is_none() && self.choices.is_empty()
    }

    /// Bind the CLI's tokens into a card, then validate it.
    ///
    /// `--choice KEY=LABEL|OUTCOME` splits on its FIRST `=` and its LAST `|`,
    /// so a label may contain `=` and may not contain `|`.
    /// `--consequence KEY=TEXT` is separate because a consequence is a
    /// sentence that will contain `|`, `=`, commas and colons.
    pub fn parse(
        question: Option<&str>,
        context: Option<&str>,
        choices: &[String],
        consequences: &[String],
        recommend: Option<&str>,
    ) -> Result<Self> {
        let mut parsed: Vec<AttentionChoice> = Vec::with_capacity(choices.len());
        for token in choices {
            let Some((key, rest)) = token.split_once('=') else {
                bail!(
                    "attention: --choice {token:?} must read KEY=LABEL|OUTCOME, \
                     such as --choice \"assign=Assign a Claude seat|approve\""
                );
            };
            let Some((label, outcome)) = rest.rsplit_once('|') else {
                bail!(
                    "attention: --choice {token:?} names no outcome; it must read \
                     KEY=LABEL|OUTCOME with the outcome one of {}",
                    outcome_list()
                );
            };
            parsed.push(AttentionChoice {
                key: key.trim().to_owned(),
                label: label.trim().to_owned(),
                consequence: String::new(),
                outcome: outcome.trim().to_owned(),
                recommended: false,
            });
        }
        // Before the consequences bind: a repeated key would take the first
        // choice's consequence and leave the second without one, so the
        // refusal would name a missing consequence rather than the duplicate
        // that caused it.
        unique_keys(&parsed)?;
        let declared = |choices: &[AttentionChoice]| {
            choices
                .iter()
                .map(|choice| choice.key.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        for token in consequences {
            let Some((key, text)) = token.split_once('=') else {
                bail!(
                    "attention: --consequence {token:?} must read KEY=TEXT, naming the \
                     choice it belongs to"
                );
            };
            let key = key.trim();
            if !parsed.iter().any(|choice| choice.key == key) {
                bail!(
                    "attention: no choice named {key}; declared keys are {}",
                    declared(&parsed)
                );
            }
            let choice = parsed
                .iter_mut()
                .find(|choice| choice.key == key)
                .expect("the key was just found");
            if !choice.consequence.is_empty() {
                bail!(
                    "attention: choice {key} is given two --consequence values; \
                     give one per choice"
                );
            }
            choice.consequence = text.trim().to_owned();
            if choice.consequence.is_empty() {
                bail!(
                    "attention: choice {key} has an empty --consequence; every choice must \
                     say what happens if it is picked"
                );
            }
        }
        if let Some(key) = parsed
            .iter()
            .find(|choice| choice.consequence.is_empty())
            .map(|choice| choice.key.clone())
        {
            bail!(
                "attention: choice {key} has no --consequence; every choice must say what \
                 happens if it is picked"
            );
        }
        if let Some(key) = recommend {
            let key = key.trim();
            if parsed.is_empty() {
                bail!("attention: --recommend needs choices; give --choice or drop it");
            }
            if !parsed.iter().any(|choice| choice.key == key) {
                bail!(
                    "attention: no choice named {key}; declared keys are {}",
                    declared(&parsed)
                );
            }
            for choice in parsed.iter_mut().filter(|choice| choice.key == key) {
                choice.recommended = true;
            }
        }
        let card = Self {
            question: question.map(str::trim).map(str::to_owned),
            context: context.map(str::trim).map(str::to_owned),
            choices: parsed,
        };
        card.validate()?;
        Ok(card)
    }

    /// Every cross-field and per-field invariant of a card, in one place.
    pub fn validate(&self) -> Result<()> {
        match (&self.question, &self.context) {
            (Some(_), Some(_)) | (None, None) => {}
            _ => bail!("attention: --question and --context are one card; give both or neither"),
        }
        if let Some(question) = &self.question {
            bounded(question, "--question", QUESTION_MAX)?;
        }
        if let Some(context) = &self.context {
            bounded(context, "--context", CONTEXT_MAX)?;
        }
        if self.choices.is_empty() {
            return Ok(());
        }
        if !(2..=4).contains(&self.choices.len()) {
            bail!(
                "attention: an item carries 2 to 4 choices; {} were given",
                self.choices.len()
            );
        }
        for choice in &self.choices {
            valid_key(&choice.key)?;
            if choice.key == CUSTOM_CHOICE {
                bail!(
                    "attention: {CUSTOM_CHOICE} is reserved for the free-text answer and \
                     cannot be a choice key"
                );
            }
            bounded(
                &choice.label,
                &format!("choice {} label", choice.key),
                LABEL_MAX,
            )?;
            if choice.label.contains('|') {
                bail!(
                    "attention: choice {} label contains '|', which separates the label from \
                     the outcome in --choice KEY=LABEL|OUTCOME; write the label without it",
                    choice.key
                );
            }
            bounded(
                &choice.consequence,
                &format!("choice {} consequence", choice.key),
                CONSEQUENCE_MAX,
            )?;
            if !ATTENTION_OUTCOMES.contains(&choice.outcome.as_str()) {
                bail!(
                    "attention: choice {} has outcome {}; an outcome is one of {}",
                    choice.key,
                    choice.outcome,
                    outcome_list()
                );
            }
        }
        unique_keys(&self.choices)?;
        let recommended = self
            .choices
            .iter()
            .filter(|choice| choice.recommended)
            .count();
        if recommended != 1 {
            bail!("attention: exactly one choice is recommended; {recommended} were");
        }
        Ok(())
    }

    /// The `choices` column's value, or `NULL` when nothing was authored.
    pub fn choices_json(&self) -> Option<String> {
        if self.choices.is_empty() {
            return None;
        }
        Some(serde_json::to_string(&self.choices).expect("choices serialize"))
    }
}

/// Keys are unique within an item: the CLI, the form POST and the ledger all
/// name a choice by its key, and two choices answering to one name make the
/// decision ambiguous.
fn unique_keys(choices: &[AttentionChoice]) -> Result<()> {
    for (index, choice) in choices.iter().enumerate() {
        if choices[..index]
            .iter()
            .any(|earlier| earlier.key == choice.key)
        {
            bail!(
                "attention: choice key {} is given twice; keys must be unique within an item",
                choice.key
            );
        }
    }
    Ok(())
}

/// A field's length bound, named with what it got (ADR-042 §4 refusal 12).
///
/// Characters rather than bytes, because the columns' CHECKs use SQLite
/// `length()`, which counts characters on TEXT.
fn bounded(value: &str, field: &str, max: usize) -> Result<()> {
    if value.is_empty() {
        bail!("attention: {field} is empty; give it a value or drop it");
    }
    let length = value.chars().count();
    if length > max {
        bail!("attention: {field} is {length} characters; the bound is {max}");
    }
    Ok(())
}

/// `[a-z0-9][a-z0-9-]{0,31}`, checked without a regex dependency.
fn valid_key(key: &str) -> Result<()> {
    let shaped = key
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && key.len() <= KEY_MAX
        && key
            .chars()
            .all(|letter| letter.is_ascii_lowercase() || letter.is_ascii_digit() || letter == '-');
    if !shaped {
        bail!(
            "attention: choice key {key:?} is not a slug; a key matches \
             [a-z0-9][a-z0-9-]{{0,31}} so it stays typeable in argv, a form POST and the ledger"
        );
    }
    Ok(())
}

/// What a resolve names: an authored choice by key, or the reserved custom
/// answer with its own outcome and a required note.
#[derive(Debug, Clone, Copy, Default)]
pub struct AttentionAnswer<'a> {
    pub choice: Option<&'a str>,
    pub outcome: Option<&'a str>,
    pub note: Option<&'a str>,
}

impl<'a> AttentionAnswer<'a> {
    /// The custom answer, which every item offers and which always carries a
    /// verdict and a note.
    pub fn custom(outcome: &'a str, note: &'a str) -> Self {
        Self {
            choice: Some(CUSTOM_CHOICE),
            outcome: Some(outcome),
            note: Some(note),
        }
    }

    /// The decision this answer records against a row's choices, and the
    /// resolution text it composes.
    ///
    /// One composer inside the write path, so no caller can produce a
    /// different trail and `resolution` is derived state rather than an
    /// independent input (ADR-042 §3).
    pub fn decide(
        &self,
        id: &str,
        choices: &[AttentionChoice],
        by: &str,
        at: i64,
    ) -> Result<(AttentionDecision, String)> {
        let note = self
            .note
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(str::to_owned);
        let Some(key) = self.choice.map(str::trim).filter(|key| !key.is_empty()) else {
            bail!(
                "attention resolve requires --choice KEY or \
                 --choice {CUSTOM_CHOICE} --outcome X --note TEXT"
            );
        };
        if key == CUSTOM_CHOICE {
            let outcome = self
                .outcome
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let (Some(outcome), Some(note)) = (outcome, note) else {
                bail!(
                    "attention: a custom answer needs --outcome ({}) and --note",
                    ATTENTION_OUTCOMES.join(", ")
                );
            };
            if !ATTENTION_OUTCOMES.contains(&outcome) {
                bail!(
                    "attention: --outcome {outcome} is not a verdict; an outcome is one of {}",
                    outcome_list()
                );
            }
            let resolution =
                format!("Decision: Custom answer, recorded as {outcome}.\nNote: {note}");
            return Ok((
                AttentionDecision {
                    choice: CUSTOM_CHOICE.to_owned(),
                    outcome: outcome.to_owned(),
                    note: Some(note),
                    by: by.to_owned(),
                    at,
                },
                resolution,
            ));
        }
        if self.outcome.is_some_and(|value| !value.trim().is_empty()) {
            bail!(
                "attention: --outcome applies only to --choice {CUSTOM_CHOICE}; \
                 an authored choice carries its own outcome"
            );
        }
        let Some(choice) = choices.iter().find(|choice| choice.key == key) else {
            bail!(
                "attention {id} has no choice {key}; its choices are {}",
                choices
                    .iter()
                    .map(|choice| choice.key.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        let mut resolution = format!("Decision: {}. {}", choice.label, choice.consequence);
        if let Some(note) = &note {
            resolution.push_str("\nNote: ");
            resolution.push_str(note);
        }
        Ok((
            AttentionDecision {
                choice: choice.key.clone(),
                outcome: choice.outcome.clone(),
                note,
                by: by.to_owned(),
                at,
            },
            resolution,
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attention {
    pub id: String,
    #[serde(rename = "taskID")]
    pub task_id: Option<String>,
    pub kind: String,
    pub body: String,
    /// What geoyws is deciding, in his terms; `None` on a row with no
    /// authored card, where the body serves as both question and context.
    pub question: Option<String>,
    pub context: Option<String>,
    /// The answers on offer: what the raiser authored, or
    /// [`default_choice_pair`] materialized on read for a row that authored
    /// none. Never empty, and never carries the reserved `custom` key.
    pub choices: Vec<AttentionChoice>,
    pub raised_by: String,
    pub created_at: i64,
    pub status: String,
    pub priority: i64,
    pub priority_level: Option<String>,
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
    /// What settling it recorded, or `None` while it is open — a reopen
    /// clears it from the row and keeps it in the ledger (ADR-042 §3).
    pub decision: Option<AttentionDecision>,
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
    use super::*;

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

    /// One well-formed pair of tokens, which every refusal case breaks in
    /// exactly one way.
    fn tokens() -> (Vec<String>, Vec<String>) {
        (
            vec![
                "assign=Assign a Claude seat to hax and log in|approve".to_owned(),
                "keep-parked=Keep it parked until a seat frees up|defer".to_owned(),
            ],
            vec![
                "assign=You buy or free one seat and finish the login: ten minutes.".to_owned(),
                "keep-parked=Nothing changes; a task is filed to re-raise it.".to_owned(),
            ],
        )
    }

    fn refusal(
        question: Option<&str>,
        context: Option<&str>,
        choices: &[String],
        consequences: &[String],
        recommend: Option<&str>,
    ) -> String {
        DecisionCard::parse(question, context, choices, consequences, recommend)
            .expect_err("the card must be refused")
            .to_string()
    }

    #[test]
    fn a_well_formed_card_parses_into_its_choices_and_one_recommendation() {
        let (choices, consequences) = tokens();
        let card = DecisionCard::parse(
            Some("  hax has no logged-in Claude account - assign a seat, or drop it?  "),
            Some("  A real turn answers HTTP 401.  "),
            &choices,
            &consequences,
            Some("assign"),
        )
        .expect("a well-formed card");
        assert_eq!(
            card.question.as_deref(),
            Some("hax has no logged-in Claude account - assign a seat, or drop it?"),
            "the question is stored trimmed"
        );
        assert_eq!(
            card.context.as_deref(),
            Some("A real turn answers HTTP 401.")
        );
        assert_eq!(card.choices.len(), 2);
        assert_eq!(card.choices[0].key, "assign");
        assert_eq!(
            card.choices[0].label,
            "Assign a Claude seat to hax and log in"
        );
        assert_eq!(card.choices[0].outcome, "approve");
        assert!(card.choices[0].recommended, "--recommend marks its choice");
        assert_eq!(card.choices[1].outcome, "defer");
        assert!(!card.choices[1].recommended);
        assert!(!card.is_empty());
        // The column's value is the array, in declared order.
        let stored: Vec<AttentionChoice> =
            serde_json::from_str(&card.choices_json().expect("authored choices")).unwrap();
        assert_eq!(stored, card.choices);
    }

    #[test]
    fn an_empty_card_is_empty_and_stores_no_choices() {
        let card = DecisionCard::parse(None, None, &[], &[], None).expect("nothing authored");
        assert!(card.is_empty());
        assert_eq!(card.choices_json(), None);
    }

    /// A label may contain `=` because the key splits on the FIRST one, and
    /// the outcome splits on the LAST `|`.
    #[test]
    fn a_label_may_contain_an_equals_sign() {
        let card = DecisionCard::parse(
            None,
            None,
            &[
                "pin=Set retries=3 and ship it|approve".to_owned(),
                "hold=Hold the release|defer".to_owned(),
            ],
            &[
                "pin=The queue retries three times and the release ships today.".to_owned(),
                "hold=Nothing ships until 2026-09-15, when this is re-raised.".to_owned(),
            ],
            Some("pin"),
        )
        .expect("an `=` in a label is legal");
        assert_eq!(card.choices[0].label, "Set retries=3 and ship it");
    }

    #[test]
    fn a_consequence_naming_no_declared_choice_is_refused_with_the_declared_keys() {
        let (choices, _) = tokens();
        let error = refusal(
            None,
            None,
            &choices,
            &["typo=A consequence for a key nobody declared.".to_owned()],
            None,
        );
        assert_eq!(
            error,
            "attention: no choice named typo; declared keys are assign, keep-parked"
        );
        // The same refusal covers --recommend, which names a key the same way.
        let (_, consequences) = tokens();
        assert_eq!(
            refusal(None, None, &choices, &consequences, Some("typo")),
            "attention: no choice named typo; declared keys are assign, keep-parked"
        );
    }

    #[test]
    fn a_repeated_choice_key_is_refused_rather_than_the_last_one_winning() {
        let error = refusal(
            None,
            None,
            &[
                "assign=Assign a seat|approve".to_owned(),
                "assign=Assign two seats|approve".to_owned(),
            ],
            &[
                "assign=One seat is bought and the login is finished today.".to_owned(),
                "assign=Two seats are bought and both logins are finished.".to_owned(),
            ],
            Some("assign"),
        );
        assert_eq!(
            error,
            "attention: choice key assign is given twice; keys must be unique within an item"
        );
    }

    #[test]
    fn a_choice_count_outside_two_to_four_is_refused_with_the_count() {
        let one = refusal(
            None,
            None,
            &["assign=Assign a seat|approve".to_owned()],
            &["assign=One seat is bought and the login is finished today.".to_owned()],
            Some("assign"),
        );
        assert_eq!(
            one,
            "attention: an item carries 2 to 4 choices; 1 were given"
        );
        let mut choices = Vec::new();
        let mut consequences = Vec::new();
        for index in 0..5 {
            choices.push(format!("k{index}=Take option {index}|other"));
            consequences.push(format!(
                "k{index}=Option {index} happens and costs nothing."
            ));
        }
        assert_eq!(
            refusal(None, None, &choices, &consequences, Some("k0")),
            "attention: an item carries 2 to 4 choices; 5 were given"
        );
    }

    #[test]
    fn a_card_with_no_recommendation_or_two_is_refused_with_the_count() {
        let (choices, consequences) = tokens();
        assert_eq!(
            refusal(None, None, &choices, &consequences, None),
            "attention: exactly one choice is recommended; 0 were"
        );
        // Two is unreachable through a single-valued --recommend and is still
        // the invariant, so the validator is asserted directly.
        let mut card =
            DecisionCard::parse(None, None, &choices, &consequences, Some("assign")).unwrap();
        card.choices[1].recommended = true;
        assert_eq!(
            card.validate()
                .expect_err("two recommendations")
                .to_string(),
            "attention: exactly one choice is recommended; 2 were"
        );
    }

    #[test]
    fn a_choice_with_no_consequence_is_refused_naming_the_choice() {
        let (choices, consequences) = tokens();
        assert_eq!(
            refusal(None, None, &choices, &consequences[..1], Some("assign")),
            "attention: choice keep-parked has no --consequence; every choice must say what \
             happens if it is picked"
        );
        assert_eq!(
            refusal(
                None,
                None,
                &choices,
                &["assign=   ".to_owned()],
                Some("assign")
            ),
            "attention: choice assign has an empty --consequence; every choice must say what \
             happens if it is picked"
        );
        assert_eq!(
            refusal(
                None,
                None,
                &choices,
                &[consequences[0].clone(), consequences[0].clone()],
                Some("assign")
            ),
            "attention: choice assign is given two --consequence values; give one per choice"
        );
    }

    #[test]
    fn a_question_without_a_context_is_refused_as_half_a_card() {
        let paired = "attention: --question and --context are one card; give both or neither";
        assert_eq!(
            refusal(Some("Assign a seat?"), None, &[], &[], None),
            paired
        );
        assert_eq!(
            refusal(None, Some("A turn answers 401."), &[], &[], None),
            paired
        );
    }

    #[test]
    fn the_reserved_custom_key_cannot_be_authored() {
        assert_eq!(
            refusal(
                None,
                None,
                &[
                    "custom=Write your own answer|other".to_owned(),
                    "assign=Assign a seat|approve".to_owned(),
                ],
                &[
                    "custom=Whatever you type is recorded as the verdict.".to_owned(),
                    "assign=One seat is bought and the login is finished today.".to_owned(),
                ],
                Some("assign"),
            ),
            "attention: custom is reserved for the free-text answer and cannot be a choice key"
        );
    }

    #[test]
    fn recommend_without_choices_is_refused() {
        assert_eq!(
            refusal(None, None, &[], &[], Some("assign")),
            "attention: --recommend needs choices; give --choice or drop it"
        );
    }

    #[test]
    fn every_bound_is_refused_naming_the_field_the_bound_and_what_it_got() {
        let (choices, consequences) = tokens();
        let long = "x".repeat(161);
        assert_eq!(
            refusal(Some(&long), Some("A turn answers 401."), &[], &[], None),
            "attention: --question is 161 characters; the bound is 160"
        );
        let long_context = "y".repeat(801);
        assert_eq!(
            refusal(Some("Assign a seat?"), Some(&long_context), &[], &[], None),
            "attention: --context is 801 characters; the bound is 800"
        );
        assert_eq!(
            refusal(
                None,
                None,
                &[
                    format!("assign={}|approve", "L".repeat(61)),
                    "keep-parked=Keep it parked|defer".to_owned(),
                ],
                &consequences,
                Some("assign"),
            ),
            "attention: choice assign label is 61 characters; the bound is 60"
        );
        assert_eq!(
            refusal(
                None,
                None,
                &choices,
                &[
                    format!("assign={}", "C".repeat(201)),
                    consequences[1].clone(),
                ],
                Some("assign"),
            ),
            "attention: choice assign consequence is 201 characters; the bound is 200"
        );
        // An empty question is refused as empty rather than as a length.
        assert_eq!(
            refusal(Some("   "), Some("A turn answers 401."), &[], &[], None),
            "attention: --question is empty; give it a value or drop it"
        );
        // A label carrying `|` after the last-`|` split is refused by name.
        assert_eq!(
            refusal(
                None,
                None,
                &[
                    "assign=Assign a seat|and log in|approve".to_owned(),
                    "keep-parked=Keep it parked|defer".to_owned(),
                ],
                &consequences,
                Some("assign"),
            ),
            "attention: choice assign label contains '|', which separates the label from the \
             outcome in --choice KEY=LABEL|OUTCOME; write the label without it"
        );
        // An empty label is empty, not zero-length-bounded.
        assert_eq!(
            refusal(
                None,
                None,
                &[
                    "assign= |approve".to_owned(),
                    "keep=Keep it|defer".to_owned()
                ],
                &[
                    consequences[0].clone(),
                    "keep=Nothing changes and it is re-raised on 2026-09-15.".to_owned(),
                ],
                Some("assign"),
            ),
            "attention: choice assign label is empty; give it a value or drop it"
        );
    }

    #[test]
    fn a_malformed_choice_token_names_the_shape_it_must_take() {
        assert_eq!(
            refusal(None, None, &["assign".to_owned()], &[], None),
            "attention: --choice \"assign\" must read KEY=LABEL|OUTCOME, such as \
             --choice \"assign=Assign a Claude seat|approve\""
        );
        assert_eq!(
            refusal(None, None, &["assign=Assign a seat".to_owned()], &[], None),
            "attention: --choice \"assign=Assign a seat\" names no outcome; it must read \
             KEY=LABEL|OUTCOME with the outcome one of approve, reject, defer, or other"
        );
        assert_eq!(
            refusal(
                None,
                None,
                &["assign=Assign a seat|approve".to_owned()],
                &["a consequence with no key".to_owned()],
                None
            ),
            "attention: --consequence \"a consequence with no key\" must read KEY=TEXT, \
             naming the choice it belongs to"
        );
    }

    #[test]
    fn an_outcome_outside_the_closed_set_is_refused_with_the_four_values() {
        let (_, consequences) = tokens();
        assert_eq!(
            refusal(
                None,
                None,
                &[
                    "assign=Assign a seat|vibes".to_owned(),
                    "keep-parked=Keep it parked|defer".to_owned(),
                ],
                &consequences,
                Some("assign"),
            ),
            "attention: choice assign has outcome vibes; an outcome is one of approve, \
             reject, defer, or other"
        );
    }

    #[test]
    fn a_key_that_is_not_a_slug_is_refused_with_the_pattern() {
        let (_, consequences) = tokens();
        let error = refusal(
            None,
            None,
            &[
                "Assign Seat=Assign a seat|approve".to_owned(),
                "keep-parked=Keep it parked|defer".to_owned(),
            ],
            &[
                "Assign Seat=One seat is bought and the login is finished today.".to_owned(),
                consequences[1].clone(),
            ],
            Some("Assign Seat"),
        );
        assert!(
            error.starts_with("attention: choice key \"Assign Seat\" is not a slug;"),
            "{error}"
        );
        assert!(error.contains("[a-z0-9][a-z0-9-]{0,31}"), "{error}");
        // A 33-character key exceeds the bound the pattern names.
        let long = "k".repeat(33);
        let over = refusal(
            None,
            None,
            &[
                format!("{long}=Assign a seat|approve"),
                "keep-parked=Keep it parked|defer".to_owned(),
            ],
            &[
                format!("{long}=One seat is bought and the login is finished today."),
                consequences[1].clone(),
            ],
            Some(&long),
        );
        assert!(over.contains("is not a slug"), "{over}");
        // A leading hyphen is refused; a digit start is not.
        assert!(
            refusal(
                None,
                None,
                &[
                    "-assign=Assign a seat|approve".to_owned(),
                    "keep=Keep it parked|defer".to_owned(),
                ],
                &[
                    "-assign=One seat is bought and the login is finished today.".to_owned(),
                    "keep=Nothing changes and it is re-raised on 2026-09-15.".to_owned(),
                ],
                Some("-assign"),
            )
            .contains("is not a slug")
        );
        DecisionCard::parse(
            None,
            None,
            &[
                "2fa=Turn on two-factor|approve".to_owned(),
                "keep=Keep it parked|defer".to_owned(),
            ],
            &[
                "2fa=Every login needs a second factor from today.".to_owned(),
                "keep=Nothing changes and it is re-raised on 2026-09-15.".to_owned(),
            ],
            Some("2fa"),
        )
        .expect("a key may start with a digit");
    }

    #[test]
    fn an_authored_choice_composes_its_label_and_consequence_and_carries_its_outcome() {
        let (choices, consequences) = tokens();
        let card =
            DecisionCard::parse(None, None, &choices, &consequences, Some("assign")).unwrap();
        let (decision, resolution) = AttentionAnswer {
            choice: Some("keep-parked"),
            ..Default::default()
        }
        .decide("a-1", &card.choices, "geoyws", 1788805112431)
        .expect("an authored key resolves");
        assert_eq!(decision.choice, "keep-parked");
        assert_eq!(decision.outcome, "defer");
        assert_eq!(decision.note, None);
        assert_eq!(decision.by, "geoyws");
        assert_eq!(decision.at, 1788805112431);
        assert_eq!(
            resolution,
            "Decision: Keep it parked until a seat frees up. \
             Nothing changes; a task is filed to re-raise it."
        );
        // A note is appended on its own line, and only when one was given.
        let noted = AttentionAnswer {
            choice: Some("assign"),
            outcome: None,
            note: Some("  after the pin lands  "),
        }
        .decide("a-1", &card.choices, "geoyws", 7)
        .expect("a note is optional on an authored choice");
        assert_eq!(noted.0.note.as_deref(), Some("after the pin lands"));
        assert!(
            noted.1.ends_with("\nNote: after the pin lands"),
            "{}",
            noted.1
        );
    }

    #[test]
    fn the_custom_answer_needs_an_outcome_and_a_note_and_records_both() {
        let needs_both = "attention: a custom answer needs --outcome \
                          (approve, reject, defer, other) and --note";
        assert_eq!(
            AttentionAnswer {
                choice: Some("custom"),
                outcome: Some("defer"),
                note: None,
            }
            .decide("a-1", &default_choice_pair(), "geoyws", 1)
            .expect_err("a custom answer with no note")
            .to_string(),
            needs_both
        );
        assert_eq!(
            AttentionAnswer {
                choice: Some("custom"),
                outcome: None,
                note: Some("do it after the pin lands"),
            }
            .decide("a-1", &default_choice_pair(), "geoyws", 1)
            .expect_err("a custom answer with no outcome")
            .to_string(),
            needs_both
        );
        assert_eq!(
            AttentionAnswer::custom("vibes", "do it")
                .decide("a-1", &default_choice_pair(), "geoyws", 1)
                .expect_err("an invented outcome")
                .to_string(),
            "attention: --outcome vibes is not a verdict; an outcome is one of approve, \
             reject, defer, or other"
        );
        let (decision, resolution) = AttentionAnswer::custom("defer", "  after the pin lands  ")
            .decide("a-1", &default_choice_pair(), "geoyws", 42)
            .expect("a custom answer with both");
        assert_eq!(decision.choice, "custom");
        assert_eq!(decision.outcome, "defer");
        assert_eq!(decision.note.as_deref(), Some("after the pin lands"));
        assert_eq!(
            resolution,
            "Decision: Custom answer, recorded as defer.\nNote: after the pin lands"
        );
    }

    #[test]
    fn an_answer_with_no_choice_or_a_stale_one_or_a_stray_outcome_is_refused() {
        assert_eq!(
            AttentionAnswer::default()
                .decide("a-1", &default_choice_pair(), "geoyws", 1)
                .expect_err("no choice at all")
                .to_string(),
            "attention resolve requires --choice KEY or \
             --choice custom --outcome X --note TEXT"
        );
        assert_eq!(
            AttentionAnswer {
                choice: Some("   "),
                ..Default::default()
            }
            .decide("a-1", &default_choice_pair(), "geoyws", 1)
            .expect_err("an empty choice is no choice")
            .to_string(),
            "attention resolve requires --choice KEY or \
             --choice custom --outcome X --note TEXT"
        );
        assert_eq!(
            AttentionAnswer {
                choice: Some("keep-parked"),
                ..Default::default()
            }
            .decide("a-347ff24c", &default_choice_pair(), "geoyws", 1)
            .expect_err("a key the row does not carry")
            .to_string(),
            "attention a-347ff24c has no choice keep-parked; its choices are approve, reject"
        );
        assert_eq!(
            AttentionAnswer {
                choice: Some("approve"),
                outcome: Some("reject"),
                note: None,
            }
            .decide("a-1", &default_choice_pair(), "geoyws", 1)
            .expect_err("an outcome on an authored choice")
            .to_string(),
            "attention: --outcome applies only to --choice custom; an authored choice \
             carries its own outcome"
        );
    }

    #[test]
    fn the_default_pair_is_two_choices_with_no_recommendation() {
        let pair = default_choice_pair();
        assert_eq!(
            pair.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
            ["approve", "reject"]
        );
        assert_eq!(pair.iter().filter(|c| c.recommended).count(), 0);
        // ASCII only: these labels travel through argv, a form POST and a
        // terminal.
        for choice in &pair {
            assert!(choice.label.is_ascii(), "{}", choice.label);
            assert!(choice.consequence.is_ascii(), "{}", choice.consequence);
        }
        // Synthesized rather than chosen, so the one card the model refuses
        // to author is the one it serves by default.
        let synthesized = DecisionCard {
            question: None,
            context: None,
            choices: pair,
        };
        assert_eq!(
            synthesized
                .validate()
                .expect_err("nothing authored it, so nothing may be marked")
                .to_string(),
            "attention: exactly one choice is recommended; 0 were"
        );
    }
}
