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
});
