# ADR-014: The repo ships its own skill, and lives as a dotfiles submodule

**Status:** Superseded by [ADR-036](ADR-036-the-kb-skill-is-a-pinned-submodule-of-the-public-package.md)
**Date:** 2026-08-21
**Deciders:** George

> **Historical.** The skill no longer lives in this repository. `skills/kb` is a
> pinned submodule of the public package `geoyws/kb-skill`, and host routing
> moved to `skills/hosts.tsv`. ADR-036 keeps this ADR's actual guarantee — one
> commit names one skill revision, checked at compile time — through the gitlink
> rather than through an in-repo copy. Read the decision below as the reasoning
> that produced that guarantee, not as current instruction.

## Context

Two questions arrived together and have one answer.

**Where does the agent-facing documentation for this CLI live?** The alias table,
the addressing rules, the closed sets and the refusals were spread across
`--help`, four ADRs and the commit log. An agent reaching for the ledger had
nowhere to read the surface at once. The short forms are the sharpest case: they
resolve by exact match with no inference
([ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)), so
one that is not written down is one nobody can use.

The operator's skills live in a single tree, `_dotfiles/agents/skills/<name>/`,
symlinked into `~/.agents/skills` so every harness reads the same files. The
obvious move was to add a `kb` skill there.

**Where does this repository live?** It sat at `~/work/src/kanban` as a
standalone clone, unrelated to the dotfiles that configure everything else on the
box, including the harnesses that drive it.

## Decision

**The `/kb` skill lives in this repository, at `skills/kb/SKILL.md`, and the
dotfiles link to it.** It documents this binary, so the two version together: a
command changes and its documentation changes in the same commit, reviewed
together, released together. A copy in the dotfiles tree would be a second
description of the same surface, and it would drift the first time a command
changed — the failure
[ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) exists to
prevent, arriving through documentation rather than through an adapter.

**This repository is a git submodule of the dotfiles**, at `_dotfiles/kanban`,
which is what makes the link above possible: `agents/skills/kb` is a relative
symlink into `../../kanban/skills/kb`, the same shape `ix-bot` already uses into
the `skills-root` submodule. The skills loop and the validator both follow it and
see an ordinary skill, so nothing else in the wiring changes.

**`~/work/src/kanban` remains the path to use, as a symlink.** Every ADR, memory
file, session handoff, and habit on the box names it. `init.sh` carries the
`link_file` line, so a rebuild reproduces the link rather than leaving a path
that only history explains — the same three-part rule the operator applies to
every managed file: content in the dotfiles, live path a symlink, `init.sh` line
so a fresh box reproduces both.

The repository was **moved rather than re-cloned**. A fresh clone would silently
have lost what is gitignored: `.atmux/` including a registered driver worktree,
and the build cache. The worktree needed `git worktree repair` pointed explicitly
at its new path — the bare form left it `prunable`.

## Consequences

**Editing this repository is now two commits.** One in the submodule, one
advancing the pointer in the dotfiles. One without the other ships nothing, and
that is the standing cost of the arrangement.

An uninitialised submodule turns the skill symlink into a dangling link, which
`[[ -d ]]` and the validator both skip *silently* — the skill would vanish from
every harness with no error anywhere. `init.sh` fails loud on a dangling link
instead, and its message now names both submodules a skill can come from, since
being sent to the wrong one is barely better than not being told.

The skill's frontmatter must satisfy the dotfiles validator, which is stricter
than any harness: it rejected an unquoted `argument-hint: [subcommand …]` on the
first run, where the value is YAML for a one-element list and every harness would
have received the wrong type.

Deliberately not decided: whether other tools George owns should follow the same
pattern. This one earned it because it ships a skill describing itself and is
driven by the harnesses the dotfiles configure. A repository with neither
property gains nothing but the two-commit cost.

## Amendment — 2026-08-23: the move made this repository's own board unreachable

Leaving a symlink at `~/work/src/kanban` kept every documented path working at
the shell and broke the one thing nobody thought to check. Registration stores a
**canonical** path, and resolution canonicalises the caller's cwd — so once the
tree lived at `_dotfiles/kanban`, `getcwd()` inside it returned the physical
path while the registry still held the old one, and **no directory in this
repository resolved to its own board**. It stayed reachable by `--project
kanban`, which is why the breakage went unnoticed for two days.

`doctor` could not have caught it: the database was structurally perfect, every
board file present, and it reported `healthy: true`. It now checks whether each
registered root still resolves to itself, names where a broken one leads today,
and refuses to call the registry healthy while one is unreachable. `kanban
workspace repoint` repairs them — by default all of them, because one tree
moving breaks its project root and each lane beneath it at once, and repairing
one command at a time invites stopping half way. A root that is simply gone is
refused rather than guessed at.

Measured 2026-08-23: exactly two rows were affected, both from this move — the
project root and its `driver-2` lane. Nothing else on the box registers a path
that is now a symlink.

The general lesson is the one this repository keeps relearning: **a path stored
as text is a claim about the filesystem that stops being true without anything
writing to the database.** Provenance capture reached the same conclusion from
the other side and chose to ask git rather than parse `.git`.

## References

- [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md) — the binary the skill documents
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — one description of the surface, not two
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the exact-match alias rule that makes writing them down load-bearing
- [ADR-007](ADR-007-global-project-addressing.md) — the addressing chain the move broke, and `--project` which kept working
