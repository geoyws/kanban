import type { KanbanStore } from "./store.js";
import type { Checkpoint, ContextPacket, Handoff, Task, TaskNote } from "./types.js";

function iso(epochMs: number | null): string {
  return epochMs === null ? "-" : new Date(epochMs).toISOString();
}

function taskLine(task: Task): string {
  return `- ${task.id} [${task.status}] P${task.priority} ${task.title}`;
}

function renderCheckpoint(checkpoint: Checkpoint): string {
  const lines = [
    `### Checkpoint ${checkpoint.seq} · ${checkpoint.state} · ${checkpoint.author} · ${iso(checkpoint.createdAt)}`,
    `Summary: ${checkpoint.summary}`,
    `Intent: ${checkpoint.intent}`,
    `Next action: ${checkpoint.nextAction}`,
  ];
  if (checkpoint.blockers.length) lines.push(`Blockers: ${checkpoint.blockers.join("; ")}`);
  if (checkpoint.validations.length) {
    lines.push(`Validations: ${checkpoint.validations.join("; ")}`);
  }
  const repo = [checkpoint.repoPath, checkpoint.branch, checkpoint.headSha]
    .filter(Boolean)
    .join(" · ");
  if (repo) lines.push(`Repository: ${repo}`);
  if (checkpoint.dirtySummary) lines.push(`Working tree: ${checkpoint.dirtySummary}`);
  return lines.join("\n");
}

function renderNote(note: TaskNote): string {
  return `- ${iso(note.createdAt)} · ${note.kind} · ${note.author}: ${note.body}`;
}

function renderHandoff(handoff: Handoff): string {
  const target = handoff.toAgent ?? "next compatible agent";
  const acceptance = handoff.acceptedBy
    ? `Accepted by: ${handoff.acceptedBy} · ${iso(handoff.acceptedAt)}`
    : "Accepted by: (pending)";
  return [
    `### Handoff ${handoff.id} · ${handoff.status} · ${handoff.reason}`,
    `From: ${handoff.fromAgent} · To: ${target}`,
    `Summary: ${handoff.summary}`,
    `Intent: ${handoff.intent}`,
    `Next action: ${handoff.nextAction}`,
    acceptance,
  ].join("\n");
}

function clip(value: string, max: number): string {
  if (value.length <= max) return value;
  return `${value.slice(0, Math.max(0, max - 15))}…[truncated]`;
}

export function contextPacket(
  store: KanbanStore,
  taskID: string,
  limits: { notes?: number; checkpoints?: number; handoffs?: number } = {},
): ContextPacket {
  return {
    task: store.requireTask(taskID),
    ancestors: store.ancestors(taskID),
    dependencies: store.dependencies(taskID),
    claim: store.getClaim(taskID),
    notes: store.notes(taskID, limits.notes ?? 50),
    checkpoints: store.checkpoints(taskID, limits.checkpoints ?? 10),
    handoffs: store.handoffs({ taskID, limit: limits.handoffs ?? 10 }).reverse(),
    generatedAt: Date.now(),
    truncated: false,
  };
}

// Cold-start context is bounded by dropping oldest history first. Task identity,
// spec, current lease, dependency state, and the newest checkpoint are never
// dropped: those are the minimum safe handoff contract.
export function renderContext(packet: ContextPacket, maxChars = 20_000): string {
  if (!Number.isInteger(maxChars) || maxChars < 1_000) {
    throw new Error("maxChars must be an integer >= 1000");
  }

  const task = packet.task;
  const fixed = [
    "# Kanban cold-start context",
    "",
    `Generated: ${iso(packet.generatedAt)}`,
    "",
    "## Current task",
    `ID: ${task.id}`,
    `Type: ${task.type}`,
    `Status: ${task.status}`,
    `Priority: ${task.priority}`,
    `Title: ${task.title}`,
    `Body: ${task.body ?? "(none)"}`,
    "",
    "## Claim",
    packet.claim
      ? `${packet.claim.agentID} · session ${packet.claim.sessionID ?? "-"} · expires ${iso(packet.claim.expiresAt)}`
      : "unclaimed",
    "",
    "## Ancestry",
    ...(packet.ancestors.length ? packet.ancestors.map(taskLine) : ["(none)"]),
    "",
    "## Dependencies",
    ...(packet.dependencies.length ? packet.dependencies.map(taskLine) : ["(none)"]),
  ].join("\n");

  const newestCheckpoint = packet.checkpoints.at(-1);
  const newestHandoff = packet.handoffs.at(-1);
  const required = [
    fixed,
    "",
    "## Latest checkpoint",
    newestCheckpoint ? renderCheckpoint(newestCheckpoint) : "(none)",
    "",
    "## Latest handoff",
    newestHandoff ? renderHandoff(newestHandoff) : "(none)",
  ].join("\n");
  if (required.length > maxChars) {
    // Compact layout keeps the operational handoff (especially next action)
    // ahead of lower-value prose when a pathological task/checkpoint exceeds
    // the entire prompt budget.
    const checkpoint = newestCheckpoint;
    const compact = [
      "# Kanban cold-start context (compact)",
      `Task: ${task.id} [${task.status}] ${clip(task.title, 160)}`,
      `Claim: ${packet.claim?.agentID ?? "unclaimed"}`,
      "",
      "## Latest checkpoint",
      `Next action: ${clip(checkpoint?.nextAction ?? "(none)", 240)}`,
      `Intent: ${clip(checkpoint?.intent ?? "(none)", 180)}`,
      `Summary: ${clip(checkpoint?.summary ?? "(none)", 180)}`,
      `Blockers: ${clip(checkpoint?.blockers.join("; ") || "(none)", 100)}`,
      `Validations: ${clip(checkpoint?.validations.join("; ") || "(none)", 100)}`,
      `Handoff: ${clip(newestHandoff ? `${newestHandoff.id} ${newestHandoff.status} from ${newestHandoff.fromAgent}; next ${newestHandoff.nextAction}` : "(none)", 240)}`,
      "",
      `Dependencies: ${clip(packet.dependencies.map((d) => `${d.id}:${d.status}`).join(", ") || "(none)", 140)}`,
      `Body: ${clip(task.body ?? "(none)", Math.max(80, Math.floor(maxChars / 4)))}`,
      "",
      "[context compacted: full durable history remains in SQLite]",
    ].join("\n");
    return compact.slice(0, maxChars);
  }

  const olderCheckpoints = packet.checkpoints.slice(0, -1).reverse();
  const newestNotes = [...packet.notes].reverse();
  const optional: string[] = [];
  let truncated = packet.truncated;

  for (const checkpoint of olderCheckpoints) {
    const section = `\n\n${renderCheckpoint(checkpoint)}`;
    if (required.length + optional.join("").length + section.length > maxChars) {
      truncated = true;
      break;
    }
    optional.unshift(section);
  }

  let text = [required, "", "## Earlier checkpoints", optional.length ? optional.join("") : "(none)"]
    .join("\n")
    .trimEnd();
  const noteLines: string[] = [];
  for (const note of newestNotes) {
    const line = `\n${renderNote(note)}`;
    const trailer = truncated ? "\n\n[older history omitted]" : "";
    if (text.length + "\n\n## Recent notes".length + noteLines.join("").length + line.length + trailer.length > maxChars) {
      truncated = true;
      break;
    }
    noteLines.unshift(line);
  }
  text += `\n\n## Recent notes\n${noteLines.length ? noteLines.join("").trimStart() : "(none)"}`;
  if (truncated) {
    const marker = "\n\n[older history omitted]";
    if (text.length + marker.length > maxChars) {
      text = text.slice(0, maxChars - marker.length).trimEnd();
    }
    text += marker;
  }
  return text;
}

export function renderTodo(store: KanbanStore): string {
  const name = store.boardName() ?? "Kanban";
  const tasks = store.listTasks();
  const active = tasks.filter((task) => !["done", "cancelled"].includes(task.status));
  const done = tasks.filter((task) => task.status === "done");
  const lines = [
    `# ${name} — generated TODO`,
    "",
    "> Projection only. SQLite is authoritative; do not edit this file as state.",
    "",
    "## Restart here",
  ];

  const inProgress = active.filter((task) => task.status === "in_progress");
  if (!inProgress.length) lines.push("", "No task is currently in progress.");
  for (const task of inProgress) {
    lines.push("", `### ${task.id} — ${task.title}`);
    const checkpoint = store.checkpoints(task.id, 1).at(-1);
    const claim = store.getClaim(task.id);
    lines.push(`- Owner: ${claim?.agentID ?? "unclaimed"}`);
    lines.push(`- Next: ${checkpoint?.nextAction ?? "No checkpoint yet"}`);
    if (checkpoint?.blockers.length) lines.push(`- Blockers: ${checkpoint.blockers.join("; ")}`);
  }

  for (const status of ["blocked", "review", "todo", "backlog"] as const) {
    const group = active.filter((task) => task.status === status);
    lines.push("", `## ${status.replace("_", " ")}`);
    if (!group.length) lines.push("", "(none)");
    else lines.push("", ...group.map(taskLine));
  }
  lines.push("", "## Recently done", "", ...(done.slice(-20).reverse().map(taskLine) || []));
  if (!done.length) lines.push("(none)");
  return `${lines.join("\n")}\n`;
}
