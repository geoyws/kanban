# ADR-019: Global rules frame every project on claim and resume

**Status:** Accepted
**Date:** 2026-08-24
**Deciders:** George

## Context

ADR-018 made project rules unavoidable at the two work boundaries that matter:
a new claim and a resumed context packet. Some constraints are not project
specific. Copying those rules into every board makes thirteen working copies
which can drift, retire at different times, and disagree about what is global.

Injecting complete rule bodies into every command would solve visibility by
making ordinary ledger traffic expensive. It would also change every CLI and
MCP response shape, including discovery commands that establish no work.

## Decision

Kanban has one global rules document stored in `registry.db`. Global rows use
`g-` identifiers and the same ordered, audited, retire-only lifecycle as project
rules. They are managed with the existing verbs plus explicit scope:

```bash
kb r new "Never store credentials in Kanban." --global --as geo
kb r ls --global
kb r cat g-12345678 --global
kb r up g-12345678 --global --body "Never store secrets in Kanban." --as geo
kb rule retire g-12345678 --global --as geo
kb ev --global --rule g-12345678
```

`--global` and an explicit board selector are refused together: one names the
registry-wide document and the other names a project board. Global rules are
stored once and are never copied into project databases.

Every successful new claim (including a task handoff acceptance), every session
handoff acceptance and every `kb context` packet carries the complete effective
table of contents: global rules first, then the addressed project's rules. Each
summary carries `scope`, id, first-line headline, byte size and whether more text
exists. Rendered context uses compact `[g]` and `[p]` markers. Bodies remain lazy
behind `rule show`; a short one-line rule needs no second fetch because its
headline is its complete body.

Rules are not injected into every command. `task list` and similar operations
are discovery, not a declaration that an agent is starting or resuming work.
Wrapping every JSON result would spend tokens repeatedly and break the existing
CLI/MCP contract. The `/kb` workflow requires agents to claim work or read its
context before acting; those are the injection points.

The web board renders global and project rules as separate escaped sections.
Global rules remain plaintext operational state: long material, secrets and
cross-machine sources of truth still belong in the versioned/git-crypt'd
dotfiles.

## Consequences

- One edit changes the inherited constraint for every project without drift.
- An agent following the claim/resume workflow receives the rules automatically.
- Context cost is one compact summary per active rule, not repeated full bodies.
- A caller that bypasses claim/context also bypasses automatic rule injection;
  that is a workflow violation rather than a reason to mutate every response.
- Registry backups now carry global rules and their audit trail automatically.

## Verification

Compiled-binary E2E coverage creates two registered projects, adds one global
and one local rule per project, and proves both claims and contexts receive the
same global row followed by the correct local row. It also opens both board
databases and proves the global id was copied into neither, verifies preserved
revision text and retirement, and drives the real HTTP server to prove escaped
global rendering. Unit/in-process checks are not substituted for this E2E.

## References

- [ADR-007](ADR-007-global-project-addressing.md)
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md)
- [ADR-018](ADR-018-project-rules-frame-work-without-replacing-private-memory.md)
