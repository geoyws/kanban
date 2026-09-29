# Specification: a deployment attempt's tier matches its host, and the dev tiers may run on `hax` for the Unum and geoyws estates (slice DEPLOY)

## 1. Identity and baseline

- **Slice ID:** `DEPLOY`. Requirement IDs are `DEPLOY-01` .. `DEPLOY-08`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-30` at commit `863667d` on branch `kanban-geoyws-driver`. Every "today"
  claim below cites the line that has it, as `<path>:<line>`. Board schema at the baseline is
  `36` (`rust/db.rs:2920`).
- **Status:** `DRAFT` on 2026-09-30, pending the independent `/quality spec` gate. No stamp here
  claims readiness; `SPEC-READY` authorises neither implementation, nor rollout, nor release.
- **Owner (product scope):** George. He approved this slice under epic `e-c0852fe7` on attention
  `a-e7c70c63` (2026-09-29, "New DEPLOY slice … owns tier-host validation"); the slice row is
  `t-c720eb6b` and the implementation row is `t-1220f80f`.
- **Decider (wording of this document):** `@:geoyws/kanban/driver`. Where this document and the
  approved rows differ on a fact, the rows win and this document is corrected.
- **Sources:**
  - **George, 2026-09-28** (dotfiles `AGENTS.md` and the kanban board-wide rule `g-ca3365d2`):
    "`@_bdt` and `@_bd` run on `@@hax` for the Unum and geoyws estates … and on geoywsMBP
    (`@@mbp`) for IFCA products and GPU-bound projects such as hom."
  - **Row `t-1220f80f` body:** "allow `@_bdt`/`@_bd` deploy records on hax … without loosening
    validation for other tiers", and "IFCA dev tiers must still validate on MBP".
  - Shipped surface at the baseline: the tier list `DEPLOYMENT_TIERS` (`rust/model.rs:1858`); the
    MBP tier list `MBP_TIERS` and the MBP host list `MBP_HOSTS`
    (`rust/model.rs:1873`, `rust/model.rs:1879`); the pairing check `require_deploy_tier_host`
    and its two refusal sentences (`rust/store.rs:9983`-`rust/store.rs:9997`), called from
    `start_deployment` after an idempotent replay has already returned
    (`rust/store.rs:10085`-`rust/store.rs:10110`); the board-to-estate map `estate_for_board`
    (`rust/store.rs:601`-`rust/store.rs:611`); the board's registered name `board_name`
    (`rust/store.rs:6056`); the three `deployments` table definitions whose `CHECK` names tier
    values only (`rust/db.rs:810`, `rust/db.rs:1671`, `rust/db.rs:2383`); the existing process
    test `deploy_start_refuses_a_tier_host_pair_the_canonical_table_forbids`
    (`tests/e2e.rs:17056`).
  - `docs/adr/ADR-030-deployment-attempt-ledger-and-self-archiving.md` — the tier table
    this slice amends; `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` —
    why an unmapped board is refused.
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`, section
  `## Requirements trace — docs/specs/deploy.md DEPLOY-01..DEPLOY-08`. §8 plans the rows; this
  specification does not restate the matrix.

## 2. Purpose and scope

**Intended outcome.** `kb deploy start` records a dev-tier attempt on `hax` for a Unum or geoyws
board, keeps refusing it for an IFCA board or a board no estate claims, and changes nothing else
about which tier may run on which host.

**Users / actors.**

- **Lane executors and deploy scripts** — record deployment attempts with
  `kb deploy start --tier … --host …` from the CLI or the generated MCP tool.
- **George** — reads the deployment ledger; owns the estate map and the tier table.

**In scope.** The tier-host pairing that `deploy start` enforces before it writes an attempt: the
MBP tiers `@_bdt` and `@_bd`, the MBP hosts `geoywsMBP` and `geoywsMBA`, and the one new host
exception `hax` for boards whose estate is `unum` or `geoyws`.

**Boundaries.** The estate of a board is whatever `estate_for_board` answers for the board's
registered name (`rust/store.rs:601`); this slice reads that map and does not change it. The
attempt lifecycle, idempotent replay, finish, current-release and archive rules stay with ADR-030
and `docs/specs/sprint.md`. Tier values and their `CHECK` constraint are unchanged.

**Non-goals.**

- Moving any existing attempt. Attempts are immutable; this slice guards new records only.
- A host registry. `hax` is named in code as the one exception, as `MBP_HOSTS` names the MBP
  hosts today.
- Allowing dev tiers on `hig` or any other Hetzner host, for any estate.
- Refusing dev tiers on the MBP for Unum or geoyws boards. The MBP stays valid for every estate:
  GPU-bound projects run there, and a project not yet migrated is legacy, not invalid.
- A performance, latency or availability target.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

Every requirement is proven at `process`: `deploy start` is a CLI verb and the compiled binary is
spawned across a real process boundary. There is no browser surface.

### Pairings that stay as they are

**DEPLOY-01** — Accept a dev tier on an MBP host for every board.
Strength: `MUST` · Layer: `process` · Source: shipped surface; George, 2026-09-28.
`deploy start` with tier `@_bdt` or `@_bd` and host `geoywsMBP` or `geoywsMBA` succeeds whatever
the board's estate, including an IFCA board and a board no estate claims.

**DEPLOY-02** — Refuse a dev tier on any non-MBP host other than `hax`, in the existing words.
Strength: `MUST` · Layer: `process` · Source: row `t-1220f80f`; shipped surface.
`deploy start` with tier `@_bdt` or `@_bd` and a host that is neither an MBP host nor `hax` (for
example `hig`) is refused, for every estate, with the byte-identical baseline sentence
`tier {tier} is an MBP tier (canonical row "{tier} -> geoywsMBP"), but host is {host}; deploy it
from geoywsMBP (or geoywsMBA)` (`rust/store.rs:9987`-`rust/store.rs:9989`).

**DEPLOY-03** — Refuse a Hetzner tier on an MBP host, in the existing words.
Strength: `MUST` · Layer: `process` · Source: shipped surface.
`deploy start` with any tier other than `@_bdt`/`@_bd` and host `geoywsMBP` or `geoywsMBA` is
refused with the byte-identical baseline sentence `tier {tier} is a Hetzner tier (canonical row
"{tier} -> Hetzner host"), but host is {host}; deploy it from a Hetzner host (e.g. hax or hig)`
(`rust/store.rs:9992`-`rust/store.rs:9994`).

**DEPLOY-04** — Accept a Hetzner tier on a non-MBP host.
Strength: `MUST` · Layer: `process` · Source: shipped surface.
`deploy start` with any tier other than `@_bdt`/`@_bd` and a non-MBP host (including `hax` and
`hig`) succeeds, whatever the board's estate.

### The `hax` exception

**DEPLOY-05** — Accept a dev tier on `hax` for a Unum or geoyws board.
Strength: `MUST` · Layer: `process` · Source: George, 2026-09-28; row `t-1220f80f`.
`deploy start` with tier `@_bdt` or `@_bd` and host exactly `hax` (byte-exact, case-sensitive)
succeeds when `estate_for_board` of the board's registered name is `unum` or `geoyws`: for
example the boards `kanban`, `acies` and `unum`.

**DEPLOY-06** — Refuse a dev tier on `hax` for an IFCA board or an unmapped board, naming why.
Strength: `MUST` · Layer: `process` · Source: row `t-1220f80f` ("IFCA dev tiers must still
validate on MBP"); ADR-008 (fail closed).
`deploy start` with tier `@_bdt` or `@_bd` and host `hax` is refused when the board's estate is
`ifca`, with exactly
`tier {tier} on host hax is a dev tier for the unum and geoyws estates only; board {board} is in
estate ifca, so deploy it from geoywsMBP (or geoywsMBA)`,
and when no estate claims the board (or the board has no registered name), with exactly
`tier {tier} on host hax is a dev tier for the unum and geoyws estates only; board {board} maps
to no estate, so deploy it from geoywsMBP (or geoywsMBA)`,
where `{board}` is the registered name, or `(unnamed)` when there is none. Either refusal writes
nothing: no attempt row, no event, no capability token.

**DEPLOY-07** — Keep an idempotent replay ahead of the pairing check.
Strength: `MUST` · Layer: `process` · Source: shipped surface (`rust/store.rs:10085`-`:10108`).
A `deploy start` that repeats the operation id of an attempt already written, with the same
fields, returns that attempt as a replay before the pairing check runs, exactly as at the
baseline. The new exception adds no check ahead of the replay.

### Data

**DEPLOY-08** — Change no schema.
Strength: `MUST` · Layer: `process` · Source: row `t-1220f80f` ("db CHECK migrated with no silent
widening").
The `deployments.tier` `CHECK` constrains tier values only (`rust/db.rs:810`, `:1671`, `:2383`)
and holds no host rule, so it needs no migration: the board schema version stays `36`, and the
pairing stays enforced in `deploy start` alone. A board opened by the new binary reports the same
schema version it reported before.

## 4. Acceptance examples

Every invocation runs against the compiled binary with `--as` on the write and `--json`. `START`
abbreviates `kb deploy start --repo geoyws/example --commit <40 hex> --environment env --url
https://x --as e2e --json`. A board is created with `kb init --name <NAME>`.

### A1 (`DEPLOY-05`)

*Given* a board named `kanban`, *when* `START --tier @_bdt --host hax` runs, *then* it exits 0
and prints the new attempt with `"host": "hax"` and `"tier": "@_bdt"`. The same holds for `@_bd`,
and for a board named `unum`.

### A2 (`DEPLOY-06`)

*Given* a board named `px`, *when* `START --tier @_bdt --host hax` runs, *then* it exits non-zero
with `tier @_bdt on host hax is a dev tier for the unum and geoyws estates only; board px is in
estate ifca, so deploy it from geoywsMBP (or geoywsMBA)`, and `kb deploy list --all --json` is
still empty. *Given* a board named `TIERHOST`, the same command is refused with `… board TIERHOST
maps to no estate, so deploy it from geoywsMBP (or geoywsMBA)` and writes nothing.

### A3 (`DEPLOY-02`, `DEPLOY-03`)

*Given* a board named `kanban`, *when* `START --tier @_bdt --host hig` runs, *then* it is refused
with the baseline MBP-tier sentence naming `hig`; *when* `START --tier @_p --host geoywsMBP` runs,
*then* it is refused with the baseline Hetzner-tier sentence. Both sentences are compared
byte-for-byte.

### A4 (`DEPLOY-01`, `DEPLOY-04`)

*Given* a board named `px`, *when* `START --tier @_bdt --host geoywsMBP` and
`START --tier @_p --host hax` run, *then* both succeed.

### A5 (`DEPLOY-07`)

*Given* a board named `kanban` and an attempt started with `START --tier @_bdt --host hax
--operation-id op-1`, *when* the identical command runs again, *then* it returns the same attempt
id with `"idempotentReplay": true`.

### A6 (`DEPLOY-08`)

*Given* a board created by the new binary, *when* `kb version` runs, *then* it reports `board
schema 36`.

## 5. Contracts and data

- **Interface version or schema:** no new verb, flag or field. The `deploy start` grammar
  (`rust/lib.rs:98`-`rust/lib.rs:105`) is unchanged; one refusal gains two new sentences
  (DEPLOY-06) and every other refusal is byte-identical.
- **Data invariants:** attempts are immutable; the pairing guards new records only. No stored
  row is rewritten, and no attempt that was refused before becomes writable except a dev tier on
  `hax` for a Unum or geoyws board.
- **Migration:** none (DEPLOY-08).
- **Compatibility:** an older client sees only the new acceptance (DEPLOY-05) and the new
  refusal wording on `hax` for other boards; before this slice those `hax` attempts were refused
  too, with the MBP-tier sentence.
- **Ownership:** ADR-030 owns the tier table and is amended in the implementation change to name
  the `hax` exception and this slice.

## 6. Quality and security

- **Reliability:** N/A — the check is a pure function of tier, host and board name; the refusal
  writes nothing.
- **Accessibility:** N/A — no rendered surface.
- **Privacy:** N/A — the refusal names only the board the caller already opened.
- **Security:** the exception widens only what may be recorded, never who may record it: the
  existing authorization on `deploy start` (tag checks against the pointed-at task) runs
  unchanged. The estate comes from the board's registered name through the compiled map, never
  from a caller-supplied field, so a caller cannot claim an estate.
- **Operability:** the new refusal names the board, its estate and the host to use instead.
- **Performance:** observation only, not a budget — one extra `board_meta` read on the refused
  or `hax` path; no timing commitment (ADR-047 §9).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | Should `hig` (or another Hetzner host) also accept dev tiers for some estate? | George | open — this document takes today's answer, no (DEPLOY-02). His 2026-09-28 rule names only `hax` | none — adding a host later widens DEPLOY-05 by supersession |

No material open question remains: OQ-1's default is the behaviour George's rule states today.

## 8. Verification

Names marked `existing` were enumerated with `cargo test --test e2e -- --list` at the baseline;
names marked `planned` are owed by the implementation row `t-1220f80f` and are not claimed to
exist. The matrix section carries the same mapping.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `DEPLOY-01` | `MUST` | `process` | existing: `deploy_start_refuses_a_tier_host_pair_the_canonical_table_forbids` | `@_bdt` on `geoywsMBP` accepted; the planned test adds an IFCA and an unmapped board (A4) |
| `DEPLOY-02` | `MUST` | `process` | planned: `deploy_start_keeps_the_mbp_tier_refusal_off_hax_in_the_same_words` | A3, byte-identical sentence for `hig` on a geoyws board |
| `DEPLOY-03` | `MUST` | `process` | planned: `deploy_start_keeps_the_mbp_tier_refusal_off_hax_in_the_same_words` | A3, byte-identical Hetzner-tier sentence |
| `DEPLOY-04` | `MUST` | `process` | existing: `deploy_start_refuses_a_tier_host_pair_the_canonical_table_forbids` | `@_p` on `hax` accepted; the planned A4 case adds an IFCA board |
| `DEPLOY-05` | `MUST` | `process` | planned: `deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards` | A1 |
| `DEPLOY-06` | `MUST` | `process` | planned: `deploy_start_refuses_dev_tiers_on_hax_for_ifca_and_unmapped_boards` | A2, both sentences byte-for-byte, nothing written |
| `DEPLOY-07` | `MUST` | `process` | planned: `deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards` | A5 replay |
| `DEPLOY-08` | `MUST` | `process` | planned: `deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards` | A6 schema version unchanged |

## 9. Change log

- `2026-09-30` — slice created at `DEPLOY-01` .. `DEPLOY-08`. No supersessions yet.
