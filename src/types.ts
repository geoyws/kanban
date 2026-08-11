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

export interface Task {
  id: string;
  type: TaskType;
  parentID: string | null;
  title: string;
  body: string | null;
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

export interface ContextPacket {
  task: Task;
  ancestors: Task[];
  dependencies: Task[];
  claim: Claim | null;
  notes: TaskNote[];
  checkpoints: Checkpoint[];
  generatedAt: number;
  truncated: boolean;
}
