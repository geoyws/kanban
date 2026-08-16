import { afterEach, describe, expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, mkdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { contextPacket, renderContext, renderTodo } from "../src/context.js";
import { importAtmuxJson, importAtmuxSqlite, importAtmuxTasks } from "../src/import-atmux.js";
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
  it("updates task parents while preventing hierarchy cycles", () => {
    const store = makeStore();
    store.addTask({ id: "e-one", type: "epic", title: "One" });
    store.addTask({ id: "s-two", type: "story", parentID: "e-one", title: "Two" });
    store.addTask({ id: "t-three", parentID: "s-two", title: "Three" });

    expect(store.updateTask("t-three", { parentID: "e-one" }, "operator").parentID).toBe("e-one");
    expect(store.updateTask("t-three", { parentID: null }, "operator").parentID).toBeNull();
    expect(() => store.updateTask("e-one", { parentID: "s-two" }, "operator")).toThrow(
      /create a cycle/,
    );
  });

  it("patches metadata atomically and treats null as deletion", () => {
    const store = makeStore();
    store.addTask({ id: "e-one", type: "epic", title: "One", metadata: { keep: true, drop: 1 } });

    expect(
      store.patchTaskMetadata("e-one", { workflowStatus: "ready", drop: null }, "operator").metadata,
    ).toEqual({ keep: true, workflowStatus: "ready" });
  });

  it("moves status and workflow metadata in one transaction", () => {
    const store = makeStore();
    store.addTask({ id: "e-one", type: "epic", title: "One" });

    const moved = store.moveTask("e-one", "review", "operator", { workflowStatus: "review" });
    expect(moved.status).toBe("review");
    expect(moved.metadata.workflowStatus).toBe("review");
  });

  it("advances story workflow, dispatches atomically, and enforces gates", () => {
    const store = makeStore();
    store.addTask({
      id: "e-one",
      type: "epic",
      title: "Epic",
      status: "todo",
      metadata: { workflowStatus: "ready" },
    });
    store.addTask({
      id: "s-one",
      type: "story",
      parentID: "e-one",
      title: "Story",
      status: "backlog",
      metadata: { workflowStatus: "planning", mergeMode: "feature-branch" },
    });
    store.addTask({ id: "t-dev", parentID: "s-one", title: "Develop", lane: "be" });
    store.addTask({ id: "t-test", parentID: "s-one", title: "Test", lane: "test" });

    expect(store.advanceStory("s-one", { actor: "driver" }).to).toBe("ready");
    expect(store.advanceStory("s-one", { actor: "driver" }).parentEpicFlipped).toBe(true);
    expect(store.requireTask("e-one").metadata.workflowStatus).toBe("in-progress");
    expect(() => store.advanceStory("s-one", { actor: "driver" })).toThrow(/t-dev/);
    store.moveTask("t-dev", "done", "worker");
    expect(store.advanceStory("s-one", { actor: "driver" }).to).toBe("testing");
    expect(() => store.advanceStory("s-one", { actor: "driver", reviewer: "reviewer" })).toThrow(
      /t-test/,
    );
    store.moveTask("t-test", "done", "tester");
    const review = store.advanceStory("s-one", { actor: "driver", reviewer: "reviewer" });
    expect(review.to).toBe("review");
    expect(store.requireTask(review.dispatchedTaskID!).assignee).toBe("reviewer");

    store.signoffStory("s-one", "reviewer", "looks good");
    store.unsignoffStory("s-one", "reviewer", "recheck");
    store.signoffStory("s-one", "reviewer");
    const merging = store.advanceStory("s-one", { actor: "driver", committer: "committer" });
    expect(merging.to).toBe("merging");
    expect(store.requireTask(merging.dispatchedTaskID!).assignee).toBe("committer");
    store.moveTask(merging.dispatchedTaskID!, "done", "committer");
    expect(store.advanceStory("s-one", { actor: "driver" }).to).toBe("done");
  });

  it("updates atmux-compatible routing fields and dependencies atomically", () => {
    const store = makeStore();
    store.addTask({ id: "t-base", title: "Base" });
    store.addTask({ id: "t-next", title: "Next" });

    const updated = store.updateTask(
      "t-next",
      {
        assignee: "driver-2",
        lane: "be",
        deliverable: "src/adapter.ts",
        staleMinutes: 45,
        driverOnly: true,
        priority: 1,
        dependencies: ["t-base"],
      },
      "operator",
    );

    expect(updated.assignee).toBe("driver-2");
    expect(updated.lane).toBe("be");
    expect(updated.deliverable).toBe("src/adapter.ts");
    expect(updated.staleMinutes).toBe(45);
    expect(updated.driverOnly).toBe(true);
    expect(store.dependencies("t-next").map((task) => task.id)).toEqual(["t-base"]);
    expect(() =>
      store.updateTask("t-base", { dependencies: ["t-next"] }, "operator"),
    ).toThrow(/create a cycle/);
    expect(store.dependencies("t-base")).toEqual([]);
  });

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

  it("selects next work by assignee, lane, role, and driver scope", () => {
    const store = makeStore();
    store.addTask({ id: "t-fe", title: "Frontend", lane: "fe", priority: 2 });
    store.addTask({ id: "t-be", title: "Backend", lane: "be", priority: 1 });
    store.addTask({ id: "t-free", title: "General", priority: 3 });
    store.addTask({
      id: "t-driver",
      title: "Driver decision",
      lane: "ops",
      driverOnly: true,
      priority: 0,
    });
    store.addTask({
      id: "t-other",
      title: "Owned elsewhere",
      assignee: "worker-b",
      priority: 0,
    });

    expect(
      store.claim(undefined, { agentID: "worker-a", callerLane: "fe" }).taskID,
    ).toBe("t-fe");
    expect(
      store.claim(undefined, { agentID: "worker-a", roleFilter: "be" }).taskID,
    ).toBe("t-be");
    expect(
      store.claim(undefined, {
        agentID: "driver",
        roleFilter: "ops",
        callerScope: "driver",
      }).taskID,
    ).toBe("t-driver");
    expect(() =>
      store.claim("t-other", { agentID: "worker-a" }),
    ).toThrow(/assigned to worker-b/);
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

  it("checks integrity and creates a private recoverable snapshot", () => {
    const root = tempDir();
    const path = join(root, "board.db");
    const backup = join(root, "backup", "board.db");
    const store = new KanbanStore(path);
    stores.push(store);
    store.initialize("Protected");
    store.addTask({ id: "t-one", title: "Back me up" });

    expect(store.integrityCheck()).toEqual(["ok"]);
    expect(statSync(path).mode & 0o777).toBe(0o600);
    store.backup(backup);
    expect(statSync(backup).mode & 0o777).toBe(0o600);
    expect(() => store.backup(backup)).toThrow(/already exists/);

    const restored = new KanbanStore(backup);
    stores.push(restored);
    expect(restored.integrityCheck()).toEqual(["ok"]);
    expect(restored.requireTask("t-one").title).toBe("Back me up");
  });
});

describe("agent handoffs", () => {
  it("atomically checkpoints, releases, and transfers a token-pressure handoff", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Continue across agents" });
    const outgoing = store.claim("t-one", { agentID: "agent-old", sessionID: "turn-1" });

    const handoff = store.createHandoff({
      taskID: "t-one",
      leaseToken: outgoing.leaseToken,
      fromAgent: "agent-old",
      fromSession: "turn-1",
      reason: "token_pressure",
      summary: "Implemented the schema",
      intent: "Keep the migration append-only",
      nextAction: "Run the handoff acceptance test",
      validations: ["typecheck passed"],
      repoPath: "/work/repo",
      branch: "main",
      headSha: "abc123",
    });

    expect(handoff.status).toBe("pending");
    expect(store.getClaim("t-one")).toBeNull();
    expect(store.requireTask("t-one").status).toBe("todo");
    expect(store.checkpoints("t-one").at(-1)?.nextAction).toBe(
      "Run the handoff acceptance test",
    );
    expect(() => store.heartbeat("t-one", outgoing.leaseToken)).toThrow(/no active claim/);

    const incoming = store.acceptHandoff(handoff.id, {
      agentID: "agent-new",
      sessionID: "turn-2",
    });
    expect(incoming.handoff.status).toBe("accepted");
    expect(incoming.handoff.acceptedBy).toBe("agent-new");
    expect(incoming.claim.agentID).toBe("agent-new");
    expect(incoming.claim.leaseToken).not.toBe(outgoing.leaseToken);
    expect(store.requireTask("t-one").status).toBe("in_progress");
  });

  it("enforces a named replacement agent", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Targeted handoff" });
    const claim = store.claim("t-one", { agentID: "agent-old" });
    const handoff = store.createHandoff({
      taskID: "t-one",
      leaseToken: claim.leaseToken,
      fromAgent: "agent-old",
      toAgent: "agent-new",
      summary: "Ready to transfer",
      intent: "Preserve continuity",
      nextAction: "Load the context",
    });

    expect(() => store.acceptHandoff(handoff.id, { agentID: "someone-else" })).toThrow(
      /targets agent-new/,
    );
    expect(store.requireHandoff(handoff.id).status).toBe("pending");
    expect(store.getClaim("t-one")).toBeNull();
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

  it("includes the newest handoff in cold-start context", () => {
    const store = makeStore();
    store.addTask({ id: "t-one", title: "Resume from Kanban" });
    const claim = store.claim("t-one", { agentID: "outgoing" });
    store.createHandoff({
      taskID: "t-one",
      leaseToken: claim.leaseToken,
      fromAgent: "outgoing",
      summary: "Tokens are sparse",
      intent: "Switch agents safely",
      nextAction: "Inspect the pending migration diff",
    });
    const text = renderContext(contextPacket(store, "t-one"));
    expect(text).toContain("Latest handoff");
    expect(text).toContain("Tokens are sparse");
    expect(text).toContain("Inspect the pending migration diff");
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
      expect(statSync(join(root, "data")).mode & 0o777).toBe(0o700);
      expect(registry.integrityCheck()).toEqual(["ok"]);
    } finally {
      registry.close();
    }
  });

  it("attaches multiple worktrees to one project board", () => {
    const root = tempDir();
    const main = join(root, "main");
    const worktree = join(root, "driver-2");
    mkdirSync(main);
    mkdirSync(join(worktree, "src"), { recursive: true });
    const registry = new Registry(join(root, "data"));
    try {
      const project = registry.register(main, "Project");
      const attached = registry.attach(worktree, main);
      expect(attached.canonical).toBe(false);
      expect(attached.boardPath).toBe(project.boardPath);
      expect(registry.resolve(join(worktree, "src"))?.boardPath).toBe(project.boardPath);
      expect(registry.projects()[0]?.workspaceRoots).toEqual([main, worktree]);
    } finally {
      registry.close();
    }
  });
});

describe("atmux migration", () => {
  it("imports stable task identities, routing fields, dependencies, and notes", () => {
    const store = makeStore();
    const imported = importAtmuxTasks(
      store,
      [
        {
          id: "t-00000001",
          subject: "Foundation",
          status: "done",
          owner: "driver",
          lane: "be",
          createdAt: 1_700_000_000,
          completedAt: 1_700_000_100,
          note: "Verified in atmux",
        },
        {
          id: "t-00000002",
          subject: "Adapter",
          status: "in-progress",
          owner: "driver-2",
          deps: ["t-00000001"],
          deliverable: "src/adapter.ts",
          staleMin: 45,
          driverOnly: true,
          createdAt: 1_700_000_200,
          customField: "preserved",
        },
      ],
      "operator",
    );

    expect(imported.map((task) => task.id)).toEqual(["t-00000001", "t-00000002"]);
    expect(store.requireTask("t-00000001").createdAt).toBe(1_700_000_000_000);
    expect(store.requireTask("t-00000002").status).toBe("in_progress");
    expect(store.requireTask("t-00000002").assignee).toBe("driver-2");
    expect(store.requireTask("t-00000002").driverOnly).toBe(true);
    expect(store.dependencies("t-00000002").map((task) => task.id)).toEqual(["t-00000001"]);
    expect(store.notes("t-00000001")[0]?.body).toBe("Verified in atmux");
    expect(
      (store.requireTask("t-00000002").metadata.atmuxExtra as Record<string, unknown>)
        .customField,
    ).toBe("preserved");
  });

  it("preserves a missing legacy dependency without creating a corrupt edge", () => {
    const store = makeStore();
    importAtmuxTasks(
      store,
      [{ id: "t-one", subject: "Historical", deps: ["t-missing"] }],
      "operator",
    );
    expect(store.dependencies("t-one")).toEqual([]);
    expect(store.requireTask("t-one").metadata.legacyDanglingDependencies).toEqual([
      "t-missing",
    ]);
  });

  it("preserves anomalous completion timestamps on non-done legacy tasks", () => {
    const store = makeStore();
    importAtmuxTasks(
      store,
      [
        {
          id: "t-active-completed",
          subject: "Still active",
          status: "in-progress",
          completedAt: 1_700_000_100,
        },
      ],
      "operator",
    );

    const imported = store.requireTask("t-active-completed");
    expect(imported.completedAt).toBeNull();
    expect(imported.metadata.legacyCompletedAt).toBe(1_700_000_100_000);
  });

  it("imports a JSON-only atmux hierarchy with parent links intact", () => {
    const store = makeStore();
    const receipt = importAtmuxJson(
      store,
      {
        epics: [
          {
            id: "e-json",
            title: "JSON epic",
            status: "in-progress",
            createdAt: 1_700_000_000,
            isReady: true,
          },
        ],
        stories: [
          {
            id: "s-json",
            epic: "e-json",
            title: "JSON story",
            status: "testing",
            createdAt: 1_700_000_010,
          },
        ],
        tasks: [
          {
            id: "t-json",
            story: "s-json",
            epic: "e-json",
            subject: "JSON task",
            status: "in-progress",
            createdAt: 1_700_000_020,
          },
        ],
      },
      "operator",
    );

    expect(receipt.counts).toEqual({ epics: 1, stories: 1, tasks: 1 });
    expect(store.requireTask("s-json").parentID).toBe("e-json");
    expect(store.requireTask("t-json").parentID).toBe("s-json");
    expect(store.requireTask("e-json").metadata.isReady).toBe(true);
  });

  it("reports nonterminal completion timestamps in JSON import receipts", () => {
    const store = makeStore();
    const receipt = importAtmuxJson(
      store,
      {
        tasks: [
          {
            id: "t-active-completed",
            subject: "Still active",
            status: "todo",
            completedAt: 1_700_000_100,
          },
        ],
      },
      "operator",
    );

    expect(receipt.warnings.nonterminalCompletions).toEqual([
      {
        taskID: "t-active-completed",
        status: "todo",
        completedAt: 1_700_000_100_000,
      },
    ]);
  });

  it("imports an atmux state.db hierarchy without mutating the source", () => {
    const root = tempDir();
    const sourcePath = join(root, "state.db");
    const source = new Database(sourcePath, { create: true, strict: true });
    source.exec(`
      CREATE TABLE epics (
        id TEXT PRIMARY KEY, title TEXT, body TEXT, status TEXT, driver_ref TEXT,
        created_at INTEGER, completed_at INTEGER, stories TEXT, depends_on TEXT,
        is_ready INTEGER, spawned_at INTEGER, extra TEXT
      );
      CREATE TABLE stories (
        id TEXT PRIMARY KEY, epic TEXT, title TEXT, body TEXT, acceptance_criteria TEXT,
        status TEXT, created_at INTEGER, completed_at INTEGER, advanced_at INTEGER,
        review_signoff INTEGER, merge_task_id TEXT, merge_mode TEXT, extra TEXT
      );
      CREATE TABLE tasks (
        id TEXT PRIMARY KEY, subject TEXT, body TEXT, status TEXT, owner TEXT, deps TEXT,
        priority INTEGER, epic TEXT, story TEXT, lane TEXT, deliverable TEXT,
        stale_min INTEGER, driver_only INTEGER, created_at INTEGER, claimed_at INTEGER,
        completed_at INTEGER, claimed_from TEXT, created_from TEXT, note TEXT, extra TEXT
      );
      INSERT INTO epics VALUES (
        'e-1','Migration epic','Body','in-progress','main',1700000000,NULL,
        '["s-1"]','[]',1,NULL,'{"autoSpawn":{"enabled":false}}'
      );
      INSERT INTO stories VALUES (
        's-1','e-1','Adapter story','Story body','Must preserve IDs','testing',
        1700000010,NULL,1700000020,0,NULL,'feature-branch','{}'
      );
      INSERT INTO tasks VALUES (
        't-1','Port adapter','Task body','done','driver','[]',1,'e-1','s-1','be',
        'src/adapter.ts',30,1,1700000030,1700000040,1700000050,NULL,NULL,
        'Verified','{"custom":"kept"}'
      );
    `);
    source.close();
    const before = statSync(sourcePath).size;
    const store = makeStore();

    const receipt = importAtmuxSqlite(store, sourcePath, "operator", 1_800_000_000_000);

    expect(receipt.counts).toEqual({ epics: 1, stories: 1, tasks: 1 });
    expect(store.requireTask("e-1").type).toBe("epic");
    expect(store.requireTask("s-1").parentID).toBe("e-1");
    expect(store.requireTask("s-1").metadata.workflowStatus).toBe("testing");
    expect(store.requireTask("t-1").parentID).toBe("s-1");
    expect(store.requireTask("t-1").driverOnly).toBe(true);
    expect(store.notes("t-1")[0]?.body).toBe("Verified");
    expect(statSync(sourcePath).size).toBe(before);
  });
});
