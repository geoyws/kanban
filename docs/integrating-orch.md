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

## Swarms

Planner output should create real Kanban tasks with dependencies. Workers use
atomic `claim --next` semantics and receive only their task context. Aggregate
steps read completed child checkpoints/evidence rather than raw conversations.
