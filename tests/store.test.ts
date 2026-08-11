import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { contextPacket, renderContext, renderTodo } from "../src/context.js";
import { Registry } from "../src/registry.js";
import { KanbanStore } from "../src/store.js";

const dirs: string[] = [];
const stores: KanbanStore[] = [];

function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "kanban-test-"));
  dirs.push(dir);
  return dir;
}

function makeStore(now?: () => number): KanbanStore {
  const store = new KanbanStore(join(tempDir(), "board.db"), now ? { now } : {});
  store.initialize("Test board");
  stores.push(store);
  return store;
}

afterEach(() => {
  while (stores.length) stores.pop()!.close();
  while (dirs.length) rmSync(dirs.pop()!, { recursive: true, force: true });
});

describe("task graph and atomic claims", () => {
  it("claims only dependency-ready work in priority order", () => {
    const store = makeStore();
    const foundation = store.addTask({ id: "t-base", title: "Foundation", priority: 2 });
    store.addTask({ id: "t-blocked", title: "Blocked", priority: 1, dependencies: [foundation.id] });
    store.addTask({ id: "t-ready", title: "Ready", priority: 3 });

    const first = store.claim(undefined, { agentID: "worker-a" });
    expect(first.taskID).toBe("t-base");
    expect(store.requireTask("t-base").status).toBe("in_progress");

    const second = store.claim(undefined, { agentID: "worker-b" });
    expect(second.taskID).toBe("t-ready");
    expect(() => store.claim("t-blocked", { agentID: "worker-c" })).toThrow(/unmet dependencies/);
  });

  it("prevents double claims across database connections", () => {
    const dir = tempDir();
    const path = join(dir, "board.db");
    const one = new KanbanStore(path);
    const two = new KanbanStore(path);
    stores.push(one, two);
    one.initialize("shared");
    one.addTask({ id: "t-one", title: "Only task" });
    one.claim("t-one", { agentID: "one" });
    expect(() => two.claim("t-one", { agentID: "two" })).toThrow(/claimed by one/);
  });

  it("allows takeover after lease expiry and rejects the stale token", () => {
    let now = 1_000;
    const store = makeStore(() => now);
    store.addTask({ id: "t-one", title: "Long task" });
    const stale = store.claim("t-one", { agentID: "old", leaseMs: 1_000 });
    now = 2_001;
    const replacement = store.claim("t-one", { agentID: "new", leaseMs: 1_000 });
    expect(replacement.agentID).toBe("new");
    expect(() =>
      store.checkpoint({
        taskID: "t-one",
        leaseToken: stale.leaseToken,
        author: "old",
        summary: "stale",
        intent: "stale",
        nextAction: "stale",
      }),
    ).toThrow(/invalid lease/);
  });

  it("reopens blocked work through an audited transition", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Blocked work", status: "blocked" });
    expect(() => store.claim("t-one", { agentID: "worker" })).toThrow(/not claimable/);
    store.moveTask("t-one", "todo", "operator");
    expect(store.claim("t-one", { agentID: "worker" }).agentID).toBe("worker");
  });
});

describe("durable continuity", () => {
  it("appends notes and checkpoints without rewriting history", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Build it", body: "Acceptance criteria" });
    const claim = store.claim("t-one", { agentID: "deepseek", sessionID: "s-1" });
    store.addNote("t-one", "deepseek", "plan", "Inspect the current architecture");
    store.addNote("t-one", "deepseek", "evidence", "Found the persistence seam");
    const checkpoint = store.checkpoint({
      taskID: "t-one",
      leaseToken: claim.leaseToken,
      author: "deepseek",
      sessionID: "s-1",
      model: "deepseek/reasoner",
      summary: "Mapped the architecture",
      intent: "Implement the store before the adapter",
      nextAction: "Write migration tests",
      validations: ["schema opened under WAL"],
      repoPath: "/work/repo",
      branch: "main",
      headSha: "abc123",
      dirtySummary: "M src/store.ts",
    });

    expect(store.notes("t-one").map((note) => note.kind)).toEqual(["plan", "evidence"]);
    expect(checkpoint.nextAction).toBe("Write migration tests");
    expect(store.getClaim("t-one")?.leaseToken).toBe(claim.leaseToken);
  });

  it("atomically finishes a task and releases its lease", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Finish" });
    const claim = store.claim("t-one", { agentID: "worker" });
    store.checkpoint({
      taskID: "t-one",
      leaseToken: claim.leaseToken,
      author: "worker",
      state: "done",
      summary: "Complete",
      intent: "Close with evidence",
      nextAction: "No further action",
      validations: ["bun test: pass"],
    });
    expect(store.requireTask("t-one").status).toBe("done");
    expect(store.getClaim("t-one")).toBeNull();
  });

  it("persists state across process-style reopen", () => {
    const dir = tempDir();
    const path = join(dir, "board.db");
    const first = new KanbanStore(path);
    first.initialize("Persistent");
    first.addTask({ id: "t-one", title: "Survive restart" });
    first.close();

    const second = new KanbanStore(path);
    stores.push(second);
    expect(second.boardName()).toBe("Persistent");
    expect(second.requireTask("t-one").title).toBe("Survive restart");
  });
});

describe("cold-start projections", () => {
  it("keeps the newest checkpoint in bounded context and drops old history first", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Resume safely", body: "Do the durable thing" });
    const claim = store.claim("t-one", { agentID: "worker" });
    for (let i = 0; i < 30; i++) {
      store.addNote("t-one", "worker", "progress", `historical note ${i} ${"x".repeat(80)}`);
    }
    store.checkpoint({
      taskID: "t-one",
      leaseToken: claim.leaseToken,
      author: "worker",
      summary: "Important latest summary",
      intent: "Preserve the continuity contract",
      nextAction: "Run the exact verification command",
      validations: ["unit suite pending"],
    });

    const text = renderContext(contextPacket(store, "t-one"), 1_800);
    expect(text.length).toBeLessThanOrEqual(1_800);
    expect(text).toContain("Run the exact verification command");
    expect(text).toContain("[older history omitted]");
  });

  it("renders TODO as an explicitly non-authoritative restart view", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Visible task" });
    const claim = store.claim("t-one", { agentID: "worker" });
    store.checkpoint({
      taskID: "t-one",
      leaseToken: claim.leaseToken,
      author: "worker",
      summary: "Started",
      intent: "Continue",
      nextAction: "Write the adapter",
    });
    const todo = renderTodo(store);
    expect(todo).toContain("Projection only. SQLite is authoritative");
    expect(todo).toContain("Write the adapter");
  });
});

describe("workspace registry", () => {
  it("resolves nested paths without writing state into the managed repository", () => {
    const root = tempDir();
    const workspace = join(root, "project");
    const nested = join(workspace, "src", "nested");
    mkdirSync(nested, { recursive: true });
    const registry = new Registry(join(root, "data"));
    try {
      const registered = registry.register(workspace, "Project");
      expect(registry.resolve(nested)?.boardPath).toBe(registered.boardPath);
      expect(registered.boardPath.startsWith(join(root, "data"))).toBe(true);
    } finally {
      registry.close();
    }
  });
});
