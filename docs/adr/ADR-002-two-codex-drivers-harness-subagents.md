# ADR-002: Use two Codex drivers and harness-native subagents

**Status:** Accepted
**Date:** 2026-08-11
**Owner:** geoyws

## Context

Kanban previously inherited atmux's standing member roster and started persistent
lead, planner, docs, reviewer, and gitter panes. That topology duplicates the
delegation and parallel-execution facilities now provided by the Codex harness,
leaves long-lived agent processes idle, and splits coordination across tmux and
the active harness.

The operator still needs two independent, human-controlled repository lanes.
The trunk lane and the existing `driver-2` worktree provide that separation
without a persistent tmux worker hierarchy.

## Decision

The Kanban atmux team has exactly two interactive drivers:

- `driver` runs Codex from the repository root on the trunk branch;
- `driver-2` runs Codex from `.atmux/worktrees/driver-2` on its isolated branch.

The team configuration keeps `members` empty. It does not start persistent
lead, planner, docs, reviewer, gitter, or specialist panes. Both drivers are
operator-controlled, receive no automated prompts or keystrokes, and remain
available as interactive Codex sessions.

Delegation, decomposition, and parallel execution use harness-native Codex
subagents. Subagents are scoped to the active harness task rather than modeled
as durable tmux team members. Durable work state remains in Kanban's ledger;
tmux supplies only the two interactive repository lanes.

## Consequences

The live atmux cage contains two panes instead of a standing worker hierarchy.
Restoring or reconfiguring the team must preserve the two-driver Codex roster
and must not repopulate `members` from an atmux default template.

Parallel capacity follows the active Codex harness and its subagent limits.
Work that must survive a harness session is checkpointed in Kanban rather than
kept alive by a persistent worker pane.
