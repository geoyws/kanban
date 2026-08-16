import type { KanbanStore } from "./store.js";
import type { Task, TaskStatus } from "./types.js";

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

function epochMs(value: number | null | undefined, fallback: number): number {
  if (value === null || value === undefined) return fallback;
  return value < 100_000_000_000 ? value * 1_000 : value;
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
  return store.importTasks(inputs, actor);
}
