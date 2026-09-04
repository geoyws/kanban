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

The drift already happened. At the time of this decision three copies existed
and disagreed: `skills/kb`, the dotfiles tree's `agents/skills/kb` — a real
directory, not the relative symlink ADR-014 describes — and the written package.
Their `SKILL.md` and `kb-board` differed.

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
