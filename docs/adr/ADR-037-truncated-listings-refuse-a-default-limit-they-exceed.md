# ADR-037: Truncated listings refuse a default limit they exceed

**Status:** Accepted
**Date:** 2026-09-05
**Deciders:** claude@driver, on the recommendation filed in t-4785c1c4, under George's standing goal of 2026-09-05 to finish the board's tasks. George did not rule on this himself and may supersede it by a later ADR.

## Context

Every capped listing in this tool — `events` (50), `sitrep list` (20),
`attention list`, `deploy list`, `claim --candidates` and `handoff list` (100),
`search` (10) — answers a bounded query with a bare array or a plain object and
says nothing about whether the bound was hit. A caller who never passed
`--limit` cannot tell fifty events from fifty-of-nine-hundred. The board's rule
that a read surface must not let absence read as a finding (epic e-df5fcee3)
already refuses an unreadable board; a silently clipped one is the same defect
with a longer list.

The tree already holds the precedent for doing this honestly, twice.
`rust/watch.rs`'s `stream` loop ends a follow only when a poll comes back
*empty*, never because a batch was short or exactly `limit` long, and its
`bounded_metadata` sets `truncated` from the bytes it measured, not from the
size it expected. `rust/store.rs`'s `context_packet` fetches one row past each
of its caps and sets `truncated` from whether that row came back, after an
earlier revision hardcoded it `false` and told resuming agents they held the
whole record while notes were being dropped. Truncation there is computed.
Every other listing assumes.

Three shapes were on the table, and the choice has to be made once for every
listing rather than by whoever patches the first one, because it fixes the
response shape of every listing in the tool:

- **A, wrap.** Return `{items, returned, limit, truncated}`. Unmissable, and
  breaks every consumer at once — the MCP adapter, the web view, the `/kb`
  skill's helper scripts — since each reads the array directly.
- **B, sibling field.** Keep the array, add `truncated` where the response is
  already an object. Cheap, and useless for the listings that return a bare
  array today, which is most of them.
- **C, refuse.** When more rows exist than the default the caller never set,
  fail naming `--limit`, the way every other refusal here names its fix
  ([ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)).
  No schema change, no consumer migration; a silent wrong answer becomes a
  loud one.

C as first written had a hazard. "Refuse when the count equals the default"
cannot tell a board with exactly fifty events from a board with more, so a
board that happens to hold exactly the default would refuse a listing that was
in fact complete — a false refusal, on a surface whose whole purpose is to be
believed.

## Decision

Option C, computed rather than assumed.

1. **Fetch `limit + 1`, compute `truncated`.** A capped listing asks the store
   for one row more than it will return and sets `truncated` from whether that
   extra row came back. A result with exactly `limit` rows and no extra is
   complete and is returned as such. This is the `context_packet` discipline
   applied to every listing — look for the next row, do not infer from the
   count.

2. **No `--limit` and more rows exist: refuse.** Non-zero exit; under `--json`
   the error object on stdout. The message names the count the listing stopped
   at and the flag that resolves it, `--limit N`, in the ADR-008 form where a
   refusal is also its own fix. A listing that is capped but has no `--limit`
   flag (`handoff list` today) must grow one before it can refuse, because a
   refusal that names a flag the command does not accept is not a fix.

3. **Explicit `--limit N` is honoured as-is, with no marker.** The caller
   stated a bound and got one. That the bound was hit is the thing they asked
   for, not something to be warned about.

4. **A is the migration path; B is rejected.** If a bare-array listing ever
   needs an in-band marker — a consumer that wants the partial rows *and* the
   fact that they are partial in one response — the shape is A's envelope,
   adopted for every listing in one cut with the consumers moved in the same
   change. B is not that path: a sibling field cannot be added to an array, so
   it would leave the listings most in need of the marker exactly as they are.

## Consequences

No schema changes and no consumer migrates. Every listing keeps its array or
object; the only new thing a caller can meet is a refusal, and refusals are
already the shape every consumer of this CLI handles.

The false-positive hazard C-naive carried is gone: `limit + 1` distinguishes
"exactly the default" from "more than the default" by observation, so a board
holding exactly fifty events lists all fifty and exits zero. The cost is one
extra row per capped query, which is nothing against a refusal a script would
have had to work around.

Truncation under an explicit `--limit` stays silent, by design. A caller who
wants to know whether their own bound was hit can ask for one more than they
need, which is the same trick the listings now use internally.

Refusing a default that was exceeded means a listing that used to answer now
fails until the caller says how much they want. That is the intended trade:
the callers this tool exists for read an exit status and move on
(ADR-008), and a failure they must act on is worth more than fifty rows they
mistake for the whole.

The implementation follows in the sibling tasks of epic e-61429870:
t-5d3c540c, t-ab9b86ea and t-06149df3 land the `limit + 1` fetch and the
refusal per listing. This ADR fixes the shape they implement; it does not
implement it.

## References

- `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md`
- `rust/watch.rs` — `stream` ends on an empty poll; `bounded_metadata` measures `truncated`
- `rust/store.rs` — `context_packet` over-fetches by one and `keep_newest` reports what it cut
- `rust/lib.rs` — `Args::limit`, the one helper every default passes through
- Kanban board: epic e-61429870; tasks t-4785c1c4 (this decision), t-5d3c540c, t-ab9b86ea, t-06149df3; epic e-df5fcee3
