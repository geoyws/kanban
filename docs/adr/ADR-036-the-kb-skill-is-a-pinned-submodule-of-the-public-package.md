# ADR-036: The kb skill is a pinned submodule of the public package

**Status:** Accepted
**Date:** 2026-09-04
**Deciders:** George
**Supersedes:** [ADR-014](ADR-014-the-repo-ships-its-own-skill-and-lives-in-the-dotfiles.md)

## Context

[ADR-014](ADR-014-the-repo-ships-its-own-skill-and-lives-in-the-dotfiles.md)
put the skill at `skills/kb/SKILL.md` inside this repository so that it versions
with the binary: a command changes and its documentation changes in the same
commit. `rust/lib.rs` enforces that at compile time — the alias-drift test does
`include_str!("../skills/kb/SKILL.md")` and asserts every documented alias
resolves against the real command table.

That decision assumed one consumer. The skill is now needed by estates that
share no infrastructure with this one, which an in-repo copy cannot serve: the
document named a specific host wrapper (`hax-kb`, 8 times), and a second copy
elsewhere is the drift ADR-010 exists to prevent.

ADR-014's own mechanism held. The dotfiles tree's `agents/skills/kb` is the
relative symlink into `../../kanban/skills/kb` that ADR-014 describes, and it
resolves through the `kanban` submodule pinned there. An earlier revision of
this ADR claimed the symlink had decayed into a divergent third copy; that was
wrong. The file seen through the dotfiles pin differs from this lane only
because the pin names an older commit, which is what a pinned submodule is for.

So this decision is not repair work. It is scope: one estate's in-repo document
cannot serve estates that share no infrastructure with it, because it named a
specific host wrapper.

## Decision

`skills/kb` is a git submodule of the public package
[`geoyws/kb-skill`](https://github.com/geoyws/kb-skill), pinned by gitlink.

ADR-014's guarantee is kept, by a different mechanism. A commit still names
exactly one skill revision — the gitlink — so the binary and its documentation
still version together and the compile-time alias check still runs, now against
the submodule's `SKILL.md`. What changes is that the skill is no longer *edited*
here: it is authored in the package and consumed at a pinned commit.

The build therefore depends on submodule initialization. Initialize it
path-specifically, never recursively:

```bash
git submodule update --init skills/kb
```

Host routing is supplied by the consumer, never by the package, so the package
stays estate-neutral. `kb-board` and `kb-host` read `KB_HOSTS_TABLE`, else a
`hosts.tsv` beside the package. This repository ships `skills/hosts.tsv` and
points `KB_HOSTS_TABLE` at it; an unknown board fails closed rather than
guessing a host.

## Consequences

A fresh clone cannot `cargo build` until `skills/kb` is initialized, because the
alias-drift test embeds a file inside the submodule. That is the cost of keeping
the check at compile time rather than weakening it to a runtime read that skips
when the file is absent — the guard is worth an init step, and initializing
submodules path-specifically before use is already how this estate works.

The embedded skill is now estate-neutral, so host knowledge that used to live in
the document lives in `skills/hosts.tsv` instead. Anything that assumed
`skills/kb/scripts/hax-kb` must use `kb-host` with a routing table.

Updating the skill is now two commits: one in the package, then a gitlink bump
here. That is the price of one authored copy instead of three, and the gitlink
makes "which skill did this binary ship with" answerable from the commit.

## References

- `docs/adr/ADR-014-the-repo-ships-its-own-skill-and-lives-in-the-dotfiles.md`
- `docs/adr/ADR-010-adapters-generated-from-the-command-surface.md`
- `rust/lib.rs` — the compile-time alias-drift test
- `skills/hosts.tsv`

## Addendum, 2026-09-05

`public-kb-skill/`, the staging directory the package was first published
from, was removed. It had already drifted 23 lines from the pinned
`skills/kb/SKILL.md`, nothing built or tested against it, and a writer edited it
on 2026-09-05 believing it was the skill source. The package repository is the
only place the skill is authored.
