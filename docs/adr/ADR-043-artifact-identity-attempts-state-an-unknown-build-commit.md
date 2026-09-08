# ADR-043: A recovery deploy proves typed artifact identities and states that its build commit is unknown

**Status:** Accepted
**Date:** 2026-09-09
**Deciders:** claude@driver on kanban `t-ef04f302`, under George's 2026-09-08
decision. George selected `promote-digest-records` on px attention `a-67a38d0f`
in the driver-3 walkthrough, and codex@driver-3 recorded it on this row as
`APPROVAL RECORDED - this draft's direction is owner-approved; scheduling and
implementation are the kanban lane's call` (note seq 129). He ruled on the
shape — typed role-qualified expected/observed identities, an honest unknown
build SHA, Docker image IDs kept distinct from OCI manifest digests, and every
existing guard preserved. Flag names, storage, refusal wording and rendering
are decided here and he may supersede any of them by a later ADR.
**Supersedes:** nothing.
[ADR-030](ADR-030-deployment-attempt-ledger-and-self-archiving.md) stays in
force: an attempt is still append-only, still token-owned, and a `succeeded`
attempt still means live verification. This ADR adds a second way for an
attempt to say WHAT it verified, and weakens nothing about the first.

## Context

**Citation convention.** A bare `<file>:NNN` below is a path under the
repository root read at commit `9086545`, the state this ADR decides against.

px `t-15121f7e` must restore two exact retained legacy Docker images. Their
build provenance is genuinely unknown: four sampled files match a historical
source tree, which is not build attestation, and the images carry no
trustworthy build-SHA metadata. px `t-7203f5e0` stays blocked until the ledger
can represent that.

The ledger could not. `deploy start` required `--commit FULL_SHA`
(`rust/store.rs:6206`) and a succeeded `deploy finish` required a served commit
exactly equal to it (`rust/store.rs:6450`), with the same equality repeated as
a column CHECK (`rust/db.rs:835`). So an identity-only
recovery had exactly three options: invent a SHA, borrow the deployer's own
checkout SHA, or never record the release. The first two are the invented
provenance the ledger exists to prevent; the third leaves a production
restoration invisible.

## Decision

### 1. An attempt names its identity mode

`identity_mode` is `git` or `artifact`, one per attempt, chosen by the caller
through the flags it passes and never inferred from a value's shape.

- Git mode: `--commit FULL_SHA`. Unchanged, including every refusal — 40-hex
  shape, served commit equal to the requested commit, capability token, actor,
  tier/host table, immutable attempt, rollback integrity.
- Artifact mode: `--artifact ROLE=KIND:VALUE`, repeatable, with
  `--build-commit unknown` **required**. The literal is required rather than
  defaulted so that the absence of a build commit is a statement by the caller,
  not a hole we filled in silently.

`--commit` is refused in artifact mode and `--artifact` in Git mode, each
naming the mode the other flag belongs to. `--build-commit` carrying a full
40-character SHA is refused with the sentence that names the fix: the build's
provenance is known, so use `--commit`.

`ROLE` is a slug `[a-z0-9][a-z0-9-]{0,31}` naming one component (`api`, `web`),
one identity per role. `KIND` is `docker-image-id` (a Docker config/image ID)
or `oci-manifest-digest` (a registry manifest digest). Both are spelled
`sha256:<64 hex>` and both are digests of different documents, so **the kind is
part of the identity and the two kinds are never compared to each other**.

`--deployer-checkout FULL_SHA` is optional in both modes, stored in its own
column, and never presented as the build commit.

### 2. Storage: schema 25 → 26

`BOARD_V26` rebuilds `deployments`, because `commit_sha`'s CHECK admits only 40
hexadecimal characters and a CHECK cannot be altered in place. It adds
`identity_mode`, `deployer_checkout`, `expected_artifacts` and
`observed_artifacts`, and widens `commit_sha` to `40 hex` in Git mode or the
literal `unknown` in artifact mode.

The expectation and the observation are **two columns**, not one document a
finish rewrites: what the start claimed must not be editable by the write that
proves it. Column CHECKs keep every invariant that is true of one row read
alone — the mode/commit pairing, the digest shapes, no served commit in
artifact mode, and a succeeded artifact attempt having a recorded observation.
The per-role match is not such an invariant and needs a refusal that names the
role and both values, so it lives in `finish_deployment` and only there
(ADR-008). The rebuild reconstructs `identity_mode` from `commit_sha` so the
ladder's last step still survives being re-run, and recreates
`search_deployments_au`, which a table rebuild drops with its table.

### 3. Verification, per role and per kind

A succeeded artifact-mode finish requires `--observed ROLE=KIND:VALUE` for
every expected role, matched exactly by role, by kind and by value. A missing
role, an extra role, a kind mismatch (a config ID offered where a manifest
digest was expected) and a value mismatch are four distinct named refusals,
each naming the role and both values. Role order is not identity: an
observation is matched by name and answered in the attempt's expected order.

`--served-commit` is refused on an artifact attempt and `--observed` on a Git
attempt. Together these are what makes a digest-only success impossible to
record as a verified Git commit. An artifact attempt that did not succeed still
records what it measured, because the mismatch is the evidence.

### 4. Rendering says it in words

`deploy show/list/current --json`, the generated MCP tools and `kanban schema`
carry `identityMode`, `buildCommit` (`unknown` in artifact mode),
`buildCommitLabel`, `deployerCheckout` and
`artifacts: [{role, kind, expected, observed}]`. `buildCommitLabel` is derived
on read beside a task's `priorityLevel`: the commit in Git mode, and in
artifact mode the exact words

> build commit unknown - recovered by artifact identity

The Deployments page and the per-attempt page render that label rather than a
blank, a dash or the bare literal, and the attempt page shows the expected and
observed identity per role plus the deployer's checkout in its own field. The
CLI's only rendering is JSON (`rust/lib.rs:2340`), so the words travel as
`buildCommitLabel` and every consumer prints one field instead of composing its
own sentence.

## Consequences

- A restoration whose build provenance is lost is recordable, verifiable and
  auditable without any invented SHA anywhere in the ledger.
- `unknown` is a value in storage, not a NULL: a row that says nothing is
  distinguishable from a row that says "nobody knows".
- Reading a release now means reading two fields, `identityMode` and
  `buildCommit`. A consumer that reads only the commit sees `unknown` — honest,
  and never a plausible-looking SHA.
- A board is at schema 26 and an older binary refuses it, as every migration
  here does.
- The two artifact kinds are a closed set. A third kind (an attestation
  predicate, a checksum manifest) needs an ADR, not a string.

## Evidence required

Compiled-process tests, per ADR-006:
`a_recovery_deploy_records_typed_artifact_identities_and_an_unknown_build_commit`,
`a_recovery_finish_refuses_a_missing_role_a_kind_mismatch_and_a_value_mismatch_by_name`,
`artifact_mode_and_git_mode_refuse_each_others_flags`,
`the_git_deploy_path_is_unchanged_by_artifact_mode` and
`the_deployments_page_says_build_commit_unknown_in_words`, with every existing
deploy case still green. Each named refusal has one wording, unit-tested on
every branch in `rust/model.rs` and `rust/store.rs`.

## References

- [ADR-006: Rust runtime and compiled binary E2E](ADR-006-rust-runtime-and-compiled-binary-e2e.md)
- [ADR-008: Fail closed on ambiguous and destructive operations](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
- [ADR-010: Adapters generated from the command surface](ADR-010-adapters-generated-from-the-command-surface.md)
- [ADR-030: Deployment attempts are append-only and self-archive from hot indexes](ADR-030-deployment-attempt-ledger-and-self-archiving.md)
- kanban `t-ef04f302`; px `a-67a38d0f`, `t-910a0e11`, `t-15121f7e`, `t-7203f5e0`
