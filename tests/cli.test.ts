import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dirs: string[] = [];

afterEach(() => {
  while (dirs.length) rmSync(dirs.pop()!, { recursive: true, force: true });
});

async function cli(cwd: string, dataDir: string, args: string[]) {
  const proc = Bun.spawn([process.execPath, join(import.meta.dir, "..", "src", "cli.ts"), ...args], {
    cwd,
    env: { ...process.env, KANBAN_DATA_DIR: dataDir },
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { stdout, stderr, exitCode };
}

describe("CLI vertical slice", () => {
  it("initializes, adds, claims, checkpoints, and resumes through a new process", async () => {
    const root = mkdtempSync(join(tmpdir(), "kanban-cli-"));
    dirs.push(root);
    const workspace = join(root, "workspace");
    const data = join(root, "data");
    mkdirSync(workspace);

    expect((await cli(workspace, data, ["init", "--name", "Demo"])).exitCode).toBe(0);
    expect(
      (await cli(workspace, data, ["task", "add", "Long task", "--id", "t-demo", "--json"]))
        .stdout,
    ).toContain('"id": "t-demo"');

    const claimed = await cli(workspace, data, ["claim", "t-demo", "--as", "deepseek", "--json"]);
    expect(claimed.exitCode).toBe(0);
    const lease = (JSON.parse(claimed.stdout) as { leaseToken: string }).leaseToken;

    const checkpoint = await cli(workspace, data, [
      "checkpoint",
      "t-demo",
      "--lease",
      lease,
      "--as",
      "deepseek",
      "--summary",
      "Inspected the project",
      "--intent",
      "Continue implementation",
      "--next-action",
      "Run tests",
      "--validation",
      "typecheck pending",
    ]);
    expect(checkpoint.exitCode).toBe(0);

    const context = await cli(workspace, data, ["context", "t-demo"]);
    expect(context.exitCode).toBe(0);
    expect(context.stdout).toContain("Inspected the project");
    expect(context.stdout).toContain("Next action: Run tests");
  });

  it("attaches a worktree and performs a handoff through separate CLI processes", async () => {
    const root = mkdtempSync(join(tmpdir(), "kanban-handoff-cli-"));
    dirs.push(root);
    const main = join(root, "main");
    const worktree = join(root, "worktree");
    const data = join(root, "data");
    mkdirSync(main);
    mkdirSync(worktree);

    expect((await cli(main, data, ["init", "--name", "Demo"])).exitCode).toBe(0);
    expect(
      (await cli(worktree, data, ["workspace", "attach", "--to", main])).exitCode,
    ).toBe(0);
    expect(
      (
        await cli(main, data, [
          "task",
          "add",
          "Transfer me",
          "--id",
          "t-transfer",
          "--lane",
          "be",
          "--assignee",
          "outgoing",
          "--driver-only",
        ])
      ).exitCode,
    ).toBe(0);
    const updated = await cli(main, data, [
      "task",
      "update",
      "t-transfer",
      "--as",
      "operator",
      "--deliverable",
      "src/adapter.ts",
      "--stale-minutes",
      "30",
      "--json",
    ]);
    expect(updated.exitCode).toBe(0);
    expect(updated.stdout).toContain('"lane": "be"');
    expect(updated.stdout).toContain('"deliverable": "src/adapter.ts"');
    const claimed = await cli(worktree, data, [
      "claim",
      "t-transfer",
      "--as",
      "outgoing",
      "--caller-scope",
      "driver",
      "--json",
    ]);
    const lease = (JSON.parse(claimed.stdout) as { leaseToken: string }).leaseToken;
    const created = await cli(worktree, data, [
      "handoff",
      "create",
      "t-transfer",
      "--lease",
      lease,
      "--as",
      "outgoing",
      "--summary",
      "Token budget is low",
      "--intent",
      "Resume in a fresh session",
      "--next-action",
      "Run the remaining tests",
      "--json",
    ]);
    expect(created.exitCode).toBe(0);
    const handoffID = (JSON.parse(created.stdout) as { id: string }).id;

    const accepted = await cli(main, data, [
      "handoff",
      "accept",
      handoffID,
      "--as",
      "incoming",
      "--caller-scope",
      "driver",
      "--json",
    ]);
    expect(accepted.exitCode).toBe(0);
    expect(accepted.stdout).toContain('"agentID": "incoming"');

    const dashboard = await cli(main, data, ["dashboard", "--json"]);
    expect(dashboard.exitCode).toBe(0);
    expect(dashboard.stdout).toContain('"workspaceRoots"');
    expect(dashboard.stdout).toContain(worktree);

    const doctor = await cli(main, data, ["doctor", "--json"]);
    expect(doctor.exitCode).toBe(0);
    expect(doctor.stdout).toContain('"healthy": true');

    const backupDir = join(root, "backup");
    const backup = await cli(main, data, ["backup", "--output", backupDir, "--json"]);
    expect(backup.exitCode).toBe(0);
    expect(backup.stdout).toContain(backupDir);
  });
});
