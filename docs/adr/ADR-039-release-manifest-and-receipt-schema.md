# ADR-039: The release manifest and receipt schema is frozen at formatVersion 1

**Status:** Accepted
**Date:** 2026-09-06
**Deciders:** claude@driver, freezing the schema `scripts/hig-release.sh` already
ships, under George's decision `a-f0ced14b` (2026-09-06) to keep the shipped
release-store paths. George did not rule on the field list himself and may
supersede it by a later ADR.
**Supersedes:** nothing.
[ADR-034](ADR-034-hig-release-packages-and-board-rule-transfer.md) introduced
the behaviour and stays in force; this ADR only freezes the schema that
behaviour already emits.

## Context

[ADR-034](ADR-034-hig-release-packages-and-board-rule-transfer.md) introduced
release packaging, HAX activation and HIG install, and has since grown two
addenda — one for the install-guard set (2026-09-05), one for the enumerated
binary list that had fallen four behind the crate (2026-09-06). What none of
them did was write the schema down. The manifest and the two receipts are
produced by `jq -n` object literals inside
`scripts/hig-release.sh` and consumed by `jq -e` predicate blocks a few hundred
lines away, and every one of them carries `formatVersion: 1` while nothing
anywhere states what version 1 contains.

That is drift waiting to happen, and it is worse than ordinary drift because
`releaseId` is a hash of one of these files. A field added to `manifest.json`
changes `manifestSha256`, which changes `releaseId`, which renames the release
directory and the receipt beside it and reorders nothing in retention but
invalidates every recorded ID on both hosts. A field added to a receipt is
cheap; a field added to the manifest is a migration. Nobody reading the script
today can tell those two apart, because both look like an extra line in a `jq`
literal.

Three of the manifest's five fields are also constants at the writer —
`formatVersion` is the literal `1` (`scripts/hig-release.sh:591`), `targets` is
the literal `["hax","hig"]` (`scripts/hig-release.sh:592`, produced only by
`package_targets_json` at `scripts/hig-release.sh:148`), and `sourceTreeClean`
is the literal `true` (`scripts/hig-release.sh:594`). They read as data and are
not. A reader who does not know that will treat `sourceTreeClean: false` as a
representable state and write a consumer branch that can never be taken.

This ADR is a freeze, not a change. Nothing in the script moves.

## Decision

`formatVersion` 1 means exactly the fields enumerated below, with exactly these
invariants. A field added, removed, retyped or given a new admissible value is a
`formatVersion` bump, landed in one cut across the writer, every validator, the
embedded remote installer and this document. Every claim here cites the line
that writes or checks it.

**Citation convention.** A bare `:NNN` in this document is
`scripts/hig-release.sh:NNN`. Any other file is named in full.

### 1. `manifest.json`

Path `<package-dir>/manifest.json` (`scripts/hig-release.sh:125`). Written by
`write_manifest` (`scripts/hig-release.sh:579`), validated by
`validate_release_files` (`scripts/hig-release.sh:159`) and `package_validate`
(`scripts/hig-release.sh:543`). Copied verbatim into the release tree
(`scripts/hig-release.sh:723`, `scripts/hig-release.sh:1228`), so the installed
release carries the same bytes the package did.

| field | type | invariant | written | validated |
| --- | --- | --- | --- | --- |
| `formatVersion` | number | exactly `1`; a literal, never computed | `:591` | `:177`, `:551`, `:1188` |
| `targets` | array of string | exactly `["hax","hig"]`, length 2, and must contain the requested install target | `:592` (from `:148`) | `:178`–`:182`, `:552`–`:556` |
| `sourceCommit` | string | 40-character hex, `git rev-parse HEAD` of the build worktree | `:593` (value `:643`) | `:184`–`:185`, `:558`–`:559` |
| `sourceTreeClean` | boolean | literal `true`; a dirty tree dies before the manifest exists | `:594` | `:183`, `:557` |
| `files` | array of object | one entry per `BINARIES` member (`:9`–`:20`), in that order, no more and no fewer | `:595` | count `:186`, names `:187` |
| `files[].name` | string | a `BINARIES` member | `:660` | `:187` |
| `files[].sha256` | string | 64-hex `sha256sum` of the shipped bytes | `:660` (via `:72`) | read at `:203`; compared `:197`, `:318`, `:1208` |
| `files[].bytes` | number | `wc -c` of the shipped bytes | `:660` (via `:651`) | read at `:203`; compared `:194`, `:317`, `:1207` |
| `files[].version` | string | the binary's own version output, trailing whitespace stripped | `:660` (via `:93`) | read at `:203`; compared `:200`, `:319`, `:1209` |

`sourceTreeClean` is an assertion, not a measurement. The measurement is the
pair of `git status --porcelain=v1 --untracked-files=all` reads that gate
packaging (`scripts/hig-release.sh:632`, refusal at
`scripts/hig-release.sh:633`) and re-check afterwards that packaging itself
changed nothing (`scripts/hig-release.sh:665`, refusal at
`scripts/hig-release.sh:666`). A manifest whose `sourceTreeClean` is `false`
cannot be produced by this script.

`files[].version` is produced by `file_version` (`scripts/hig-release.sh:93`),
which refuses a name outside the release set
(`scripts/hig-release.sh:96`), probes `kanban` and `kb` with the `version`
subcommand (`scripts/hig-release.sh:101`) and everything else with `--version`
(`scripts/hig-release.sh:104`).

The package directory holds exactly `manifest.json` plus the release binaries
and nothing else: the expected set is derived from `BINARIES`
(`scripts/hig-release.sh:136`) and compared against the actual directory
listing (`scripts/hig-release.sh:168`, refusal at
`scripts/hig-release.sh:170`). No payload entry may be a symlink
(`scripts/hig-release.sh:193`).

### 2. The package receipt

Path `<package-dir>.receipt.json` — a **sibling** of the package directory, not
a member of it (`scripts/hig-release.sh:129`), which is what lets the
exact-contents check above be exact. Written at
`scripts/hig-release.sh:671`–`:685`, validated by `validate_receipt`
(`scripts/hig-release.sh:212`) and again inside the remote installer
(`scripts/hig-release.sh:1187`).

| field | type | invariant | written | validated |
| --- | --- | --- | --- | --- |
| `formatVersion` | number | exactly `1` | `:678` | `:223`, `:1188` |
| `host` | string | exactly `"hax"` — the build host, forced by `require_host hax` at `:620` | `:679` | `:224`, `:1195` |
| `targets` | array of string | identical to the manifest's | `:680` | `:225`–`:229`, `:1189`–`:1193` |
| `manifestSha256` | string | 64-hex sha256 of the package `manifest.json` **bytes** | `:681` (computed `:669`, via `:141`) | recomputed and compared `:233`; type and length `:1198`–`:1199` |
| `sourceCommit` | string | 40-hex, equal to the manifest's | `:682` | `:231`–`:232`; equality `:237` |
| `sourceTreeClean` | boolean | literal `true` | `:683` | `:230`, `:1194` |
| `files` | array of object | the same array object the manifest carries | `:684` | count and names `:234`–`:235`; bytes re-verified against it remotely `:1204`–`:1210` |

`package_create` also prints a summary object on stdout —
`{packageDir, manifest, receipt, manifestSha256, targets}`
(`scripts/hig-release.sh:692`). That is command output for the caller, not a
stored artifact, and is not covered by this freeze. The same applies to the
install and rollback summaries (`scripts/hig-release.sh:797`,
`scripts/hig-release.sh:1321`, `scripts/hig-release.sh:1416`).

### 3. The activation receipt

Path `<install-root>/releases/<releaseId>.receipt.json`
(`scripts/hig-release.sh:338`). Written locally at
`scripts/hig-release.sh:764`–`:784` and remotely at
`scripts/hig-release.sh:1240`–`:1259`. It is the **package receipt plus eight
fields** — the writer is literally `$receipt + { … }`
(`scripts/hig-release.sh:774`, `scripts/hig-release.sh:1250`) — so every field
in §2 is also an activation-receipt field with the same invariant.

| field | type | invariant | written | validated |
| --- | --- | --- | --- | --- |
| `activationSequence` | number | integer ≥ 1, strictly increasing per install root | `:775`, `:1251` | ordering key `:506`, `:1155` |
| `installedAt` | number | unix milliseconds, `date +%s` × 1000 | `:776` (value `:763`), `:1252` (value `:1236`) | type and positivity `:309`–`:310`; tiebreak key `:506` |
| `target` | string | `hax` or `hig` — the host this tree was installed **for** | `:777`, `:1253` | value originates in the CLI target checked at `:532`; the field itself must read `hax` on a HAX receipt `:293` |
| `releaseId` | string | `"<sourceCommit>-<manifestSha256>"` | `:778`, `:1254` | `:303` |
| `releaseDir` | string | absolute `<install-root>/releases/<releaseId>` | `:779`, `:1255` (from `:332`) | value `:304`, type `:306` |
| `currentLink` | string | absolute `<install-root>/current` | `:780`, `:1256` (from `:328`) | value `:305`, type `:307` |
| `binDir` | string | absolute directory holding the public links | `:781`, `:1257` | type `:308` |
| `installerHost` | string | short hostname of the machine that ran the activation | `:782` (from `:61`), `:1258` | must be `hax` on a HAX receipt `:294` |

`host` and `installerHost` are different facts and must not be conflated:
`host` is inherited from the package receipt and is always the build host
(`scripts/hig-release.sh:679`), `installerHost` is the activating host. Both are
checked independently on a HAX receipt (`scripts/hig-release.sh:292`,
`scripts/hig-release.sh:294`).

The receipt is **write-once per release ID**, guarded by an existence test
before the write (`scripts/hig-release.sh:760`,
`scripts/hig-release.sh:1238`). Re-activating the same release therefore leaves
its receipt byte-identical, including its original `activationSequence` and
`installedAt`. Locally the bytes land through `.tmp` and `mv -f`
(`scripts/hig-release.sh:783`–`:784`); remotely they are written to the final
path directly (`scripts/hig-release.sh:1259`).

`releases/.activation-sequence` (`scripts/hig-release.sh:133`,
`scripts/hig-release.sh:1124`) is installer state, not an artifact. It holds one
decimal integer and is not part of this schema.

### 4. Identity versus record, and the circularity rule

**Identity** is `sourceCommit` + `manifestSha256`, concatenated with a hyphen
into `releaseId` (`scripts/hig-release.sh:325`, and independently in the remote
installer at `scripts/hig-release.sh:1212`). `sourceCommit` names the source;
`manifestSha256` names the manifest bytes, and the manifest covers every shipped
byte transitively through per-file `sha256` and `bytes`
(`scripts/hig-release.sh:660`). The pair therefore names the payload, and the
store's directory names are that name (`scripts/hig-release.sh:335`).

**Recorded only** are `installedAt`, `installerHost`, `activationSequence`,
`target`, `releaseDir`, `currentLink` and `binDir`
(`scripts/hig-release.sh:775`–`:782`). None of them enters the ID. That is why
the same package activated on `hax` and on `hig` yields one `releaseId` and two
records, and why a second activation on one host is idempotent rather than a new
release.

**The circularity rule.** The release ID is derived from a payload that excludes
its own ID and every activation fact. Concretely:

- The hash input is the manifest file, and nothing else:
  `package_manifest_sha256` hashes `<dir>/manifest.json`
  (`scripts/hig-release.sh:141`); `validate_hax_activation_receipt` recomputes
  it the same way (`scripts/hig-release.sh:279`); the remote installer compares
  the staged copy's hash to the receipt's field
  (`scripts/hig-release.sh:1229`).
- `manifest.json` contains exactly `formatVersion`, `targets`, `sourceCommit`,
  `sourceTreeClean` and `files` (`scripts/hig-release.sh:590`–`:596`). It carries
  no `releaseId`, no `manifestSha256` and no activation field.
- The files that *do* carry `releaseId` and `manifestSha256` — both receipts —
  are never hashed into the ID. Neither receipt has a hash-of-itself field
  anywhere.

The consequence is a rule for future fields: **a new field goes in a receipt,
not in `manifest.json`**, unless the change is worth a `formatVersion` bump and
the renaming of every stored release directory, receipt and recorded ID on both
hosts. Putting `releaseId` into `manifest.json` is not merely discouraged; it is
not expressible, because the hash would then depend on itself.

### 5. The publication sequence

Frozen order for `install_release_tree` (`scripts/hig-release.sh:695`):

1. Validate the package (`:702`) and the package receipt (`:703`).
2. Derive `releaseId` from the receipt (`:707`).
3. `ensure_safe_release_view` — before any write (`:708`).
4. `mkdir -p releases` (`:709`); remember the outgoing `current` target
   (`:713`–`:714`).
5. Stage into `releases/.<releaseId>.XXXXXX` (`:718`), `install -m 0755` each
   binary (`:721`), copy the manifest (`:723`).
6. Validate the **staged** tree (`:724`).
7. `mv` staging onto `releases/<releaseId>` (`:725`) — a rename within one
   directory.
8. Public bin links (`:732`), each through `atomic_symlink`.
9. Flip `current` (`:734`).
10. Write the activation receipt, write-once (`:760`–`:784`).
11. Re-validate the live release tree (`:787`).
12. Prune (`:788`).

The remote installer runs the same sequence with the receipt written **before**
the links and `current` rather than after: guard (`:1213`), stage and `mv`
(`:1224`–`:1232`), receipt (`:1238`–`:1259`), links (`:1262`), `current`
(`:1263`), target re-check on the written receipt (`:1288`–`:1294`), prune
(`:1296`–`:1306`). This asymmetry is real and is frozen as-is: the invariant is
that the receipt is written exactly once and never before the release tree
exists, not that it sits on a particular side of the `current` flip. A future
change that unifies the two must say so, because either order alone looks
canonical to a reader of one half.

Rollback undoes the sequence rather than the specific failure: whichever step
trips, the `ERR` trap (`:731`, `:1237`) runs `rollback_activation_view`
(`:450`), which restores the previous `current` or removes it (`:466`–`:470`),
deletes a tree this run created along with its receipt (`:474`–`:476`), and
removes the public links only when there was no previous release to fall back
to (`:479`–`:482`). The same recovery is inlined for the injected
post-`current` failure (`:740`–`:757`, `:1269`–`:1285`), whose hook is
`maybe_fail_after_current` (`:489`).

### 6. The refusal set

`ensure_safe_release_view` (`scripts/hig-release.sh:377`) runs from a clean
state, before anything is written, and refuses exactly:

1. install root is a symlink — `:385`
2. install root exists and is not a directory — `:386`
3. `releases/` is a symlink — `:388`
4. `releases/` exists and is not a directory — `:389`
5. `releases/<releaseId>` is a symlink — `:392`
6. `releases/<releaseId>` exists and is not a directory — `:393`
7. `current` exists and is not a symlink this installer manages — `:395`–`:397`
8. bin dir is a symlink — `:400`
9. bin dir exists and is not a directory — `:401`
10. any `<bin-dir>/<release-binary>` exists and is not a symlink this installer
    manages — `:406`–`:408`
11. `releases/<releaseId>.receipt.json` exists and is not a receipt this
    installer could have written — called between refusals 6 and 7 (`:463`),
    and `ensure_managed_activation_receipt` (`:407`, embedded verbatim at
    `:1090` and pinned by the same drift test) requires all three of: a
    regular non-symlink file that parses as a JSON object (`:412`–`:417`);
    `releaseId`, `sourceCommit` and `manifestSha256` all equal to the release
    being installed (`:418`–`:423`); and an integer `activationSequence` from
    1 up to the `releases/.activation-sequence` counter, which reads 0 when
    that counter is absent, so a sequence no activation ever issued is refused
    (`:424`–`:437`). A receipt this installer wrote passes all three, which is
    what keeps re-activating an installed release idempotent rather than
    refused.

"Managed" is decided by `managed_symlink` (`scripts/hig-release.sh:358`): the
link's target parent directory must be named `releases` (for `current`) or
`current` (for a bin link) and must sit physically directly inside the install
root (`:367`–`:370`, resolving through `physical_dir` at `:351`). An operator's
own file or a symlink pointing anywhere else is never replaced. The whole guard
is embedded verbatim in the remote installer (`scripts/hig-release.sh:1006`
onward, the same refusals 1–10 at `:1014`, `:1015`, `:1017`, `:1018`, `:1021`,
`:1022`, `:1026`, `:1029`, `:1030`, `:1037`), and
`hig_release_script_local_and_remote_install_guards_are_identical` in
`tests/e2e.rs` fails when the two copies drift.

`atomic_symlink` carries a second line of refusals for the same paths at write
time: a non-symlink at the link path (`:418`–`:419`) and a symlinked link parent
(`:422`–`:423`). Around the edges: `--output` must not already exist and its
parent must not be a symlink (`:117`–`:121`), a package directory must not be a
symlink (`:165`, refusal text at `:112`), a receipt's parent must not be a
symlink (`:221`), a HAX activation receipt must not be a symlink (`:287`), a HAX
release directory must not be a symlink (`:289`), and a rollback install root
must not be a symlink (`:1364`).

### 7. Retention and the store path

`MAX_RELEASES` is 10 (`scripts/hig-release.sh:21`). `prune_releases`
(`scripts/hig-release.sh:511`) keeps the newest `keep` and deletes each older
release's tree and receipt together (`:526`–`:527`). It runs after an activation
(`:788`) and after a rollback (`:1408`); the remote installer inlines the
equivalent (`:1296`–`:1306`) using the `keep` value passed as a positional
argument (`:915`, received at `:924`).

Order is decided by `release_entries` (`scripts/hig-release.sh:497`): it reads
every `releases/*.receipt.json` (`:502`) and sorts by `activationSequence`
descending, then `installedAt` descending (`:506`–`:508`). Retention is
therefore keyed on the activation record, not on mtime and not on the lexical
ID — and a failed activation is invisible to it, because its receipt is deleted
during rollback (`:476`, `:748`, `:1277`) and a release with no receipt is never
enumerated at all.

The store is `/root/.local/share/kanban-releases`, and per `a-f0ced14b`
(2026-09-06) it stays. The script does not encode that path: it arrives as
`--install-root`, which is mandatory and has no default
(`scripts/hig-release.sh:840`), so the store path is operator convention plus
that board decision. What the script *does* fix relative to that root
is the layout — `releases/` and `releases/<releaseId>`
(`scripts/hig-release.sh:335`), `releases/<releaseId>.receipt.json` (`:341`),
`current` (`:329`) and `releases/.activation-sequence` (`:133`). The bin
directory is separate and defaults to `${HOME:-/root}/.local/bin`
(`scripts/hig-release.sh:23`). The live store is observable in-tree at
`docs/testing/graphql-agent-loop-benchmark-2026-09-05.json:303`, which resolved
`kb` through
`/root/.local/share/kanban-releases/releases/<40-hex>-<64-hex>/kb`.

### 8. Where the parent epic's concerns actually live

Epic `e-c3c8a863` listed fourteen concerns beyond the schema itself. Each is
either covered here, covered by the deployment-attempt ledger
([ADR-030](ADR-030-deployment-attempt-ledger-and-self-archiving.md), surface at
`rust/lib.rs:96`–`rust/lib.rs:102`), or out of scope. No concern is claimed as
covered without a line.

**Build-attempt IDs — covered by the deploy ledger, deliberately not here.**
The release path has no attempt identity. A failed `package` run leaves nothing
behind (`scripts/hig-release.sh:36`, under the `cleanup` trap installed at
`scripts/hig-release.sh:41`), and a successful one is named only by its payload
(`scripts/hig-release.sh:325`). Attempt identity is `d-*`, minted by
`kanban deploy start` with repo, full commit, tier, environment, host, URL,
actor and an optional idempotency key (`rust/lib.rs:96`–`rust/lib.rs:98`) and
closed by `kanban deploy finish` with a phase and receipt
(`rust/lib.rs:99`–`rust/lib.rs:101`). The two identities answer different
questions — `releaseId` names bytes, `d-*` names an attempt — and the script
writes to neither ledger: it contains no `deploy` invocation at all.

**Provenance — covered here, and bounded.** Source commit
(`scripts/hig-release.sh:593`), clean-tree assertion earned by measurement
(`:633`, `:666`), build command `cargo build --release --locked --bins` in a
scratch `CARGO_TARGET_DIR` (`:640`), build host pinned to `hax` (`:620`, via
`require_host` at `:65`), `host: "hax"` recorded and validated (`:679`, `:224`,
`:1195`), activating host recorded (`:782`). Not covered: no signature, no
builder identity beyond a short hostname, no toolchain version. Those would be
new fields and therefore a `formatVersion` bump.

**Command and database compatibility — recorded here, enforced elsewhere.**
`files[].version` for `kanban` and `kb` is the `version` subcommand's output
(`scripts/hig-release.sh:101`), which is
`kanban <pkg-version> (board schema N; registry schema M)`
(`rust/lib.rs:6467`–`rust/lib.rs:6470`). So the manifest does carry the schema
numbers the release expects, as a string. Nothing in the release path compares
them against the board or registry actually present; the binary refuses at
runtime instead (`rust/db.rs:2093` for a board, `rust/db.rs:2434` for a
registry). A consumer wanting machine-comparable numbers needs a structured
field, hence a bump.

**Hashes — covered here.** Per-file `sha256` and `bytes` are written once
(`scripts/hig-release.sh:660`) and re-verified at package validation
(`:197`, `:194`), on the staged tree before `mv` (`:724`), on the live tree
after activation (`:787`), on the HAX tree during a HIG install
(`:318`, `:317`), and remotely against the receipt's array
(`:1208`, `:1207`). `manifestSha256` is recomputed and compared (`:233`) and the
staged manifest's hash is compared remotely (`:1229`). There is no hash of a
receipt and no whole-package archive digest: the manifest hash is the single
covering digest, which is exactly what makes it usable as identity.

**Ownership — out of scope.** Mode is fixed at `0755` everywhere a payload is
written (`scripts/hig-release.sh:649`, `:721`, `:1226`), and that is all. No uid
or gid is recorded or asserted; the installer inherits the invoking user's
ownership. The honest reason to leave it out: the store is a single-operator
root-owned tree, so an ownership field would record the same value on every row
and prove nothing. If the store ever becomes multi-user, ownership becomes a
real field and a real bump.

**Path safety — covered here in full.** §6 above, plus the exact-contents check
(`scripts/hig-release.sh:168`) and the no-symlink-payload check
(`scripts/hig-release.sh:193`).

**Monotonic publication sequence — covered here.** `activationSequence`
(`scripts/hig-release.sh:775`, `:1251`), minted by `next_activation_sequence`
(`scripts/hig-release.sh:242`, `:1121`), and used as the primary ordering key
for both retention and rollback (`:506`, `:1155`). It is monotonic **per install
root** — the counter file lives under that root (`:133`) — and it is a record,
never part of the ID.

**Staging, fsync and atomic publish — partly covered; the boundary is stated.**
Staging plus a same-directory `mv` publishes the tree
(`scripts/hig-release.sh:718`–`:725`, `:1224`–`:1232`). Pointer replacement is
atomic: `atomic_symlink` builds the link in a sibling temp directory and
`os.replace`s it into place (`:428`–`:430`), so `current` never transiently
vanishes. `fsync` covers exactly one file — the activation-sequence counter
(`:261`–`:262`, `:1140`–`:1141`). Neither the staged binaries, nor the copied
manifest, nor either receipt is fsynced, and no directory is fsynced after the
`mv`. The publish is therefore atomic against a concurrent reader and not
durable against power loss. That is the frozen boundary; closing it is a script
change, not a schema change, and needs no new field.

**Locks — partly covered; the gap is named.** The only lock in the release path
is `flock(LOCK_EX)` on the activation-sequence file
(`scripts/hig-release.sh:254`, `:1133`), held for the read-increment-write only.
There is no lock spanning an activation. Two concurrent installs of different
releases into one install root would each take a distinct sequence number and
then race on `current`. The race is bounded rather than prevented: staging
directory names are unique (`:718`), the tree `mv` is per-release, and
`atomic_symlink` leaves `current` pointing at one of the two releases and never
at nothing (`:430`). Serializing a whole activation is out of scope for a schema
freeze.

**Active pointer — covered here.** `current` is the one active pointer: an
atomic symlink into `releases/` (`scripts/hig-release.sh:734`, `:1263`), with
every public binary link pointing at `current/<name>` rather than at a release
directory (`:443`–`:445`), which is what keeps public paths stable across
activations. A HIG install refuses unless HAX's `current` actually resolves to
the release directory it claims (`:312`). The pointer's path is also recorded on
the receipt as `currentLink` (`:780`).

**Derived current receipt — out of scope, by design.** There is no
`current.receipt.json` and this freeze does not add one. The current release's
receipt is reached by reading `current`'s target and taking its basename as the
ID (`scripts/hig-release.sh:312`, `:325`), or by taking the first row of
`release_entries` (`:506`–`:508`). A derived file would be a second writable copy
of a fact the symlink already holds, which is the shape ADR-030 explicitly
rejected for deployments: "There is no hand-writable current-release table that
can diverge from the immutable ledger." Cross-environment current state is
`kanban deploy current` (`rust/lib.rs:104`).

**Pins — partly covered; the omission is deliberate.**
`cargo build --release --locked` pins every crate to `Cargo.lock`
(`scripts/hig-release.sh:640`), and packaging refuses outright unless the
`skills/kb` submodule is initialized (`scripts/hig-release.sh:629`–`:630`).
Neither the lockfile hash nor the `skills/kb` gitlink SHA appears in any
manifest or receipt field, and should not: both are named by `sourceCommit`
(`:593`) under `sourceTreeClean` (`:594`, earned at `:633`), so an explicit pin
field would be a second copy of a fact the commit already fixes — the exact
duplication that let the binary list fall four behind the crate
(ADR-034, addendum 2026-09-06).

**Release-published events — out of scope.** The script emits no board event. It
never invokes `kanban` for anything except a version probe
(`scripts/hig-release.sh:101`, `:104`) and contains no `deploy` call, so a
release going live is invisible to
[ADR-031](ADR-031-ledger-first-pubsub-uses-the-append-only-event-ledger.md)'s
event ledger unless an operator or a wrapper records it — which today is the
deploy ledger's job (`rust/lib.rs:96`–`rust/lib.rs:101`). Emitting from the
script would give a host-install path a board dependency and a board write.
That is a separate decision about coupling, not a schema field.

**Deployment artifact URIs — covered by the deploy ledger.** `deploy finish`
accepts `--artifact-uri URI` alongside `--served-commit FULL_SHA`
(`rust/lib.rs:101`). The release path has no URI concept because a release is
never fetched over a network: the package is `tar`-piped over ssh from the build
host into a path the caller names (`scripts/hig-release.sh:913`). The natural
artifact URI for a kanban release is its release directory, which the receipt
already carries as `releaseDir` (`scripts/hig-release.sh:779`).

## Consequences

Nothing executable changes, so nothing can regress. What changes is that a field
addition now has a stated cost and a stated place.

The manifest is the hard surface and the receipts are the soft one. Adding to a
receipt is additive and cheap — existing validators use `jq -e` predicates over
named fields (`scripts/hig-release.sh:290`) and ignore fields they do not
mention. Adding to the manifest re-hashes it, so every `releaseId` on both hosts
changes, every `releases/<id>` directory is renamed, every recorded ID in a
report or a board row goes stale, and the store shows a full set of new
releases. Freezing this makes the second option visibly expensive instead of
accidentally cheap.

Three constants stay constants. `formatVersion`, `targets` and `sourceTreeClean`
are literals at the writer, and consumers must not branch on values they cannot
observe. The temptation this closes is writing a `sourceTreeClean: false`
handler; the script refuses at `scripts/hig-release.sh:633` instead, and any
future dirty-tree release is a new decision, not a new value.

The known-unclosed items are now written down rather than discoverable only by
reading 1459 lines of bash: crash durability stops at the sequence counter, no
lock spans an activation, ownership is mode-only, and publication emits no board
event. Each is a script or coupling change and none needs a schema field, which
is why this ADR does not pre-allocate one.

Addendum 2026-09-09: §5's publication sequence gains one step and the freeze
is untouched. After the public links and the `current` flip, both install
paths call `serve_restart_and_prove`, which restarts `kanban-serve` and then
measures the process that came back in one poll — `ActiveState=active` with a
`MainPID` whose executable resolves inside the `releases/<id>` this run
activated, within a 15 s deadline — and then requires a 200 from the port the
unit's own `ExecStart` names. It runs before the activation receipt locally
and after it remotely, which is the same asymmetry §5 already froze, and a
failed measurement takes the `ERR` trap
into `rollback_activation_view` exactly like any other post-`current` failure.
No stored artifact changes: `manifest.json` and both receipts carry the same
fields with the same invariants, so no `releaseId` moves. The measurement is
reported as `serve: {restarted, mainPid, exe, http}` — or
`serve: {skipped: <reason>}` on a host with no such unit — on the install
summary printed to stdout, which §2 states is command output for the caller
rather than a stored artifact and is not covered by this freeze. Reason and
incident: ADR-034, addendum 2026-09-09. The bare `:NNN` citations above were
written against the 2026-09-06 revision of `scripts/hig-release.sh` and were
already offset before this change; they locate a claim by name, not by line.

Addendum 2026-09-09 (second): the asymmetry the addendum above recorded is
GONE, and the freeze is still untouched. The remote leg wrote its activation
receipt — and drew an activation sequence number, which this store hands out
exactly once — BEFORE the proof, and its rollback deleted that receipt only
when the same run had created the release directory. An activation onto a
directory already present therefore left a receipt for an activation that
never happened, with a sequence number spent on it, and `kb rel ls` counted
it. Both legs now write the activation receipt AFTER
`serve_restart_and_prove` returns, so a receipt exists only for an activation
that was proved, and `next_activation_sequence` is reached only on that path.
No stored field changes: the receipt written is byte-for-byte the one §5
describes, only later. `hig_release_script_install_writes_no_activation_receipt_when_the_proof_fails`
drives a refused activation onto a retained release directory through both
legs and asserts no receipt and an unmoved sequence counter.

The stdout summary — command output for the caller, not a stored artifact, and
so outside this freeze — gains two fields:
`serve: {restarted, mainPid, exe, exeSource, listener, http}`. `exeSource`
says which witness produced `exe`, `/proc/<pid>/exe` or the named test
override, so a fixture's answer can never be presented as the kernel's;
`listener` is `{port: N}` or `{socket: PATH}`, read from the unit's own
`ExecStart`, because `kanban serve` has no default listener to fall back to.
Reason and incidents: ADR-034, addendum 2026-09-09 (second).

## References

- `scripts/hig-release.sh` — the whole release path; every citation above
- [ADR-034: HIG release packages and explicit board-rule transfer](ADR-034-hig-release-packages-and-board-rule-transfer.md) — introduced the behaviour this ADR freezes the schema of; superseded by nothing here
- [ADR-030: Deployment attempts are append-only and self-archive from hot indexes](ADR-030-deployment-attempt-ledger-and-self-archiving.md) — attempt identity, served-commit gate, artifact URI, current projection
- [ADR-031: Ledger-first pubsub uses the append-only event ledger](ADR-031-ledger-first-pubsub-uses-the-append-only-event-ledger.md) — the event ledger the release path does not write to
- [ADR-008: Fail closed on ambiguous and destructive operations](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the refusal form §6 follows
- `rust/lib.rs:96`–`rust/lib.rs:102` — `deploy start` / `finish` / `abandon`; `rust/lib.rs:104` — `deploy current`; `rust/lib.rs:6467` — the version string that carries the schema numbers
- `rust/db.rs:2093`, `rust/db.rs:2434` — runtime schema refusals that stand in for release-time compatibility checks
- `tests/e2e.rs` — `hig_release_script_*`, including `hig_release_script_local_and_remote_install_guards_are_identical` and `hig_release_script_enumerates_exactly_the_executables_the_crate_declares`
- `docs/testing/graphql-agent-loop-benchmark-2026-09-05.json:303` — the live store path in use
- Kanban board: epic `e-c3c8a863`; task `t-66ca0c2d` (this ADR); decision `a-f0ced14b` (2026-09-06, the store paths stay)
