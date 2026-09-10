# ADR-044: A release is a linux x86-64 artifact whose build provenance is measured, not assumed

**Status:** Accepted
**Date:** 2026-09-10
**Amended:** 2026-09-10 — §2's "measured" framing, §3's refusal list, §3's
`HIG_RELEASE_TARGET_RUNNER` rule and §3's claim that a lying runner cannot be
caught are each corrected in place against the landed implementation and
dated where they sit; further implementation facts are recorded in the
amendment at the end of this document.
**Deciders:** the kanban driver lane on `t-51d5505e` (claim `codex@driver`), under
George's 2026-09-10 decision on attention `a-4741a3c3`, where he selected the
choice `mbp-path` — "Add a supported MBP Linux packaging path" — with the note
"Implement truthful local Linux packaging without spoofing host identity". He
ruled on the shape: releases buildable on `@@mbp`, provenance truthful rather
than assumed, no host-identity spoofing, and the `hax`/`hig` installation
guards preserved. Field names, the container mechanism, the gate's refusal
wording and the `formatVersion` number are decided here and he may supersede
any of them by a later ADR.
**Supersedes:** the build-host clause of
[ADR-034](ADR-034-hig-release-packages-and-board-rule-transfer.md) — "The
release path needs a deterministic package that is built on `hax`" (§Context)
and "`package` is HAX-only" (§Decision) — and nothing else in it. Bumps both
receipts frozen by
[ADR-039](ADR-039-release-manifest-and-receipt-schema.md) from `formatVersion`
1 to 2, exactly as that ADR requires a new provenance field to do. ADR-039's
`manifest.json` freeze is untouched and stays at `formatVersion` 1.

## Context

**Citation convention.** A bare `:NNN` below is `scripts/hig-release.sh:NNN`
read at commit `43eb9de`, the state this ADR decides against. ADR-039's bare
citations were written against the 2026-09-06 revision and are offset from
these; they locate a claim by name, not by line.

**Amended 2026-09-10.** Citations added by the amendments below name their
enclosing function or test **first** and a line second, so a reader can
relocate a claim after any later shift. They read the wave-2 working tree at
`/Users/geoyws/work/src/.kanban-worktrees/kanban-t-f03dbe4d-gate-80aa7489`
at one settled snapshot: `scripts/hig-release.sh` sha256
`7b3424eb281862ac49d12835904edd3bca4496829c7491a0051c11f74fd02c38` (3080
lines) and `tests/e2e.rs` sha256
`63f9740d83e275cc50cc2f8dbc902da609c4c74ac098a16725dd0d48ee3c08f1` (39397
lines), resolved at 2026-09-10 08:34 MYT after the wave stopped moving. Every
number below was located by content in that snapshot, not copied forward. A
bare `:NNN` keeps its original meaning: the same file at `43eb9de`, the state
this ADR decides against.

Packaging is refused anywhere but `hax` twice over: the CLI target must be
`hax` (`:1118`) and the shell must be on a host that calls itself `hax`
(`:1137`, through `require_host` at `:65`, which compares `host_short`'s output
at `:61`). Every gate, test and build in this estate otherwise runs on `@@mbp`,
and `@@hig` is deploy-only, so the one operation that turns a reviewed commit
into a release is the one operation that cannot be run where the work happens.
That is the contradiction George resolved on `a-4741a3c3`.

The gate is also weaker than it looks, in both directions. What it checks is a
hostname: `host_short` runs `$HOSTNAME_BIN` (`:22`, `:61`), which is an
environment variable, so the authorization is a string a caller can choose.
What it does not check is the artifact. `cargo build --release --locked --bins`
runs at `:1157` with no `--target`, so the packaged binary is whatever the
build host produces natively; nothing downstream looks at a single byte of the
result's format. Run today on a Mac that passed the hostname check, this script
would package ten Mach-O arm64 binaries, hash them, write a manifest over them
and validate the package successfully, because per-file validation compares
size, sha256 and the binary's own version output (`:217`–`:225`) and never the
platform.

And the provenance is a literal. The package receipt writes `--arg host "hax"`
(`:1189`, into `host` at `:1196`) whatever machine it ran on, and three
validators assert that string back: `validate_receipt` (`:247`),
`validate_hax_activation_receipt` (`:316`) and the embedded remote copy
(`:2172`). The manifest records the source commit and per-file sha256, bytes
and version (`:1107`–`:1112`) and carries no platform, libc or toolchain field
at all. So the one artifact-borne claim about where a release came from is a
constant, and the fields that would make it a measurement do not exist.

There is no Dockerfile, no `.cargo/config.toml` and no cross toolchain in the
repository today.

## Decision

### 1. One target platform, two ways to reach it, nothing cross-guessed

A Kanban release is a **linux x86-64** artifact: ELF 64-bit, little-endian,
x86-64. That is the only platform `targets: ["hax","hig"]` (`:1102`, from
`:148`) has ever meant, and it is now stated instead of assumed.

Exactly two ways to produce one are supported:

- **native** — the build machine is itself linux x86-64 and `cargo build
  --release --locked --bins` runs on it as today (`:1152`–`:1158`), unchanged
  in flags, in `CARGO_TARGET_DIR` placement and in behaviour. This is the
  `hax` path and it must stay bit-for-bit the operation it is now.
- **container** — the build machine is not linux x86-64, and the same cargo
  command runs inside a **digest-pinned** container image whose digest is
  recorded on the receipt.

There is no third way. Cross-compiling from darwin-arm64 with a novel linker
and a novel libc is not a supported path (see §Alternatives rejected), and no
build ever "assumes" a platform: the artifact is verified (§4) and the way it
was produced is recorded (§2).

`targets` keeps its meaning — the ordered **install-target** set, `hax` then
`hig` (`:148`) — and is not a platform field. The two are different facts and
the receipt now carries both. `[[ "$target" == hax ]]` at `:1118` also stays:
it names the install-target set a package is built for, not the machine doing
the building.

The container leg mounts the worktree read-only and keeps `CARGO_TARGET_DIR`
outside it, because `:1182`–`:1183` refuse a `package` run that changed the
worktree by so much as an untracked file, and a root-owned `target/` written
into the tree from inside a container would trip exactly that. It also adds no
file to the package directory: the builder image and its digest live on the
receipt, which is a **sibling** of the package directory (`:1187`, path at
`:128`–`:130`), so the exact-contents check (`:190`–`:196`, expected set from
`:136`–`:138`) is unaffected.

### 2. Provenance is measured: both receipts move to `formatVersion` 2

The literal `"hax"` at `:1189` is deleted. In its place the package receipt
records what was actually true of the build, and ADR-039 §2's `host == "hax"`
row is retired with it.

**The bump is on the receipts only.** `manifest.json` keeps its five fields
(`:1107`–`:1112`) and stays at `formatVersion` 1, so its bytes are unchanged,
`manifestSha256` is unchanged, and no `releaseId`, no `releases/<id>`
directory and no recorded ID on either host moves. That is ADR-039 §4's rule
applied, not evaded: a new field goes in a receipt, not in the manifest. The
three artifacts version independently; they merely happened to all read 1.

The activation receipt inherits every field below, because it is written as
`$receipt + { … }` (`:1299`–`:1308` locally, `:2260`–`:2269` remotely), so it
reaches `formatVersion` 2 with no change to either writer and its eight
installer-written fields keep their invariants exactly.

#### The package receipt at `formatVersion` 2

Written at `:1188`–`:1202`. Six fields are new — `buildPlatform`,
`artifactPlatform`, `buildKind`, `builderImage`, `toolchain` and
`versionProbe` — `host` is retyped, and `formatVersion` reads 2. Every other
field in ADR-039 §2 is unchanged in name, type and invariant.

| field | type | invariant |
| --- | --- | --- |
| `formatVersion` | number | exactly `2`. Written at `:1195`; asserted at `:246`, `:315` and `:2165` |
| `host` | string | **retyped.** Non-empty, no whitespace: the short hostname of the machine that ran `package`, exactly as `host_short` (`:61`) reports it. v1 required the literal `"hax"` (`:1189`; asserted `:247`, `:316`, `:2172`) — v2 admits any non-empty short hostname and asserts shape only, because a name is now a record and never an authorization |
| `buildPlatform` | string | the **build machine**: `"<kernel>-<machine>"`, `uname -s` lowercased, a hyphen, then `uname -m` verbatim — `linux-x86_64`, `darwin-arm64`. Matches `^[a-z0-9_]+-[a-z0-9_.]+$` |
| `artifactPlatform` | string | the **binaries**: exactly `"linux-x86_64"` at `formatVersion` 2, the only platform §1 admits. Earned by §4's verification of every packaged file, never copied from a flag or from `buildPlatform` |
| `buildKind` | string | exactly `"native"` or `"container"` — a closed set, chosen by the capability gate in §3 and never inferred from another field's shape |
| `builderImage` | string or null | the key is **always present**. `"container"` ⇒ the digest-pinned reference actually run, matching `^[^[:space:]@]+@sha256:[0-9a-f]{64}$`; `"native"` ⇒ JSON `null`, which says "no image was used" rather than "unknown" |
| `toolchain` | object | exactly the two keys below, no more |
| `toolchain.rustc` | string | non-empty: the single line `rustc --version` prints **in the environment that compiled the binaries** — inside the image on the container path, on the host on the native path — trailing whitespace stripped, as `file_version` already does at `:106` |
| `toolchain.cargo` | string | non-empty: the same for `cargo --version` |
| `versionProbe` | string | exactly `"native"` or `"runner"` — a closed set, written from the **same branch** of `file_version` that produced the version strings in `files[]`, never from a flag and never from `buildKind`. `"native"` means every `files[].version` was produced by executing the artifact on the machine this receipt names in `host`; `"runner"` means it was produced through the named `HIG_RELEASE_TARGET_RUNNER` (§3), so the version was measured through that runner rather than by running the artifact on the build host |

Cross-field invariants, asserted wherever `formatVersion` is asserted:

1. `artifactPlatform == "linux-x86_64"`.
2. `buildKind == "native"` ⇒ `buildPlatform == artifactPlatform` **and**
   `builderImage == null`. A native build on a machine that is not linux
   x86-64 is not representable.
3. `buildKind == "container"` ⇒ `builderImage` is a non-null digest-pinned
   reference. `buildPlatform` is unconstrained: a linux x86-64 host may also
   build in a container, and saying so is not a contradiction.
4. `versionProbe == "native"` ⇒ `buildPlatform == artifactPlatform`: a host
   that cannot execute the artifact cannot have produced a native version
   string. The converse is not an invariant — a linux x86-64 host may
   deliberately probe through a runner, and `"runner"` says so.
5. No field is optional and none may be null except `builderImage` under
   `"native"`. A receipt missing one is refused by the existing sentence at
   `:259`.

`buildPlatform` and `artifactPlatform` are the two facts this ADR exists to
separate, and prose about them always says which: **`buildPlatform` is where
it was built, `artifactPlatform` is what it is.** On George's MBP they read
`darwin-arm64` and `linux-x86_64`; on `hax` both read `linux-x86_64`.

**Amended 2026-09-10.** Which of these fields is a **record** and which is
**earned** has to be said plainly, because "measured" reads stronger than
most of them are. Every field a v2 receipt carries about the build machine is
what that machine's own `PATH` answered, and each is one program away from a
caller who redirects it:

- `host` — whatever `host_short` gets from the program `HOSTNAME_BIN` names
  (`host_short`, `scripts/hig-release.sh:78`–`:80`; the variable at `:22`),
  now with a write-time refusal when it answers nothing usable
  (`package_create`, `:1563`–`:1566`);
- `buildPlatform` — `uname -s` and `uname -m` (`build_platform`,
  `scripts/hig-release.sh:1332`–`:1339`, the two reads at `:1334`–`:1335`);
- `buildKind` — derived from that same answer, with no second opinion
  (`require_release_build_capability`, `scripts/hig-release.sh:1376`–`:1433`,
  the branch at `:1378`–`:1383`);
- `builderImage` — the operator's own argument, checked for digest shape only
  (same function, `:1399`–`:1400`) and probed through a runtime the operator
  or `PATH` named (`container_runtime_path`,
  `scripts/hig-release.sh:1346`–`:1356`; the bounded probe at
  `:1414`–`:1415`);
- `toolchain` — `rustc --version` and `cargo --version` as they resolve in
  whichever environment compiled (`release_toolchain_version`,
  `scripts/hig-release.sh:1445`–`:1474`, in the image at `:1455`–`:1456` and
  on the host at `:1467`).

None of that is tamper-proof and none of it needs to be: it is a record of
what the machine reported, and §3's point is that the record authorizes
nothing. The script says the same in its own comments, over `build_platform`
(`scripts/hig-release.sh:1312`–`:1319`) and over the gate (`:1346`–`:1355`).

Exactly one field is **earned**: `artifactPlatform`. It exists only because
§4's gate read the ELF header bytes of every file — `require_release_platform`
(`scripts/hig-release.sh:252`–`:291`), run on the build output
(`package_create`, `:1614`), on every packaged copy (`package_validate`,
`:1247`), on the staged and live trees (`validate_release_files`, `:329`) and
again on the remote leg (the `REMOTE` heredoc's per-file loop, `:2790`) — and
that is a claim no `PATH` entry can supply. `versionProbe` is a third kind:
not a claim about the machine but a record of **which branch** answered
(`version_probe_kind`, `scripts/hig-release.sh:117`–`:123`). Its `"native"`
value leans on the same `PATH`-reported `buildPlatform`
(`receipt_provenance_defs`, `:389`), and even a faked `buildPlatform` cannot
buy it, because that branch has to execute the artifact — see the amendment
at the end of this document.

Measured rather than argued. The suite itself produces a receipt reading
`buildKind` `native`, `buildPlatform` `linux-x86_64` and `builderImage` `null`
on this darwin-arm64 machine out of nothing but a `uname` stub on `PATH`:
`hig_release_script_package_builds_natively_when_the_machine_is_already_the_release_platform`
(`tests/e2e.rs:32001`–`:32041`) sets `FAKE_UNAME_S` and `FAKE_UNAME_M`
(`:32007`–`:32008`) against the stub written at `tests/e2e.rs:13648`, whose
defaults are `Darwin` and `arm64` (`:13662`, `:13665`), and asserts those
three fields at `:32023`–`:32030`. The recorded `toolchain` is likewise
whatever the `rustc` and `cargo` stubs answered (`tests/e2e.rs:13674`–`:13680`,
set at `:32010`–`:32011`, asserted at `:32032`). A receipt of that shape
installs on both legs — the `a-native-linux-build` row of
`hig_release_script_install_refuses_a_receipt_whose_provenance_could_not_have_been_produced`
(`tests/e2e.rs:35767`, installed at `:35778` and `:35811`) — and the reviewer
of the landed wave confirmed end to end on 2026-09-10 that every validator
accepts one produced this way. `artifactPlatform` stayed `linux-x86_64`
throughout, because the bytes were read. So a v2 receipt reads as what the
build machine said plus one field the artifact itself had to satisfy, never as
a tamper-proof document.

Every site that asserts a receipt is updated in one cut, as ADR-039 requires:
`validate_receipt` (`:245`–`:259`), `validate_hax_activation_receipt`
(`:314`–`:335`) and the embedded remote package-receipt validator
(`:2164`–`:2179`). Each replaces `formatVersion == 1` with `== 2`, drops
`.host == "hax"`, and adds the six fields' shape checks and the four
cross-field invariants. The two manifest validators (`:198`–`:210`,
`:1066`–`:1078`) are **not** touched, and neither is the remote leg's per-file
re-verification, which reads bytes, sha256 and version out of the receipt's
own `files` array rather than a manifest (`:2182`–`:2188`): the manifest did
not change.

Two guards deliberately stay as they are. `reject_carried_release_identity`
(`:168`–`:179`) keeps its forbidden-key list: none of the six is an activation
field, so a package receipt legitimately carries all of them, and adding one to
that list would refuse every valid v2 package. And `.target == "hax"` (`:317`)
and `.installerHost == "hax"` (`:318`) keep asserting `hax` — they are facts
about the machine that **activated** a release, which is still `hax`, and
ADR-039 §3's rule that `host` and `installerHost` are different facts is now
load-bearing rather than incidental.

### 3. The host gate becomes a capability gate

`require_host hax` at `:1137` is replaced by a gate that measures whether this
machine can produce a genuine linux x86-64 artifact, and picks the `buildKind`
that will:

1. `uname -s` is `Linux` and `uname -m` is `x86_64` → `buildKind=native`.
   Build as today (`:1152`–`:1158`).
2. Otherwise → `buildKind=container`, permitted only when **all** of these
   hold: a usable container runtime is found; the operator named a builder
   image that is digest-pinned; and that image actually runs linux x86-64,
   measured by running `uname -m` inside it and requiring `x86_64`. The image
   is named by `--builder-image <ref>@sha256:<64 hex>` or, absent the flag, by
   `KANBAN_RELEASE_BUILDER_IMAGE`; there is no default image, because a
   default would be an unpinned promise.
3. Neither → refuse, before anything is built or written.

**Which runtime.** Any OCI runtime qualifies, and the requirement is stated
as a capability rather than a brand: the runtime must accept
`run --rm --platform linux/amd64 <image>@sha256:<digest> <command>`.
Detection is `command -v` over the ordered list `docker`, `podman`,
`nerdctl`, first hit wins, overridable in full by
`KANBAN_RELEASE_CONTAINER_RUNTIME` naming an executable — the same override
the tests use, so no test needs a real daemon. On George's MBP that resolves
to `docker`, which is the Docker dependency the Consequences section names.
The runtime is deliberately **not** a receipt field: `builderImage` already
names the bytes the build ran on by digest, and which local CLI drove it
proves nothing about them. Nothing is inferred from the runtime's name: a
runtime on `PATH` that cannot actually run the image's `linux/amd64` fails
the `uname -m` measurement and refuses like any other missing capability.

The gate never reads a hostname. `HOSTNAME_BIN` (`:22`, `:61`) survives as
what it already is — a recording seam the tests set, and the value that lands
in `host` — and it can no longer authorize anything, which is what "no
host-identity spoofing" means concretely: a spoofed name now buys a false
record and no permission, and the record is checked for shape, not for
identity.

Every refusal is one `hig-release:`-prefixed sentence naming the platform this
machine is and what was missing, per ADR-008:

- `hig-release: cannot package a linux x86-64 release on darwin-arm64: no container runtime found; none of docker, podman, nerdctl is on PATH and KANBAN_RELEASE_CONTAINER_RUNTIME is unset, so there is no linux x86-64 build environment on this machine`
- `hig-release: cannot package a linux x86-64 release on darwin-arm64: no container runtime found; KANBAN_RELEASE_CONTAINER_RUNTIME names <value>, which is not an executable program, so there is no linux x86-64 build environment on this machine`
- `hig-release: cannot package a linux x86-64 release on darwin-arm64: no builder image was named; pass --builder-image <ref>@sha256:<64 hex> or set KANBAN_RELEASE_BUILDER_IMAGE`
- `hig-release: cannot package a linux x86-64 release on darwin-arm64: builder image rust:1.90-bookworm is not digest-pinned, and a tag can move, so its digest would not name the bytes that built this release`
- `hig-release: cannot package a linux x86-64 release on darwin-arm64: builder image <ref>@sha256:<digest> does not run linux x86-64; uname -m inside it reported aarch64`

Each has one wording, unit-pinned on its branch.

**Amended 2026-09-10.** The second sentence above is new, and the first is
narrower than it reads: "no container runtime found" is two different
absences, and the landed gate says them apart rather than papering over them
(`require_release_build_capability`, `scripts/hig-release.sh:1388`–`:1395`).
An operator who set `KANBAN_RELEASE_CONTAINER_RUNTIME` to something
`command -v` cannot resolve is told what they named and that it is not an
executable program (same function, `:1391`–`:1392`; the search it replaces is
`container_runtime_path`, `:1346`–`:1356`); only an operator who named nothing
is told the variable is unset, and that sentence is the one that also names
the three runtimes it searched (`:1395`, list at `:34`). Telling the first
operator their variable was unset would have been false, which is why one
wording could not cover both. `<value>` above is the value as the operator set
it, printed verbatim. Both wordings are pinned byte for byte by the
`no-runtime` and `unusable-runtime` rows of
`hig_release_script_package_refuses_a_machine_that_cannot_produce_a_linux_x86_64_artifact_naming_what_was_missing`
(`tests/e2e.rs:32056` and `:32064`, compared at `:32172`, the shared prefix
constant at `:31762`–`:31763`).

**Amended 2026-09-10, second pass.** The list above is not exhaustive of the
landed gate, and this ADR does not pretend otherwise. Both container probes
were bounded after this ADR was accepted — one `uname -m` and one
`--version`, each run through `run_bounded`
(`scripts/hig-release.sh:1287`–`:1310`) with the ceiling from
`container_probe_seconds` (`:1317`–`:1322`), while the build between them
stays deliberately unbounded — and bounding them added these branches:

- a deadline refusal when the image does not answer the `uname -m` probe
  (`require_release_build_capability`, `scripts/hig-release.sh:1419`, the
  bounded call at `:1414`–`:1415`) — pinned by the `wedged-runtime` row,
  which also measures that the refusal arrives inside the deadline
  (`tests/e2e.rs:32090`, timing assertion at `:32161`);
- a refusal carrying the runtime's own complaint when that probe fails for
  any other reason (same function, `:1426`) — pinned by the
  `failing-runtime` row (`tests/e2e.rs:32102`);
- the same deadline refusal around the toolchain probe
  (`release_toolchain_version`, `:1459`, the bounded call at
  `:1455`–`:1456`) — pinned by
  `hig_release_script_package_refuses_a_container_toolchain_probe_that_never_answers`
  (`tests/e2e.rs:32199`, wording at `:32232`–`:32233`, timing at `:32243`);
- the toolchain probe's own complaint refusal (same function, `:1463`) and
  the deadline override's validation refusal (`container_probe_seconds`,
  `:1319`–`:1320`), neither of which any test names — measured 2026-09-10:
  `must be a whole number of seconds` occurs in `scripts/hig-release.sh` and
  nowhere in `tests/e2e.rs`. Both are listed as owed in §Evidence required;
- and a refusal when `host_short` reports nothing usable (`package_create`,
  `:1563`–`:1566`), which enforces §2's non-empty, no-whitespace `host`
  invariant where the receipt is **written** and not only where it is read —
  pinned inside
  `hig_release_script_package_records_the_real_build_host_platform_and_toolchain`
  (`tests/e2e.rs:31892`).

Their **branches** are recorded here; the two unpinned **wordings** are
deliberately not quoted, so that nothing in this ADR pins a sentence no test
holds. Whoever adds those two tests pins each wording on its branch, exactly
as the five above are.

`install_package`'s `require_host hax` (`:2431`) is **not** touched. Installing
and rolling back are still launched on `hax` for both targets, exactly as
ADR-034 decided; only packaging moved. An MBP-built package therefore has to
reach `hax` before it can be installed, and this ADR adds no transport for
that: the operator moves the package directory and its sibling receipt, and
`install` validates both on arrival with the same validators (`:1220`,
`:1436`).

One consequence of packaging off-host falls out of `file_version` (`:93`–
`:107`, embedded verbatim in the remote installer at `:1488`), which
**executes** each packaged binary: at `:1170` while building the `files` array
and again at `:223` during package validation. A darwin host cannot execute
one at all, and the wall is lower than "wrong architecture": bash itself
hard-codes an ELF-magic sniff in `check_binary_file`, so a file whose first
four bytes are `7f 45 4c 46` exits 126, "cannot execute binary file", on
macOS regardless of what is inside it. Measured on `@@mbp` on 2026-09-10.

So running a target-platform binary becomes a named seam rather than an
assumption. `HIG_RELEASE_TARGET_RUNNER` is **how this host runs a
target-platform binary** — named for that, not for the version string it
happens to elicit, precisely so it cannot read as a second source of
`files[].version`:

- **Unset** means this host can execute the release artifact directly. That
  is `hax` and `hig`, and it leaves the native path byte-identical in
  behaviour to today.
- **Set**, it is invoked as
  `"$HIG_RELEASE_TARGET_RUNNER" <binary-path> <version|--version>` and must
  print exactly what running the binary would have printed. The container
  path sets it to the invocation that dispatches into the **same** pinned
  image the build ran in, against the same package directory.
- It can **never weaken a gate**. The ELF header read of §4 and every sha256
  and byte-size read go straight to the bytes on disk (`:1168`–`:1169`,
  `:217`–`:222`) and never pass through the runner, so a runner cannot
  make a wrong-platform or wrong-hash payload look right.
- A runner that fails, or that prints nothing, is a **refusal** naming the
  file — never an empty `version` recorded into the manifest. An unanswered
  probe is not a version.

**Amended 2026-09-10.** The container path sets `HIG_RELEASE_TARGET_RUNNER`
**only when the operator named no runner**: the assignment is guarded by
`[[ "$RELEASE_BUILD_KIND" == container && -z "${HIG_RELEASE_TARGET_RUNNER:-}" ]]`
(`package_create`, `scripts/hig-release.sh:1604`–`:1606`), so a runner an
operator exported is theirs and is kept, and the script never overwrites it
with the image dispatcher it would otherwise write
(`release_container_target_runner`, `scripts/hig-release.sh:1500`–`:1520`).
Provenance is unaffected, because `versionProbe` records the **branch** and
not who supplied the program: the branch is one reading of whether the
variable is non-empty (`version_probe_kind`,
`scripts/hig-release.sh:117`–`:123`), `file_version` probes through that same
reading (`file_version`, `:157`), and the receipt is written from that same
function (`package_create`, `:1652`), so both cases record `"runner"` — which
is exactly what is true of both: the version came through a named runner
rather than from the artifact executing on the machine `host` names. Which
bytes the build ran on is named by the digest either way (`package_create`,
`:1649`, into the field at `:1663`). Both halves are measured. With a runner
exported, every packaged binary goes through it and the image is never asked
for a version
(`hig_release_script_package_records_which_branch_measured_the_version`,
`tests/e2e.rs:32379`–`:32440`: the runner log at `:32405`, the untouched image
at `:32413`) while the receipt reads `"runner"` (`:32398`); with none
exported, no operator runner is called at all, the receipt still reads
`"runner"`, and every recorded version came back through the pinned image
(`hig_release_script_package_records_the_pinned_builder_image_digest_on_the_container_path`,
`tests/e2e.rs:31918`–`:31987`: `:31925`, `:31940`, `:31943`,
`:31967`–`:31984`).

What the seam does **not** buy, stated plainly because the previous draft of
this ADR overclaimed it: when a runner is set it is not one source among two,
it is the **only** source of `files[].version`. A lying runner writes its lie
into `files[].version`, into `manifest.json` and into both receipts, and
`package_validate` then compares that value to itself (`:223` against
`:226`), so nothing downstream can catch it. `files[].version` keeps
ADR-039's invariant in the sense ADR-039 meant it — the string is whatever
asking the binary for its version produced, never a value composed by the
script — and no more than that. The runner is therefore trusted exactly as
much as the pinned image it dispatches into, which is why the image must be
digest-pinned (§3) and why the receipt says which branch answered:
`versionProbe`, specified in §2, makes "this version came through a named
runner" a recorded fact instead of an invisible one. A reader who needs a
version string that was produced by executing the artifact on the machine
that reports it must require `versionProbe == "native"`.

**Amended 2026-09-10.** "Nothing downstream can catch it" was too
pessimistic, and the reviewer of the landed wave measured the catch. Both
install legs **re-execute** every packaged binary and compare its answer
against the recorded string, so a lie told at package time and not repeated
at install time dies there:

- the `hax` leg re-executes in `validate_release_files`
  (`scripts/hig-release.sh:293`–`:344`, the comparison at `:340` against the
  manifest's own `files[].version` read at `:343`), reached through
  `install_release_tree`'s `validate_package` (`:1693`) **before**
  `ensure_safe_release_view` and `mkdir -p "$install_root/releases"`
  (`:1699`–`:1700`);
- the `hig` leg re-executes twice more: `package_validate` on the sending
  side (`scripts/hig-release.sh:1222`–`:1259`, comparison at `:1255`, called
  from `install_remote` at `:1909`, ahead of the `hax` activation check at
  `:1913` and every path the install computes at `:1914`), and the remote
  script's own per-file loop against the receipt's `files` array (the
  `REMOTE` heredoc, `:2794`, source at `:2795`);
- `validate_hax_activation_receipt` re-executes the already-installed release
  as well (`scripts/hig-release.sh:451`–`:508`, comparison at `:506`).

The first two emit the same sentence — `hig-release: package binary kanban
version mismatch` (`validate_release_files`, `scripts/hig-release.sh:341`;
`package_validate`, `:1256`) — and both run before the install root gains a
`releases/` directory, which is what the reviewer measured on 2026-09-10 from
a package built through a lying runtime.

What remains uncaught is narrower, and naming it is the point: a runner that
lies **consistently** — one whose lie is repeated at install time because the
installing host dispatches through the same program. `file_version` honours
`HIG_RELEASE_TARGET_RUNNER` wherever it runs
(`scripts/hig-release.sh:157`–`:161`), so such a host reproduces the same
false string and the comparison passes. `hax` and `hig` set no runner: the
production branch executes the artifact itself (`file_version`,
`:163`–`:164`), so on the hosts that actually install, the comparison is the
artifact's own answer, and that branch carries its own coverage
(`hig_release_script_probes_the_version_by_running_the_binary_when_no_runner_is_configured`,
`tests/e2e.rs:31636`–`:31678`). That is what `versionProbe` is for: a
`"runner"` receipt says the string is trusted exactly as far as the
digest-pinned image that answered, and a reader who needs the artifact itself
to have answered requires `"native"`.

The seam carries the same obligation as every other guard in this script: it
exists in both the local `file_version` and its remote heredoc twin, and
`hig_release_script_local_and_remote_install_guards_are_identical` pins the
two copies by naming `file_version` in its list of pinned functions — a
guard absent from that list is not pinned at all, which is what this ADR
requires the list to fix.

### 4. Every release binary is verified to be linux x86-64 wherever it is measured

This is what makes §2 and §3 safe to trust, and it is the only check in the
release path that looks at what a file *is*.

The check runs **at every point a release payload is measured**, and at each
one it runs **first**:

- in `package_create`, **once on the build output** (`:1164`–`:1165`) before
  `install -m 0755` copies it into the package directory (`:1166`), and
  therefore before any sha256, byte count or version is taken of it at all
  (`:1168`–`:1170`). Refusing before the copy is what leaves the output
  directory with nothing in it: no binary, no manifest (`:1180`), no receipt.
  The packaged copy is re-verified at package time by `package_validate`
  (`:1184`), before the receipt is written (`:1188`–`:1202`);
- in `validate_release_files` (`:181`) and `package_validate` (`:1059`),
  before the per-file size and hash comparisons those functions perform
  (`:217`–`:222`, in the manifest-driven loop at `:213`–`:226`) — which puts
  it on the staged tree before the publishing `mv` (`:1244`) and on the live
  tree after activation (`:1248`, `:1283`);
- in the remote installer's own per-file loop (`:2182`–`:2188`), before its
  size and hash comparisons;
- in `rollback_release` (`:2319`), through the same
  `validate_release_files` (`:2378`), so a release being put **back** into
  service is re-verified. Reactivating a retained release directory does not
  escape the gate, and that is intended: a retained payload that is not linux
  x86-64 is not something to hand a live listener.

Ordering is the whole point at the install sites: a forged package whose
manifest and receipt agree perfectly with a foreign binary's own size and hash
passes every other check, because size and hash are values the forger
controls. The platform gate is the only check that can refuse it, and it
refuses it before a byte is copied into a release store. That covers the one
realistic route a foreign binary has to `hig` — replacement of the staged copy
after transfer and before the remote script runs.

The rule, in full:

- Five facts must be established from the **file's own header** — magic
  `7f 45 4c 46`, `EI_CLASS` = 2 (64-bit), `EI_DATA` = 1 (little-endian),
  `e_machine` = `0x3e` (x86-64), and `e_type` of either `ET_EXEC` (2) or
  `ET_DYN` (3), the two object types a Rust release binary can legitimately
  be. `e_type` is read, not assumed: a relocatable object or a core file
  carrying the right machine is refused. Reading
  those bytes directly and asking a header-reading tool such as `file(1)`
  both satisfy the rule; **executing the file does not**, and neither does
  trusting the name of a flag. A check that ran the artifact could not work
  on the very host this ADR exists to support, and the implementation's own
  choice is pinned by its tests rather than by this document.
- A file that yields no readable header is **refused, never skipped** —
  whether it is too short, empty or unreadable. A truncated payload is a
  failed build, and a check that could not read a header has established
  nothing. One sentence covers the whole case, so no reader has to guess
  which of the three it was: it names how many header bytes are needed and
  how many the file produced.
- A file that is not linux x86-64 is refused with one
  `hig-release:`-prefixed sentence naming **the file** and **what it actually
  is** — Mach-O, an ELF of another class, data order or machine, or not an
  ELF image at all — against the named requirement, linux x86-64
  (ELF 64-bit, little-endian). Quoting the observed magic or the observed
  class/data/machine triple is what makes the sentence a measurement rather
  than a verdict.
- The check is one function, and any copy of it inside the remote installer
  is **verbatim**, like every other guard in this script, so
  `hig_release_script_local_and_remote_install_guards_are_identical`
  (`tests/e2e.rs:31954`) fails if the two drift.
- The check runs on both paths, native and container. On `hax` it must always
  pass; a day it does not is a day the toolchain changed under us.
- It contributes **no field to `files[]`**. The same `$files` array is written
  into `manifest.json` (`:1180`, via `:1112`) and into the package receipt
  (`:1192`), so a per-file platform field would re-hash the manifest and
  rename every `releaseId` on both hosts for a fact that is identical on every
  row. The verdict is a gate whose one recorded consequence is
  `artifactPlatform` on the receipt — earned by measurement exactly as
  `sourceTreeClean` (`:1111`) is earned by the two worktree reads at `:1149`
  and `:1182`.

### 5. Reading a `formatVersion` 1 receipt

A v1 receipt stays readable and stays history. What a reader must do with one:

- Read its `host` as an **assertion, not a measurement**: it says the script
  wrote `"hax"`, which under v1 it did unconditionally (`:1189`). It is
  evidence that packaging ran under the `require_host hax` gate of the day,
  and nothing about the platform, the image or the toolchain.
- **Never backfill.** A v1 receipt does not gain `buildKind: "native"`,
  `artifactPlatform: "linux-x86_64"`, `versionProbe: "native"` or a toolchain
  by inference, and no
  migration writes those fields into a stored receipt. A receipt is
  write-once per release ID (ADR-039 §3); synthesising provenance into one
  would be the invented provenance this whole change exists to remove.
- Expect a v2 script to **refuse a v1 package receipt** at `:246`, `:315` and
  `:2165`. That is correct rather than unfortunate: a package is ephemeral —
  built and installed in one operation, and a failed `package` leaves nothing
  behind (`cleanup` at `:31`–`:39`, under the trap at `:41`) — so there is no
  fleet of old package receipts to carry forward.
- Expect a v1 **activation** receipt already under `releases/` to keep
  working, because the readers that manage the store do not read
  `formatVersion` at all: `release_entries` (`:1006`, reading at `:1015`) reads
  `activationSequence`, `installedAt` and `releaseId`;
  `ensure_managed_activation_receipt` (`:407`–`:437`) reads `releaseId`,
  `sourceCommit`, `manifestSha256` and `activationSequence`. Releases
  activated under v1 therefore stay listable, prunable, re-installable and
  rollback-able. Only `validate_hax_activation_receipt` (`:291`) requires v2,
  and it is reached solely by a HIG install of a package built by this
  script — which is a v2 package by construction.

So: older receipts are readable, and not re-verifiable against fields they
never carried.

### 6. What this does not change

Stated because a packaging change is exactly the kind of change that quietly
takes an install guarantee with it:

- **The install legs' behaviour.** `install_local`, `install_remote` and
  `rollback` change in exactly two ways, and the two are independent — each
  lands in its own wave and neither brings the other. §4's platform gate adds
  one refusal ahead of the per-file size and hash comparisons in
  `validate_release_files`, `package_validate` and the remote loop; §2's
  provenance change retypes the three `formatVersion`/`host` assertions those
  legs already make on a receipt. Both are refusals added to legs that
  already refuse; neither moves a step, a write, a pointer or a receipt.
  Every refusal in ADR-039 §6 stands, the publication sequence of ADR-039 §5
  stands, and `require_host hax` at `:2431` stands.
- **The byte-identical twin requirement.** The local and embedded-remote
  guards remain literal twins, and
  `hig_release_script_local_and_remote_install_guards_are_identical`
  (`tests/e2e.rs:31954`) remains the thing that fails when they drift. The
  receipt-validator edit in §2 lands in both copies in one cut, or that test
  fails — which is the point of it.
- **The served-exe proof.** `serve_restart_and_prove` (`:604`) and the
  recovery it falls into (ADR-034, addenda 2026-09-09) are untouched. An
  install is still green only when the process answering the unit's own
  listener resolves to the `kanban` binary of the release just activated.
- **Retention and rollback.** `MAX_RELEASES` is still 10 (`:21`) and retention
  is still keyed on `activationSequence` then `installedAt` — `release_entries`
  at `:1006` and its remote twin at `:2115` — and a failed activation still
  leaves no receipt to be counted.
- **Identity.** `releaseId` is still `sourceCommit-manifestSha256`, still
  derived from manifest bytes that did not change, and still carried by no
  artifact that is hashed into it (ADR-039 §4).
- **The board's rules.** No board rule names `hax` as the build host: the
  nearest, `r-7af4dd57`, says `hig` "receives byte-identical releases only as
  the release script's second target", which is about install targets and
  stays true. Nothing on the board needs retiring for this decision;
  ADR-034's clause was the only written contradiction.

## Consequences

- Packaging becomes possible where the work already happens. `@@mbp` can turn
  a reviewed commit into a release without pretending to be `hax`, and `hax`
  keeps its native path unchanged.
- **The MBP path depends on Docker.** A workstation without a working
  container runtime cannot package, and it will say so in one sentence naming
  what was missing rather than building something else. There is also no
  default image: the first MBP release needs a digest chosen and recorded
  somewhere durable, and a digest is a thing that must be refreshed
  deliberately when the toolchain moves.
- **Older receipts are readable but not re-verifiable** against
  `buildPlatform`, `artifactPlatform`, `buildKind`, `builderImage` or
  `toolchain`. A v1 activation receipt in the store keeps working for
  listing, pruning and rollback; it can never answer what platform or
  toolchain produced its bytes, and nothing will invent an answer.
- A v2 script refuses a v1 package receipt. In practice this is invisible —
  packages do not outlive the operation that builds them — but an operator
  holding a package directory from before 2026-09-10 must rebuild rather than
  install it.
- Provenance stops being a constant, so it can now be *wrong* in a way v1
  could not: a caller who points `HOSTNAME_BIN` at a script gets a false
  `host` on a real receipt. That is deliberate and bounded — the name never
  authorizes anything, and every claim a machine could fake is beside a claim
  measured from the artifact itself.
- Two more things must be recorded for every release, and the receipt is the
  cheap surface (ADR-039 §Consequences): six added receipt fields cost no
  `releaseId` churn, and every release directory on both hosts keeps its name.
- The ELF check adds a refusal that can fail a build host we currently trust.
  If `hax` ever produces something that is not linux x86-64, packaging stops
  instead of shipping it, which is the trade this ADR is buying.

## Alternatives rejected

- **Cross-compile from darwin-arm64 with a novel toolchain.** Adding a
  `x86_64-unknown-linux-gnu` target, a cross linker and a libc story would put
  a second, differently-produced binary into the same release store, and its
  correctness would rest on a toolchain nobody in this estate runs elsewhere.
  A container runs the *same* cargo command in the *same* kind of environment
  `hax` uses, which is why it can be trusted with a one-line digest instead of
  a new build system. Rejected as more novelty than the problem earns.
- **Keep `hax`-only builds and live with the contradiction.** This was the
  option George was offered on `a-4741a3c3` and declined. It also leaves the
  hostname gate — an environment variable — standing in for a platform check,
  which is the actual defect: the gate would have permitted a Mach-O release
  on any machine willing to answer `hax`.
- **Record the platform per file in `manifest.json`.** Truthful, and it
  re-hashes the manifest, renames every `releases/<id>` on both hosts and
  invalidates every recorded `releaseId` — for a value identical on all ten
  rows. Rejected under ADR-039 §4.
- **Rename `host` to `buildHost`.** Clearer in isolation, but `host` already
  means the build host in ADR-039 §2 and §3, its pairing with `installerHost`
  is already documented, and a rename would touch consumers for no new fact.
  Retyping its admissible values and saying so is the smaller true change.

## Evidence required

Compiled-process tests, per ADR-006, driving the real script.

**§4, landed with the platform gate:**

- `hig_release_script_refuses_to_package_a_binary_that_is_not_linux_x86_64` —
  Mach-O, ELF32/i386, ELF64 aarch64, and right class and machine with the
  wrong byte order, each refused before a byte reaches the output, so no
  manifest, no receipt and nothing in the package directory
- `hig_release_script_refuses_to_install_a_package_binary_that_is_not_linux_x86_64`
  — both legs, on a package whose manifest and receipt agree perfectly with
  the foreign image's own size and hash
- `hig_release_script_remote_install_refuses_a_staged_binary_that_is_not_linux_x86_64`
  — the staged copy replaced after transfer and before the remote script
  runs, refused without creating a release store
- `hig_release_script_refuses_a_payload_with_no_readable_executable_header`
- `hig_release_script_runs_target_binaries_through_the_configured_target_runner`
- `hig_release_script_refuses_a_target_runner_that_reports_no_version`

**Negative control, measured rather than assumed** (reported by the
implementing lane on 2026-09-10): with the platform gate's call sites
deleted, all four platform cases fail, and the forged remote package is
stopped by nothing but "remote binary kanban has the wrong size" — a value
the forger controls. With the version refusals reverted, packaging *succeeds*
and records an empty version string, which is the failure mode §3's runner
rule forbids. A gate whose removal changes no test is not a gate.

**§2 and §3, landed 2026-09-10** with the capability gate, the container path
and the `formatVersion` 2 receipts:

- `hig_release_script_package_records_the_real_build_host_platform_and_toolchain`
  (`tests/e2e.rs:31777`) — which also pins the write-time `host` refusal
  (`:31892`)
- `hig_release_script_package_records_the_pinned_builder_image_digest_on_the_container_path`
  (`tests/e2e.rs:31918`)
- `hig_release_script_package_refuses_a_machine_that_cannot_produce_a_linux_x86_64_artifact_naming_what_was_missing`
  (`tests/e2e.rs:32047`) — six refusal rows, including the fifth wording §3
  now pins (`:32064`) and the two bounded-probe wordings §3's second-pass
  amendment records (`:32090`, `:32102`)
- `hig_release_script_package_refuses_a_builder_image_that_is_not_digest_pinned`
  (`tests/e2e.rs:32253`)
- `hig_release_script_package_records_which_branch_measured_the_version`
  (`tests/e2e.rs:32379`) — the `versionProbe` field: `"runner"` with a runner
  (`:32398`), and the native branch refusing on a host that cannot execute
  the artifact (`:32419`–`:32439`), which is the coverage boundary named in
  the amendment below
- `hig_release_script_provenance_fields_leave_the_manifest_bytes_and_release_id_unchanged`
  (`tests/e2e.rs:32447`)
- `hig_release_script_install_refuses_a_format_version_1_package_receipt`
  (`tests/e2e.rs:35863`)
- the twin test's pinned list now names `file_version`
  (`tests/e2e.rs:33952`), and with it `release_binary_known` (`:33953`),
  `version_probe_kind` (`:33957`) and `receipt_provenance_defs` (`:33963`),
  so neither the runner seam nor the provenance definition can drift between
  the local functions and the remote heredoc

Landed with the bounded container probes, which this ADR did not foresee:

- `hig_release_script_package_refuses_a_container_toolchain_probe_that_never_answers`
  (`tests/e2e.rs:32199`) — a wedged probe refuses inside its own deadline
  rather than hanging packaging (`:32243`)

**Still owed** (as of 2026-09-10):

- `hig_release_script_refuses_a_relocatable_object_carrying_the_right_machine`
  — the `e_type` half of the header check, which an `ET_REL` fixture with a
  correct class, byte order and machine passes without it
- one wording test for `build_platform`'s own refusal
  (`build_platform`, `scripts/hig-release.sh:1336`–`:1337`), which no case
  names: measured 2026-09-10, `did not report a platform` occurs in
  `scripts/hig-release.sh` and nowhere in `tests/e2e.rs`
- one wording test for the container toolchain probe's complaint refusal
  (`release_toolchain_version`, `scripts/hig-release.sh:1463`) and one for
  the deadline override's validation refusal (`container_probe_seconds`,
  `:1319`–`:1320`); every other refusal in the packaging path is pinned on
  its branch, and these three are the exceptions

plus every existing `hig_release_script_*` case still green, including
`hig_release_script_local_and_remote_install_guards_are_identical`
(`tests/e2e.rs:31954`) and
`hig_release_script_enumerates_exactly_the_executables_the_crate_declares`.
Each named refusal has one wording, unit-pinned on its branch.

## Amendment — 2026-09-10: three facts the landed implementation settled

§§1, 2, 3 and 5 landed on 2026-09-10 in the wave-2 working tree at
`/Users/geoyws/work/src/.kanban-worktrees/kanban-t-f03dbe4d-gate-80aa7489`,
unstaged over commit `2dc2e0f`; every line number in these amendments was
resolved against the settled snapshot named in §Context, after the wave
stopped moving. The places where the landed behaviour is not what this ADR
wrote are corrected in place and dated where they sit — §3's refusal list
gains the fifth sentence the code actually emits and a second-pass note for
the bounded-probe and `host` branches, §3's `HIG_RELEASE_TARGET_RUNNER` rule
is narrowed to "only when the operator named none", §3's claim that a lying
runner cannot be caught is replaced by where it is caught, and §2 says which
of its fields are records rather than measurements. Three further facts the
implementation settled belong in the record.

**`build_platform` refuses rather than recording a platform it did not
measure.** It reads `uname -s` lowercased and `uname -m` verbatim, and an
empty answer from either ends the run before the gate has an opinion about
anything else: `hig-release: this machine did not report a platform: uname -s
said nothing and uname -m said nothing` (`build_platform`,
`scripts/hig-release.sh:1332`–`:1339`, refused at `:1336`–`:1337`, prefixed
by `die` at `:44`). Each clause carries the value that was read, and the word
`nothing` stands where the read produced nothing. This matters because
`buildPlatform` is the first field §2 records: a machine that cannot say what
it is would otherwise have carried an empty string, a lone hyphen or `linux-`
into a receipt claiming every field was measured. The validators' shape check
does refuse all three — measured 2026-09-10, `jq -n '$v |
test("^[a-z0-9_]+-[a-z0-9_.]+$")'` is `false` for `""`, `"-"` and `"linux-"`
and `true` for `"darwin-arm64"` (`receipt_provenance_defs`,
`scripts/hig-release.sh:375`) — but it refuses them only after a build has
run and a package has been written. Refusing at measurement time is what
keeps §3's promise that nothing is built or written first: the gate calls
`build_platform` as its own first act (`require_release_build_capability`,
`scripts/hig-release.sh:1378`, before the platform comparison at `:1379`),
and the gate itself runs before the output directory exists and before the
build (`package_create`, `:1557`, ahead of `:1567`–`:1571` and
`:1584`–`:1589`). Its wording is not pinned by a test; it is listed as owed
above.

**Receipt provenance is one jq definition, twinned, not three hand-copied
predicates.** `receipt_provenance_defs`
(`scripts/hig-release.sh:369`–`:394`) emits a single
`def receipt_provenance_ok` (`:371`) carrying the six field checks and the
four cross-field invariants (`:372`–`:389`), and each of the three validation
sites §2 names prepends that one definition to its own program:
`validate_receipt` (`scripts/hig-release.sh:406`–`:407`),
`validate_hax_activation_receipt` (`:474`–`:475`) and the remote
package-receipt validator inside the `REMOTE` heredoc (`:2770`–`:2771`).
Three copies of an invariant are three chances to check a different thing on
the local leg, the `hax` activation check and the remote installer; one
definition is one thing to read and one thing to change. It is twinned across
the two legs like every other guard — local at `scripts/hig-release.sh:369`,
remote at `:2060` — and the twin test pins it by name
(`hig_release_script_local_and_remote_install_guards_are_identical`,
`tests/e2e.rs:33934`, the name at `:33963`), asserting exactly two
definitions, one on each side of the `REMOTE` heredoc, with byte-equal bodies
(`tests/e2e.rs:33989`, `:33998`). So §2's "updated in one cut" is enforced by
construction rather than by care: there is one text to cut, and the test fails
if the two copies of it differ by a byte.

**A receipt reading `versionProbe: "native"` cannot be produced on this
machine at all, so the tests prove the branch, the refusal and the read.**
`"native"` requires two things at once: the runner variable unset, which is
the branch `version_probe_kind` reports
(`scripts/hig-release.sh:117`–`:123`, the read at `:118`), and
`buildPlatform == artifactPlatform`, which every validator requires of a
native probe (`receipt_provenance_defs`, `:389`). On darwin-arm64 the first
without the second ends exactly where §3 said it would: the native branch of
`file_version` executes the artifact (`scripts/hig-release.sh:163`–`:164`),
and a Mac cannot execute a linux x86-64 image, so packaging refuses with
`refusing <path>: it could not report a version` and records nothing
(`hig_release_script_package_records_which_branch_measured_the_version`,
`tests/e2e.rs:32419`–`:32439`;
`hig_release_script_probes_the_version_by_running_the_binary_when_no_runner_is_configured`,
`:31636`–`:31678`). The coverage boundary is therefore named rather than
implied. Three things are proved here:

- **the branch**, recorded as `"runner"` on the container path
  (`tests/e2e.rs:31940`), on the native path taken with a runner configured
  (`:32035`) and through an operator's own runner (`:32398`);
- **the refusal of a false native claim**: a receipt asserting `"native"` on
  a darwin build platform is refused by the package-receipt validator
  (`hig_release_script_install_refuses_a_receipt_whose_provenance_could_not_have_been_produced`,
  `tests/e2e.rs:35730`, refused without mutation at `:35747`) and again by
  the `hax` activation validator (same test, `:35833`, refused at `:35850`);
- **that a genuine native receipt is read and installed**: the accepted
  `a-native-linux-build` row — `buildKind` `native`, `buildPlatform`
  `linux-x86_64`, `builderImage` null, `versionProbe` `"native"` — installs
  on the `hax` leg and on the `hig` leg and reaches the activation receipt
  with all seven provenance fields unchanged (`tests/e2e.rs:35767`, installed
  at `:35778` and `:35811`, fields compared at `:35803`).

What no test on `@@mbp` can do is **write** one. The first genuine v2
`"native"` receipt is the first `package` run on `hax`, and that run is the
measurement this suite cannot stand in for — which is also why the native
leg's flags, placement and behaviour had to stay byte-for-byte what §1 froze.

## References

- `scripts/hig-release.sh` — every bare `:NNN` citation above, at commit `43eb9de`; narrowed from "every citation" on 2026-09-10, when the amendments added citations written in full against the tree below
- `/Users/geoyws/work/src/.kanban-worktrees/kanban-t-f03dbe4d-gate-80aa7489/scripts/hig-release.sh` and `.../tests/e2e.rs` — the base of every citation added by the 2026-09-10 amendments: the wave-2 working tree, unstaged over commit `2dc2e0f`, where §§1, 2, 3 and 5 landed, at sha256 `7b3424eb281862ac49d12835904edd3bca4496829c7491a0051c11f74fd02c38` and `63f9740d83e275cc50cc2f8dbc902da609c4c74ac098a16725dd0d48ee3c08f1` (see §Context). The twin test sits at `tests/e2e.rs:33934` in that snapshot, not at the `:31954` of the pre-wave line below
- [ADR-034: HIG release packages and explicit board-rule transfer](ADR-034-hig-release-packages-and-board-rule-transfer.md) — its build-host clause is superseded here; install, activation, the served-exe proof and rollback are not
- [ADR-039: The release manifest and receipt schema is frozen at formatVersion 1](ADR-039-release-manifest-and-receipt-schema.md) — §2 and §3 move to `formatVersion` 2 here; §1's manifest, §4's identity rule, §5's sequence, §6's refusals and §7's retention are unchanged
- [ADR-006: Rust runtime and compiled binary E2E](ADR-006-rust-runtime-and-compiled-binary-e2e.md) — why the evidence is a real process
- [ADR-008: Fail closed on ambiguous and destructive operations](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the refusal form §3 and §4 follow
- `tests/e2e.rs:31954` — the local/remote twin test that keeps the validator edit in both copies
- Kanban board: epic `e-6cd91fd9`; task `t-51d5505e` (this ADR); decision `a-4741a3c3` (2026-09-10, choice `mbp-path`) on `t-491ebb8e`; rule `r-7af4dd57` (install targets, not build hosts)
