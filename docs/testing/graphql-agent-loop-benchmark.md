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
  Volatile keys a read declares in the fixture (`normalize_drop_keys`) are
  removed before digesting, at the top level and from each element of an
  array response. `workspace_list` declares `lastUsedAt`: selecting a board
  stamps that field on the workspace row, so the bench's own reads move it on
  every iteration and a raw digest could never be equivalent on any board.
  Measured 2026-09-07 on the frozen v1f fixture - eleven of twelve reads
  equivalent across arms, this one drifting within every arm with a constant
  body length.

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

## 8. The write half — `kanban-agent-loop-v3`

The read half above is half a loop. A `/kb` worker also writes: it claims a
task, checkpoints under that lease, leaves a note and releases. Four board
writes, and until kanban `2815693` four round trips, because the lease token
the last three need is minted by the first. `kanban transact` (ADR-041) carries
the whole unit in one ordered, all-or-nothing request and resolves that token on
the host with a `$ref`. Section 8 is how that claim is measured.

`docs/testing/bench/fixture-v3.json` is v2's twelve reads carried over byte for
byte — `diff <(jq -S .reads fixture-v2.json) <(jq -S .reads fixture-v3.json)` is
empty, which is what keeps a v3 receipt's read numbers comparable with a v2
receipt's — plus one new `writes` section. The write numbers have no earlier
baseline; v3 is their first.

### 8.1 The unit of work

One write iteration, in this order, as `writes.transact_items`:

| # | operation | arguments that matter |
|---|---|---|
| 0 | `claim` | `id` TASK, `as` `bench@driver` |
| 1 | `checkpoint` | `lease` `{"$ref": {"item": 0, "path": "/leaseToken"}}`, `state` `continue`, `summary`, `intent`, `next-action`, and explicit `repo`/`branch`/`head`/`dirty` |
| 2 | `note` | `text`, `as`, `kind` `progress` |
| 3 | `release` | the same `$ref` lease |

`writes.per_command` is the same four writes as four separate argv/tool forms
with a `TOKEN` placeholder the driver fills in from the claim's own answer. That
hand-threading is the thing being compared: `transact` needs one request where
the per-command form needs four.

The checkpoint carries its git provenance explicitly. ADR-008
(`rust/lib.rs:3576`) refuses a checkpoint whose provenance would be blank and
captures it from the caller's checkout otherwise; `ssh host kb …` lands in
`$HOME`, so capture would either fail or record whatever checkout happens to be
there. Explicit values make the write identical on every arm and on every host.

**TASK is never hardcoded.** A frozen id may already be claimed or done, so the
driver picks one at run time: `claim --candidates --as bench@driver --lane
driver --limit 5 --json` on a scratch copy, first candidate whose status is
`todo` (`claim --candidates` already excludes rows with unmet dependencies and
rows someone holds). No candidate fails the run loudly — `writes.error` in the
receipt, a `verdict` reason, and a non-zero exit.

### 8.2 The scratch-copy rule

**No arm ever writes the frozen fixture.** Per iteration, per arm:

```
scratch=$(mktemp -d /tmp/kb-bench-scratch-XXXXXXXX)
cp -r /root/bench-fixture/. "$scratch/"
chmod -R u+w "$scratch"                 # the fixture is 0400; a copy of a read-only file is read-only
… the four writes, against $scratch …
kb --db $scratch/boards/<board>.db task show TASK --json    # the readback
rm -rf "$scratch"                       # guarded by the /tmp/kb-bench-scratch- prefix, here and on the host
```

The copy and the readback each travel over their own cold ssh — on every arm,
the MCP ones included — and are timed separately as `scratch_copy_ms` and
`readback_ms`. They are **excluded from `write_loop_ms`**, and reported so that
their exclusion is checkable rather than promised. Keeping them off the arm's
own channel also keeps its reuse proof honest: a readback down the MCP pipe
would make `requests_answered` disagree with `requests_per_write_loop`.

Writes address the copy with `--db $scratch/boards/<board>.db`, never with
`--project kanban`. `registry.db` stores absolute board paths, so a copied
registry still names the frozen board: the board name would open the very file
this rule exists to protect. `--db` addresses the copy itself, and a board
outside the data root takes no data-root lock (`rust/lock.rs:130-147`), so a
temp directory needs no registry surgery. `KANBAN_DATA_DIR` is deliberately not
overridden per iteration, because it could not be — an MCP arm is one persistent
`ssh host kb mcp` child whose environment is fixed when the session opens, while
a per-iteration `--db` reaches every arm identically. The board file's basename
is discovered from the frozen registry at run time and never hardcoded.

The rule is argued above and **evidenced** in the receipt: the frozen board's
path, size and mtime are probed before and after every write arm, and a change
disqualifies the receipt.

### 8.3 Arms

| arm | halves | the write half |
|---|---|---|
| ssh | reads + writes | four writes, one fresh ssh each |
| ssh-controlmaster | reads + writes | four writes over one master |
| mcp-over-ssh | reads + writes | four `tools/call` on one pipe |
| mcp-batch | reads only | none: `batch` refuses a tool that is not `readOnlyHint` true |
| mcp-transact | writes only | one `tools/call transact` on one pipe |
| ssh-transact | writes only | one cold ssh, `transact --items-file /dev/stdin`, items on stdin |

`requests_per_write_loop` is 4, 4, 4, —, 1, 1. The two transact arms verify the
whole envelope — `ok`, one result per item, every result `ok`, and a non-zero
exit exactly when `ok` is false — because a rolled-back batch reporting success
would leave every arm's state equal *and wrong*. The item list travels on stdin
rather than in `--items` because an argv string has a size limit a batch does
not (ADR-041 §8), and the board selector travels on the `transact` call itself:
an item naming a board of its own is refused (`rust/lib.rs:4856`).

Each half is its own pass with its own cold setup per arm, so a read receipt is
shaped exactly as v2 left it and an arm with only one half needs no special
case.

### 8.4 Write equivalence — what makes the write half comparable

The read half compares response digests. The write half compares **the state
the board was left in**. After each arm's write half, the readback projects
`task show TASK --json` to four values: `status`, `claim_released` (the claim
row is gone), `notes` (count) and `checkpoints` (count). Counts are absolute
rather than deltas because every iteration starts from a fresh copy of the same
frozen board, so the baseline is a constant.

The write half is comparable iff every arm left the same projected state on
every iteration. Two questions are kept apart, because they have different
answers: whether an arm agreed with itself (`drift_within_an_arm`) and whether
the arms agreed with each other (`all_equivalent`). The fixture also states the
state it expects — `status: todo`, `claim_released: true` — and an iteration
that ends anywhere else fails, so five arms that are wrong in the same way
cannot pass as equivalent.

### 8.5 Receipt — `writes` beside the read half

The read half's schema is unchanged. A v3 receipt adds one top-level `writes`
object, self-identifying as `kanban-agent-loop-benchmark-writes/1`:

```
"writes": {
  "writes_schema": "kanban-agent-loop-benchmark-writes/1",
  "scratch_rule": …, "excluded_from_write_loop_ms": ["scratch_copy_ms", "readback_ms"],
  "board_file": …, "frozen_source": …,
  "task": { task, selected_index, required_status, candidates[], command },
  "arms": [ { arm, half: "writes", transport, description, writes_per_loop,
              requests_per_write_loop, setup, host_before, host_after,
              warmups{…},
              measured{ iterations_ok, failures,
                        write_loop_ms{n,min,p50,p95,p99,max,mean,stdev},
                        write_loop_ms_samples[], scratch_copy_ms{…}, readback_ms{…},
                        bytes_sent_per_write_loop{…}, bytes_received_per_write_loop{…},
                        per_step{ id: {ms{…}, bytes_sent, bytes_received} },
                        state{ distinct[], value } },
              connection_reuse{…}, frozen_board{ before, after, unchanged },
              scratch_leaked[], failure_detail[], valid } ],
  "equivalence": { state_by_arm, projection, all_equivalent, drift_within_an_arm }
}
```

A run with only one half says so rather than leaving a key out: the other half
carries `not_run` with the reason. `verdict.reasons` prefixes every write-half
reason with `writes:` or `writes/<arm>:`, so one merged verdict still names
which half disqualified the receipt.

### 8.6 Running it

```
python3 docs/testing/bench/agent-loop-bench.py \
  --fixture docs/testing/bench/fixture-v3.json \
  --out docs/testing/graphql-agent-loop-benchmark-v3-<date>.json
```

Both halves run by default, reads first; `--reads-only` and `--writes-only`
select one. `--warmups`, `--iterations`, `--arms`, `--out` and `--quiet` are
unchanged, and lowering either count still disqualifies the receipt.

`--dry-run` prints the exact remote command lines and JSON-RPC frames one
iteration of every selected arm would issue — placeholders substituted from a
fake candidate `t-dry` — and exits 0 without contacting any host. It is built
from the same code the measured run uses, which is the only reason a printed
line is worth reading, and it is how this driver is reviewed before it is ever
pointed at the board home.

## 9. Receipt, 2026-09-07 — the write half in one request

`docs/testing/graphql-agent-loop-benchmark-v3-2026-09-07.json`, run from the
MBP against the frozen fixture on hax, bench binary built from kanban
`2815693` (the served release stayed at `1de9bbf`), 5 warmups + 30 measured
iterations per arm per half, task `t-38ee2070` selected by `claim --candidates`
on a throwaway copy. Every arm 30/30, one distinct end state across all five
arms (`status: todo`, claim released, 2 notes, 1 checkpoint), the frozen board
byte-identical before and after every arm, no scratch directory leaked.

### 9.1 The write half — claim, checkpoint by `$ref`, note, release

| arm | requests | p50 | p95 | p99 | per step p50 |
|---|---|---|---|---|---|
| ssh, cold per command | 4 | **10,698 ms** | 12,091 | 12,625 | 2.5–2.6 s each |
| ssh ControlMaster, per command | 4 | **2,223 ms** | 2,776 | 3,540 | ~0.5 s each |
| MCP over ssh, per command | 4 | **1,081 ms** | 1,506 | 1,557 | ~0.26 s each |
| MCP `transact`, one call | 1 | **312 ms** | 343 | 571 | 312 |
| ssh `transact --items-file /dev/stdin`, one cold ssh | 1 | **2,741 ms** | 4,644 | 6,721 | 2,740 |

The write half costs one request on both surfaces. On the persistent MCP
session that is 312 ms where four calls cost 1,081 ms — the difference is
three round trips, exactly as ADR-041 predicted, and the atomic batch is the
only arm that can fail cleanly. On the cold-ssh CLI path a single `transact`
is one ssh setup (2.7 s) where four commands cost four (10.7 s); it is not
faster than one command, and it was never going to be — the CLI arm's floor is
the ssh handshake, which is why the persistent MCP session exists.

### 9.2 The read half, and why this run's read numbers are not comparable to v2

| arm | requests | p50 v2 (2026-09-07 morning) | p50 v3 (2026-09-07 afternoon) |
|---|---|---|---|
| ssh, cold per read | 12 | 26,582 ms | 31,659 ms |
| ssh ControlMaster | 12 | 4,686 ms | 10,110 ms |
| MCP over ssh | 12 | 2,451 ms | 8,637 ms |
| MCP `batch` | 2 | **424 ms** | 6,776 ms |

The reads are byte-identical to v2 and the bench binary answers `task list` in
9 ms on hax under both builds (`kb-bench-9d9685d` and `kb-bench-2815693`,
measured back to back). What changed is the link: the run's own `rtt` block
records 190.9 ms mean with 10.9 ms jitter against v2's 168.6 ms with 0.5 ms,
and the heavy reads moved at ~30–35 KB/s (`attention_resolved_500`, 72 KB, in
2,357 ms on the per-command MCP arm; the 238 KB batch in ~6.5 s) where the
morning run moved them at ~180 KB/s. A cold ssh pulling 240 KB measured 9.4 s
after the run, so the link had not recovered when this was written. Within the
run the ordering holds — `batch` is still the fastest read arm — but the
absolute read numbers here are a measurement of the afternoon's route, not of
the code, and v2's read half remains the reference until a reads-only re-run
on a healthy link replaces it. The write arms are unaffected as a comparison:
all five ran on the same link in the same hour, and their payloads are small
enough (a few KB) that bandwidth was not the term.

### 9.2.1 Reads-only re-run, 2026-09-09 — the healthy link confirms v2

`docs/testing/graphql-agent-loop-benchmark-v3-reads-2026-09-09.json`, run from
the MBP against the frozen fixture on hax at 02:39–03:01 MYT with
`--reads-only`, once the link had recovered: rtt 168.1 ms mean with 0.46 ms
jitter (v2: 168.6 / 0.5), a cold ssh pulling 240 KB in 1.03 s (the 2026-09-07
afternoon: 9.4–10.8 s). The verdict block says `comparable: true`.

| arm | requests | p50 v2 (2026-09-07) | p50 v3 reads (2026-09-09) |
|---|---|---|---|
| ssh, cold per read | 12 | 26,111 ms | 26,218 ms |
| ssh ControlMaster | 12 | 4,371 ms | 4,235 ms |
| MCP over ssh | 12 | 2,108 ms | 2,086 ms |
| MCP `batch` | 2 | 424 ms | **427 ms** |

Every arm lands within 3% of v2 (p95 on `batch`: 441 ms). So §9.2's afternoon
numbers were the route and nothing else; the read half is unchanged by the v3
binary, and this receipt replaces v2 as the read reference. Served binary
during the run: kanban `b048e02` on hax, unchanged between probes.

### 9.3 What a loop costs now

Two requests for the reads (`batch`) and one for the writes (`transact`) on
the MCP session; on the cold-ssh CLI path, the same three requests are three
ssh setups instead of sixteen. The idempotency gap stands as ADR-041 §9
states it: a replayed START batch is refused at its claim and lands nothing.
