#!/usr/bin/env bun
import { readFileSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { contextPacket, renderContext, renderTodo } from "./context.js";
import { dataRoot, Registry } from "./registry.js";
import { KanbanStore } from "./store.js";
import {
  importAtmuxSqlite,
  importAtmuxTasks,
  type LegacyAtmuxTask,
} from "./import-atmux.js";
import {
  NOTE_KINDS,
  HANDOFF_REASONS,
  HANDOFF_STATUSES,
  TASK_STATUSES,
  TASK_TYPES,
  type CheckpointState,
  type HandoffReason,
  type HandoffStatus,
  type NoteKind,
  type TaskStatus,
  type TaskType,
} from "./types.js";

const HELP = `kanban — durable work ledger for agents

Usage:
  kanban init [--name NAME] [--workspace PATH]
  kanban workspace list [--json]
  kanban workspace attach --to REGISTERED_PATH [--workspace PATH]
  kanban dashboard [--json]
  kanban doctor [--json]
  kanban backup [--output DIRECTORY] [--json]
  kanban task add TITLE [--id ID] [--type epic|story|task] [--parent ID]
             [--body TEXT] [--status STATUS] [--priority N] [--depends-on ID ...]
             [--assignee AGENT] [--lane LANE] [--deliverable TEXT]
             [--stale-minutes N] [--driver-only]
  kanban task list [--status STATUS] [--with-relations] [--json]
  kanban task show ID [--json]
  kanban task move ID STATUS --as ACTOR [--metadata-patch-json JSON_OBJECT]
  kanban task update ID --as ACTOR [--title TEXT] [--body TEXT] [--priority N]
             [--parent ID|--clear-parent]
             [--assignee AGENT|--unassign] [--lane LANE|--clear-lane]
             [--deliverable TEXT|--clear-deliverable] [--stale-minutes N]
             [--driver-only|--no-driver-only] [--depends-on ID ...|--clear-dependencies]
  kanban task metadata ID --as ACTOR --patch-json JSON_OBJECT
  kanban claim [ID | --next] --as AGENT [--session ID] [--lease-minutes N]
             [--lane LANE] [--role LANE] [--caller-scope member|driver]
             [--no-cross-lane] [--allow-reassign] [--json]
  kanban heartbeat ID --lease TOKEN [--lease-minutes N]
  kanban release ID --lease TOKEN [--keep-status]
  kanban note ID TEXT --as AGENT [--kind KIND]
  kanban checkpoint ID --lease TOKEN --as AGENT --summary TEXT --intent TEXT --next-action TEXT
             [--state continue|blocked|done] [--blocker TEXT ...] [--validation TEXT ...]
             [--session ID] [--model ID] [--repo PATH] [--branch NAME] [--head SHA]
             [--dirty TEXT]
  kanban handoff create ID --lease TOKEN --as AGENT --summary TEXT --intent TEXT --next-action TEXT
             [--reason token_pressure|provider_limit|session_end|manual] [--to AGENT]
             [--blocker TEXT ...] [--validation TEXT ...] [--session ID] [--model ID]
             [--repo PATH] [--branch NAME] [--head SHA] [--dirty TEXT]
  kanban handoff list [--task ID] [--status pending|accepted|cancelled] [--json]
  kanban handoff accept HANDOFF_ID --as AGENT [--session ID] [--lease-minutes N]
             [--caller-scope member|driver] [--json]
  kanban import atmux-json PATH --as ACTOR [--json]
  kanban import atmux-sqlite PATH --as ACTOR [--json]
  kanban context ID [--max-chars N] [--json]
  kanban todo [--output PATH]

Board discovery:
  --db PATH        use an explicit board
  KANBAN_DB        use an explicit board from the environment
  otherwise        resolve the nearest workspace registered by 'kanban init'

SQLite is authoritative. Generated TODO files are read-only projections.`;

const BOOLEAN_FLAGS = new Set([
  "help",
  "json",
  "next",
  "keep-status",
  "driver-only",
  "no-driver-only",
  "unassign",
  "clear-lane",
  "clear-deliverable",
  "no-cross-lane",
  "allow-reassign",
  "with-relations",
  "clear-parent",
  "clear-dependencies",
]);

interface ParsedArgs {
  positionals: string[];
  flags: Map<string, string[]>;
}

function parseArgs(argv: string[]): ParsedArgs {
  const positionals: string[] = [];
  const flags = new Map<string, string[]>();
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    if (!arg.startsWith("--")) {
      positionals.push(arg);
      continue;
    }
    const equals = arg.indexOf("=");
    const name = arg.slice(2, equals === -1 ? undefined : equals);
    let value = equals === -1 ? undefined : arg.slice(equals + 1);
    if (value === undefined && !BOOLEAN_FLAGS.has(name)) {
      const next = argv[i + 1];
      if (next === undefined || next.startsWith("--")) throw new Error(`--${name} requires a value`);
      value = next;
      i++;
    }
    const values = flags.get(name) ?? [];
    values.push(value ?? "true");
    flags.set(name, values);
  }
  return { positionals, flags };
}

function one(args: ParsedArgs, name: string): string | undefined {
  const values = args.flags.get(name);
  if (!values?.length) return undefined;
  return values.at(-1);
}

function requireFlag(args: ParsedArgs, name: string): string {
  const value = one(args, name);
  if (!value) throw new Error(`--${name} is required`);
  return value;
}

function integer(value: string | undefined, fallback: number, label: string): number {
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed)) throw new Error(`${label} must be an integer`);
  return parsed;
}

function has(args: ParsedArgs, name: string): boolean {
  return args.flags.has(name);
}

function callerScope(args: ParsedArgs): "member" | "driver" | undefined {
  const value = one(args, "caller-scope");
  if (value === undefined) return undefined;
  if (value !== "member" && value !== "driver") {
    throw new Error("caller scope must be member or driver");
  }
  return value;
}

function output(value: unknown, json: boolean): void {
  if (json) console.log(JSON.stringify(value, null, 2));
  else if (typeof value === "string") console.log(value);
  else console.log(JSON.stringify(value, null, 2));
}

function openStore(args: ParsedArgs, cwd: string): { store: KanbanStore; close(): void } {
  const explicit = one(args, "db") ?? process.env.KANBAN_DB;
  if (explicit) {
    const store = new KanbanStore(resolve(explicit));
    return { store, close: () => store.close() };
  }
  const registry = new Registry();
  const workspace = registry.resolve(cwd);
  if (!workspace) {
    registry.close();
    throw new Error(`no Kanban workspace contains ${cwd}; run 'kanban init' first`);
  }
  const store = new KanbanStore(workspace.boardPath);
  return {
    store,
    close: () => {
      store.close();
      registry.close();
    },
  };
}

export function run(argv: string[], cwd = process.cwd()): void {
  const args = parseArgs(argv);
  if (has(args, "help") || args.positionals.length === 0) {
    console.log(HELP);
    return;
  }
  const [command, subcommand, ...rest] = args.positionals;

  if (command === "init") {
    const workspacePath = resolve(one(args, "workspace") ?? cwd);
    const registry = new Registry();
    try {
      const record = registry.register(workspacePath, one(args, "name") ?? basename(workspacePath));
      const store = new KanbanStore(record.boardPath);
      try {
        store.initialize(record.name);
      } finally {
        store.close();
      }
      output(record, has(args, "json"));
    } finally {
      registry.close();
    }
    return;
  }

  if (command === "workspace" && subcommand === "list") {
    const registry = new Registry();
    try {
      output(registry.list(), has(args, "json"));
    } finally {
      registry.close();
    }
    return;
  }

  if (command === "workspace" && subcommand === "attach") {
    const registry = new Registry();
    try {
      output(
        registry.attach(resolve(one(args, "workspace") ?? cwd), requireFlag(args, "to")),
        has(args, "json"),
      );
    } finally {
      registry.close();
    }
    return;
  }

  if (command === "dashboard") {
    const registry = new Registry();
    try {
      const projects = registry.projects().map((project) => {
        const projectStore = new KanbanStore(project.boardPath);
        try {
          const tasks = projectStore.listTasks();
          return {
            ...project,
            taskCounts: Object.fromEntries(
              TASK_STATUSES.map((status) => [status, tasks.filter((task) => task.status === status).length]),
            ),
            pendingHandoffs: projectStore.handoffs({ status: "pending" }).length,
            totalTasks: tasks.length,
          };
        } finally {
          projectStore.close();
        }
      });
      output(projects, has(args, "json"));
    } finally {
      registry.close();
    }
    return;
  }

  if (command === "doctor") {
    const registry = new Registry();
    try {
      const projects = registry.projects().map((project) => {
        const projectStore = new KanbanStore(project.boardPath);
        try {
          return { name: project.name, boardPath: project.boardPath, integrity: projectStore.integrityCheck() };
        } finally {
          projectStore.close();
        }
      });
      const report = { registry: registry.integrityCheck(), projects };
      const healthy =
        report.registry.length === 1 &&
        report.registry[0] === "ok" &&
        projects.every((project) => project.integrity.length === 1 && project.integrity[0] === "ok");
      output(has(args, "json") ? { healthy, ...report } : healthy ? "Kanban integrity: ok" : report, has(args, "json"));
      if (!healthy) process.exitCode = 1;
    } finally {
      registry.close();
    }
    return;
  }

  if (command === "backup") {
    const registry = new Registry();
    try {
      const timestamp = new Date().toISOString().replaceAll(":", "-");
      const directory = resolve(one(args, "output") ?? join(dataRoot(), "backups", timestamp));
      registry.backup(join(directory, "registry.db"));
      const boards: string[] = [];
      for (const project of registry.projects()) {
        const projectStore = new KanbanStore(project.boardPath);
        try {
          const destination = join(directory, "boards", basename(project.boardPath));
          projectStore.backup(destination);
          boards.push(destination);
        } finally {
          projectStore.close();
        }
      }
      output({ directory, registry: join(directory, "registry.db"), boards }, has(args, "json"));
    } finally {
      registry.close();
    }
    return;
  }

  const opened = openStore(args, cwd);
  const { store } = opened;
  try {
    if (command === "task" && subcommand === "add") {
      const title = rest[0];
      if (!title) throw new Error("task title is required");
      const type = (one(args, "type") ?? "task") as TaskType;
      if (!TASK_TYPES.includes(type)) throw new Error(`invalid task type ${type}`);
      const status = (one(args, "status") ?? "todo") as TaskStatus;
      if (!TASK_STATUSES.includes(status)) throw new Error(`invalid task status ${status}`);
      output(
        store.addTask({
          ...(one(args, "id") ? { id: one(args, "id")! } : {}),
          type,
          ...(one(args, "parent") ? { parentID: one(args, "parent")! } : {}),
          title,
          ...(one(args, "body") ? { body: one(args, "body")! } : {}),
          ...(one(args, "assignee") ? { assignee: one(args, "assignee")! } : {}),
          ...(one(args, "lane") ? { lane: one(args, "lane")! } : {}),
          ...(one(args, "deliverable") ? { deliverable: one(args, "deliverable")! } : {}),
          ...(one(args, "stale-minutes")
            ? { staleMinutes: integer(one(args, "stale-minutes"), 0, "stale minutes") }
            : {}),
          ...(has(args, "driver-only") ? { driverOnly: true } : {}),
          status,
          priority: integer(one(args, "priority"), 3, "priority"),
          dependencies: args.flags.get("depends-on") ?? [],
        }),
        has(args, "json"),
      );
      return;
    }

    if (command === "task" && subcommand === "list") {
      const rawStatus = one(args, "status");
      const status = rawStatus as TaskStatus | undefined;
      if (status && !TASK_STATUSES.includes(status)) throw new Error(`invalid task status ${status}`);
      const tasks = store.listTasks(status);
      output(
        has(args, "with-relations")
          ? tasks.map((task) => ({
              ...task,
              dependencies: store.dependencies(task.id).map((dependency) => dependency.id),
            }))
          : tasks,
        has(args, "json"),
      );
      return;
    }

    if (command === "task" && subcommand === "show") {
      if (!rest[0]) throw new Error("task id is required");
      const task = store.requireTask(rest[0]);
      const claim = store.getClaim(task.id);
      output(
        {
          ...task,
          dependencies: store.dependencies(task.id),
          claim: claim
            ? {
                taskID: claim.taskID,
                agentID: claim.agentID,
                sessionID: claim.sessionID,
                claimedAt: claim.claimedAt,
                heartbeatAt: claim.heartbeatAt,
                expiresAt: claim.expiresAt,
              }
            : null,
          notes: store.notes(task.id),
          checkpoints: store.checkpoints(task.id),
          handoffs: store.handoffs({ taskID: task.id }),
        },
        has(args, "json"),
      );
      return;
    }

    if (command === "task" && subcommand === "move") {
      if (!rest[0] || !rest[1]) throw new Error("task id and target status are required");
      const status = rest[1] as TaskStatus;
      if (!TASK_STATUSES.includes(status)) throw new Error(`invalid task status ${status}`);
      const rawPatch = one(args, "metadata-patch-json");
      const parsedPatch = rawPatch === undefined ? {} : (JSON.parse(rawPatch) as unknown);
      if (parsedPatch === null || Array.isArray(parsedPatch) || typeof parsedPatch !== "object") {
        throw new Error("--metadata-patch-json must be a JSON object");
      }
      output(
        store.moveTask(
          rest[0],
          status,
          requireFlag(args, "as"),
          parsedPatch as Record<string, unknown>,
        ),
        has(args, "json"),
      );
      return;
    }

    if (command === "task" && subcommand === "metadata") {
      const taskID = rest[0];
      if (!taskID) throw new Error("task id is required");
      const raw = requireFlag(args, "patch-json");
      const patch = JSON.parse(raw) as unknown;
      if (patch === null || Array.isArray(patch) || typeof patch !== "object") {
        throw new Error("--patch-json must be a JSON object");
      }
      output(
        store.patchTaskMetadata(taskID, patch as Record<string, unknown>, requireFlag(args, "as")),
        has(args, "json"),
      );
      return;
    }

    if (command === "task" && subcommand === "update") {
      const taskID = rest[0];
      if (!taskID) throw new Error("task id is required");
      if (has(args, "driver-only") && has(args, "no-driver-only")) {
        throw new Error("--driver-only and --no-driver-only are mutually exclusive");
      }
      if (one(args, "assignee") && has(args, "unassign")) {
        throw new Error("--assignee and --unassign are mutually exclusive");
      }
      if (one(args, "lane") && has(args, "clear-lane")) {
        throw new Error("--lane and --clear-lane are mutually exclusive");
      }
      if (one(args, "deliverable") && has(args, "clear-deliverable")) {
        throw new Error("--deliverable and --clear-deliverable are mutually exclusive");
      }
      if (one(args, "parent") && has(args, "clear-parent")) {
        throw new Error("--parent and --clear-parent are mutually exclusive");
      }
      if (args.flags.has("depends-on") && has(args, "clear-dependencies")) {
        throw new Error("--depends-on and --clear-dependencies are mutually exclusive");
      }
      output(
        store.updateTask(
          taskID,
          {
            ...(one(args, "parent")
              ? { parentID: one(args, "parent")! }
              : has(args, "clear-parent")
                ? { parentID: null }
                : {}),
            ...(one(args, "title") ? { title: one(args, "title")! } : {}),
            ...(args.flags.has("body") ? { body: one(args, "body")! } : {}),
            ...(one(args, "priority")
              ? { priority: integer(one(args, "priority"), 3, "priority") }
              : {}),
            ...(one(args, "assignee")
              ? { assignee: one(args, "assignee")! }
              : has(args, "unassign")
                ? { assignee: null }
                : {}),
            ...(one(args, "lane")
              ? { lane: one(args, "lane")! }
              : has(args, "clear-lane")
                ? { lane: null }
                : {}),
            ...(one(args, "deliverable")
              ? { deliverable: one(args, "deliverable")! }
              : has(args, "clear-deliverable")
                ? { deliverable: null }
                : {}),
            ...(one(args, "stale-minutes")
              ? { staleMinutes: integer(one(args, "stale-minutes"), 0, "stale minutes") }
              : {}),
            ...(has(args, "driver-only")
              ? { driverOnly: true }
              : has(args, "no-driver-only")
                ? { driverOnly: false }
                : {}),
            ...(has(args, "clear-dependencies")
              ? { dependencies: [] }
              : args.flags.has("depends-on")
              ? { dependencies: args.flags.get("depends-on") ?? [] }
              : {}),
          },
          requireFlag(args, "as"),
        ),
        has(args, "json"),
      );
      return;
    }

    if (command === "claim") {
      const taskID = has(args, "next") ? undefined : subcommand;
      if (!taskID && !has(args, "next")) throw new Error("task id or --next is required");
      const minutes = integer(one(args, "lease-minutes"), 15, "lease minutes");
      output(
        store.claim(taskID, {
          agentID: requireFlag(args, "as"),
          ...(one(args, "session") ? { sessionID: one(args, "session")! } : {}),
          leaseMs: minutes * 60_000,
          ...(one(args, "lane") ? { callerLane: one(args, "lane")! } : {}),
          ...(one(args, "role") ? { roleFilter: one(args, "role")! } : {}),
          ...(callerScope(args) ? { callerScope: callerScope(args)! } : {}),
          crossLaneClaim: !has(args, "no-cross-lane"),
          allowReassign: has(args, "allow-reassign"),
        }),
        has(args, "json"),
      );
      return;
    }

    if (command === "heartbeat") {
      if (!subcommand) throw new Error("task id is required");
      const minutes = integer(one(args, "lease-minutes"), 15, "lease minutes");
      output(store.heartbeat(subcommand, requireFlag(args, "lease"), minutes * 60_000), has(args, "json"));
      return;
    }

    if (command === "release") {
      if (!subcommand) throw new Error("task id is required");
      store.release(subcommand, requireFlag(args, "lease"), { keepStatus: has(args, "keep-status") });
      output(`released ${subcommand}`, false);
      return;
    }

    if (command === "note") {
      if (!subcommand || !rest[0]) throw new Error("task id and note text are required");
      const kind = (one(args, "kind") ?? "progress") as NoteKind;
      if (!NOTE_KINDS.includes(kind)) throw new Error(`invalid note kind ${kind}`);
      output(store.addNote(subcommand, requireFlag(args, "as"), kind, rest[0]), has(args, "json"));
      return;
    }

    if (command === "checkpoint") {
      if (!subcommand) throw new Error("task id is required");
      const state = (one(args, "state") ?? "continue") as CheckpointState;
      output(
        store.checkpoint({
          taskID: subcommand,
          leaseToken: requireFlag(args, "lease"),
          author: requireFlag(args, "as"),
          state,
          summary: requireFlag(args, "summary"),
          intent: requireFlag(args, "intent"),
          nextAction: requireFlag(args, "next-action"),
          blockers: args.flags.get("blocker") ?? [],
          validations: args.flags.get("validation") ?? [],
          ...(one(args, "session") ? { sessionID: one(args, "session")! } : {}),
          ...(one(args, "model") ? { model: one(args, "model")! } : {}),
          ...(one(args, "repo") ? { repoPath: one(args, "repo")! } : {}),
          ...(one(args, "branch") ? { branch: one(args, "branch")! } : {}),
          ...(one(args, "head") ? { headSha: one(args, "head")! } : {}),
          ...(one(args, "dirty") ? { dirtySummary: one(args, "dirty")! } : {}),
        }),
        has(args, "json"),
      );
      return;
    }

    if (command === "handoff" && subcommand === "create") {
      const taskID = rest[0];
      if (!taskID) throw new Error("task id is required");
      const reason = (one(args, "reason") ?? "token_pressure") as HandoffReason;
      if (!HANDOFF_REASONS.includes(reason)) throw new Error(`invalid handoff reason ${reason}`);
      output(
        store.createHandoff({
          taskID,
          leaseToken: requireFlag(args, "lease"),
          fromAgent: requireFlag(args, "as"),
          reason,
          summary: requireFlag(args, "summary"),
          intent: requireFlag(args, "intent"),
          nextAction: requireFlag(args, "next-action"),
          blockers: args.flags.get("blocker") ?? [],
          validations: args.flags.get("validation") ?? [],
          ...(one(args, "session") ? { fromSession: one(args, "session")! } : {}),
          ...(one(args, "model") ? { fromModel: one(args, "model")! } : {}),
          ...(one(args, "to") ? { toAgent: one(args, "to")! } : {}),
          ...(one(args, "repo") ? { repoPath: one(args, "repo")! } : {}),
          ...(one(args, "branch") ? { branch: one(args, "branch")! } : {}),
          ...(one(args, "head") ? { headSha: one(args, "head")! } : {}),
          ...(one(args, "dirty") ? { dirtySummary: one(args, "dirty")! } : {}),
        }),
        has(args, "json"),
      );
      return;
    }

    if (command === "handoff" && subcommand === "list") {
      const status = one(args, "status") as HandoffStatus | undefined;
      if (status && !HANDOFF_STATUSES.includes(status)) {
        throw new Error(`invalid handoff status ${status}`);
      }
      output(
        store.handoffs({
          ...(one(args, "task") ? { taskID: one(args, "task")! } : {}),
          ...(status ? { status } : {}),
        }),
        has(args, "json"),
      );
      return;
    }

    if (command === "handoff" && subcommand === "accept") {
      const handoffID = rest[0];
      if (!handoffID) throw new Error("handoff id is required");
      const minutes = integer(one(args, "lease-minutes"), 15, "lease minutes");
      output(
        store.acceptHandoff(handoffID, {
          agentID: requireFlag(args, "as"),
          ...(one(args, "session") ? { sessionID: one(args, "session")! } : {}),
          leaseMs: minutes * 60_000,
          ...(callerScope(args) ? { callerScope: callerScope(args)! } : {}),
        }),
        has(args, "json"),
      );
      return;
    }

    if (command === "import" && subcommand === "atmux-json") {
      const path = rest[0];
      if (!path) throw new Error("atmux kanban.json path is required");
      const parsed = JSON.parse(readFileSync(resolve(path), "utf8")) as { tasks?: unknown };
      if (!Array.isArray(parsed.tasks)) throw new Error("atmux JSON must contain a tasks array");
      const imported = importAtmuxTasks(
        store,
        parsed.tasks as LegacyAtmuxTask[],
        requireFlag(args, "as"),
      );
      output({ source: resolve(path), counts: { tasks: imported.length } }, has(args, "json"));
      return;
    }

    if (command === "import" && subcommand === "atmux-sqlite") {
      const path = rest[0];
      if (!path) throw new Error("atmux state.db path is required");
      const receipt = importAtmuxSqlite(store, resolve(path), requireFlag(args, "as"));
      output(
        {
          source: receipt.source,
          counts: receipt.counts,
          warnings: receipt.warnings,
          imported: receipt.imported.length,
        },
        has(args, "json"),
      );
      return;
    }

    if (command === "context") {
      if (!subcommand) throw new Error("task id is required");
      const packet = contextPacket(store, subcommand);
      if (has(args, "json")) output(packet, true);
      else output(renderContext(packet, integer(one(args, "max-chars"), 20_000, "max chars")), false);
      return;
    }

    if (command === "todo") {
      const text = renderTodo(store);
      const path = one(args, "output");
      if (path) {
        writeFileSync(resolve(path), text, "utf8");
        output(`wrote ${resolve(path)}`, false);
      } else output(text, false);
      return;
    }

    throw new Error(`unknown command: ${args.positionals.join(" ")}`);
  } finally {
    opened.close();
  }
}

if (import.meta.main) {
  try {
    run(process.argv.slice(2));
  } catch (error) {
    console.error(`Error: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  }
}
