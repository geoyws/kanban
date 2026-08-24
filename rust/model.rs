use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// One project-level operator rule, ordered as part of a document.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub body: String,
    pub author: String,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Present only for registry-wide rules. `ALL` and `EXCEPT:<name>` form an
    /// all-minus set; `ONLY:<name>` rows form an explicit include set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board_tags: Option<Vec<String>>,
}

/// The always-carried table-of-contents entry for a project rule.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSummary {
    /// `global` rules precede `project` rules in effective work context.
    pub scope: String,
    pub id: String,
    pub headline: String,
    pub has_more: bool,
    pub bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board_tags: Option<Vec<String>>,
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
    pub canonical: bool,
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
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
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
    pub board_path: String,
    pub canonical: bool,
    pub created_at: i64,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub name: String,
    pub board_path: String,
    pub canonical_root: String,
    pub workspace_roots: Vec<String>,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPacket {
    pub task: Task,
    pub ancestors: Vec<Task>,
    pub dependencies: Vec<Task>,
    pub claim: Option<ClaimSummary>,
    pub notes: Vec<TaskNote>,
    pub checkpoints: Vec<Checkpoint>,
    pub handoffs: Vec<Handoff>,
    /// Active project rules, as an untruncated table of contents.
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
    /// Who created the row. Optional, so existing callers keep working; an
    /// absent actor is recorded as absent rather than invented.
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

/// The kinds of thing that can need the operator, and nothing else.
///
/// Deliberately no `info`: a note that does not need anyone is a note, and
/// `task note` already holds those. Everything here is something only the
/// operator can retire.
pub const ATTENTION_KINDS: [&str; 5] = ["blocking", "decision", "approval", "review", "risk"];

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
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
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
