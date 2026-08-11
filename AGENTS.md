# Kanban working agreements

## Runtime and package management

- Use Bun only for this project.
- Use `bun install`, `bun run`, `bun test`, `bunx`, and the `#!/usr/bin/env bun` runtime.
- Do not invoke `node`, `npm`, `npx`, `pnpm`, `yarn`, or Corepack for project work.
- Do not add `package-lock.json`, `pnpm-lock.yaml`, or `yarn.lock`; `bun.lock` is the only package-manager lockfile.
- Node-compatible standard-library imports are allowed when executed by Bun.

## Agent topology

- Keep the atmux roster to exactly two human-operated drivers: `driver` on the repository trunk and `driver-2` in `.atmux/worktrees/driver-2`.
- Run Codex in both driver panes. Do not replace either driver with Claude.
- Keep `members` empty. Use harness-native Codex subagents for delegation and parallel work; do not create persistent lead, planner, docs, reviewer, gitter, or specialist tmux panes.
- Preserve this topology when starting, restoring, or reconfiguring the Kanban team. See [ADR-002](docs/adr/ADR-002-two-codex-drivers-harness-subagents.md).
