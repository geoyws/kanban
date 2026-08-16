# Kanban working agreements

## Runtime and package management

- Kanban's production runtime is Rust. Use `cargo build`, `cargo test`,
  `cargo fmt`, and `cargo clippy`; `Cargo.lock` is authoritative.
- The installed `kanban` command must execute the compiled Rust binary.
- Do not add Bun, Node, npm, pnpm, Yarn, or Corepack runtime dependencies.
- Every release gate must spawn the compiled binary across real process
  boundaries; in-process domain tests do not count as E2E evidence.
- See [ADR-006](docs/adr/ADR-006-rust-runtime-and-compiled-binary-e2e.md).

## Agent topology

- Keep the atmux roster to exactly two human-operated drivers: `driver` on the repository trunk and `driver-2` in `.atmux/worktrees/driver-2`.
- Run Codex in both driver panes. Do not replace either driver with Claude.
- Keep `members` empty. Use harness-native Codex subagents for delegation and parallel work; do not create persistent lead, planner, docs, reviewer, gitter, or specialist tmux panes.
- Preserve this topology when starting, restoring, or reconfiguring the Kanban team. See [ADR-002](docs/adr/ADR-002-two-codex-drivers-harness-subagents.md).
