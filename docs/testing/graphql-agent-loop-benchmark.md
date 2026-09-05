# The representative agent loop, and how any transport must be measured against it

**Status:** contract accepted for use; first receipt disqualified by its own protocol
**Board:** kanban task t-4cc2dca7, epic e-764e9902
**Reproduce:** `python3 docs/testing/bench/agent-loop-bench.py --out docs/testing/<receipt>.json`

George asked on 2026-09-03 whether kb reads from the MBP are slow because of the
index or because of SSH, and whether a GraphQL agent API is worth building. The
answer needs a number that a future arm can be compared against without anyone
arguing about what was measured. This document is that number's contract: what
the loop is, what a receipt must contain, what makes two receipts comparable,
and what disqualifies one. The driver in `docs/testing/bench/` is the
executable half; nothing here is true that the driver does not enforce.

## 1. The loop

One iteration is the read sequence a `/session cont` followed by a `/kb-att`
walkthrough performs against one lane, taken from those skills' scripts and
frozen as `docs/testing/bench/fixture.json` (`fixture_id:
kanban-agent-loop-v1`). Twelve reads, in this order, all read-only, none
carrying a lease token:

| id | command |
|---|---|
| workspace_list | `workspace list` |
| handoff_pending | `handoff list --status pending` |
| attention_open_100 | `attention list --status open --limit 100` |
| stale | `stale` |
| attention_open_500 | `attention list --status open --limit 500` |
| attention_resolved_500 | `attention list --status resolved --limit 500` |
| task_list | `task list` |
| context_blocked_1 | `context t-0110a25c` |
| context_blocked_2 | `context t-8c656910` |
| claim_candidates | `claim --candidates --as bench@driver --lane driver --limit 100` |
| sitrep_lane_20 | `sitrep list --lane driver --limit 20` |
| context_candidate | `context t-5c98cfcf` |

Every read names its `--limit` explicitly because ADR-037 makes a capped
listing without one refuse on a board that exceeds its default. The three
`context` ids are pinned task ids on the kanban board; they exist so the loop
exercises the bounded cold-start packet, and they are part of the fixture
identity. Changing any command, its order, or a pinned id is a new fixture
version and a new baseline, never an edit to this one.

The loop is **payload projection v1: whole rows**. The fixture was frozen
against the release served on the board home at the time (kanban `119e41b`),
which predates `--fields` / `--no-body` (kanban `53e6ed1`). Once the home
host serves a release with projection, a `kanban-agent-loop-v2` fixture that
asks for `--fields id,title,status,lane,priority,tags` on the listings becomes
the second baseline; v1 stays as the whole-row reference. Section 4 explains
why that matters more than the transport.

## 2. Fixture identity — what is held fixed, and what is only recorded

A receipt records every item below. The first group must be **equal** across
two receipts for them to be comparable; the second group is recorded so a
difference can be named, but is not required to be equal.

Held fixed:

- **Board:** `kanban`, addressed by name through `--project`; `kb_exec`
  `/root/.local/bin/kb`; `ssh_target` `hax`; `expected_remote_hostname` `hax`,
  verified by `hostname` before any read.
- **Server release:** `kb v` on the home host at setup, recorded per arm
  (`setup.kb_version`). Two receipts against different server commits are not
  comparable.
- **Authentication identity:** the caller's ssh identity for `hax` as the ssh
  config resolves it; `actor` on the one read that takes one is
  `bench@driver`. No token, no header.
- **Read sequence, warmups, iterations:** the twelve reads above, 5 warmups,
  30 measured, per arm, from the fixture. The driver refuses a receipt as
  disqualified if either count is lowered.
- **Response equivalence:** the sha-256 of every read's response body, per
  arm. A read whose digest differs between arms means the arms did not read
  the same board, and the receipt is disqualified (section 5).

Recorded only:

- **DNS and connection policy:** `ssh -G hax` effective options
  (`run.ssh_effective_options`); RTT to the resolved address measured by ping
  before the run (`run.rtt`: min/avg/max/stdev ms).
- **Compression:** per arm, on or off (`arms[].compression`).
- **SQLite cache state on the host:** not controlled and not readable from the
  client; recorded as unknown. The 5 warmups exist to amortize it.
- **Host load:** `uptime` 1-minute load on the home host before and after each
  arm (`arms[].host_before/host_after`).
- **Client load:** 1/5/15-minute load on the client before and after each arm
  (`arms[].client_before/client_after`).
- **Failure policy:** a read that exits non-zero, times out
  (`per_read_timeout_s` 60) or returns unparseable output fails its iteration;
  the iteration is excluded from percentiles and counted in
  `measured.failures`; a warmup failure aborts the arm.

## 3. Arms

Each arm has its own cold setup, its own warmups and its own measured
iterations, and proves its connection behaviour rather than asserting it:

| arm | what it is | reuse proof |
|---|---|---|
| ssh | one `ssh hax kb …` per read, `ControlMaster=no ControlPath=none` | `ssh -vv` on a fixture read before and after shows exactly one `Authenticated to` and no mux client lines |
| ssh-controlmaster | one master per arm via `-o ControlMaster=auto -o ControlPath=<tmp> -o ControlPersist=60`, closed with `ssh -O exit` at teardown | `ssh -O check` reports the same master pid before and after; a `-vv` read shows mux client lines and zero `Authenticated to` |
| mcp-over-ssh | one `ssh hax kb mcp` child for the whole arm, JSON-RPC over its stdio, every read a `tools/call` | the same child pid is alive before and after, every request was answered on that one pipe, and its stderr holds exactly one `Authenticated to` |

The ControlMaster arm never touches `~/.ssh/config`; its flags are
per-invocation so the run cannot depend on the operator's ssh configuration.

## 4. Receipt schema — `kanban-agent-loop-benchmark-receipt/1`

```
{
  "schema": "kanban-agent-loop-benchmark-receipt/1",
  "fixture":  { fixture_id, board, kb_exec, ssh_target, expected_remote_hostname,
                actor, lane, warmups, iterations, per_read_timeout_s, reads[] },
  "run":      { command, started_at, finished_at, client{hostname,platform,python},
                rtt{host,min_ms,avg_ms,max_ms,stdev_ms}, ssh_effective_options{},
                percentile_method, warmups, iterations },
  "arms": [ { arm, transport, description, compression, valid,
              setup{cold_probe_ms, kb_version, remote_hostname, …},
              host_before, host_after, client_before, client_after,
              warmups{…}, requests_per_loop,
              measured{ iterations_requested, iterations_run, iterations_ok, failures,
                        loop_ms{n,min,p50,p95,p99,max,mean,stdev}, loop_ms_samples[],
                        bytes_sent_per_loop{…}, bytes_received_per_loop{…} },
              connection_reuse{ reused, how_proven, evidence[] },
              failure_detail, teardown } ],
  "equivalence": { per_read: [ { id, digests_by_arm{arm: [sha256]} } ] },
  "verdict":  { comparable: bool, reasons[] }
}
```

`loop_ms` percentiles are nearest-rank over successful measured loops (value at
`ceil(P/100·n)`, 1-indexed); the raw samples are kept so anyone can recompute.
`requests_per_loop` is 12 for every arm by construction. `bytes_sent` and
`bytes_received` are the request and response payloads as the client saw them,
not wire bytes after compression.

## 5. The immutable comparison protocol

A new arm (GraphQL, HTTP, anything) is comparable to a baseline receipt only
when all of the following hold, and the driver's `verdict.comparable` says so:

1. Same `fixture_id`, same twelve reads in the same order, same pinned ids.
2. Same `kb_version` on the home host as the baseline's `setup.kb_version`,
   or the baseline is re-run on the new release in the same session.
3. 5 warmups and 30 measured iterations, no fewer.
4. Every read's response digest equal across every arm in the receipt. This is
   the load-bearing check: it proves the arms read the same board, and it is
   what caught the first run (below).
5. Zero failures in the measured iterations of every arm compared.
6. Host and client load recorded before and after each arm; a run where the
   client load exceeds twice the RTT-bound expectation is reported with that
   caveat, never silently.

What disqualifies a receipt: any of 1–5 false; a warmup or iteration count
below the fixture's; `expected_remote_hostname` not matching at setup; a
lowered `--limit` on any read.

**The board must be frozen for the run.** The digests in (4) can only match if
nothing writes to the fixture board between the first arm's first read and the
last arm's last read — about 75 minutes at v1 payloads. The kanban board is a
live board, so a comparable run needs either a quiet window with no lane
writing, or a dedicated frozen fixture board adopted from a kanban snapshot
(`workspace adopt --from-board … --name bench-fixture --rootless`, an audited
registry write that belongs to the owner). Until one exists, receipts against
the live board are indicative and their verdict will say `comparable: false`.

## 6. First receipt, 2026-09-05 — disqualified, and what it still shows

`docs/testing/graphql-agent-loop-benchmark-2026-09-05.json`, run from
geoywsMBP against hax (RTT 184–216 ms, avg 205) with the home host serving
`kanban 0.3.0 (board schema 23; registry schema 13)` at `119e41b`.

| arm | p50 | p95 | p99 | min | ok | reuse | recv/loop |
|---|---|---|---|---|---|---|---|
| ssh | 38 761 ms | 58 477 | 58 901 | 31 396 | 30/30 | fresh connection proven, 12 per loop | 719 KB |
| ssh-controlmaster | 36 590 ms | 41 522 | 41 767 | 29 643 | 30/30 | proven: master pid 32246 before and after, 0 re-auth | 727 KB |
| mcp-over-ssh | 47 782 ms | 57 764 | 61 007 | 35 282 | 30/30 | proven: one ssh child pid 93675, 420 requests on one pipe, 1 auth | 772 KB |

**Verdict: `comparable: false`.** Response equivalence failed on seven reads
(`attention_open_100`, `attention_open_500`, `attention_resolved_500`,
`task_list`, `claim_candidates`, `sitrep_lane_20`, `context_candidate`) and held
on the five that read registry or pinned-task state. The cause is known and was
the operator lane itself: the kanban board received checkpoints, notes,
attention rows and new tasks throughout the 74-minute run. The protocol did
its job — a run that looks like a 1.5× spread between arms was not measuring
the same reads.

Declared noise, so nobody reads these as clean: the client ran concurrent
`cargo test` gates for most of the run (client 1-minute load 4.0 → 8.4, against
an idle expectation under 2), and the home host reached load 2.14 during the
MCP arm.

What the disqualified run still establishes, because it does not depend on the
digests matching:

- **Connection setup is not where the time goes.** Removing the handshake
  (ControlMaster) saves about 2 s per loop out of ~37 s. The loop is bound by
  ~720 KB of response payload per iteration over a 205 ms link, which is the
  whole-row `task list` and the two 500-row attention listings.
- **A persistent channel does not beat a reused one at this payload.** MCP
  over one pipe was slower than ControlMaster one-shots: the server runs the
  binary per tool call and every response is framed as JSON-RPC, and it paid
  the host-load spike. Its value is a live session for an interactive harness
  (one connect, ~250 ms per small read — see the /kb skill), not throughput
  on a 720 KB loop.
- **The lever is projection, not transport.** `--fields` / `--no-body` on
  the listings (kanban `53e6ed1`) is expected to cut the payload by an order of
  magnitude; that is the v2 fixture, and it is the comparison a GraphQL arm
  must beat. A GraphQL API whose only advantage is field selection is
  competing with a flag that already exists.

## 7. What a GraphQL arm must do to be measured

Implement nothing until this contract is accepted and a comparable v1 or v2
baseline receipt exists on a frozen fixture board. Then: add the arm to the
driver as a fourth transport with its own reuse proof, run all four arms in one
receipt in one session against the frozen board, and let `verdict.comparable`
decide whether the numbers may be compared. A GraphQL receipt with no ssh
arms beside it is not a comparison.
