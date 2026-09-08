# ADR-034: HIG release packages and explicit board-rule transfer

**Status:** Accepted
**Date:** 2026-09-03
**Deciders:** Team

## Context

The release path needs a deterministic package that is built on `hax`, carries an explicit ordered install-target set for `hax` and `hig`, and includes every declared Rust executable. Rule movement also needs a safe, auditable path between registries without mutating source rules in place.

## Decision

Release packaging now builds the release executables it enumerates with `cargo build --release --locked --bins`, writes a manifest with the source commit and canonical ordered `targets: ["hax","hig"]` compatibility set, binary hashes, and byte sizes, and refuses installation until the full package validates against the requested install host. HAX activation publishes a canonical receipt under the HAX install root only after activation succeeds; repeated activation of the same release ID must not rewrite that receipt with different bytes. HIG installation is launched on HAX. Before its first SSH or staging action, `install_remote` validates the package and build receipt, then validates the exact release ID's canonical HAX activation receipt, release tree, and `current` pointer from the local HAX install root. It then transfers the package and receipt to HIG. The HIG-side installer revalidates only that transferred package and receipt, including host compatibility and every enumerated binary's hash, size, and version, and installs those identical bytes before atomically advancing `current`. It never accepts or trusts a HIG-local path as a substitute for HAX storage. If activation fails after `current` moves, rollback restores the previous `current` target and removes the newly created release tree and receipt.

Every package and activation validation path probes `kanban` and `kb` with
`version`, and probes `kanban-dispatcher` and every adapter with
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

Addendum 2026-09-06: the crate declares TEN Rust executables, and the release
package now enumerates all ten. The gap this addendum first recorded — a
package that shipped SIX while the crate declared ten — is closed, because a
declared executable the package omits never reaches a host at all.

The four beyond the original six are all pubsub adapters:
`kanban-opencode-adapter` (POSTs a delivery to a local HTTP server instead of
spawning a turn), `kanban-kimi-acp-adapter` (one framed JSON-RPC exchange over
an ACP peer's stdio), `kanban-cursor-worker-adapter` (a serialized worker, one
turn at a time per state directory), and `kanban-zcode-notify-adapter`
(notify-only, with ingress made structurally unwritable).

`cargo build --release --locked --bins` builds all ten, so nothing was ever
missing from a source build; what enumerated six was the release path. That
set is now stated once, in `scripts/hig-release.sh`'s `BINARIES` array, and
every other site derives from it: the exact package-contents check, the
manifest and receipt name and count checks, the `--version` probe's known-name
guard, the bin-link, staging and rollback loops, and the embedded remote
installer, which receives the set as positional arguments rather than
repeating it. A second literal list is what let the package fall four
binaries behind the crate, so the promotion removed the duplicates instead of
extending them.

`tests/e2e.rs` pins the agreement from both ends:
`hig_release_script_enumerates_exactly_the_executables_the_crate_declares`
compares the script's array against the `[[bin]]` blocks in `Cargo.toml` and
against the executables Cargo actually built, so an added `[[bin]]` fails by
name instead of silently missing the package, and
`hig_release_script_installs_every_declared_binary_without_remote_hax_access_and_refuses_partial_activation`
(renamed from `..._installs_six_real_binaries_...`, whose name asserted a
count that would go stale) drives package, HAX activation, HIG install and a
partial-package refusal across the full declared set.

Addendum 2026-09-09: activation now ends in a measurement of the running
service, because swapping the pointers is not the same as changing what
serves. On 2026-09-09 around 01:55 MYT an install on `hax` replaced
`/root/.local/bin/kanban` and the `current` link and stopped there;
`kanban-serve.service` was never restarted, so `/proc/<MainPID>/exe` still
resolved under the previous release. When that release's board schema 26
migrated the first board the old server was asked for, it could not open it,
and every route answered 500 for about two minutes until
`systemctl restart kanban-serve` was run by hand. `hig` carried the same stale
exe. Both install paths therefore restart `kanban-serve` after `current` and
the public links are in place and then PROVE the result: the unit reaches
`ActiveState=active` with a `MainPID` within 15 s, that pid's executable
resolves (realpath) inside the `releases/<id>` this run activated, and
`http://127.0.0.1:<port>/` answers 200, where `<port>` is read from the unit's
own `ExecStart --port` and defaults to 14200. A failed measurement is not a
warning: it names what was measured — pid, exe, resolved path, release, status
code — and falls into the previous-view rollback an activation failure already
performs, so the operator keeps the release that is actually serving. A host
without the unit is not a failure and is not silent either: it prints
`serve restart skipped: <reason>` once and the install receipt says
`serve: {skipped: <reason>}`, so a release report can never read as proof of a
restart that did not happen. The probe that reads a pid's executable is
overridable (`HIG_RELEASE_EXE_OF_PID`) because `/proc` does not exist on every
host that runs the tests; the default is the real `readlink /proc/<pid>/exe`.
`serve_restart_and_prove` is embedded verbatim in the remote installer and is
pinned by `hig_release_script_local_and_remote_install_guards_are_identical`
alongside the other guards, and
`hig_release_script_install_restarts_kanban_serve_and_proves_the_served_exe`,
`hig_release_script_install_refuses_when_the_served_exe_is_not_the_installed_release`
and `hig_release_script_install_skips_the_restart_when_the_unit_is_absent`
drive the three outcomes through both install legs.

## References

- `scripts/hig-release.sh`
- `rust/lib.rs`
- `rust/registry.rs`
- `tests/e2e.rs`
