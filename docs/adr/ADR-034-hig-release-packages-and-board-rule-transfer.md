# ADR-034: HIG release packages and explicit board-rule transfer

**Status:** Accepted
**Date:** 2026-09-03
**Deciders:** Team

## Context

The release path needs a deterministic package that is built on `hax`, carries an explicit ordered install-target set for `hax` and `hig`, and includes all seven Rust executables. Rule movement also needs a safe, auditable path between registries without mutating source rules in place.

## Decision

Release packaging now builds all seven release executables with `cargo build --release --locked --bins`, writes a manifest with the source commit and canonical ordered `targets: ["hax","hig"]` compatibility set, binary hashes, and byte sizes, and refuses installation until the full package validates against the requested install host. HAX activation publishes a canonical receipt under the HAX install root only after activation succeeds; repeated activation of the same release ID must not rewrite that receipt with different bytes. HIG installation is launched on HAX. Before its first SSH or staging action, `install_remote` validates the package and build receipt, then validates the exact release ID's canonical HAX activation receipt, release tree, and `current` pointer from the local HAX install root. It then transfers the package and receipt to HIG. The HIG-side installer revalidates only that transferred package and receipt, including host compatibility and all seven binaries' hashes, sizes, and versions, and installs those identical bytes before atomically advancing `current`. It never accepts or trusts a HIG-local path as a substitute for HAX storage. If activation fails after `current` moves, rollback restores the previous `current` target and removes the newly created release tree and receipt.

Every package and activation validation path probes `kanban` and `kb` with
`version`, and probes `kanban-dispatcher` and all four adapters with
`--version`.

The release script itself is intended to ship as an executable shell script with mode `0755`. `package` is HAX-only; `install` and `rollback` accept `hax` and `hig`.

Registry rule transfer is now explicit:

- `rule export --board ... --as ACTOR` writes a deterministic bundle for an allowlisted set of source boards.
- `rule import PATH --as ACTOR` imports that bundle into a destination registry with fresh destination rule IDs.
- Both sides refuse missing, unregistered, unreadable, duplicate, or out-of-scope boards.
- Imports preserve body, author, timestamps, and tags, but do not mutate source rules.

## Consequences

Release artifacts are auditable and reproducible across hosts, and a partial package cannot become live. Public executable paths stay stable because they always traverse the `current` pointer, failed activations do not count toward release retention, HAX authorization evidence is canonical and append-only for each release ID, and rule movement is now idempotent and traceable by source fingerprint instead of relying on in-place consolidation.

Addendum 2026-09-05: before any write, both the local and the embedded remote install paths refuse an install view that is not the shape the installer creates — a symlink at `releases/` or `releases/<id>`, a regular file or foreign symlink at `current` or at a public binary destination, or a symlinked bin dir. Only symlinks whose target sits directly in `<install-root>/releases/` (for `current`) or `<install-root>/current/` (for bin links) are ever replaced, so a refused install leaves the previous `current`, binaries, and the operator's files byte-identical.

Addendum 2026-09-06: the seventh Rust executable is `kanban-opencode-adapter` (`rust/bin/kanban-opencode-adapter.rs`), the fourth adapter and the first that POSTs a delivery to a local HTTP server instead of spawning a turn; `cargo build --release --locked --bins` already builds it, but `scripts/hig-release.sh`'s explicit `BINARIES` list, its manifest/receipt validation sets, its `--version` probe arm, its bin-link loops, and the matching `tests/e2e.rs` package assertion still enumerate six, so they must all be extended to seven in the same change that promotes this executable into the release package.

## References

- `scripts/hig-release.sh`
- `rust/lib.rs`
- `rust/registry.rs`
- `tests/e2e.rs`
