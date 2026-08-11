import { randomUUID } from "node:crypto";
import type { Database } from "bun:sqlite";
import { closeDatabase, openBoardDatabase } from "./db.js";
import {
  NOTE_KINDS,
  TASK_STATUSES,
  TASK_TYPES,
  type Checkpoint,
  type CheckpointState,
  type Claim,
  type NoteKind,
  type Task,
  type TaskNote,
  type TaskStatus,
  type TaskType,
} from "./types.js";

interface TaskRow {
  id: string;
  type: string;
  parent_id: string | null;
  title: string;
  body: string | null;
  status: string;
  priority: number;
  created_at: number;
  updated_at: number;
  completed_at: number | null;
  metadata: string;
}

interface ClaimRow {
  task_id: string;
  agent_id: string;
  session_id: string | null;
  lease_token: string;
  claimed_at: number;
  heartbeat_at: number;
  expires_at: number;
}

interface NoteRow {
  seq: number;
  task_id: string;
  author: string;
  kind: string;
  body: string;
  created_at: number;
}

interface CheckpointRow {
  seq: number;
  task_id: string;
  author: string;
  session_id: string | null;
  model: string | null;
  state: string;
  summary: string;
  intent: string;
  next_action: string;
  blockers: string;
  validations: string;
  repo_path: string | null;
  branch: string | null;
  head_sha: string | null;
  dirty_summary: string | null;
  created_at: number;
}

export interface AddTaskInput {
  id?: string;
  type?: TaskType;
  parentID?: string;
  title: string;
  body?: string;
  status?: TaskStatus;
  priority?: number;
  dependencies?: string[];
  metadata?: Record<string, unknown>;
}

export interface ClaimOptions {
  agentID: string;
  sessionID?: string;
  leaseMs?: number;
}

export interface CheckpointInput {
  taskID: string;
  leaseToken: string;
  author: string;
  sessionID?: string;
  model?: string;
  state?: CheckpointState;
  summary: string;
  intent: string;
  nextAction: string;
  blockers?: string[];
  validations?: string[];
  repoPath?: string;
  branch?: string;
  headSha?: string;
  dirtySummary?: string;
}

function assertOneOf<T extends string>(value: string, values: readonly T[], label: string): T {
  if (!values.includes(value as T)) {
    throw new Error(`invalid ${label} ${JSON.stringify(value)}; expected ${values.join(", ")}`);
  }
  return value as T;
}

function nonempty(value: string, label: string): string {
  const trimmed = value.trim();
  if (!trimmed) throw new Error(`${label} must not be empty`);
  return trimmed;
}

function taskFromRow(row: TaskRow): Task {
  return {
    id: row.id,
    type: assertOneOf(row.type, TASK_TYPES, "task type"),
    parentID: row.parent_id,
    title: row.title,
    body: row.body,
    status: assertOneOf(row.status, TASK_STATUSES, "task status"),
    priority: row.priority,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
    completedAt: row.completed_at,
    metadata: JSON.parse(row.metadata) as Record<string, unknown>,
  };
}

function claimFromRow(row: ClaimRow): Claim {
  return {
    taskID: row.task_id,
    agentID: row.agent_id,
    sessionID: row.session_id,
    leaseToken: row.lease_token,
    claimedAt: row.claimed_at,
    heartbeatAt: row.heartbeat_at,
    expiresAt: row.expires_at,
  };
}

function noteFromRow(row: NoteRow): TaskNote {
  return {
    seq: row.seq,
    taskID: row.task_id,
    author: row.author,
    kind: assertOneOf(row.kind, NOTE_KINDS, "note kind"),
    body: row.body,
    createdAt: row.created_at,
  };
}

function checkpointFromRow(row: CheckpointRow): Checkpoint {
  return {
    seq: row.seq,
    taskID: row.task_id,
    author: row.author,
    sessionID: row.session_id,
    model: row.model,
    state: assertOneOf(row.state, ["continue", "blocked", "done"] as const, "checkpoint state"),
    summary: row.summary,
    intent: row.intent,
    nextAction: row.next_action,
    blockers: JSON.parse(row.blockers) as string[],
    validations: JSON.parse(row.validations) as string[],
    repoPath: row.repo_path,
    branch: row.branch,
    headSha: row.head_sha,
    dirtySummary: row.dirty_summary,
    createdAt: row.created_at,
  };
}

export class KanbanStore {
  readonly path: string;
  private db: Database;
  private now: () => number;

  constructor(path: string, options: { now?: () => number } = {}) {
    this.path = path;
    this.db = openBoardDatabase(path);
    this.now = options.now ?? Date.now;
  }

  close(): void {
    closeDatabase(this.db);
  }

  initialize(name: string): void {
    const value = nonempty(name, "board name");
    this.db
      .query("INSERT INTO board_meta(key,value) VALUES('name',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
      .run(value);
  }

  boardName(): string | null {
    const row = this.db.query("SELECT value FROM board_meta WHERE key='name'").get() as
      | { value: string }
      | null;
    return row?.value ?? null;
  }

  addTask(input: AddTaskInput): Task {
    const id = input.id ?? `t-${randomUUID().slice(0, 8)}`;
    const type = input.type ?? "task";
    const status = input.status ?? "todo";
    assertOneOf(type, TASK_TYPES, "task type");
    assertOneOf(status, TASK_STATUSES, "task status");
    const title = nonempty(input.title, "title");
    const priority = input.priority ?? 3;
    if (!Number.isInteger(priority)) throw new Error("priority must be an integer");
    const now = this.now();
    const dependencies = [...new Set(input.dependencies ?? [])];

    this.db.transaction(() => {
      if (input.parentID) this.requireTask(input.parentID);
      this.db
        .query(
          `INSERT INTO tasks(id,type,parent_id,title,body,status,priority,created_at,updated_at,completed_at,metadata)
           VALUES(?,?,?,?,?,?,?,?,?,?,?)`,
        )
        .run(
          id,
          type,
          input.parentID ?? null,
          title,
          input.body?.trim() || null,
          status,
          priority,
          now,
          now,
          status === "done" ? now : null,
          JSON.stringify(input.metadata ?? {}),
        );
      for (const dependency of dependencies) {
        this.requireTask(dependency);
        this.db
          .query("INSERT INTO task_dependencies(task_id,depends_on) VALUES(?,?)")
          .run(id, dependency);
      }
      this.event(id, "task_created", null, { type, status, dependencies });
    }).immediate();
    return this.requireTask(id);
  }

  getTask(id: string): Task | null {
    const row = this.db.query("SELECT * FROM tasks WHERE id = ?").get(id) as TaskRow | null;
    return row ? taskFromRow(row) : null;
  }

  requireTask(id: string): Task {
    const task = this.getTask(id);
    if (!task) throw new Error(`task ${id} not found`);
    return task;
  }

  listTasks(status?: TaskStatus): Task[] {
    if (status) assertOneOf(status, TASK_STATUSES, "task status");
    const rows = status
      ? (this.db
          .query("SELECT * FROM tasks WHERE status = ? ORDER BY priority, created_at, id")
          .all(status) as TaskRow[])
      : (this.db.query("SELECT * FROM tasks ORDER BY priority, created_at, id").all() as TaskRow[]);
    return rows.map(taskFromRow);
  }

  moveTask(taskID: string, status: TaskStatus, actor: string): Task {
    assertOneOf(status, TASK_STATUSES, "task status");
    const who = nonempty(actor, "actor");
    this.db.transaction(() => {
      this.requireTask(taskID);
      const now = this.now();
      this.db
        .query("UPDATE tasks SET status=?,updated_at=?,completed_at=? WHERE id=?")
        .run(status, now, status === "done" ? now : null, taskID);
      if (status !== "in_progress") {
        this.db.query("DELETE FROM task_claims WHERE task_id=?").run(taskID);
      }
      this.event(taskID, "task_moved", who, { status });
    }).immediate();
    return this.requireTask(taskID);
  }

  dependencies(taskID: string): Task[] {
    return (
      this.db
        .query(
          `SELECT t.* FROM tasks t
           JOIN task_dependencies d ON d.depends_on=t.id
           WHERE d.task_id=? ORDER BY t.created_at,t.id`,
        )
        .all(taskID) as TaskRow[]
    ).map(taskFromRow);
  }

  ancestors(taskID: string): Task[] {
    const out: Task[] = [];
    let current = this.requireTask(taskID);
    const seen = new Set<string>([taskID]);
    while (current.parentID) {
      if (seen.has(current.parentID)) throw new Error(`parent cycle detected at ${current.parentID}`);
      seen.add(current.parentID);
      current = this.requireTask(current.parentID);
      out.unshift(current);
    }
    return out;
  }

  claim(taskID: string | undefined, options: ClaimOptions): Claim {
    const agentID = nonempty(options.agentID, "agent id");
    const leaseMs = options.leaseMs ?? 15 * 60_000;
    if (!Number.isInteger(leaseMs) || leaseMs < 1_000) {
      throw new Error("lease must be at least 1000ms");
    }

    let claimed!: Claim;
    this.db.transaction(() => {
      const now = this.now();
      this.expireClaims(now);
      const task = taskID ? this.requireTask(taskID) : this.nextClaimable();
      if (!task) throw new Error("no claimable task");
      if (!["todo", "in_progress"].includes(task.status)) {
        throw new Error(`task ${task.id} is ${task.status}, not claimable`);
      }
      const unmet = this.dependencies(task.id).filter((dependency) => dependency.status !== "done");
      if (unmet.length) {
        throw new Error(`task ${task.id} has unmet dependencies: ${unmet.map((d) => d.id).join(", ")}`);
      }
      const existing = this.getClaim(task.id);
      if (existing) throw new Error(`task ${task.id} is claimed by ${existing.agentID}`);

      const token = randomUUID();
      this.db
        .query(
          `INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at)
           VALUES(?,?,?,?,?,?,?)`,
        )
        .run(task.id, agentID, options.sessionID ?? null, token, now, now, now + leaseMs);
      this.db
        .query("UPDATE tasks SET status='in_progress',updated_at=? WHERE id=?")
        .run(now, task.id);
      this.event(task.id, "task_claimed", agentID, {
        sessionID: options.sessionID ?? null,
        expiresAt: now + leaseMs,
      });
      claimed = this.getClaim(task.id)!;
    }).immediate();
    return claimed;
  }

  getClaim(taskID: string): Claim | null {
    const row = this.db.query("SELECT * FROM task_claims WHERE task_id=?").get(taskID) as
      | ClaimRow
      | null;
    if (!row) return null;
    if (row.expires_at <= this.now()) {
      this.db.transaction(() => this.expireClaim(row, this.now())).immediate();
      return null;
    }
    return claimFromRow(row);
  }

  heartbeat(taskID: string, leaseToken: string, leaseMs = 15 * 60_000): Claim {
    let result!: Claim;
    this.db.transaction(() => {
      const now = this.now();
      const claim = this.requireLease(taskID, leaseToken, now);
      this.db
        .query("UPDATE task_claims SET heartbeat_at=?,expires_at=? WHERE task_id=? AND lease_token=?")
        .run(now, now + leaseMs, taskID, leaseToken);
      this.event(taskID, "claim_heartbeat", claim.agentID, { expiresAt: now + leaseMs });
      result = claimFromRow(
        this.db.query("SELECT * FROM task_claims WHERE task_id=?").get(taskID) as ClaimRow,
      );
    }).immediate();
    return result;
  }

  release(taskID: string, leaseToken: string, options: { keepStatus?: boolean } = {}): void {
    this.db.transaction(() => {
      const claim = this.requireLease(taskID, leaseToken, this.now());
      this.db.query("DELETE FROM task_claims WHERE task_id=?").run(taskID);
      if (!options.keepStatus) {
        this.db
          .query("UPDATE tasks SET status='todo',updated_at=? WHERE id=? AND status='in_progress'")
          .run(this.now(), taskID);
      }
      this.event(taskID, "claim_released", claim.agentID, {});
    }).immediate();
  }

  addNote(taskID: string, author: string, kind: NoteKind, body: string): TaskNote {
    this.requireTask(taskID);
    assertOneOf(kind, NOTE_KINDS, "note kind");
    const now = this.now();
    const result = this.db
      .query("INSERT INTO task_notes(task_id,author,kind,body,created_at) VALUES(?,?,?,?,?)")
      .run(taskID, nonempty(author, "author"), kind, nonempty(body, "note"), now);
    this.event(taskID, "note_added", author, { kind, seq: Number(result.lastInsertRowid) });
    return this.notes(taskID).at(-1)!;
  }

  notes(taskID: string, limit = 100): TaskNote[] {
    this.requireTask(taskID);
    const rows = this.db
      .query(
        `SELECT * FROM (
           SELECT * FROM task_notes WHERE task_id=? ORDER BY seq DESC LIMIT ?
         ) ORDER BY seq ASC`,
      )
      .all(taskID, limit) as NoteRow[];
    return rows.map(noteFromRow);
  }

  checkpoint(input: CheckpointInput): Checkpoint {
    const state = input.state ?? "continue";
    assertOneOf(state, ["continue", "blocked", "done"] as const, "checkpoint state");
    let checkpoint!: Checkpoint;
    this.db.transaction(() => {
      const now = this.now();
      const claim = this.requireLease(input.taskID, input.leaseToken, now);
      if (claim.agentID !== input.author) {
        throw new Error(`lease belongs to ${claim.agentID}, not ${input.author}`);
      }
      const result = this.db
        .query(
          `INSERT INTO checkpoints(
             task_id,author,session_id,model,state,summary,intent,next_action,
             blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at
           ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
        )
        .run(
          input.taskID,
          input.author,
          input.sessionID ?? null,
          input.model ?? null,
          state,
          nonempty(input.summary, "summary"),
          nonempty(input.intent, "intent"),
          nonempty(input.nextAction, "next action"),
          JSON.stringify(input.blockers ?? []),
          JSON.stringify(input.validations ?? []),
          input.repoPath ?? null,
          input.branch ?? null,
          input.headSha ?? null,
          input.dirtySummary ?? null,
          now,
        );

      let status: TaskStatus = "in_progress";
      let completedAt: number | null = null;
      if (state === "blocked") status = "blocked";
      if (state === "done") {
        status = "done";
        completedAt = now;
      }
      this.db
        .query("UPDATE tasks SET status=?,updated_at=?,completed_at=? WHERE id=?")
        .run(status, now, completedAt, input.taskID);
      if (state !== "continue") {
        this.db.query("DELETE FROM task_claims WHERE task_id=?").run(input.taskID);
      }
      this.event(input.taskID, "checkpoint_added", input.author, {
        seq: Number(result.lastInsertRowid),
        state,
      });
      checkpoint = this.checkpoints(input.taskID, 1).at(-1)!;
    }).immediate();
    return checkpoint;
  }

  checkpoints(taskID: string, limit = 20): Checkpoint[] {
    this.requireTask(taskID);
    const rows = this.db
      .query(
        `SELECT * FROM (
           SELECT * FROM checkpoints WHERE task_id=? ORDER BY seq DESC LIMIT ?
         ) ORDER BY seq ASC`,
      )
      .all(taskID, limit) as CheckpointRow[];
    return rows.map(checkpointFromRow);
  }

  private nextClaimable(): Task | null {
    const row = this.db
      .query(
        `SELECT t.* FROM tasks t
         LEFT JOIN task_claims c ON c.task_id=t.id
         WHERE t.status='todo'
           AND c.task_id IS NULL
           AND NOT EXISTS (
             SELECT 1 FROM task_dependencies d
             JOIN tasks dep ON dep.id=d.depends_on
             WHERE d.task_id=t.id AND dep.status <> 'done'
           )
         ORDER BY t.priority ASC,t.created_at ASC,t.id ASC
         LIMIT 1`,
      )
      .get() as TaskRow | null;
    return row ? taskFromRow(row) : null;
  }

  private requireLease(taskID: string, token: string, now: number): Claim {
    const row = this.db.query("SELECT * FROM task_claims WHERE task_id=?").get(taskID) as
      | ClaimRow
      | null;
    if (!row) throw new Error(`task ${taskID} has no active claim`);
    const claim = claimFromRow(row);
    if (claim.leaseToken !== token) throw new Error(`invalid lease for task ${taskID}`);
    if (claim.expiresAt <= now) {
      this.expireClaim(row, now);
      throw new Error(`lease for task ${taskID} expired`);
    }
    return claim;
  }

  private expireClaims(now: number): void {
    const expired = this.db
      .query("SELECT * FROM task_claims WHERE expires_at <= ?")
      .all(now) as ClaimRow[];
    for (const row of expired) {
      this.expireClaim(row, now);
    }
  }

  private expireClaim(row: ClaimRow, now: number): void {
    this.db.query("DELETE FROM task_claims WHERE task_id=?").run(row.task_id);
    this.db
      .query("UPDATE tasks SET status='todo',updated_at=? WHERE id=? AND status='in_progress'")
      .run(now, row.task_id);
    this.event(row.task_id, "claim_expired", row.agent_id, {
      previousSessionID: row.session_id,
      expiredAt: row.expires_at,
    });
  }

  private event(taskID: string | null, kind: string, actor: string | null, payload: unknown): void {
    this.db
      .query("INSERT INTO events(task_id,kind,actor,payload,created_at) VALUES(?,?,?,?,?)")
      .run(taskID, kind, actor, JSON.stringify(payload), this.now());
  }
}
