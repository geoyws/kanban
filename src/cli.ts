#!/usr/bin/env bun
import { writeFileSync } from "node:fs";
import { basename, resolve } from "node:path";
import { contextPacket, renderContext, renderTodo } from "./context.js";
import { Registry } from "./registry.js";
import { KanbanStore } from "./store.js";
import {
  NOTE_KINDS,
  TASK_STATUSES,
  TASK_TYPES,
  type CheckpointState,
  type NoteKind,
  type TaskStatus,
  type TaskType,
} from "./types.js";

const HELP = `kanban — durable work ledger for agents

Usage:
  kanban init [--name NAME] [--workspace PATH]
  kanban workspace list [--json]
  kanban task add TITLE [--id ID] [--type epic|story|task] [--parent ID]
             [--body TEXT] [--status STATUS] [--priority N] [--depends-on ID ...]
  kanban task list [--status STATUS] [--json]
  kanban task show ID [--json]
  kanban task move ID STATUS --as ACTOR
  kanban claim [ID | --next] --as AGENT [--session ID] [--lease-minutes N] [--json]
  kanban heartbeat ID --lease TOKEN [--lease-minutes N]
  kanban release ID --lease TOKEN [--keep-status]
  kanban note ID TEXT --as AGENT [--kind KIND]
  kanban checkpoint ID --lease TOKEN --as AGENT --summary TEXT --intent TEXT --next-action TEXT
             [--state continue|blocked|done] [--blocker TEXT ...] [--validation TEXT ...]
             [--session ID] [--model ID] [--repo PATH] [--branch NAME] [--head SHA]
             [--dirty TEXT]
  kanban context ID [--max-chars N] [--json]
  kanban todo [--output PATH]

Board discovery:
  --db PATH        use an explicit board
  KANBAN_DB        use an explicit board from the environment
  otherwise        resolve the nearest workspace registered by 'kanban init'

SQLite is authoritative. Generated TODO files are read-only projections.`;

const BOOLEAN_FLAGS = new Set(["help", "json", "next", "keep-status"]);

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
      output(store.listTasks(status), has(args, "json"));
      return;
    }

    if (command === "task" && subcommand === "show") {
      if (!rest[0]) throw new Error("task id is required");
      const task = store.requireTask(rest[0]);
      output(
        {
          ...task,
          dependencies: store.dependencies(task.id),
          claim: store.getClaim(task.id),
          notes: store.notes(task.id),
          checkpoints: store.checkpoints(task.id),
        },
        has(args, "json"),
      );
      return;
    }

    if (command === "task" && subcommand === "move") {
      if (!rest[0] || !rest[1]) throw new Error("task id and target status are required");
      const status = rest[1] as TaskStatus;
      if (!TASK_STATUSES.includes(status)) throw new Error(`invalid task status ${status}`);
      output(store.moveTask(rest[0], status, requireFlag(args, "as")), has(args, "json"));
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
