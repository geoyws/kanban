# ADR-049: Model-restricted tasks

- Status: Accepted
- Date: 2026-09-19
- Deciders: George (request) / planner (shape)

## Context

George, 2026-09-19, in the kanban planner pane: "add a new feature called model
restricted — only a subset of models, or one model, can claim this task.
Blender must only be used by Astra."

The point is scheduling, not documentation. A task that needs Blender must
force the harness to run Astra for it, because any other model claiming it
would burn a lane on work it cannot finish. The board already refuses claims
for two reasons that live on the claim path — `driver_only` and `assignee` —
and neither can express "this model, and no other".

## Decision

A task carries an allow-list of model names. A claim declares the model it runs
as. A restricted task refuses any claim whose model is missing or not in the
list, and the refusal prints the list. Empty list = unrestricted, byte-identical
wire shape to before except one new array key.

Concretely (`BOARD_V30`, `BOARD_SCHEMA_VERSION` 30):

- `task_models(task_id, model)` plus `idx_task_models_model`; `task_claims`
  gains a nullable `model`.
- A model name is `^[A-Za-z0-9][A-Za-z0-9._:/-]{0,63}$`, matched
  case-sensitively, validated on the task side and the claim side before any
  `INSERT`.
- `Task.allowedModels` is a sorted array, always present. `Claim.model` and
  `ClaimSummary.model` are nullable strings.
- CLI: `task add --allowed-model NAME` (repeatable), `task update
  --allowed-model NAME | --clear-allowed-models` (both together refused),
  `task list --allowed-model NAME` (server-side filter), `claim --model NAME`
  (honoured by `--next` and `--candidates`), `handoff accept --model NAME`. The
  MCP tool schemas follow the surface table (ADR-010).
- Enforcement sits immediately after the driver-only check, so the refusal
  order is type → draft → status → gates → held → driver-only → MODEL →
  assignee, on `Store::claim`, `accept_handoff`, and the candidate router.

## Alternatives rejected

- **A registry-owned rule (ADR-027 `ONLY:` / `EXCEPT:` tags).** Rules frame
  work and are injected on claim; they never refuse a claim. The ask is a
  refusal on the claim path, which is where `driver_only` and `assignee`
  already live.
- **A tag.** Tags say what a row is *about*, and `--tag` on `claim
  --candidates` is a caller filter, not a server-side refusal. A model that
  ignored the tag would still claim the row.
- **A registered model master file, like `tags`.** The claimant's `--model` is
  already free text on `checkpoint` and `handoff create` (`checkpoints.model`,
  `handoffs.from_model`, since `BOARD_V1`). Adding a registry for one field
  would be a second convention beside the existing one.

## Consequences

- A typo in an allow-list **fails closed**: nobody can claim the row. That is
  deliberate, and it is visible rather than silent, because the refusal prints
  the set the row wants. Revisit only if it bites.
- A claim that declares no model is still legal on an unrestricted task, and
  `model` is absent from the `task_claimed` / `handoff_accepted` payload unless
  it was given — so a board that never uses the feature reads exactly as it did.
- `task_updated` gains `allowedModels` in its changed-fields list.
- `BOARD_V30` is re-run safe: `task_models` is `IF NOT EXISTS`, and the claim
  column arrives by a column-naming rebuild of `task_claims`, the mechanism
  `BOARD_V24` and `BOARD_V25` already use because SQLite has no
  `ADD COLUMN IF NOT EXISTS`.
- Deliberately out of scope: a model registry; changing checkpoint/handoff
  `--model` semantics; per-lane default models; enforcing that a claim's
  `--model` agrees with later checkpoints'.

## References

- Board row `t-1331c416` (epic `e-c0852fe7`) — the request and the contract.
- `docs/specs/model-restriction.md`, slice `MODEL` — requirements and traces.
- ADR-008 (refusals name the fix), ADR-010 (one surface table), ADR-027
  (registry rules), ADR-047 §7 (slice approval).
