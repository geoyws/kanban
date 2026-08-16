import { Database } from "bun:sqlite";
import type { KanbanStore } from "./store.js";
import type { ImportTaskInput } from "./store.js";
import type { Task, TaskStatus, TaskType } from "./types.js";

export interface LegacyAtmuxTask {
  id: string;
  subject?: string;
  body?: string | null;
  status?: string;
  owner?: string | null;
  deps?: string[];
  priority?: number | null;
  epic?: string | null;
  story?: string | null;
  lane?: string | null;
  deliverable?: string | null;
  staleMin?: number | null;
  driverOnly?: boolean;
  createdAt?: number;
  claimedAt?: number | null;
  completedAt?: number | null;
  note?: string | null;
  [key: string]: unknown;
}

const ATMUX_STATUS: Record<string, TaskStatus> = {
  backlog: "backlog",
  todo: "todo",
  "in-progress": "in_progress",
  in_progress: "in_progress",
  blocked: "blocked",
  review: "review",
  done: "done",
  cancelled: "cancelled",
  wontfix: "cancelled",
};

const WORKFLOW_STATUS: Record<string, TaskStatus> = {
  planning: "backlog",
  ready: "todo",
  "in-progress": "in_progress",
  testing: "review",
  review: "review",
  merging: "review",
  done: "done",
};

function epochMs(value: number | null | undefined, fallback: number): number {
  if (value === null || value === undefined) return fallback;
  return value < 100_000_000_000 ? value * 1_000 : value;
}

function json(value: unknown, fallback: unknown): unknown {
  if (typeof value !== "string" || !value.trim()) return fallback;
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

function status(value: unknown, workflow = false): TaskStatus {
  const raw = typeof value === "string" ? value : workflow ? "planning" : "todo";
  const mapped = (workflow ? WORKFLOW_STATUS : ATMUX_STATUS)[raw];
  if (!mapped) throw new Error(`unsupported atmux status ${JSON.stringify(value)}`);
  return mapped;
}

function integer(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isInteger(value) ? value : fallback;
}

function nullableString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

interface RelationshipWarnings {
  danglingDependencies: Array<{ taskID: string; dependencyID: string }>;
  missingParents: Array<{ taskID: string; parentID: string }>;
  nonterminalCompletions: Array<{ taskID: string; status: TaskStatus; completedAt: number }>;
}

function normalizeRelationships(inputs: ImportTaskInput[]): {
  inputs: ImportTaskInput[];
  warnings: RelationshipWarnings;
} {
  const known = new Set(inputs.map((input) => input.id));
  const warnings: RelationshipWarnings = {
    danglingDependencies: [],
    missingParents: [],
    nonterminalCompletions: [],
  };
  const normalized = inputs.map((input) => {
    const { parentID: originalParentID, ...rest } = input;
    const dependencies: string[] = [];
    const dangling: string[] = [];
    for (const dependency of input.dependencies ?? []) {
      if (known.has(dependency)) dependencies.push(dependency);
      else {
        dangling.push(dependency);
        warnings.danglingDependencies.push({ taskID: input.id, dependencyID: dependency });
      }
    }
    let parentID = originalParentID;
    if (parentID && !known.has(parentID)) {
      warnings.missingParents.push({ taskID: input.id, parentID });
      parentID = undefined;
    }
    const nonterminalCompletion =
      input.completedAt != null && input.status !== "done" ? input.completedAt : null;
    if (nonterminalCompletion != null) {
      warnings.nonterminalCompletions.push({
        taskID: input.id,
        status: input.status ?? "todo",
        completedAt: nonterminalCompletion,
      });
    }
    return {
      ...rest,
      ...(parentID ? { parentID } : {}),
      dependencies,
      metadata: {
        ...(input.metadata ?? {}),
        ...(dangling.length ? { legacyDanglingDependencies: dangling } : {}),
        ...(originalParentID && !parentID ? { legacyMissingParent: originalParentID } : {}),
        ...(nonterminalCompletion == null
          ? {}
          : { legacyCompletedAt: nonterminalCompletion }),
      },
    };
  });
  return { inputs: normalized, warnings };
}

export function importAtmuxTasks(
  store: KanbanStore,
  rows: LegacyAtmuxTask[],
  actor: string,
  importedAt = Date.now(),
): Task[] {
  const inputs = rows.map((row) => {
    const status = ATMUX_STATUS[row.status ?? "todo"];
    if (!status) throw new Error(`unsupported atmux task status ${JSON.stringify(row.status)}`);
    const createdAt = epochMs(row.createdAt, importedAt);
    const claimedAt = epochMs(row.claimedAt, createdAt);
    const completedAt = row.completedAt == null ? null : epochMs(row.completedAt, claimedAt);
    const {
      id,
      subject,
      body,
      owner,
      deps,
      priority,
      lane,
      deliverable,
      staleMin,
      driverOnly,
      note,
      epic,
      story,
      ...extra
    } = row;
    return {
      id,
      title: subject?.trim() || id,
      ...(body ? { body } : {}),
      status,
      ...(owner ? { assignee: owner } : {}),
      dependencies: deps ?? [],
      priority: priority ?? 3,
      ...(lane ? { lane } : {}),
      ...(deliverable ? { deliverable } : {}),
      ...(staleMin == null ? {} : { staleMinutes: staleMin }),
      driverOnly: driverOnly ?? false,
      createdAt,
      updatedAt: completedAt ?? claimedAt,
      completedAt,
      metadata: {
        importedFrom: "atmux",
        legacyEpic: epic ?? null,
        legacyStory: story ?? null,
        atmuxExtra: extra,
      },
      ...(note
        ? {
            notes: [
              {
                author: "atmux/import",
                kind: "progress" as const,
                body: note,
                createdAt: completedAt ?? claimedAt,
              },
            ],
          }
        : {}),
    };
  });
  return store.importTasks(normalizeRelationships(inputs).inputs, actor);
}

export interface AtmuxSqliteImportReceipt {
  source: string;
  counts: { epics: number; stories: number; tasks: number };
  warnings: RelationshipWarnings;
  imported: Task[];
}

export interface LegacyAtmuxJson {
  tasks?: LegacyAtmuxTask[];
  epics?: Array<Record<string, unknown>>;
  stories?: Array<Record<string, unknown>>;
}

export interface AtmuxJsonImportReceipt extends AtmuxSqliteImportReceipt {}

export function importAtmuxJson(
  store: KanbanStore,
  parsed: LegacyAtmuxJson,
  actor: string,
  importedAt = Date.now(),
): AtmuxJsonImportReceipt {
  const epicRows = parsed.epics ?? [];
  const storyRows = parsed.stories ?? [];
  const taskRows = parsed.tasks ?? [];
  const inputs: ImportTaskInput[] = [];

  for (const row of epicRows) {
    const createdAt = epochMs(row.createdAt as number | null, importedAt);
    const completedAt =
      row.completedAt == null ? null : epochMs(row.completedAt as number, createdAt);
    inputs.push({
      id: String(row.id),
      type: "epic",
      title: nullableString(row.title) ?? String(row.id),
      ...(nullableString(row.body) ? { body: String(row.body) } : {}),
      status: status(row.status, true),
      priority: 1,
      dependencies: Array.isArray(row.dependsOn) ? (row.dependsOn as string[]) : [],
      createdAt,
      updatedAt: completedAt ?? createdAt,
      completedAt,
      metadata: {
        importedFrom: "atmux/kanban.json",
        workflowStatus: typeof row.status === "string" ? row.status : "planning",
        driverRef: row.driverRef ?? null,
        isReady: row.isReady === true,
        spawnedAt: row.spawnedAt ?? null,
        legacyStories: Array.isArray(row.stories) ? row.stories : [],
        atmuxExtra: row,
      },
    });
  }

  for (const row of storyRows) {
    const createdAt = epochMs(row.createdAt as number | null, importedAt);
    const completedAt =
      row.completedAt == null ? null : epochMs(row.completedAt as number, createdAt);
    inputs.push({
      id: String(row.id),
      type: "story",
      ...(nullableString(row.epic) ? { parentID: String(row.epic) } : {}),
      title: nullableString(row.title) ?? String(row.id),
      ...(nullableString(row.body) ? { body: String(row.body) } : {}),
      status: status(row.status, true),
      priority: 2,
      createdAt,
      updatedAt: completedAt ?? epochMs(row.advancedAt as number | null, createdAt),
      completedAt,
      metadata: {
        importedFrom: "atmux/kanban.json",
        workflowStatus: typeof row.status === "string" ? row.status : "planning",
        acceptanceCriteria: row.acceptanceCriteria ?? null,
        reviewSignoff: row.reviewSignoff === true,
        mergeTaskID: row.mergeTaskId ?? null,
        mergeMode: row.mergeMode ?? "feature-branch",
        advancedAt: row.advancedAt ?? null,
        atmuxExtra: row,
      },
    });
  }

  for (const row of taskRows) {
    const createdAt = epochMs(row.createdAt, importedAt);
    const claimedAt = epochMs(row.claimedAt, createdAt);
    const completedAt = row.completedAt == null ? null : epochMs(row.completedAt, claimedAt);
    const parentID = row.story ?? row.epic ?? undefined;
    inputs.push({
      id: row.id,
      type: "task" as TaskType,
      ...(parentID ? { parentID } : {}),
      title: row.subject?.trim() || row.id,
      ...(row.body ? { body: row.body } : {}),
      status: status(row.status),
      ...(row.owner ? { assignee: row.owner } : {}),
      dependencies: row.deps ?? [],
      priority: row.priority ?? 3,
      ...(row.lane ? { lane: row.lane } : {}),
      ...(row.deliverable ? { deliverable: row.deliverable } : {}),
      ...(row.staleMin == null ? {} : { staleMinutes: row.staleMin }),
      driverOnly: row.driverOnly ?? false,
      createdAt,
      updatedAt: completedAt ?? claimedAt,
      completedAt,
      metadata: {
        importedFrom: "atmux/kanban.json",
        legacyEpic: row.epic ?? null,
        legacyStory: row.story ?? null,
        atmuxExtra: row,
      },
      ...(row.note
        ? {
            notes: [
              {
                author: "atmux/import",
                kind: "progress" as const,
                body: row.note,
                createdAt: completedAt ?? claimedAt,
              },
            ],
          }
        : {}),
    });
  }

  const normalized = normalizeRelationships(inputs);
  return {
    source: "kanban.json",
    counts: { epics: epicRows.length, stories: storyRows.length, tasks: taskRows.length },
    warnings: normalized.warnings,
    imported: store.importTasks(normalized.inputs, actor),
  };
}

export function importAtmuxSqlite(
  store: KanbanStore,
  path: string,
  actor: string,
  importedAt = Date.now(),
): AtmuxSqliteImportReceipt {
  const source = new Database(path, { readonly: true, strict: true });
  try {
    const tables = new Set(
      (
        source
          .query("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('tasks','stories','epics')")
          .all() as Array<{ name: string }>
      ).map((row) => row.name),
    );
    if (!tables.has("tasks")) throw new Error("atmux state.db has no tasks table");
    const taskRows = source.query("SELECT * FROM tasks ORDER BY created_at,id").all() as Array<
      Record<string, unknown>
    >;
    const storyRows = tables.has("stories")
      ? (source.query("SELECT * FROM stories ORDER BY created_at,id").all() as Array<
          Record<string, unknown>
        >)
      : [];
    const epicRows = tables.has("epics")
      ? (source.query("SELECT * FROM epics ORDER BY created_at,id").all() as Array<
          Record<string, unknown>
        >)
      : [];
    const inputs: ImportTaskInput[] = [];

    for (const row of epicRows) {
      const createdAt = epochMs(row.created_at as number | null, importedAt);
      const completedAt =
        row.completed_at == null ? null : epochMs(row.completed_at as number, createdAt);
      inputs.push({
        id: String(row.id),
        type: "epic",
        title: nullableString(row.title) ?? String(row.id),
        ...(nullableString(row.body) ? { body: String(row.body) } : {}),
        status: status(row.status, true),
        priority: 1,
        dependencies: (json(row.depends_on, []) as string[]) ?? [],
        createdAt,
        updatedAt: completedAt ?? createdAt,
        completedAt,
        metadata: {
          importedFrom: "atmux/state.db",
          workflowStatus: typeof row.status === "string" ? row.status : "planning",
          driverRef: row.driver_ref ?? null,
          isReady: row.is_ready === 1,
          spawnedAt: row.spawned_at ?? null,
          legacyStories: json(row.stories, []),
          atmuxExtra: json(row.extra, {}),
        },
      });
    }

    for (const row of storyRows) {
      const createdAt = epochMs(row.created_at as number | null, importedAt);
      const completedAt =
        row.completed_at == null ? null : epochMs(row.completed_at as number, createdAt);
      inputs.push({
        id: String(row.id),
        type: "story",
        ...(nullableString(row.epic) ? { parentID: String(row.epic) } : {}),
        title: nullableString(row.title) ?? String(row.id),
        ...(nullableString(row.body) ? { body: String(row.body) } : {}),
        status: status(row.status, true),
        priority: 2,
        createdAt,
        updatedAt: completedAt ?? epochMs(row.advanced_at as number | null, createdAt),
        completedAt,
        metadata: {
          importedFrom: "atmux/state.db",
          workflowStatus: typeof row.status === "string" ? row.status : "planning",
          acceptanceCriteria: row.acceptance_criteria ?? null,
          reviewSignoff: row.review_signoff === 1,
          mergeTaskID: row.merge_task_id ?? null,
          mergeMode: row.merge_mode ?? "feature-branch",
          advancedAt: row.advanced_at ?? null,
          atmuxExtra: json(row.extra, {}),
        },
      });
    }

    for (const row of taskRows) {
      const createdAt = epochMs(row.created_at as number | null, importedAt);
      const claimedAt = epochMs(row.claimed_at as number | null, createdAt);
      const completedAt =
        row.completed_at == null ? null : epochMs(row.completed_at as number, claimedAt);
      const parentID = nullableString(row.story) ?? nullableString(row.epic);
      const note = nullableString(row.note);
      inputs.push({
        id: String(row.id),
        type: "task" as TaskType,
        ...(parentID ? { parentID } : {}),
        title: nullableString(row.subject) ?? String(row.id),
        ...(nullableString(row.body) ? { body: String(row.body) } : {}),
        status: status(row.status),
        ...(nullableString(row.owner) ? { assignee: String(row.owner) } : {}),
        dependencies: (json(row.deps, []) as string[]) ?? [],
        priority: integer(row.priority, 3),
        ...(nullableString(row.lane) ? { lane: String(row.lane) } : {}),
        ...(nullableString(row.deliverable) ? { deliverable: String(row.deliverable) } : {}),
        ...(row.stale_min == null ? {} : { staleMinutes: integer(row.stale_min, 0) }),
        driverOnly: row.driver_only === 1,
        createdAt,
        updatedAt: completedAt ?? claimedAt,
        completedAt,
        metadata: {
          importedFrom: "atmux/state.db",
          claimedFrom: json(row.claimed_from, row.claimed_from ?? null),
          createdFrom: json(row.created_from, row.created_from ?? null),
          legacyEpic: row.epic ?? null,
          legacyStory: row.story ?? null,
          atmuxExtra: json(row.extra, {}),
        },
        ...(note
          ? {
              notes: [
                {
                  author: "atmux/import",
                  kind: "progress" as const,
                  body: note,
                  createdAt: completedAt ?? claimedAt,
                },
              ],
            }
          : {}),
      });
    }

    const normalized = normalizeRelationships(inputs);
    return {
      source: path,
      counts: { epics: epicRows.length, stories: storyRows.length, tasks: taskRows.length },
      warnings: normalized.warnings,
      imported: store.importTasks(normalized.inputs, actor),
    };
  } finally {
    source.close();
  }
}
