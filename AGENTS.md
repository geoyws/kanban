# Kanban working agreements

## Runtime and package management

- Use Bun only for this project.
- Use `bun install`, `bun run`, `bun test`, `bunx`, and the `#!/usr/bin/env bun` runtime.
- Do not invoke `node`, `npm`, `npx`, `pnpm`, `yarn`, or Corepack for project work.
- Do not add `package-lock.json`, `pnpm-lock.yaml`, or `yarn.lock`; `bun.lock` is the only package-manager lockfile.
- Node-compatible standard-library imports are allowed when executed by Bun.
