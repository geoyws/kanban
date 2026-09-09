# ADR-044: A release is a linux x86-64 artifact whose build provenance is measured, not assumed

**Status:** Accepted
**Date:** 2026-09-10
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
- `hig-release: cannot package a linux x86-64 release on darwin-arm64: no builder image was named; pass --builder-image <ref>@sha256:<64 hex> or set KANBAN_RELEASE_BUILDER_IMAGE`
- `hig-release: cannot package a linux x86-64 release on darwin-arm64: builder image rust:1.90-bookworm is not digest-pinned, and a tag can move, so its digest would not name the bytes that built this release`
- `hig-release: cannot package a linux x86-64 release on darwin-arm64: builder image <ref>@sha256:<digest> does not run linux x86-64; uname -m inside it reported aarch64`

Each has one wording, unit-pinned on its branch.

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

**Still owed by the capability-gate and container waves:**

- `hig_release_script_package_records_the_real_build_host_platform_and_toolchain`
- `hig_release_script_package_records_the_pinned_builder_image_digest_on_the_container_path`
- `hig_release_script_package_refuses_a_machine_that_cannot_produce_a_linux_x86_64_artifact_naming_what_was_missing`
- `hig_release_script_package_refuses_a_builder_image_that_is_not_digest_pinned`
- `hig_release_script_install_refuses_a_format_version_1_package_receipt`
- `hig_release_script_provenance_fields_leave_the_manifest_bytes_and_release_id_unchanged`
- `hig_release_script_package_records_which_branch_measured_the_version` — the
  `versionProbe` field, `"native"` with no runner and `"runner"` with one
- `hig_release_script_refuses_a_relocatable_object_carrying_the_right_machine`
  — the `e_type` half of the header check, which an `ET_REL` fixture with a
  correct class, byte order and machine passes without it
- the twin test's own pinned list must name `file_version`, so the runner
  seam cannot drift between the local function and the remote heredoc; a
  guard absent from that list is unpinned, which is how `file_version` stood
  when this ADR was written

plus every existing `hig_release_script_*` case still green, including
`hig_release_script_local_and_remote_install_guards_are_identical`
(`tests/e2e.rs:31954`) and
`hig_release_script_enumerates_exactly_the_executables_the_crate_declares`.
Each named refusal has one wording, unit-pinned on its branch.

## References

- `scripts/hig-release.sh` — every citation above, at commit `43eb9de`
- [ADR-034: HIG release packages and explicit board-rule transfer](ADR-034-hig-release-packages-and-board-rule-transfer.md) — its build-host clause is superseded here; install, activation, the served-exe proof and rollback are not
- [ADR-039: The release manifest and receipt schema is frozen at formatVersion 1](ADR-039-release-manifest-and-receipt-schema.md) — §2 and §3 move to `formatVersion` 2 here; §1's manifest, §4's identity rule, §5's sequence, §6's refusals and §7's retention are unchanged
- [ADR-006: Rust runtime and compiled binary E2E](ADR-006-rust-runtime-and-compiled-binary-e2e.md) — why the evidence is a real process
- [ADR-008: Fail closed on ambiguous and destructive operations](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the refusal form §3 and §4 follow
- `tests/e2e.rs:31954` — the local/remote twin test that keeps the validator edit in both copies
- Kanban board: epic `e-6cd91fd9`; task `t-51d5505e` (this ADR); decision `a-4741a3c3` (2026-09-10, choice `mbp-path`) on `t-491ebb8e`; rule `r-7af4dd57` (install targets, not build hosts)
