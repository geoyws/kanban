# ADR-009: Absorb swarm write contention instead of dropping the write

**Status:** Accepted
**Date:** 2026-08-20
**Deciders:** George

## Context

Kanban exists to coordinate fan-out: many agents, one board, each turn issuing
a command and reading an exit status. A write that fails is not retried by
anything downstream, because the agent has already moved on to its next turn.
A dropped `note` or `claim` is lost work that nobody notices is missing.

Every transaction already opens `BEGIN IMMEDIATE`, so the classic deferred
read-to-write upgrade deadlock cannot occur, and `synchronous=FULL`
([ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md))
means each commit fsyncs. Contention was bounded by `PRAGMA busy_timeout=5000`
and had never been measured under load.

Measured, sixteen concurrent processes writing one board:

| | writes | failed | median | slowest success |
| --- | --- | --- | --- | --- |
| `busy_timeout=5000` | 192 | 6 (3.1%) | 208ms | 6699ms |
| jittered backoff, 15s | 192 | 0 | 573ms | 4537ms |

A mixed `claim`/`add`/`note` fan-out over the same board failed 10 of 384
operations before and 0 of 384 after.

Two things stood out. Every single failure landed at 5006–5391ms — so the busy
handler was working exactly as configured, and five seconds was simply less
than the workload needed. And `sqlite3_busy_timeout` installs a handler whose
sleep schedule carries **no randomization**: writers that collide wake together
and collide again, so the board makes progress while one unlucky process loses
every round. The 3% was not uniform slowness, it was starvation.

## Decision

**A contended board is waited on, with randomized backoff, for fifteen
seconds.** `busy_timeout` is replaced by a custom handler that pauses for a
jittered interval — exponential to a 100ms ceiling, decorrelated per process —
so colliding writers stop retrying in lockstep. Jitter is what converts a
starving writer into a merely slow one; the longer budget is what covers a
write queue roughly seventy deep at the measured median.

**The budget is wall-clock, not extrapolated from the retry count.** SQLite
passes the handler a retry count and no clock, and the handler is a bare
function pointer with nowhere to keep state, so the start of each contention
episode is anchored in a thread-local. A budget inferred from the count would
be an estimate, and an error that reports a wait that did not happen is the
same class of untruth as a `truncated: false` that dropped rows.

**Fifteen seconds, and then it still fails.** Waiting forever would turn an
oversubscribed board into a hung agent, which is worse than an error: an
exit status is something the caller can act on, and a wedged process is not.
Past this budget the board is genuinely oversubscribed and saying so is the
honest answer.

## Consequences

Under fan-out, individual writes are slower and the slowest write is faster.
The median rose from 208ms to 573ms while the tail fell from 6.7s to 4.5s and
the failures went away — the herd was previously spending its time colliding,
not working.

A pathologically oversubscribed board now takes up to fifteen seconds to
report `database is locked` where it used to take five. That is the cost of
not dropping writes that were only ever going to be slow.

Deliberately not decided: relaxing `synchronous=FULL`. It is the dominant cost
per commit, and lowering it to `NORMAL` would raise throughput by giving up the
durability ADR-008 chose deliberately. Contention is a scheduling problem and
was fixed as one.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) — durable resume contract
- [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md) — compiled-binary E2E
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — `synchronous=FULL`, failing closed
