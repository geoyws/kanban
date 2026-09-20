# Kanban working agreements

## Runtime and package management

- Kanban's production runtime is Rust. Use `cargo build`, `cargo test`,
  `cargo fmt`, and `cargo clippy`; `Cargo.lock` is authoritative.
- The installed `kanban` command must execute the compiled Rust binary.
- Do not add Bun, Node, npm, pnpm, Yarn, or Corepack runtime dependencies.
- Every release gate must spawn the compiled binary across real process
  boundaries; in-process domain tests do not count as E2E evidence.
- See [ADR-006](docs/adr/ADR-006-rust-runtime-and-compiled-binary-e2e.md).

## Specification before implementation

- A change to Kanban product behaviour runs `/quality spec` before
  implementation: a new or edited `docs/specs/<slice>.md`, which the gate exits
  `SPEC-READY` or `BLOCKED`. Product behaviour means a new route, verb,
  subcommand or field; changed served markup or a changed CLI contract; changed
  refusal wording; or a board schema migration.
- Specification, tests and code land in the same change, and so does the
  requirement's row in `docs/testing/compiled-rust-e2e-matrix.md` — one trace,
  never a second one.
- An unmet mandatory requirement blocks acceptance. Acceptance or release is
  `PASS`/`GO` only once every in-scope mandatory requirement has adequate,
  current evidence; the only alternative is an owner-authorised scope change
  recorded against the original requirement and baseline with traceable
  history, after which the remaining mandatory requirements are re-verified. A
  requirement is never quietly dropped or rewritten.
- Proportionality, in the same breath: a bounded fix inside an
  already-specified slice needs only a requirement delta plus one acceptance
  example in that slice's existing specification — a bug fix restoring what a
  requirement already says, a copy fix inside existing wording, or a test-only
  change. Which side a change falls on is
  [docs/specs/README.md](docs/specs/README.md) §"When a specification is
  required, and when a delta is enough"; four new documents for a typo is a
  misreading of this section.
- Reviewing for mandatory obligations is not grepping for the literal word
  `MUST`. Equivalent normative language counts, read together with the
  requirement strengths the specification declares.
- `SPEC-READY` is specification readiness and nothing else: it authorises
  neither implementation, nor rollout, nor release. Product readiness stays
  `/quality`, then `/tidy`, then the served-tier receipt (ADR-044, ADR-045 §4).
- The rollout is slices `WEB` and `SPA` and nothing else, and nothing is
  specified retroactively; adding a slice needs a board row under epic
  `e-c0852fe7` with George's approval.
- The decision is
  [ADR-047](docs/adr/ADR-047-kanban-adopts-specification-driven-development.md);
  the formats, the ID rules and the trace convention are written out in
  [docs/specs/README.md](docs/specs/README.md). Read those two rather than
  re-deriving them here; where they disagree, the ADR wins.

## The kb skill submodule

- `skills/kb` is a pinned submodule of the public package
  [`geoyws/kb-skill`](https://github.com/geoyws/kb-skill). Initialize it
  path-specifically before building, never recursively:
  `git submodule update --init skills/kb`.
- The build needs it: the alias-drift test in `rust/lib.rs` does
  `include_str!("../skills/kb/SKILL.md")`, so an uninitialized submodule fails
  to compile rather than silently skipping the check.
- The gate runs the package's own wrapper tests from the submodule:
  `bash skills/kb/tests/kb-wrapper-tests.sh`.
- Host routing is consumer-supplied so the package stays estate-neutral. Point
  `KB_HOSTS_TABLE` at `skills/hosts.tsv`; an unknown board fails closed.
- Edit the skill in the package and bump the gitlink here; do not edit
  `skills/kb` in place. See
  [ADR-036](docs/adr/ADR-036-the-kb-skill-is-a-pinned-submodule-of-the-public-package.md),
  which supersedes ADR-014.

## Agent topology

- Keep the atmux roster to exactly two human-operated drivers: `driver` on the repository trunk and `driver-2` in `.atmux/worktrees/driver-2`.
- Run Codex in both driver panes. Do not replace either driver with Claude.
- Keep `members` empty. Use harness-native Codex subagents for delegation and parallel work; do not create persistent lead, planner, docs, reviewer, gitter, or specialist tmux panes.
- Preserve this topology when starting, restoring, or reconfiguring the Kanban team. See [ADR-002](docs/adr/ADR-002-two-codex-drivers-harness-subagents.md).
