# Integrating opencode-plugin-orch

The first orch integration should add a `long-horizon` runner without moving
Kanban's schema into the plugin.

## Turn protocol

1. Resolve or initialize the workspace board.
2. Create/select a task and claim it for `orch/<run-id>`.
3. Before every model invocation, call `contextPacket()` and `renderContext()`.
4. Give the model one bounded work turn in the managed repository.
5. Require a terminal envelope:

```json
{
  "state": "continue | blocked | done",
  "summary": "what changed or was learned",
  "intent": "why the next action is correct",
  "nextAction": "one concrete restart action",
  "blockers": [],
  "validations": []
}
```

6. The runner writes the checkpoint itself. Do not rely exclusively on the
   model remembering to call a tool.
7. For `continue`, renew the lease and create a fresh session. For `blocked` or
   `done`, checkpoint atomically releases the lease and stops.

## Safety limits

- `maxTurns`, wall-clock deadline, per-turn timeout, and cancellation remain
  orch policy. Kanban stores progress but does not grant infinite execution.
- A turn that edits files but emits an invalid envelope is checkpointed by orch
  as blocked with the parse failure and repository state.
- The lease token stays in runner memory and must not be inserted into prompts
  or logs. Models identify the task; the runner authorizes mutations.
- On plugin restart, orch may resume only from the newest durable checkpoint
  after reacquiring an expired/released lease. It must not pretend an old
  OpenCode session is still authoritative.

## What Kanban guarantees, and what orch must still do

The protocol above is implementable against the shipped CLI today. Kanban owns
these, and `tests/e2e.rs::the_long_horizon_turn_protocol_survives_a_runner_restart`
drives the whole sequence against the compiled binary:

- **A `continue` checkpoint keeps the lease; `blocked` and `done` release it in
  the same transaction that writes the checkpoint.** There is no window in which
  a run is recorded finished but still holds its lease, or has released the lease
  with no record of why.
- **A live lease cannot be taken over.** A runner that crashes mid-turn does not
  hand its task to a second runner; the work becomes claimable only once the
  lease lapses and the sweep retires it.
- **A superseded lease is refused, and told so.** A runner that comes back
  holding a token from before a handover is not told "no active lease" — that
  would send it to claim a task another runner is executing. It is told the task
  is leased by the named holder and that its own lease was superseded. The
  distinction matters only on the restart path, which is the path this document
  exists for.
- **The lease token never appears on a read surface.** `context` in either
  rendering, `task show`, `events` and `dashboard` are all safe to place in a
  prompt. Writes are authorized by the token the runner holds in memory; the
  model only ever names the task.

Orch still owns, and Kanban will not enforce: `maxTurns`, the wall-clock
deadline, per-turn timeouts, cancellation, and deciding that a turn which edited
files but emitted an unparseable envelope is checkpointed as `blocked`. Kanban
stores progress; it does not grant execution.

## Swarms

Planner output should create real Kanban tasks with dependencies. Workers use
atomic `claim --next` semantics and receive only their task context. Aggregate
steps read completed child checkpoints/evidence rather than raw conversations.
