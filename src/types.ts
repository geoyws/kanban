export const TASK_STATUSES = [
  "backlog",
  "todo",
  "in_progress",
  "blocked",
  "review",
  "done",
  "cancelled",
] as const;
export type TaskStatus = (typeof TASK_STATUSES)[number];

export const TASK_TYPES = ["epic", "story", "task"] as const;
export type TaskType = (typeof TASK_TYPES)[number];

export const NOTE_KINDS = [
  "plan",
  "progress",
  "blocker",
  "decision",
  "evidence",
  "done",
] as const;
export type NoteKind = (typeof NOTE_KINDS)[number];

export type CheckpointState = "continue" | "blocked" | "done";

export const HANDOFF_REASONS = [
  "token_pressure",
  "provider_limit",
  "session_end",
  "manual",
] as const;
export type HandoffReason = (typeof HANDOFF_REASONS)[number];

export const HANDOFF_STATUSES = ["pending", "accepted", "cancelled"] as const;
export type HandoffStatus = (typeof HANDOFF_STATUSES)[number];

export interface Task {
  id: string;
  type: TaskType;
  parentID: string | null;
  title: string;
  body: string | null;
  assignee: string | null;
  lane: string | null;
  deliverable: string | null;
  staleMinutes: number | null;
  driverOnly: boolean;
  status: TaskStatus;
  priority: number;
  createdAt: number;
  updatedAt: number;
  completedAt: number | null;
  metadata: Record<string, unknown>;
}

export interface Claim {
  taskID: string;
  agentID: string;
  sessionID: string | null;
  leaseToken: string;
  claimedAt: number;
  heartbeatAt: number;
  expiresAt: number;
}

export type ClaimSummary = Omit<Claim, "leaseToken">;

export interface TaskNote {
  seq: number;
  taskID: string;
  author: string;
  kind: NoteKind;
  body: string;
  createdAt: number;
}

export interface Checkpoint {
  seq: number;
  taskID: string;
  author: string;
  sessionID: string | null;
  model: string | null;
  state: CheckpointState;
  summary: string;
  intent: string;
  nextAction: string;
  blockers: string[];
  validations: string[];
  repoPath: string | null;
  branch: string | null;
  headSha: string | null;
  dirtySummary: string | null;
  createdAt: number;
}

export interface Handoff {
  id: string;
  taskID: string;
  checkpointSeq: number;
  reason: HandoffReason;
  status: HandoffStatus;
  fromAgent: string;
  fromSession: string | null;
  fromModel: string | null;
  toAgent: string | null;
  summary: string;
  intent: string;
  nextAction: string;
  blockers: string[];
  validations: string[];
  repoPath: string | null;
  branch: string | null;
  headSha: string | null;
  dirtySummary: string | null;
  createdAt: number;
  acceptedAt: number | null;
  acceptedBy: string | null;
  acceptedSession: string | null;
}

export interface ContextPacket {
  task: Task;
  ancestors: Task[];
  dependencies: Task[];
  claim: ClaimSummary | null;
  notes: TaskNote[];
  checkpoints: Checkpoint[];
  handoffs: Handoff[];
  generatedAt: number;
  truncated: boolean;
}
