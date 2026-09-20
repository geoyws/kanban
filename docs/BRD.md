# Kanban business requirements

**Status:** Active
**Owner:** George (`geoyws`)
**Date:** 2026-09-20
**Scope:** the product as a whole. There is exactly one of this document
([ADR-047](adr/ADR-047-kanban-adopts-specification-driven-development.md) §1).

This document states why Kanban exists, whose problem it solves, what counts as
a good outcome, and where the business boundary of the product runs. It decides
nothing about design and describes no user journey: architecture decisions live
in `docs/adr/`, product behaviour and workflows live in [docs/PRD.md](PRD.md),
and requirement-level obligations live in `docs/specs/`. Every claim below cites
the source it comes from.

## Purpose

Kanban is a durable work ledger for one operator's long-horizon agent work
across many projects and Git worktrees. Its purpose is to hold the plan, the
ownership, the evidence and the resumption contract for that work somewhere that
outlives the process doing it — see `README.md:3`-`README.md:14` for the
one-paragraph product statement and
[ADR-001](adr/ADR-001-durable-agent-work-ledger.md) for the decision that
established it.

Within the operator's estate the ledger also has an ownership role: it is the
single source of truth for work state, and orchestration tools consume it rather
than keep their own copy ([ADR-005](adr/ADR-005-kanban-owns-work-state-atmux-consumes-it.md)).
There is **one ledger, on `hax`, and it owns every board across both estates —
personal and IFCA alike**. `hig` receives byte-identical releases as the release
script's second install target and holds no workspaces (registry rule
`r-7af4dd57`, quoted under Non-goals). A two-registry split putting IFCA's
boards on `hig` was proposed in the dotfiles as ADR-017 and **withdrawn on
2026-09-08**; it is history, not guidance.

## Problem

Three problems create the need, and each is recorded elsewhere rather than
argued here.

1. **Conversation history is not durable coordination state.** An agent can be
   replaced at any moment by token pressure, rate limits, provider failure,
   process restart, or deliberate swarm scheduling; anything held only in its
   context is lost with it ([ADR-001](adr/ADR-001-durable-agent-work-ledger.md)
   Context; `README.md:16`-`README.md:20`).
2. **Two ledgers mean two truths.** Before
   [ADR-005](adr/ADR-005-kanban-owns-work-state-atmux-consumes-it.md), work state
   existed in both atmux and this repository, which made neither authoritative
   and locked the board to one harness.
3. **Approvals were the bottleneck, and the bottleneck was measured.** On the day
   the web surface shipped there were **75 open attention items across 6 of the
   13 boards, the oldest waiting 65 hours**, and seeing them at all took thirteen
   commands, one per project
   ([ADR-016](adr/ADR-016-kanban-serves-its-own-read-only-ui.md) Context). That
   measurement — operator decisions queueing behind the cost of looking at them —
   is the business reason the product has a web surface.

## Outcomes

Measured outcomes are marked as such; the rest are unmeasured, and this
document invents no number for them.

- **Work outlives the agent doing it**, so an interruption costs a turn rather
  than a project. Unmeasured as a rate; the mechanism is
  [ADR-001](adr/ADR-001-durable-agent-work-ledger.md) and its behaviour is
  [docs/PRD.md](PRD.md)'s.
- **One board per project estate, shared by every worktree and harness.**
  Unmeasured; the ownership rule is
  [ADR-005](adr/ADR-005-kanban-owns-work-state-atmux-consumes-it.md).
- **Operator decisions stop queueing.** The 2026-08-24 baseline above (75 open,
  oldest 65h, thirteen commands to see them) is the measured starting point in
  [ADR-016](adr/ADR-016-kanban-serves-its-own-read-only-ui.md); no later
  re-measurement of that queue exists, so no improvement figure is claimed here.
- **The decision surface is cheap enough to open.** Measured 2026-09-19: on a
  seeded three-board fixture the landing payload is **517 888 B** total, against
  **460 019 B** for the previous build and the **1.32 MB / 55 ms** `hax`
  server-rendered baseline of 2026-09-17
  ([docs/specs/spa.md](specs/spa.md) SPA-50). Those are observations, not
  budgets, and this document sets no performance, availability or latency
  commitment — consistent with
  [ADR-047](adr/ADR-047-kanban-adopts-specification-driven-development.md) §9.
- **Business truth and product truth move together.** Since
  [ADR-047](adr/ADR-047-kanban-adopts-specification-driven-development.md), a
  slice that changes business framing edits this document in the same change.

## Stakeholder

One: **George (`geoyws`)**, the operator, who is simultaneously the only user,
the product owner and the authority for every boundary below. Agent harnesses
are consumers of the ledger, not stakeholders with requirements of their own.
There is no user population, no adoption target, no revenue outcome and no
service-level commitment, because the product has none
([ADR-016](adr/ADR-016-kanban-serves-its-own-read-only-ui.md): "For a
single-operator tool that is the intended authority";
[ADR-047](adr/ADR-047-kanban-adopts-specification-driven-development.md) §9).

## Business boundaries

- **Single operator, single host, SQLite.** One ledger on `hax` holding every
  board of both estates; `hig` is an install target only. This is the defining
  boundary of the product and is quoted in full under Non-goals (registry rule
  `r-7af4dd57`, George, 2026-09-08).
- **Boards follow durable product-estate ownership boundaries** — not individual
  apps or services. A new board exists only for genuinely independent ownership,
  and lanes identify executors, never subsystems (registry rule `r-4da8aed9`).
  This is the rule that shapes how much estate the ledger is allowed to grow.
- **The ledger owns work state; orchestration consumes it**
  ([ADR-005](adr/ADR-005-kanban-owns-work-state-atmux-consumes-it.md)).
- **State is operator-private and lives outside product repositories**
  ([ADR-001](adr/ADR-001-durable-agent-work-ledger.md) §2).

## Non-goals

The first is the constraint most likely to be violated by a well-meaning future
proposal, so it is recorded in George's own words (registry rule `r-7af4dd57`,
2026-09-08):

> this kanban is a SINGLE-OPERATOR ledger on ONE host (hax) and stays on SQLite.
> Not multi-user, not moving to hig, not being rebuilt on PostgreSQL. The
> multi-user, team, PostgreSQL kanban is an entirely new open-source project
> geoyws will define himself; do not file successor, migration, per-person
> identity or second-host work against this ledger. hig receives byte-identical
> releases only as the release script's second target and holds no workspaces.

A proposal for multi-user access, per-person identity, a PostgreSQL rebuild, a
migration off `hax`, or workspaces on `hig` is out of scope for this product by
that rule, and belongs to the separate project named in it. Any such row filed
against this ledger should be refused rather than triaged.

Further non-goals, each already recorded and not re-argued here:

- A shared issue tracker for coworkers, customers or delivery teams
  (`README.md:5`-`README.md:6`; [docs/PRD.md](PRD.md) Non-goals).
- A second BRD, per-feature or per-slice
  ([ADR-047](adr/ADR-047-kanban-adopts-specification-driven-development.md) §1).
- A new board for anything short of genuinely independent estate ownership
  (registry rule `r-4da8aed9`).

The product-level non-goals — committing board state into product repos,
arbitrary SQL for agents, cross-host replication, replacing Git — are
[docs/PRD.md](PRD.md)'s and are not restated here.

## Sources

- [`README.md`](../README.md) — the one-paragraph purpose (lines 3-14).
- [ADR-001](adr/ADR-001-durable-agent-work-ledger.md) — the original purpose: a
  durable agent-work ledger.
- [ADR-005](adr/ADR-005-kanban-owns-work-state-atmux-consumes-it.md) — the
  ownership boundary: Kanban owns work state, atmux consumes it.
- [ADR-016](adr/ADR-016-kanban-serves-its-own-read-only-ui.md) — the measured
  approvals bottleneck that justifies the web surface.
- [ADR-047](adr/ADR-047-kanban-adopts-specification-driven-development.md) —
  SDD adoption; the singleton rule for this file and the refusal to invent
  commitments.
- [docs/PRD.md](PRD.md) — product truth; read to avoid duplication, not restated.
- [docs/specs/spa.md](specs/spa.md) SPA-50 — the measured landing-payload
  figures (`t-bf255880`, 2026-09-19).
- Registry rules `r-7af4dd57` (single operator / one host / SQLite) and
  `r-4da8aed9` (boards follow estate ownership) on the `kanban` board.
- `/Users/geoyws/work/journals/.sb/_dotfiles/docs/adr/017-two-kb-ledgers-hax-personal-hig-ifca.md`
  — the proposed two-ledger `hax`/`hig` split, **Withdrawn 2026-09-08**. Cited
  as history only; it is not current guidance and `r-7af4dd57` is the standing
  position.
