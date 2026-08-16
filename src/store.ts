import { randomUUID } from "node:crypto";
import type { Database } from "bun:sqlite";
import {
  closeDatabase,
  databaseIntegrity,
  openBoardDatabase,
  writeDatabaseSnapshot,
} from "./db.js";
import {
  NOTE_KINDS,
  HANDOFF_REASONS,
  HANDOFF_STATUSES,
  TASK_STATUSES,
  TASK_TYPES,
  type Checkpoint,
  type CheckpointState,
  type Claim,
  type Handoff,
  type HandoffReason,
  type HandoffStatus,
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
  assignee: string | null;
  lane: string | null;
  deliverable: string | null;
  stale_minutes: number | null;
  driver_only: number;
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

interface HandoffRow {
  id: string;
  task_id: string;
  checkpoint_seq: number;
  reason: string;
  status: string;
  from_agent: string;
  from_session: string | null;
  from_model: string | null;
  to_agent: string | null;
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
  accepted_at: number | null;
  accepted_by: string | null;
  accepted_session: string | null;
}

export interface AddTaskInput {
  id?: string;
  type?: TaskType;
  parentID?: string;
  title: string;
  body?: string;
  assignee?: string;
  lane?: string;
  deliverable?: string;
  staleMinutes?: number;
  driverOnly?: boolean;
  status?: TaskStatus;
  priority?: number;
  dependencies?: string[];
  metadata?: Record<string, unknown>;
}

export interface UpdateTaskInput {
  parentID?: string | null;
  title?: string;
  body?: string | null;
  assignee?: string | null;
  lane?: string | null;
  deliverable?: string | null;
  staleMinutes?: number | null;
  driverOnly?: boolean;
  priority?: number;
  dependencies?: string[];
  metadata?: Record<string, unknown>;
}

export interface ImportTaskInput extends Omit<AddTaskInput, "id"> {
  id: string;
  createdAt: number;
  updatedAt?: number;
  completedAt?: number | null;
  notes?: Array<{
    author: string;
    kind: NoteKind;
    body: string;
    createdAt?: number;
  }>;
}

export interface ClaimOptions {
  agentID: string;
  sessionID?: string;
  leaseMs?: number;
  callerLane?: string;
  roleFilter?: string;
  crossLaneClaim?: boolean;
  callerScope?: "member" | "driver";
  allowReassign?: boolean;
}

export interface AdvanceStoryOptions {
  actor: string;
  target?: string;
  reviewer?: string;
  committer?: string;
}

export interface AdvanceStoryResult {
  from: string;
  to: string;
  parentEpicFlipped: boolean;
  dispatchedTaskID: string | null;
  noop: boolean;
}

export interface StorySignoffResult {
  storyID: string;
  actor: string;
  at: number;
  note: string | null;
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

export interface CreateHandoffInput {
  taskID: string;
  leaseToken: string;
  fromAgent: string;
  fromSession?: string;
  fromModel?: string;
  toAgent?: string;
  reason?: HandoffReason;
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

export interface AcceptHandoffOptions extends ClaimOptions {}

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
    assignee: row.assignee,
    lane: row.lane,
    deliverable: row.deliverable,
    staleMinutes: row.stale_minutes,
    driverOnly: row.driver_only === 1,
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

function handoffFromRow(row: HandoffRow): Handoff {
  return {
    id: row.id,
    taskID: row.task_id,
    checkpointSeq: row.checkpoint_seq,
    reason: assertOneOf(row.reason, HANDOFF_REASONS, "handoff reason"),
    status: assertOneOf(row.status, HANDOFF_STATUSES, "handoff status"),
    fromAgent: row.from_agent,
    fromSession: row.from_session,
    fromModel: row.from_model,
    toAgent: row.to_agent,
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
    acceptedAt: row.accepted_at,
    acceptedBy: row.accepted_by,
    acceptedSession: row.accepted_session,
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

  integrityCheck(): string[] {
    return databaseIntegrity(this.db);
  }

  backup(path: string): void {
    writeDatabaseSnapshot(this.db, path);
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
    const type = input.type ?? "task";
    const id = input.id ?? `${type === "epic" ? "e" : type === "story" ? "s" : "t"}-${randomUUID().slice(0, 8)}`;
    const status = input.status ?? "todo";
    assertOneOf(type, TASK_TYPES, "task type");
    assertOneOf(status, TASK_STATUSES, "task status");
    const title = nonempty(input.title, "title");
    const priority = input.priority ?? 3;
    if (!Number.isInteger(priority)) throw new Error("priority must be an integer");
    if (
      input.staleMinutes !== undefined &&
      (!Number.isInteger(input.staleMinutes) || input.staleMinutes < 0)
    ) {
      throw new Error("stale minutes must be a non-negative integer");
    }
    const now = this.now();
    const dependencies = [...new Set(input.dependencies ?? [])];

    this.db.transaction(() => {
      if (input.parentID) this.requireTask(input.parentID);
      this.db
        .query(
          `INSERT INTO tasks(
             id,type,parent_id,title,body,assignee,lane,deliverable,stale_minutes,driver_only,
             status,priority,created_at,updated_at,completed_at,metadata
           ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
        )
        .run(
          id,
          type,
          input.parentID ?? null,
          title,
          input.body?.trim() || null,
          input.assignee?.trim() || null,
          input.lane?.trim() || null,
          input.deliverable?.trim() || null,
          input.staleMinutes ?? null,
          input.driverOnly ? 1 : 0,
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

  updateTask(taskID: string, input: UpdateTaskInput, actor: string): Task {
    const who = nonempty(actor, "actor");
    if (input.priority !== undefined && !Number.isInteger(input.priority)) {
      throw new Error("priority must be an integer");
    }
    if (
      input.staleMinutes !== undefined &&
      input.staleMinutes !== null &&
      (!Number.isInteger(input.staleMinutes) || input.staleMinutes < 0)
    ) {
      throw new Error("stale minutes must be a non-negative integer");
    }
    this.db.transaction(() => {
      const current = this.requireTask(taskID);
      if (input.parentID !== undefined) {
        if (input.parentID === taskID) throw new Error("task cannot be its own parent");
        let parentID = input.parentID;
        const seen = new Set<string>([taskID]);
        while (parentID) {
          if (seen.has(parentID)) throw new Error(`parent ${input.parentID} would create a cycle`);
          seen.add(parentID);
          parentID = this.requireTask(parentID).parentID;
        }
      }
      const next = {
        parentID: input.parentID === undefined ? current.parentID : input.parentID,
        title: input.title === undefined ? current.title : nonempty(input.title, "title"),
        body: input.body === undefined ? current.body : input.body?.trim() || null,
        assignee:
          input.assignee === undefined ? current.assignee : input.assignee?.trim() || null,
        lane: input.lane === undefined ? current.lane : input.lane?.trim() || null,
        deliverable:
          input.deliverable === undefined
            ? current.deliverable
            : input.deliverable?.trim() || null,
        staleMinutes:
          input.staleMinutes === undefined ? current.staleMinutes : input.staleMinutes,
        driverOnly: input.driverOnly ?? current.driverOnly,
        priority: input.priority ?? current.priority,
        metadata: input.metadata ?? current.metadata,
      };
      this.db
        .query(
          `UPDATE tasks SET
             parent_id=?,title=?,body=?,assignee=?,lane=?,deliverable=?,stale_minutes=?,driver_only=?,
             priority=?,metadata=?,updated_at=? WHERE id=?`,
        )
        .run(
          next.parentID,
          next.title,
          next.body,
          next.assignee,
          next.lane,
          next.deliverable,
          next.staleMinutes,
          next.driverOnly ? 1 : 0,
          next.priority,
          JSON.stringify(next.metadata),
          this.now(),
          taskID,
        );
      if (input.dependencies !== undefined) {
        const dependencies = [...new Set(input.dependencies)];
        for (const dependency of dependencies) {
          if (dependency === taskID) throw new Error("task cannot depend on itself");
          this.requireTask(dependency);
          if (this.dependsTransitivelyOn(dependency, taskID)) {
            throw new Error(`dependency ${dependency} would create a cycle`);
          }
        }
        this.db.query("DELETE FROM task_dependencies WHERE task_id=?").run(taskID);
        for (const dependency of dependencies) {
          this.db
            .query("INSERT INTO task_dependencies(task_id,depends_on) VALUES(?,?)")
            .run(taskID, dependency);
        }
      }
      this.event(taskID, "task_updated", who, {
        fields: Object.keys(input),
      });
    }).immediate();
    return this.requireTask(taskID);
  }

  patchTaskMetadata(taskID: string, patch: Record<string, unknown>, actor: string): Task {
    const who = nonempty(actor, "actor");
    this.db.transaction(() => {
      const current = this.requireTask(taskID);
      const metadata = { ...current.metadata };
      for (const [key, value] of Object.entries(patch)) {
        if (value === null) delete metadata[key];
        else metadata[key] = value;
      }
      this.db
        .query("UPDATE tasks SET metadata=?,updated_at=? WHERE id=?")
        .run(JSON.stringify(metadata), this.now(), taskID);
      this.event(taskID, "task_metadata_patched", who, { keys: Object.keys(patch) });
    }).immediate();
    return this.requireTask(taskID);
  }

  advanceStory(storyID: string, options: AdvanceStoryOptions): AdvanceStoryResult {
    const actor = nonempty(options.actor, "actor");
    let result!: AdvanceStoryResult;
    this.db.transaction(() => {
      const story = this.requireTask(storyID);
      if (story.type !== "story") throw new Error(`${storyID} is not a story`);
      const current =
        typeof story.metadata.workflowStatus === "string"
          ? story.metadata.workflowStatus
          : "planning";
      const mergeMode =
        story.metadata.mergeMode === "trunk-direct" ? "trunk-direct" : "feature-branch";
      const nextByState: Record<string, string | null> = {
        planning: "ready",
        ready: "in-progress",
        "in-progress": "testing",
        testing: "review",
        review: mergeMode === "trunk-direct" ? "done" : "merging",
        merging: "done",
        done: null,
      };
      const target = options.target?.trim() || nextByState[current];
      if (!target) throw new Error(`story ${storyID} is in terminal state ${current}`);
      const legal = target === current || target === nextByState[current];
      if (!legal) throw new Error(`illegal story transition ${current} -> ${target}`);
      if (target === current) {
        result = {
          from: current,
          to: target,
          parentEpicFlipped: false,
          dispatchedTaskID: null,
          noop: true,
        };
        return;
      }

      const children = this.db
        .query("SELECT * FROM tasks WHERE parent_id=? AND type='task' ORDER BY created_at,id")
        .all(storyID) as TaskRow[];
      if (target === "testing") {
        const blockers = children
          .map(taskFromRow)
          .filter((task) => (task.lane ?? "misc") !== "test" && task.status !== "done")
          .map((task) => task.id);
        if (blockers.length) throw new Error(`non-test-lane tasks still open: ${blockers.join(",")}`);
      }
      if (target === "review") {
        const blockers = children
          .map(taskFromRow)
          .filter((task) => task.lane === "test" && task.status !== "done")
          .map((task) => task.id);
        if (blockers.length) throw new Error(`test-lane tasks still open: ${blockers.join(",")}`);
        if (!options.reviewer) throw new Error("reviewer is required when entering review");
      }
      if (target === "merging") {
        if (story.metadata.reviewSignoff !== true) throw new Error("reviewer signoff is required");
        if (!options.committer) throw new Error("committer is required when entering merging");
      }
      if (target === "done") {
        if (mergeMode === "trunk-direct") {
          if (story.metadata.reviewSignoff !== true) throw new Error("reviewer signoff is required");
        } else {
          const mergeTaskID =
            typeof story.metadata.mergeTaskID === "string" ? story.metadata.mergeTaskID : null;
          if (!mergeTaskID || this.requireTask(mergeTaskID).status !== "done") {
            throw new Error(`merge task ${mergeTaskID ?? "(missing)"} is not done`);
          }
        }
      }

      const now = this.now();
      let parentEpicFlipped = false;
      if (current === "ready" && target === "in-progress" && story.parentID) {
        const parent = this.requireTask(story.parentID);
        if (parent.type === "epic" && parent.metadata.workflowStatus === "ready") {
          this.db
            .query("UPDATE tasks SET status='in_progress',metadata=?,updated_at=? WHERE id=?")
            .run(JSON.stringify({ ...parent.metadata, workflowStatus: "in-progress" }), now, parent.id);
          this.event(parent.id, "epic_advanced", actor, { from: "ready", to: "in-progress" });
          parentEpicFlipped = true;
        }
      }

      let dispatchedTaskID: string | null = null;
      const storyMetadata: Record<string, unknown> = {
        ...story.metadata,
        workflowStatus: target,
        advancedAt: Math.floor(now / 1_000),
      };
      if (target === "review" || target === "merging") {
        dispatchedTaskID = `t-${randomUUID().slice(0, 8)}`;
        const enteringReview = target === "review";
        const assignee = nonempty(enteringReview ? options.reviewer! : options.committer!, "dispatch assignee");
        this.db
          .query(
            `INSERT INTO tasks(
               id,type,parent_id,title,body,assignee,lane,deliverable,stale_minutes,driver_only,
               status,priority,created_at,updated_at,completed_at,metadata
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
          )
          .run(
            dispatchedTaskID,
            "task",
            storyID,
            `${enteringReview ? "review" : "merge"} ${storyID}`,
            `Story ${storyID} entered ${target}.`,
            assignee,
            enteringReview ? "review" : "misc",
            null,
            null,
            0,
            "in_progress",
            1,
            now,
            now,
            null,
            JSON.stringify({ workflowDispatch: target }),
          );
        this.event(dispatchedTaskID, "task_created", actor, { storyID, workflowDispatch: target });
        if (!enteringReview) storyMetadata.mergeTaskID = dispatchedTaskID;
      }

      const status: TaskStatus =
        target === "planning"
          ? "backlog"
          : target === "ready"
            ? "todo"
            : target === "in-progress"
              ? "in_progress"
              : target === "done"
                ? "done"
                : "review";
      this.db
        .query("UPDATE tasks SET status=?,metadata=?,updated_at=?,completed_at=? WHERE id=?")
        .run(status, JSON.stringify(storyMetadata), now, status === "done" ? now : null, storyID);
      this.event(storyID, "story_advanced", actor, { from: current, to: target, dispatchedTaskID });
      result = {
        from: current,
        to: target,
        parentEpicFlipped,
        dispatchedTaskID,
        noop: false,
      };
    }).immediate();
    return result;
  }

  signoffStory(storyID: string, actorValue: string, noteValue?: string): StorySignoffResult {
    return this.setStorySignoff(storyID, actorValue, true, noteValue);
  }

  unsignoffStory(storyID: string, actorValue: string, noteValue?: string): StorySignoffResult {
    return this.setStorySignoff(storyID, actorValue, false, noteValue);
  }

  importTasks(inputs: ImportTaskInput[], actor: string): Task[] {
    const who = nonempty(actor, "actor");
    const ids = new Set<string>();
    for (const input of inputs) {
      if (ids.has(input.id)) throw new Error(`duplicate imported task ${input.id}`);
      ids.add(input.id);
      assertOneOf(input.type ?? "task", TASK_TYPES, "task type");
      assertOneOf(input.status ?? "todo", TASK_STATUSES, "task status");
      nonempty(input.id, "task id");
      nonempty(input.title, "title");
    }
    this.db.transaction(() => {
      for (const input of inputs) {
        const status = input.status ?? "todo";
        const createdAt = input.createdAt;
        const updatedAt = input.updatedAt ?? input.completedAt ?? createdAt;
        const completedAt = status === "done" ? (input.completedAt ?? updatedAt) : null;
        this.db
          .query(
            `INSERT INTO tasks(
               id,type,parent_id,title,body,assignee,lane,deliverable,stale_minutes,driver_only,
               status,priority,created_at,updated_at,completed_at,metadata
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
          )
          .run(
            input.id,
            input.type ?? "task",
            null,
            nonempty(input.title, "title"),
            input.body?.trim() || null,
            input.assignee?.trim() || null,
            input.lane?.trim() || null,
            input.deliverable?.trim() || null,
            input.staleMinutes ?? null,
            input.driverOnly ? 1 : 0,
            status,
            input.priority ?? 3,
            createdAt,
            updatedAt,
            completedAt,
            JSON.stringify(input.metadata ?? {}),
          );
        for (const note of input.notes ?? []) {
          assertOneOf(note.kind, NOTE_KINDS, "note kind");
          this.db
            .query("INSERT INTO task_notes(task_id,author,kind,body,created_at) VALUES(?,?,?,?,?)")
            .run(
              input.id,
              nonempty(note.author, "note author"),
              note.kind,
              nonempty(note.body, "note"),
              note.createdAt ?? updatedAt,
            );
        }
      }
      for (const input of inputs) {
        if (input.parentID) {
          this.requireTask(input.parentID);
          this.db.query("UPDATE tasks SET parent_id=? WHERE id=?").run(input.parentID, input.id);
        }
        for (const dependency of [...new Set(input.dependencies ?? [])]) {
          if (dependency === input.id) throw new Error("task cannot depend on itself");
          this.requireTask(dependency);
          if (this.dependsTransitivelyOn(dependency, input.id)) {
            throw new Error(`dependency ${dependency} would create a cycle`);
          }
          this.db
            .query("INSERT INTO task_dependencies(task_id,depends_on) VALUES(?,?)")
            .run(input.id, dependency);
        }
      }
      this.event(null, "tasks_imported", who, { taskIDs: inputs.map((input) => input.id) });
    }).immediate();
    return inputs.map((input) => this.requireTask(input.id));
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

  moveTask(
    taskID: string,
    status: TaskStatus,
    actor: string,
    metadataPatch: Record<string, unknown> = {},
  ): Task {
    assertOneOf(status, TASK_STATUSES, "task status");
    const who = nonempty(actor, "actor");
    this.db.transaction(() => {
      const current = this.requireTask(taskID);
      const now = this.now();
      const metadata = { ...current.metadata };
      for (const [key, value] of Object.entries(metadataPatch)) {
        if (value === null) delete metadata[key];
        else metadata[key] = value;
      }
      this.db
        .query("UPDATE tasks SET status=?,metadata=?,updated_at=?,completed_at=? WHERE id=?")
        .run(status, JSON.stringify(metadata), now, status === "done" ? now : null, taskID);
      if (status !== "in_progress") {
        this.db.query("DELETE FROM task_claims WHERE task_id=?").run(taskID);
      }
      this.event(taskID, "task_moved", who, { status, metadataKeys: Object.keys(metadataPatch) });
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
      let task = taskID ? this.requireTask(taskID) : this.nextClaimable(options);
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
      task = this.requireTask(task.id);
      if (task.driverOnly && options.callerScope !== "driver") {
        throw new Error(`task ${task.id} is driver-only`);
      }
      if (task.assignee && task.assignee !== agentID && !options.allowReassign) {
        throw new Error(`task ${task.id} is assigned to ${task.assignee}`);
      }
      const token = randomUUID();
      this.db
        .query(
          `INSERT INTO task_claims(task_id,agent_id,session_id,lease_token,claimed_at,heartbeat_at,expires_at)
           VALUES(?,?,?,?,?,?,?)`,
        )
        .run(task.id, agentID, options.sessionID ?? null, token, now, now, now + leaseMs);
      this.db
        .query("UPDATE tasks SET status='in_progress',assignee=?,updated_at=? WHERE id=?")
        .run(agentID, now, task.id);
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

  createHandoff(input: CreateHandoffInput): Handoff {
    const reason = input.reason ?? "token_pressure";
    assertOneOf(reason, HANDOFF_REASONS, "handoff reason");
    let handoff!: Handoff;
    this.db.transaction(() => {
      const now = this.now();
      const claim = this.requireLease(input.taskID, input.leaseToken, now);
      const fromAgent = nonempty(input.fromAgent, "from agent");
      if (claim.agentID !== fromAgent) {
        throw new Error(`lease belongs to ${claim.agentID}, not ${fromAgent}`);
      }
      const summary = nonempty(input.summary, "summary");
      const intent = nonempty(input.intent, "intent");
      const nextAction = nonempty(input.nextAction, "next action");
      const blockers = JSON.stringify(input.blockers ?? []);
      const validations = JSON.stringify(input.validations ?? []);
      const checkpointResult = this.db
        .query(
          `INSERT INTO checkpoints(
             task_id,author,session_id,model,state,summary,intent,next_action,
             blockers,validations,repo_path,branch,head_sha,dirty_summary,created_at
           ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
        )
        .run(
          input.taskID,
          fromAgent,
          input.fromSession ?? claim.sessionID,
          input.fromModel ?? null,
          "continue",
          summary,
          intent,
          nextAction,
          blockers,
          validations,
          input.repoPath ?? null,
          input.branch ?? null,
          input.headSha ?? null,
          input.dirtySummary ?? null,
          now,
        );
      const checkpointSeq = Number(checkpointResult.lastInsertRowid);
      const id = `h-${randomUUID().slice(0, 8)}`;
      this.db
        .query(
          `INSERT INTO handoffs(
             id,task_id,checkpoint_seq,reason,status,from_agent,from_session,from_model,to_agent,
             summary,intent,next_action,blockers,validations,repo_path,branch,head_sha,dirty_summary,
             created_at,accepted_at,accepted_by,accepted_session
           ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
        )
        .run(
          id,
          input.taskID,
          checkpointSeq,
          reason,
          "pending",
          fromAgent,
          input.fromSession ?? claim.sessionID,
          input.fromModel ?? null,
          input.toAgent?.trim() || null,
          summary,
          intent,
          nextAction,
          blockers,
          validations,
          input.repoPath ?? null,
          input.branch ?? null,
          input.headSha ?? null,
          input.dirtySummary ?? null,
          now,
          null,
          null,
          null,
        );
      this.db.query("DELETE FROM task_claims WHERE task_id=?").run(input.taskID);
      this.db
        .query("UPDATE tasks SET status='todo',updated_at=?,completed_at=NULL WHERE id=?")
        .run(now, input.taskID);
      this.event(input.taskID, "handoff_created", fromAgent, {
        handoffID: id,
        checkpointSeq,
        reason,
        toAgent: input.toAgent ?? null,
      });
      handoff = this.requireHandoff(id);
    }).immediate();
    return handoff;
  }

  getHandoff(id: string): Handoff | null {
    const row = this.db.query("SELECT * FROM handoffs WHERE id=?").get(id) as HandoffRow | null;
    return row ? handoffFromRow(row) : null;
  }

  requireHandoff(id: string): Handoff {
    const handoff = this.getHandoff(id);
    if (!handoff) throw new Error(`handoff ${id} not found`);
    return handoff;
  }

  handoffs(options: { taskID?: string; status?: HandoffStatus; limit?: number } = {}): Handoff[] {
    if (options.status) assertOneOf(options.status, HANDOFF_STATUSES, "handoff status");
    const limit = options.limit ?? 100;
    if (!Number.isInteger(limit) || limit < 1) throw new Error("limit must be a positive integer");
    const clauses: string[] = [];
    const params: Array<string | number> = [];
    if (options.taskID) {
      this.requireTask(options.taskID);
      clauses.push("task_id=?");
      params.push(options.taskID);
    }
    if (options.status) {
      clauses.push("status=?");
      params.push(options.status);
    }
    const where = clauses.length ? `WHERE ${clauses.join(" AND ")}` : "";
    const rows = this.db
      .query(`SELECT * FROM handoffs ${where} ORDER BY created_at DESC,id DESC LIMIT ?`)
      .all(...params, limit) as HandoffRow[];
    return rows.map(handoffFromRow);
  }

  acceptHandoff(id: string, options: AcceptHandoffOptions): { handoff: Handoff; claim: Claim } {
    const agentID = nonempty(options.agentID, "agent id");
    const leaseMs = options.leaseMs ?? 15 * 60_000;
    if (!Number.isInteger(leaseMs) || leaseMs < 1_000) {
      throw new Error("lease must be at least 1000ms");
    }
    let result!: { handoff: Handoff; claim: Claim };
    this.db.transaction(() => {
      const now = this.now();
      this.expireClaims(now);
      const handoff = this.requireHandoff(id);
      if (handoff.status !== "pending") throw new Error(`handoff ${id} is ${handoff.status}`);
      if (handoff.toAgent && handoff.toAgent !== agentID) {
        throw new Error(`handoff ${id} targets ${handoff.toAgent}, not ${agentID}`);
      }
      const task = this.requireTask(handoff.taskID);
      if (task.status !== "todo") {
        throw new Error(`task ${task.id} is ${task.status}, not claimable`);
      }
      if (task.driverOnly && options.callerScope !== "driver") {
        throw new Error(`task ${task.id} is driver-only`);
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
        .query("UPDATE tasks SET status='in_progress',assignee=?,updated_at=? WHERE id=?")
        .run(agentID, now, task.id);
      this.db
        .query(
          `UPDATE handoffs SET status='accepted',accepted_at=?,accepted_by=?,accepted_session=?
           WHERE id=? AND status='pending'`,
        )
        .run(now, agentID, options.sessionID ?? null, id);
      this.event(task.id, "handoff_accepted", agentID, {
        handoffID: id,
        sessionID: options.sessionID ?? null,
        expiresAt: now + leaseMs,
      });
      result = { handoff: this.requireHandoff(id), claim: this.getClaim(task.id)! };
    }).immediate();
    return result;
  }

  private setStorySignoff(
    storyID: string,
    actorValue: string,
    signed: boolean,
    noteValue?: string,
  ): StorySignoffResult {
    const actor = nonempty(actorValue, "actor");
    let result!: StorySignoffResult;
    this.db.transaction(() => {
      const story = this.requireTask(storyID);
      if (story.type !== "story") throw new Error(`${storyID} is not a story`);
      if (story.metadata.workflowStatus !== "review") {
        throw new Error(`story signoff is only valid in review`);
      }
      if (!signed && typeof story.metadata.mergeTaskID === "string") {
        throw new Error(`story ${storyID} signoff has already been consumed`);
      }
      const at = this.now();
      const note = noteValue?.trim() || null;
      const prior = Array.isArray(story.metadata.signoffAudit)
        ? story.metadata.signoffAudit
        : [];
      const entry = signed
        ? { signedOffBy: actor, signedOffAt: at, note }
        : { unsignedBy: actor, unsignedAt: at, note };
      const metadata = {
        ...story.metadata,
        reviewSignoff: signed,
        signoffAudit: [...prior, entry],
      };
      this.db
        .query("UPDATE tasks SET metadata=?,updated_at=? WHERE id=?")
        .run(JSON.stringify(metadata), at, storyID);
      this.event(storyID, signed ? "story_signed_off" : "story_signoff_revoked", actor, { note });
      result = { storyID, actor, at, note };
    }).immediate();
    return result;
  }

  private nextClaimable(options: ClaimOptions): Task | null {
    const candidates = (
      this.db
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
           ORDER BY t.priority ASC,t.created_at ASC,t.id ASC`,
        )
        .all() as TaskRow[]
    )
      .map(taskFromRow)
      .filter(
        (task) =>
          (!task.assignee || task.assignee === options.agentID || options.allowReassign === true) &&
          (!task.driverOnly || options.callerScope === "driver"),
      );
    if (options.roleFilter?.trim()) {
      return candidates.find((task) => task.lane === options.roleFilter!.trim()) ?? null;
    }
    const callerLane = options.callerLane?.trim();
    if (callerLane) {
      const ownLane = candidates.find((task) => task.lane === callerLane);
      if (ownLane) return ownLane;
      if (options.crossLaneClaim === false) return null;
    }
    return candidates.find((task) => task.lane === null) ?? null;
  }

  private dependsTransitivelyOn(taskID: string, targetID: string): boolean {
    const row = this.db
      .query(
        `WITH RECURSIVE dependency_tree(id) AS (
           SELECT depends_on FROM task_dependencies WHERE task_id=?
           UNION
           SELECT d.depends_on
           FROM task_dependencies d
           JOIN dependency_tree tree ON d.task_id=tree.id
         )
         SELECT 1 AS found FROM dependency_tree WHERE id=? LIMIT 1`,
      )
      .get(taskID, targetID) as { found: number } | null;
    return row !== null;
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
      .query(
        `UPDATE tasks
         SET status='todo',assignee=CASE WHEN assignee=? THEN NULL ELSE assignee END,updated_at=?
         WHERE id=? AND status='in_progress'`,
      )
      .run(row.agent_id, now, row.task_id);
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
