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
the public links are in place and then PROVE the result in ONE poll: until a
deadline of 15 s — overridable through `HIG_RELEASE_SERVE_DEADLINE_SECONDS`,
which only the tests set — each tick reads `ActiveState` and `MainPID`, and
when the unit is active with a pid it resolves that pid's executable and
checks it sits (realpath) inside the `releases/<id>` this run activated. Once
that holds, `http://127.0.0.1:<port>/` must answer 200 within the same
deadline, where `<port>` is read from the unit's own `ExecStart --port` and
defaults to 14200. Readiness and the exe are one poll rather than two steps
because they are one window: a unit reports `active` with a `MainPID` before
the service binary has exec'd, and in that window `/proc/<MainPID>/exe` is
systemd's pre-exec helper, `/usr/lib/systemd/systemd-executor`. The first cut
read the exe once, straight after the active-poll, and at 03:10 MYT on
2026-09-09 it refused a CORRECT install of 8332e03 on hax on
`MainPID=3963673 exe=/usr/lib/systemd/systemd-executor` and rolled it back. So
an exe that is empty, unreadable or outside the release is `not yet`: the loop
sleeps 0.5 s and looks again. Only the deadline is fatal, and it is not a
warning: it names what was last measured — state, pid, exe, resolved path,
release — and falls into the previous-view rollback an activation failure
already performs, so the operator keeps the release that is actually serving.
A host without the unit is not a failure and is not silent either: it prints
`serve restart skipped: <reason>` once and the install receipt says
`serve: {skipped: <reason>}`, so a release report can never read as proof of a
restart that did not happen. The probe that reads a pid's executable is
overridable (`HIG_RELEASE_EXE_OF_PID`) because `/proc` does not exist on every
host that runs the tests; the default is the real `readlink /proc/<pid>/exe`.
`serve_restart_and_prove` is embedded verbatim in the remote installer and is
pinned by `hig_release_script_local_and_remote_install_guards_are_identical`
alongside the other guards, and
`hig_release_script_install_restarts_kanban_serve_and_proves_the_served_exe`,
`hig_release_script_install_waits_out_systemd_executor_before_judging_the_served_exe`,
`hig_release_script_install_refuses_when_the_served_exe_is_not_the_installed_release`
and `hig_release_script_install_skips_the_restart_when_the_unit_is_absent`
drive the four outcomes through both install legs.

Addendum 2026-09-09 (second, before the measurement had ever run on a host):
reviewing the guard above against the source found five ways it could pass or
hang where it should refuse, and one way its refusal could leave a host worse
than it found it. They are fixed together because they are one claim — "the
release that is installed is the one serving" — and any of them alone makes
that claim false.

1. The rollback restored PATHS only. `current` and the bin links went back to
   the previous release while the unit kept running the candidate, and then
   the candidate's directory was deleted from under it. A rollback now
   restarts the unit onto the restored links and PROVES the previous release
   is serving (`serve_restore_previous`) before removing anything; a first
   install, which has no previous release to serve, STOPS the unit first. A
   recovery that cannot be proved is never silent and never green: it reports
   its own failure next to the one that started the rollback and keeps the
   candidate release on disk to recover from.
2. `curl` had no per-request bound, so the deadline could not apply to a
   stalled socket: the loop only looked at the clock between requests, and a
   peer that accepts and never answers held the install open indefinitely.
   Each request now gets `--connect-timeout`/`--max-time` equal to what is
   LEFT of the one deadline.
3. `is-enabled` and `is-active` both fail for a unit that does not exist AND
   for a manager that cannot be reached, and failing both was read as "no unit
   on this host": an unreachable systemd turned the proof into a skip notice
   and the install reported green. Classification is now one
   `systemctl show -p LoadState -p UnitFileState -p ActiveState`, which exits
   0 whenever the MANAGER answers: a non-zero exit or a missing `LoadState` is
   a failed install, `LoadState=not-found` is the absent-unit skip, and a unit
   that is loaded but neither enabled nor active is skipped as deliberately
   stopped rather than started by an installer.
4. The exe check was a directory prefix, which `kb`, `manifest.json` and the
   kernel's `<path> (deleted)` all pass — the last being exactly what a
   rolled-back release leaves a running service holding. It is now an
   identity: the exe must resolve to `<release>/kanban` and that path must
   still be a regular executable file.
5. A 200 proved that something answered, not that the process measured
   answered. The pid and its exe are re-read after the 200 and must still
   agree, so a candidate that crashes into whatever systemd starts next, or an
   unrelated listener on the same port or socket, cannot supply the green.

Two more came out of the same reading. `kanban serve` has no default listener
— exactly one of `--port N` or `--socket PATH`, both and neither being usage
errors — so the probe no longer falls back to 14200: it parses the unit's own
`ExecStart` argv (tokenized, quoted words honoured, never `eval`ed), probes a
socket through `curl --unix-socket`, and refuses a unit naming none or two
rather than proving a stranger. And `systemctl` writes job progress to stdout,
which on the remote leg IS the JSON channel the caller parses, so the unit's
output goes to stderr with the diagnostics. The receipt gains `exeSource` and
`listener`: `exeSource` is `/proc/<pid>/exe` on a host that has one and names
`HIG_RELEASE_EXE_OF_PID` where the test seam answered instead, so a fixture's
answer can never be read as the kernel's.
`hig_release_script_install_proves_the_served_exe_through_proc_on_linux` takes
the seam away entirely on Linux and proves a real process through the real
`/proc`. The behaviours are pinned by
`hig_release_script_install_puts_the_previous_release_back_in_service_when_a_candidate_fails`,
`hig_release_script_install_refuses_when_the_service_manager_cannot_be_asked`,
`hig_release_script_install_bounds_a_stalled_http_probe_by_its_deadline`,
`hig_release_script_install_refuses_an_exe_that_is_not_the_retained_release_binary`,
`hig_release_script_install_probes_the_listener_the_unit_names` and
`hig_release_script_install_refuses_a_restart_that_never_produces_a_serving_release`,
each driven through both install legs.

Four smaller decisions fall out of the same reading. The `rollback`
subcommand moves the links an install moves, so it now calls the same
`serve_restart_and_prove` after the switch, reports the measurement in its
summary, and falls into the same recovery when the proof fails; a rollback
that only moved symlinks was a claim about symlinks. `curl` is checked for
BEFORE the candidate is restarted, because a missing curl exits 127 into a
swallowed status, which would read as an HTTP timeout and then fail the
recovery the same way. A relative `current` target - which
`ensure_safe_release_view` accepts - is restored verbatim and normalised
against the install root before it is handed to a proof that must resolve it.
And the receipt is the commit: every check that can still refuse an
activation runs before it, the `ERR` trap is disarmed immediately after it,
and retention runs on the far side, so a failed prune reports itself loudly
(non-zero, naming the release and saying not to roll it back) instead of
deleting the release that is answering the listener.

What this measurement CANNOT do is bound the blast radius of a schema
migration, and the recovery must not be read as though it could. Rolling the
code back does not roll the store back: the candidate may already have opened
and migrated a board, and the previous release the recovery restarts may then
be unable to read it. That case ends exactly where every unprovable recovery
ends - both failures reported, the candidate release retained, nothing
repointed at anything automatically - and it is NOT a healthy host. The
release path never rolls back or restores a database; a store that has moved
forward is an operator decision with `kb backup`/`kb restore`.

The tokenizer is written against systemd's rendering as this repository's
fixtures reproduce it - space-separated argv words, spaces inside a word
rendered in double quotes, backslash escapes inside those quotes. That is not
a proof about every systemd version's quoting, so it fails closed: an
unbalanced quote or a trailing escape refuses the install naming the argv it
could not read, rather than guessing or reaching for `eval`. The ExecStart a
live host actually reports is worth capturing against this the first time it
runs there.

## References

- `scripts/hig-release.sh`
- `rust/lib.rs`
- `rust/registry.rs`
- `tests/e2e.rs`
