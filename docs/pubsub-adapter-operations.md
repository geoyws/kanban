# Pub/sub adapter operations

**Epic:** `e-pubsub-ops`. **Task:** `t-c0b8f7b5`. **Date:** 2026-09-06.
**Read at:** `37a6f56`. Every claim below is either cited to a `file:line` in
this tree or backed by a command recorded in this document and run against a
hermetic fake.

**What was NOT involved.** No real OpenCode server, no Kimi login or account,
no Cursor account or installed Cursor worker, no ZCode installation, no
installed `codex-cli`, and no installed Claude Code took part in anything
recorded here. Every receipt drives a dependency-free fake compiled from
`tests/fixtures/`. Nothing in this document asserts a live-host behaviour.
Live-host support for the Codex bridges is a separate named smoke receipt and
is not implied by anything here — see
`docs/adr/ADR-031-ledger-first-pubsub-uses-the-append-only-event-ledger.md`
lines 486-490 and 514-519.

This document does not restate ADR-031 (the ledger-as-bus decision, cursor
semantics, dispatcher configuration) or `docs/ui-pubsub-consumption-seams.md`
(which channel each UI surface may consume). Read those for *why*. Read this
for *what to do at 03:00*.

---

## 1. Read this part first, or the rest will mislead you

### 1.1 The board does not show you the adapter's error code

Each adapter classifies its own failures into a distinct code string and a
distinct process exit status. **The dispatcher records neither.**

- `run_process` collapses every non-zero adapter exit into the single literal
  `"adapter_exit"` — `rust/adapter_process.rs:373-375`:
  `ChildState::Exited(status) if !status.success() => { return Err(failure("adapter_exit", false)); }`
- That string is what the dispatcher hands to the store —
  `rust/dispatcher.rs:731-734` (`code: failure.code`) and
  `rust/dispatcher.rs:758-767` (`finalize_subscription_delivery_failure(..., failure.code)`).
- The store writes it verbatim into `subscription_deliveries.last_error_code`
  — `rust/store.rs:2924-2931`.
- The adapter's stderr, which carries the real code, is drained, bounded, and
  then **dropped**: `run_process` binds it at
  `rust/adapter_process.rs:381` and thereafter only checks it for overflow
  (`rust/adapter_process.rs:385-387`). It is never returned and never stored.
- The adapter's numeric exit status is not stored either. Only the boolean
  "did it succeed".

So `last_error_code = 'adapter_exit'` means *"an adapter failed and told us
why on a channel we threw away"*. **To learn which failure it was, you must
re-run the adapter by hand.** Section 2 is how. Every per-adapter section
gives you the exact command.

The codes you can actually see on a delivery row, all produced by the
dispatcher rather than by an adapter, are:

| `last_error_code` | Source | Meaning |
|---|---|---|
| `adapter_exit` | `rust/adapter_process.rs:374` | The adapter exited non-zero. Cause discarded. |
| `adapter_timeout` | `rust/adapter_process.rs:370` | The subscription's own `timeoutMs` elapsed; the adapter's process group was terminated. |
| `adapter_cancelled` | `rust/adapter_process.rs:371` | SIGINT/SIGTERM reached the dispatcher mid-delivery. |
| `adapter_response_invalid` | `rust/dispatcher.rs:735-738` | The adapter exited 0 but its stdout is not an `AdapterResponse` for this exact delivery. |
| `adapter_stdout_overflow` / `adapter_stderr_overflow` | `rust/adapter_process.rs:382-387` | The adapter wrote more than `STREAM_LIMIT` = 1 MiB (`rust/adapter_process.rs:11`) on that stream. |
| `adapter_target_invalid` | `rust/adapter_process.rs:236-238` | Host config named a relative executable, or `timeoutMs` outside `1..=300000`. |
| `adapter_spawn` | `rust/adapter_process.rs:258-260` | The configured executable could not be started. |
| `dispatcher_lease_expired` | `rust/store.rs:2741` | The worker or host died mid-delivery; the lease was recovered. At-least-once boundary (ADR-031 lines 341-346). |

### 1.2 "Terminal" is advice to you, not behaviour of the dispatcher

Every adapter labels each failure class `retryable` or `terminal`, and prints
that word on stderr. **The dispatcher never sees it.** Whether a delivery is
retried or dead-lettered is decided purely by attempt count:

```
rust/store.rs:2912   let terminal = delivery.attempts > subscription.max_retries;
```

Backoff is `min(1000 << (attempt-1), timeout_ms)` milliseconds —
`rust/store.rs:1286-1291`.

Consequence, and it is the reason this section exists: **a `terminal`
classification still burns the subscription's whole retry budget.** An
adapter that reports `opencode_request_rejected` on attempt 1 will be invoked
again on attempts 2..=`maxRetries+1`, fail identically each time, and only
then dead-letter. The adapters' own doc comments describe terminal classes as
avoiding exactly that (`rust/opencode_adapter.rs:176-178`,
`rust/cursor_worker_adapter.rs:230-232`), which is the *intent* of the
vocabulary — but no code consumes it. If you see a delivery burning attempts,
do not wait for the classification to save the budget; pause the subscription.

### 1.3 What every adapter refuses before it touches its peer

All seven share `rust/adapter_protocol.rs`, and all seven read one delivery
document from stdin and refuse it before starting or contacting anything:

- Protocol version must be exactly `1` — `rust/adapter_protocol.rs:66-71`.
- `attempt` must be `>= 1` — `:72-74`.
- `eventID` must be lowercase 64-hex — `:75-77`, `:48-53`.
- `event.eventID`, `event.eventHash` and `event.timestamp` must match the
  delivery envelope — `:82-90`. An adapter cannot be handed an envelope for
  one event carrying another event's payload.
- The event must not contain a secret-shaped key — `:91-93` via
  `:55-63`, which recurses the whole JSON and matches any key whose
  alphanumeric-normalised form ends in `token`, `secret`, `credential` or
  `material` (`rust/watch.rs:826-844`).
- Each adapter additionally pins its own `consumerID` and `actionID` and
  refuses any other pair, **before** the peer is contacted. So an operator
  cannot re-point a subscription at the wrong binary by editing an action ID.
- Unknown flags, repeated flags and positional arguments are all refused by
  name in every adapter (e.g. `rust/zcode_notify_adapter.rs:249-274`).

An accepted acknowledgement must name this exact delivery: subscription,
event and `createdAt` all compared — `rust/adapter_protocol.rs:130-138`.

### 1.4 Exit statuses are not comparable across adapters

Three adapters carry a numeric classification; four exit plain `1` for
everything.

| Adapter | Classified exit statuses | Wiring |
|---|---|---|
| `kanban-opencode-adapter` | 10-14 | `rust/lib.rs:4384-4396` |
| `kanban-kimi-acp-adapter` | 10-15 | `rust/lib.rs:4399-4411` |
| `kanban-cursor-worker-adapter` | 20-25 | `rust/lib.rs:4414-4426` |
| `kanban-zcode-notify-adapter` | 10, 11, 13, 14 | `rust/lib.rs:4429-4442` |
| `kanban-codex-queue-adapter` | always `1` | `rust/lib.rs:4347-4356` |
| `kanban-codex-app-server-adapter` | always `1` | `rust/lib.rs:4359-4370` |
| `kanban-claude-print-adapter` | always `1` | `rust/lib.rs:4373-4381` |

`1` also means "unclassified local error" in the four classified adapters:
bad arguments, an undecodable delivery, an untrusted peer path. See
`rust/opencode_adapter.rs:52-53` and `rust/kimi_acp_adapter.rs:117-119`. The
ZCode statuses are chosen to mean the same thing as OpenCode's — 10 peer
unreachable, 11 peer refused, 13 deadline, 14 answer not believable — with no
analogue for 12 because there is no response channel to fail halfway
(`rust/zcode_notify_adapter.rs:174-179`). Cursor's 20-25 are deliberately
disjoint.

---

## 2. Reproducing any adapter by hand

Every receipt in this document was produced with this preamble. `$REPO` is
this worktree.

```bash
cd "$REPO"
cargo build --bins                     # 12.20s observed, from a cold target/
SMOKE=/tmp/kanban-adapter-smoke
rm -rf "$SMOKE"; mkdir -p "$SMOKE"; chmod 700 "$SMOKE"
```

`$SMOKE` must be mode `0700` and owned by you: the Kimi and Claude adapters
walk the whole ancestor chain and refuse a group- or world-writable directory
unless it is sticky (`rust/kimi_acp_adapter.rs:377-391`). `/tmp` itself
passes because it is root-owned and sticky.

Fakes are compiled with plain `rustc`, no dependencies, exactly as the e2e
harness does it (`tests/opencode_adapter_e2e.rs:20-37`):

```bash
rustc --edition=2024 tests/fixtures/<fake>.rs -o "$SMOKE/<name>"
```

Then feed the adapter one delivery document on stdin and read its exit status
and stderr. That stderr line — `Error: <code> (<disposition>): <detail>` — is
the thing the dispatcher discarded.

Every argument-surface table in this document is the adapter's own `HELP`
constant. Confirm all seven in one command — RUN 2026-09-06:

```bash
for b in codex-queue codex-app-server claude-print opencode kimi-acp cursor-worker zcode-notify; do
  ./target/debug/kanban-$b-adapter --help
done
```
Observed:
```
kanban-codex-queue-adapter --codex PATH --codex-home PATH --thread NAME --required-version VER
kanban-codex-app-server-adapter --codex PATH --codex-home PATH --cwd PATH --required-version VER --client-request-sha256 HEX --protocol-schema-sha256 HEX --protocol-timeout-ms N
kanban-claude-print-adapter --claude ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --required-version VERSION
kanban-opencode-adapter --endpoint http://LOOPBACK_IP:PORT/ABSOLUTE_PATH --request-timeout-ms N
kanban-kimi-acp-adapter --kimi ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --request-timeout-ms N
kanban-cursor-worker-adapter --worker ABSOLUTE_PATH --state-dir ABSOLUTE_PATH --turn-timeout-ms N --queue-wait-ms N
kanban-zcode-notify-adapter --sink ABSOLUTE_PATH --notify-timeout-ms N
```

Note what is absent: no adapter accepts a `--project`, `--db`, `--consumer`,
or credential flag. Everything an adapter is allowed to know comes from the
host allow-list's fixed `args` and the delivery on stdin (ADR-031 lines
317-323).

---

## 3. `kanban-opencode-adapter`

### Talks to
One local HTTP endpoint. It opens a TCP connection to a **loopback literal**,
sends one `POST` carrying the delivery document as the body, and reads one
response. It is an HTTP *client* and never listens.

Request head is fixed — `rust/opencode_adapter.rs:447-459`: `POST <path>
HTTP/1.1`, `Host`, `User-Agent: kanban-opencode-adapter/<version>`,
`Content-Type: application/json`, `Content-Length`, `Connection: close`.
There is no `Authorization` header and no way to add one.

Target pair: `opencode.server` / `enqueue-turn`
(`rust/opencode_adapter.rs:25-26`, enforced at `:367-375`).

### Argument surface
```
kanban-opencode-adapter --endpoint http://LOOPBACK_IP:PORT/ABSOLUTE_PATH --request-timeout-ms N
```
(`rust/opencode_adapter.rs:11`; also `--help` / `--version` alone.)

| Flag | Rule | Citation |
|---|---|---|
| `--endpoint` | Required. 1..=256 bytes, printable ASCII without spaces. Must start `http://`. Must carry an absolute path. No userinfo, no query, no fragment, no `.`/`..` segments, path ≤ 128 bytes. Host must be a bare IPv4 or bracketed IPv6 **literal** — no DNS. Explicit unpadded port `1..=65535`. Host must be loopback. | `:280-353` |
| `--request-timeout-ms` | Required. Integer in `1000..=300000`. | `:261-269`, `:17-18` |

Stdin is capped at 1 MiB (`:12`, `:355-365`). Response body must declare a
`Content-Length` ≤ 65536 (`:14`, `:626-671`).

### Failure codes

| Code | Exit | Disposition | What it means | Your next move |
|---|---|---|---|---|
| `opencode_endpoint_unreachable` | 10 | retryable | TCP connect refused. The OpenCode server is not listening, is restarting, or is on another port (`:427-445`). | Start or re-point the server. Do not look at the event. |
| `opencode_endpoint_failed` | 12 | retryable | 5xx, 408, or 429; or the connection died mid-write or mid-body. The server took the bytes then failed (`:193`, `:461-515`, `:673-700`). 408/429 are classed here because RFC 9110 makes them retry invitations (`:164-170`). | Look at the server's own logs. The delivery is fine. |
| `opencode_deadline_exceeded` | 13 | retryable | `--request-timeout-ms` elapsed during connect, write, or read (`:412-421`, `:497-505`). | Raise the timeout or fix the wedged server. Never read as a rejection. |
| `opencode_request_rejected` | 11 | **terminal** | Any other 4xx. The server understood and refused this route or payload — unknown path, unsupported version, body it will not take (`:194`, `:180-183`). | Fix the configured path or the server's route. Retrying sends byte-identical bytes. Pause the subscription; see §1.2. |
| `opencode_response_invalid` | 14 | **terminal** | 1xx/3xx, no `Content-Length`, conflicting or malformed `Content-Length`, over-long body, or a 2xx body that is not an `AdapterResponse` naming this delivery (`:195`, `:626-671`, `:184-189`). A redirect here means the configured URL is wrong. | Re-check `--endpoint`. A chunked answer lands here by design (`:617-625`). |

Codes at `:101-109`, dispositions at `:113-118`, statuses at `:120-128`.

### What it explicitly does not do
- Does not resolve a hostname. Ever. Name resolution is removed from the
  delivery path (`:277-279`).
- Does not read the endpoint from the environment (`:271-276`).
- Does not spawn any process.
- Does not authenticate. No token, header, or credential is sent (`:447-459`).
- Does not accept a chunked or length-less response (`:617-625`).
- Does not retry internally. One attempt per invocation.

### Smoke receipt — RUN 2026-09-06

```bash
E=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
mkdir -p "$SMOKE/opencode"
rustc --edition=2024 tests/fixtures/opencode_adapter_fake_server.rs -o "$SMOKE/opencode/fake-server"
cat > "$SMOKE/opencode/delivery.json" <<JSON
{"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":"$E","attempt":2,"createdAt":1720000000},"target":{"consumerID":"opencode.server","actionID":"enqueue-turn"},"event":{"eventID":"$E","eventHash":"$E","timestamp":1720000000,"body":"smoke"}}
JSON
cat > "$SMOKE/opencode/ack.json" <<JSON
{"protocolVersion":1,"subscriptionID":"sub-test","eventID":"$E","createdAt":1720000000,"replay":true}
JSON
"$SMOKE/opencode/fake-server" accept "$SMOKE/opencode/port" "$SMOKE/opencode/capture" "$SMOKE/opencode/ack.json" &
until [ -s "$SMOKE/opencode/port" ]; do sleep 0.05; done
PORT=$(cat "$SMOKE/opencode/port")
./target/debug/kanban-opencode-adapter --endpoint "http://127.0.0.1:$PORT/delivery" \
  --request-timeout-ms 5000 < "$SMOKE/opencode/delivery.json" 2>&1; echo " exit=$?"
sed -n '1,6p' "$SMOKE/opencode/capture"
```

Observed — the acknowledgement carries no trailing newline, so ` exit=0`
lands on the same line:
```
{"protocolVersion":1,"subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","createdAt":1720000000,"replay":true} exit=0
POST /delivery HTTP/1.1
Host: 127.0.0.1:57348
User-Agent: kanban-opencode-adapter/0.3.0
Content-Type: application/json
Content-Length: 446
Connection: close
```

Failure classes, same fixture (`reject` scenario) plus a released port:
```bash
# $1 is a label, printed so the transcript is literally what the terminal showed.
o() { printf '%-16s' "$1"; shift; ./target/debug/kanban-opencode-adapter "$@" \
        < "$SMOKE/opencode/delivery.json" 2>&1; echo " exit=$?"; }
DEAD=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
o unreachable  --endpoint "http://127.0.0.1:$DEAD/delivery" --request-timeout-ms 5000
o non-loopback --endpoint "http://10.0.0.1:8080/delivery"   --request-timeout-ms 5000
"$SMOKE/opencode/fake-server" reject "$SMOKE/opencode/port2" "$SMOKE/opencode/capture2" "$SMOKE/opencode/ack.json" &
until [ -s "$SMOKE/opencode/port2" ]; do sleep 0.05; done
o rejected     --endpoint "http://127.0.0.1:$(cat "$SMOKE/opencode/port2")/delivery" --request-timeout-ms 5000
```
Observed:
```
unreachable     Error: opencode_endpoint_unreachable (retryable): connecting to 127.0.0.1:57350: Connection refused (os error 61)
 exit=10
non-loopback    Error: --endpoint host must be a loopback address
 exit=1
rejected        Error: opencode_request_rejected (terminal): the endpoint answered HTTP 400
 exit=11
```

---

## 4. `kanban-kimi-acp-adapter`

### Talks to
One spawned ACP peer over its stdio. JSON-RPC 2.0 over NDJSON: this adapter
writes exactly one request frame and reads exactly one response frame
(`rust/kimi_acp_adapter.rs:1-26`). Method is `_kanban/deliverEvent`, id is
always the literal `1`, `params` is the `AdapterRequest` verbatim
(`:87`, `:91`, `:475-502`).

Target pair: `kimi.acp` / `enqueue-turn` (`:80-85`, enforced `:457-465`).

### Argument surface
```
kanban-kimi-acp-adapter --kimi ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --request-timeout-ms N
```
(`:67`.)

| Flag | Rule | Citation |
|---|---|---|
| `--kimi` | Required, absolute. Must not be a symlink. Must be a regular file, executable, with no group/other write bit, owned by this euid or root. Every canonical ancestor must be a directory owned by this euid or root, and not group/other-writable unless sticky. Re-opened after the stat to confirm the inode did not change. | `:399-423`, `:377-391` |
| `--home` | Required, absolute. Becomes the peer's `HOME`. Must be a directory with `0o077` clear (private), same owner and ancestor rules. | `:399-423`, `:410` |
| `--cwd` | Required, absolute. Becomes the peer's working directory. Same directory rules **and must be empty**. | `:428-433` |
| `--request-timeout-ms` | Required. Integer `1000..=300000`. | `:358-366` |

The peer is spawned with **no argv at all**, a cleared environment carrying
only `HOME` and `PATH=/usr/bin:/bin`, its stderr on `/dev/null`, and its own
process group (`:755-774`). No argv because the flag that puts a given vendor
CLI into ACP mode is a property of that CLI's release, not of this protocol
(`:28-38`).

### Failure codes

| Code | Exit | Disposition | What it means | Your next move |
|---|---|---|---|---|
| `kimi_peer_unanswered` | 10 | retryable | The peer closed stdout, exited, or wrote an unterminated fragment before one complete frame arrived (`:627-660`, `:603-625`). A truncated frame is reported as *no* answer and is never parsed (`:544-569`). | Peer is starting, crashed, or not logged in. Fix the peer. At-least-once plus the peer's own `subscriptionID:eventID` dedup covers a peer that had accepted before dying (`:198-203`). |
| `kimi_deadline_exceeded` | 14 | retryable | `--request-timeout-ms` elapsed while writing the frame or waiting for the answer (`:583-593`, `:616-619`, `:651-654`). | Raise the timeout or unwedge the peer. Not a refusal. |
| `kimi_frame_malformed` | 11 | **terminal** | Not one JSON-RPC 2.0 response object: unparseable, trailing bytes, unrecognised envelope member, both or neither of `result`/`error`, or a `result` that is not a protocol-1 acknowledgement (`:663-715`, `:717-741`). | Peer version or configuration mismatch. Do not hunt a framing bug in the adapter; the frame is byte-identical every attempt. |
| `kimi_frame_oversized` | 12 | **terminal** | The answer passed 65536 bytes with no terminator, and was refused **unread** (`:70`, `:544-569`, `:631-636`). | The peer's answer shape is wrong. A retry reproduces it. |
| `kimi_identity_mismatch` | 13 | **terminal** | The answer does not name this delivery: a JSON-RPC id other than `1`, or an acknowledgement whose subscription, event or timestamp belongs elsewhere (`:686-696`, `:742-752`). | This is the class that would corrupt the ledger if believed. Suspect a shared or mis-multiplexed peer. Retrying only hides it behind a loop (`:229-237`). |
| `kimi_request_rejected` | 15 | **terminal** | A well-formed JSON-RPC error for this exact id — usually `-32601 Method not found`, i.e. the peer does not implement `_kanban/deliverEvent` (`:706-712`). | Peer version mismatch. This is a legible refusal, not a framing bug (`:238-243`). |

Codes at `:171-180`, dispositions `:244-252`, statuses `:254-263`.

### What it explicitly does not do
- Does not run the ACP `initialize` / `session/new` handshake, so it does not
  drive a stock vendor CLI's interactive session lifecycle (`:28-38`).
- Does not pass any argv to the peer (`:755-757`).
- Does not read a second frame. Bytes after the first terminator stay unread
  (`:24-26`, `:542-543`).
- Does not read the peer's stderr — it is `/dev/null`, deliberately, so a
  peer banner cannot corrupt this adapter's own classification channel and an
  undrained pipe cannot wedge the peer mid-answer (`:764-769`).
- Does not close the peer's stdin before the answer arrives (`:600-602`).
- Does not turn a proven acknowledgement into a failure because of what the
  peer does afterwards; an acknowledged peer gets a 2s window to leave, then
  the group is terminated (`:783-804`, `:78-79`).

### Smoke receipt — RUN 2026-09-06

```bash
E=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
mkdir -p "$SMOKE/kimi/home" "$SMOKE/kimi/cwd"
chmod 700 "$SMOKE/kimi" "$SMOKE/kimi/home" "$SMOKE/kimi/cwd"
rustc --edition=2024 tests/fixtures/kimi_acp_adapter_fake_peer.rs -o "$SMOKE/kimi/fake-peer"
cat > "$SMOKE/kimi/delivery.json" <<JSON
{"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":"$E","attempt":2,"createdAt":1720000000},"target":{"consumerID":"kimi.acp","actionID":"enqueue-turn"},"event":{"eventID":"$E","eventHash":"$E","timestamp":1720000000,"body":"smoke"}}
JSON
for S in accept reject alien-id truncate malformed oversized mismatch hang; do
  printf '%s' "$S" > "$SMOKE/kimi/home/scenario.txt"
  printf '%-10s' "$S"
  ./target/debug/kanban-kimi-acp-adapter --kimi "$SMOKE/kimi/fake-peer" \
    --home "$SMOKE/kimi/home" --cwd "$SMOKE/kimi/cwd" \
    --request-timeout-ms 1000 < "$SMOKE/kimi/delivery.json" 2>&1; echo " exit=$?"
done
python3 -c "import json; r=json.loads(open('$SMOKE/kimi/capture.ndjson').readline()); print('argv=',r['argv'],'env=',r['env'],'cwd=',r['cwd'])"
ls -A "$SMOKE/kimi/cwd" | wc -l
```
(`accept` through `mismatch` were also run at `--request-timeout-ms 5000`;
`hang` needs the short deadline. The `printf` prefixes the scenario name so
the transcript below is literally what the terminal showed.)

Observed — all six classes, one command:
```
accept    {"protocolVersion":1,"subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","createdAt":1720000000,"replay":true} exit=0
reject    Error: kimi_request_rejected (terminal): the peer refused the delivery with JSON-RPC error -32601: Method not found
 exit=15
alien-id  Error: kimi_identity_mismatch (terminal): the peer answered request id 99, not the id 1 this delivery asked
 exit=13
truncate  Error: kimi_peer_unanswered (retryable): the peer closed its stdout after 33 bytes of an unterminated frame, which is not an answer
 exit=10
malformed Error: kimi_frame_malformed (terminal): the peer's frame is not one JSON-RPC response object: EOF while parsing an object at line 1 column 1
 exit=11
oversized Error: kimi_frame_oversized (terminal): the peer's answer passed 65536 bytes with no frame terminator, so it was refused unread
 exit=12
mismatch  Error: kimi_identity_mismatch (terminal): the peer acknowledged a delivery this process never sent: adapter response event identity does not match request
 exit=13
hang      Error: kimi_deadline_exceeded (retryable): the peer did not answer the delivery before the deadline
 exit=14
argv= [] env= [['HOME', '/private/tmp/kanban-adapter-smoke/kimi/home'], ['PATH', '/usr/bin:/bin']] cwd= /private/tmp/kanban-adapter-smoke/kimi/cwd
0
```

The last two lines are the fake's own record of how it was spawned — no argv,
an environment of exactly `HOME` and `PATH` — and the count of entries in
`--cwd` after all eight runs: still zero.

Note the `alien-id` line: the answer carried a *valid* acknowledgement for
this exact delivery under id 99, and the adapter refused it on the id alone —
the payload was never inspected. That is `:686-696`, comment
`// Identity before content`.

---

## 5. `kanban-cursor-worker-adapter`

### Talks to
One host-configured worker executable that runs a single agent turn per
invocation. Delivery goes in on stdin; the acknowledgement comes back on
stdout. **One turn at a time per state directory.**

Target pair: `cursor.worker` / `start-turn` (`rust/cursor_worker_adapter.rs:49-50`).
`start-turn`, not `enqueue-turn`, because the turn runs inside the invocation
rather than being queued for an open session (`:45-48`).

### Argument surface
```
kanban-cursor-worker-adapter --worker ABSOLUTE_PATH --state-dir ABSOLUTE_PATH --turn-timeout-ms N --queue-wait-ms N
```
(`:33`.)

| Flag | Rule | Citation |
|---|---|---|
| `--worker` | Required, absolute. Not a symlink; regular file; has an execute bit; no group/other write bit; owned by this euid or root. | `:566-588`, `:617-623` |
| `--state-dir` | Required, absolute. Not a symlink; a directory; `0o077` clear; owned by this euid or root. Becomes the worker's `HOME`, its working directory, and the turn slot's identity. | `:597-615` |
| `--turn-timeout-ms` | Required. Integer `1000..=600000`. Measured from slot acquisition, not process start. | `:661-665`, `:38-39`, `:354-359` |
| `--queue-wait-ms` | Required. Integer `0..=600000`. `0` is the honest spelling of "refuse immediately". | `:666-670`, `:40`, `:287-297` |

Child argv is the fixed `["--headless","--protocol-version","1"]` (`:51-54`).
Child environment is cleared to `HOME=<state-dir>` and
`PATH=/usr/bin:/bin` (`:415-436`).

### The turn slot
An exclusive `flock` on `<state-dir>/.kanban-cursor-turn.lock`, created if
absent, never truncated, never unlinked (`:44`, `:313-345`). Keyed on the
resolved state directory's inode — not the host, not the subscription, not
the executable (`:276-285`). Two deliveries pointed at one state directory
cannot both run; two pointed at different state directories run in parallel.

**A holder that died without releasing needs no cleanup.** An `flock` lives
on the open file description, so the kernel drops it when the descriptor
closes — clean exit, panic, `process::exit`, and `SIGKILL` alike — and the
next waiter is granted the slot on its next 25 ms poll (`:299-307`, `:41`,
`:347-352`). There is no PID file and no liveness guess to get wrong. This is
verified by receipt below, not only by reading.

Ordering within `run` is deliberate: stdin is read before the slot is taken,
and the slot is released before stdout is written, so a slow dispatcher never
pins the worker (`:88-107`).

### Failure codes

| Code | Exit | Disposition | What it means | Your next move |
|---|---|---|---|---|
| `cursor_worker_busy` | 20 | retryable | The whole `--queue-wait-ms` budget elapsed with another turn holding the slot (`:328-336`). | Nothing is wrong. The holder finishes on its own. Raise `--queue-wait-ms` if you see this often. |
| `cursor_worker_unavailable` | 21 | retryable | The worker executable or state dir is not something we will exec into, the slot could not be opened, or the spawn failed (`:566-588`, `:597-615`, `:415-436`). | Host condition. Install the worker or fix the mode/owner. The delivery is fine. |
| `cursor_worker_turn_failed` | 22 | retryable | The worker ran and exited non-zero, or died on a signal. The worker's stderr is quoted into the message (`:247-257`, `:259-272`). | Read the quoted stderr. Stays retryable deliberately: an agent turn's exit status carries no refusal semantics (`:218-224`). |
| `cursor_worker_deadline_exceeded` | 23 | retryable | The turn outran `--turn-timeout-ms`. Ended with SIGTERM → grace → SIGKILL → reap, so session files get a chance to stay consistent (`:460-483`, `:438-452`). | Raise the timeout or fix the wedged worker. |
| `cursor_worker_response_invalid` | 24 | **terminal** | Turn exited 0 but stdout is not a protocol-1 `AdapterResponse`, or exceeded 65536 bytes (`:406-411`, `:524-536`). | Worker speaks the wrong contract. Configuration fault. |
| `cursor_worker_response_mismatched` | 25 | **terminal** | Stdout **is** a well-formed protocol-1 acknowledgement but names a different subscription, event or timestamp (`:527-530`, `:545-548`). | This is the cross-talk signature serialization exists to prevent. Two turns are sharing one state directory, or the worker is answering for someone else. Never record it as delivered. |

Codes `:143-151`, dispositions `:156-163`, statuses `:166-175`.

### What it explicitly does not do
- Does not put any event content in the worker's argv — the delivery travels
  on stdin precisely because argv is world-readable in the process table
  (`:3-8`, `:51-54`). Verified by receipt.
- Does not treat non-empty stderr on a *successful* turn as a failure; an
  agent CLI logs progress there (`:259-263`).
- Does not chase descendants the worker orphaned; the dispatcher's outer
  process-group kill already covers them, and taking a private group would
  remove them from that net (`:454-459`).
- Does not unlink the lock file (`:309-312`).
- Does not promise the worker is read-only (`:45-48`).

### Smoke receipt — RUN 2026-09-06

```bash
A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
mkdir -p "$SMOKE/cursor/state"; chmod 700 "$SMOKE/cursor" "$SMOKE/cursor/state"
rustc --edition=2024 tests/fixtures/cursor_worker_adapter_fake_worker.rs -o "$SMOKE/cursor/fake-worker"
mk() { printf '{"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":"%s","attempt":1,"createdAt":1720000000},"target":{"consumerID":"cursor.worker","actionID":"start-turn"},"event":{"eventID":"%s","eventHash":"%s","timestamp":1720000000,"body":"smoke"}}' "$1" "$1" "$1"; }
mk "$A" > "$SMOKE/cursor/a.json"; mk "$B" > "$SMOKE/cursor/b.json"
# $6 is a label, printed so the transcript below is literally what the terminal showed.
run() { printf '%-15s' "$6"; ./target/debug/kanban-cursor-worker-adapter --worker "$1" \
          --state-dir "$2" --turn-timeout-ms "$3" --queue-wait-ms "$4" < "$5" 2>&1; echo " exit=$?"; }

printf ok > "$SMOKE/cursor/state/scenario.txt"
run "$SMOKE/cursor/fake-worker" "$SMOKE/cursor/state" 10000 10000 "$SMOKE/cursor/a.json" ok

for S in nonzero malformed wrong-delivery; do
  printf '%s' "$S" > "$SMOKE/cursor/state/scenario.txt"
  run "$SMOKE/cursor/fake-worker" "$SMOKE/cursor/state" 10000 0 "$SMOKE/cursor/a.json" "$S"
done
printf hang > "$SMOKE/cursor/state/scenario.txt"
run "$SMOKE/cursor/fake-worker" "$SMOKE/cursor/state" 1000 0 "$SMOKE/cursor/a.json" hang

cp "$SMOKE/cursor/fake-worker" "$SMOKE/cursor/loose-worker"; chmod 775 "$SMOKE/cursor/loose-worker"
run "$SMOKE/cursor/loose-worker" "$SMOKE/cursor/state" 10000 0 "$SMOKE/cursor/a.json" mode-775-worker
mkdir -p "$SMOKE/cursor/open-state"; chmod 755 "$SMOKE/cursor/open-state"
run "$SMOKE/cursor/fake-worker" "$SMOKE/cursor/open-state" 10000 0 "$SMOKE/cursor/a.json" mode-755-statedir
```

Observed:
```
ok             {"protocolVersion":1,"subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","createdAt":1720000000,"replay":false} exit=0
nonzero        Error: cursor_worker_turn_failed (retryable): the worker turn exited with status 23; the worker's stderr was: the fake worker refused this turn
 exit=22
malformed      Error: cursor_worker_response_invalid (terminal): the worker did not acknowledge this delivery: EOF while parsing a value at line 1 column 21
 exit=24
wrong-delivery Error: cursor_worker_response_mismatched (terminal): the worker acknowledged a different delivery: adapter response subscription identity does not match request
 exit=25
hang           Error: cursor_worker_deadline_exceeded (retryable): the worker turn did not finish before the deadline and was ended
 exit=23
mode-775-workerError: cursor_worker_unavailable (retryable): --worker must not be group- or world-writable
 exit=21
mode-755-statedirError: cursor_worker_unavailable (retryable): --state-dir must not be accessible to others
 exit=21
```

**Serialization, and the SIGKILL release.** This is the receipt to keep.

```bash
run() { printf '%-18s' "$6"; ./target/debug/kanban-cursor-worker-adapter --worker "$1" \
          --state-dir "$2" --turn-timeout-ms "$3" --queue-wait-ms "$4" < "$5" 2>&1; echo " exit=$?"; }
printf hang > "$SMOKE/cursor/state/scenario-$A.txt"
printf ok   > "$SMOKE/cursor/state/scenario-$B.txt"
./target/debug/kanban-cursor-worker-adapter --worker "$SMOKE/cursor/fake-worker" \
  --state-dir "$SMOKE/cursor/state" --turn-timeout-ms 600000 --queue-wait-ms 10000 \
  < "$SMOKE/cursor/a.json" >/dev/null 2>&1 &
HOLDER=$!; sleep 0.4
run "$SMOKE/cursor/fake-worker" "$SMOKE/cursor/state" 10000 0 "$SMOKE/cursor/b.json" holder-alive
kill -KILL $HOLDER; wait $HOLDER 2>/dev/null; sleep 0.2
run "$SMOKE/cursor/fake-worker" "$SMOKE/cursor/state" 10000 0 "$SMOKE/cursor/b.json" holder-SIGKILLed
```

Observed:
```
holder-alive      Error: cursor_worker_busy (retryable): another turn held /private/tmp/kanban-adapter-smoke/cursor/state/.kanban-cursor-turn.lock for the whole 0ms queue wait
 exit=20
holder-SIGKILLed  {"protocolVersion":1,"subscriptionID":"sub-test","eventID":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","createdAt":1720000000,"replay":false} exit=0
```

The second delivery was granted the slot with no cleanup step of any kind
after the holder was killed with an uncatchable signal. Nothing reaped, no
lock file removed, no PID inspected.

An overlapping pair also refuses correctly with a `hold` (400 ms) holder and
`--queue-wait-ms 0`: `cursor_worker_busy`, exit 20, while the holder went on
to acknowledge its own delivery successfully.

---

## 6. `kanban-zcode-notify-adapter`

### Talks to
One spawned sink, **one way**. The delivery document plus one trailing
newline goes to the sink's stdin (`rust/zcode_notify_adapter.rs:387-397`).
The sink's own stdout and stderr are opened on `/dev/null` by the kernel at
spawn time. Its exit status is the entire acknowledgement channel.

Target pair: `zcode.notify` / `post-notification` (`:24-30`). The action
vocabulary is deliberately not `enqueue-turn` or `start-readonly-turn`, so a
subscription meaning to drive a turn cannot be re-pointed here by editing an
action ID — the delivery is refused before the sink is started (`:24-28`,
`:366-374`).

### Argument surface
```
kanban-zcode-notify-adapter --sink ABSOLUTE_PATH --notify-timeout-ms N
```
(`:13`.) **These two flags are the entire accepted surface** (`:263-274`).

| Flag | Rule | Citation |
|---|---|---|
| `--sink` | Required. 1..=512 bytes, absolute, no `.` or `..` segment — checked on the raw text, not on normalised `Path::components`, so what the operator wrote is what is judged. | `:309-344` |
| `--notify-timeout-ms` | Required. Integer `1000..=300000`. | `:299-307` |

The sink is spawned with **no arguments**, a cleared environment carrying
only `PATH=/usr/bin:/bin`, `/dev/null` on stdout and stderr, and its own
process group (`:445-464`).

### Failure codes

| Code | Exit | Disposition | What it means | Your next move |
|---|---|---|---|---|
| `zcode_sink_unreachable` | 10 | retryable | The sink could not be started at all (`:445-464`). | The sink is being installed or the path is wrong. Fix the host, not the event. |
| `zcode_deadline_exceeded` | 13 | retryable | `--notify-timeout-ms` elapsed; the sink was killed (`:466-504`). | Raise the timeout or unwedge the sink. |
| `zcode_sink_refused` | 11 | **terminal** | The sink ran and exited non-zero. It understood the notice and rejected it (`:549-559`). | Read the sink's own logs — this adapter has none of its output. Byte-identical bytes next attempt. |
| `zcode_acknowledgement_malformed` | 14 | **terminal** | Either the sink died on a signal, so there is no exit code at all, **or** it exited 0 while closing the notice pipe early, so it claims to have taken a notice it demonstrably did not read (`:560-578`). | Broken or mismatched sink. An OOM-reaped sink lands here too, accepted deliberately so a sink that crashes on every notice cannot silently eat the retry budget (`:538-542`). |

Codes `:156-163`, dispositions `:167-172`, statuses `:174-187`.

### What it explicitly does not do — and cannot be made to
This is the adapter's whole point, and it is enforced by types rather than by
convention.

- **`Ingress` is uninhabited.** `enum Ingress {}` has no variants
  (`:95-105`), so `Option<Ingress>` has exactly one inhabitant. The
  `Notified.acted_on` field is therefore `None` in every state that can ever
  be written (`:131-144`, `:579-585`). The operator report prints
  `acted-on=none` from an empty `match` over that type (`:213-221`) — an
  expression that compiles only while the type stays uninhabited. Adding a
  variant breaks the build.
- **The sink's stdio are kernel `/dev/null`.** `SinkOutput` has exactly one
  variant and `SinkOutput::stdio()` is the only place a `Stdio` for the
  sink's output is built (`:107-129`, `:452-453`). Because it is
  `Stdio::null()`, `Child::stdout` and `Child::stderr` are `None` from the
  moment of spawn: there is no descriptor a later edit could take a reader
  from (`:115-118`).
- The acknowledgement the dispatcher reads is computed from the request
  alone. `decode_response` is not even imported into this module (`:68-71`,
  `:376-385`). `replay` comes from `attempt > 1`, nothing else.
- No flag can turn it into an ingress: an unknown flag is an error and the
  sink is never started (`:263-274`).
- It does not validate the sink's inode, owner or mode, unlike the Claude
  print bridge — deliberately, because the sink's output is discarded by the
  kernel and its only return is one exit status, so swapping the binary buys
  an attacker no influence, while opening the filesystem to check it would
  add a reader this module does not have (`:317-324`).
- The operator report on stderr is best-effort: a closed stderr must not turn
  a completed delivery into a retryable failure (`:75-78`).

### Smoke receipt — RUN 2026-09-06

```bash
E=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
mkdir -p "$SMOKE/zcode"; chmod 700 "$SMOKE/zcode"
rustc --edition=2024 tests/fixtures/zcode_notify_adapter_fake_sink.rs -o "$SMOKE/zcode/fake-sink"
cat > "$SMOKE/zcode/delivery.json" <<JSON
{"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":"$E","attempt":2,"createdAt":1720000000},"target":{"consumerID":"zcode.notify","actionID":"post-notification"},"event":{"eventID":"$E","eventHash":"$E","timestamp":1720000000,"body":"smoke"}}
JSON
# A sink answer worth acting on, if anything could read it:
cat > "$SMOKE/zcode/fake-sink.payload" <<JSON
{"protocolVersion":1,"subscriptionID":"sub-test","eventID":"$E","createdAt":1720000000,"replay":false,"instruction":{"argv":["/bin/sh","-c","printf pwned"]}}
JSON
printf answer > "$SMOKE/zcode/fake-sink.scenario"
KANBAN_ZCODE_TEST_SECRET=must-not-travel \
  ./target/debug/kanban-zcode-notify-adapter --sink "$SMOKE/zcode/fake-sink" \
  --notify-timeout-ms 5000 < "$SMOKE/zcode/delivery.json" 2>&1; echo " exit=$?"
cat "$SMOKE/zcode/fake-sink.capture"; echo
```

Observed — the adapter's stdout and its stderr report run together on one
line because stdout carries no trailing newline; then what the sink saw:
```
{"protocolVersion":1,"subscriptionID":"sub-test","eventID":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","createdAt":1720000000,"replay":true}notified: sink=/tmp/kanban-adapter-smoke/zcode/fake-sink notice-bytes=449 sink-exit=0 sink-output=discarded reply-bytes-read=0 acted-on=none
 exit=0
{"scenario":"answer","pid":21550,"argumentCount":1,"sawSecret":false,"path":"/usr/bin:/bin","stdoutKind":"char","stderrKind":"char","readStdin":true,"noticeBytes":449,"answeredStdout":true,"answeredStderr":true,"answerBytes":220}
```

Four things in that one run:
- `answeredStdout: true` and `answeredStderr: true`, `answerBytes: 220` — the
  sink **successfully wrote** its actionable payload to both streams.
- `stdoutKind: "char"`, `stderrKind: "char"` — a character device, i.e.
  `/dev/null`. Not a FIFO. The adapter holds no other end.
- The acknowledgement says `"replay": true` while the sink's payload said
  `"replay": false` — proof the answer came from the request (`attempt: 2`),
  not from the sink.
- `sawSecret: false` with `KANBAN_ZCODE_TEST_SECRET` set in the adapter's own
  environment, and `argumentCount: 1` (argv0 only).

The remaining classes:
```bash
# $1 is a label, printed so the transcript is literally what the terminal showed.
z() { printf '%-16s' "$1"; shift; ./target/debug/kanban-zcode-notify-adapter "$@" 2>&1; echo " exit=$?"; }
for S in refuse close-stdin signal; do
  printf '%s' "$S" > "$SMOKE/zcode/fake-sink.scenario"
  z "$S" --sink "$SMOKE/zcode/fake-sink" --notify-timeout-ms 5000 < "$SMOKE/zcode/delivery.json"
done
printf hang > "$SMOKE/zcode/fake-sink.scenario"
z hang          --sink "$SMOKE/zcode/fake-sink"     --notify-timeout-ms 1000 < "$SMOKE/zcode/delivery.json"
z missing-sink  --sink "$SMOKE/zcode/not-installed" --notify-timeout-ms 5000 < "$SMOKE/zcode/delivery.json"
z allow-ingress --sink "$SMOKE/zcode/fake-sink"     --notify-timeout-ms 5000 --allow-ingress 1 < "$SMOKE/zcode/delivery.json"
```
Observed:
```
refuse          Error: zcode_sink_refused (terminal): the sink /tmp/kanban-adapter-smoke/zcode/fake-sink refused the notification and exited 7
 exit=11
close-stdin     {"protocolVersion":1,"subscriptionID":"sub-test","eventID":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","createdAt":1720000000,"replay":true}notified: sink=/tmp/kanban-adapter-smoke/zcode/fake-sink notice-bytes=449 sink-exit=0 sink-output=discarded reply-bytes-read=0 acted-on=none
 exit=0
signal          Error: zcode_acknowledgement_malformed (terminal): the sink /tmp/kanban-adapter-smoke/zcode/fake-sink was terminated before it could acknowledge the notification: signal: 9 (SIGKILL)
 exit=14
hang            Error: zcode_deadline_exceeded (retryable): the sink did not finish before the notify deadline
 exit=13
missing-sink    Error: zcode_sink_unreachable (retryable): starting the sink /tmp/kanban-adapter-smoke/zcode/not-installed: No such file or directory (os error 2)
 exit=10
allow-ingress   Error: unknown argument: --allow-ingress
 exit=1
```

Read the `close-stdin` line carefully — it says `exit=0`, and the warning
below explains why.

**Operationally important, and it surprised me.** The `close-stdin` sink
exited 0 having read *nothing*, and the notification was reported as
**delivered**. That is not a defect: a 449-byte notice fits entirely in the
kernel pipe buffer, so `write_all` + `flush` complete before the sink's exit
closes the read end, and `written` is `Ok(())` at `:570`. The
"exited 0 without taking the notice" class is only reachable when the notice
exceeds the pipe buffer. The e2e test knows this and pads deliberately
(`tests/zcode_notify_adapter_e2e.rs:426-429`). Reproduced:

```bash
python3 -c 'import json;E="b"*64;print(json.dumps({"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":E,"attempt":2,"createdAt":1720000000},"target":{"consumerID":"zcode.notify","actionID":"post-notification"},"event":{"eventID":E,"eventHash":E,"timestamp":1720000000,"body":"p"*(256*1024)}}))' > "$SMOKE/zcode/padded.json"
printf close-stdin > "$SMOKE/zcode/fake-sink.scenario"
./target/debug/kanban-zcode-notify-adapter --sink "$SMOKE/zcode/fake-sink" --notify-timeout-ms 5000 < "$SMOKE/zcode/padded.json" 2>&1; echo " exit=$?"
```
Observed:
```
Error: zcode_acknowledgement_malformed (terminal): the sink /tmp/kanban-adapter-smoke/zcode/fake-sink exited 0 without taking all 262588 notice bytes: Broken pipe (os error 32)
 exit=14
```
So: **for a small notice, a sink that never reads still marks the delivery
delivered.** If you need proof a ZCode sink consumed a notice, the exit
status is not it.

The negative capability is also checked structurally rather than trusted.
`tests/zcode_notify_adapter_e2e.rs:504-559` parses `rust/zcode_notify_adapter.rs`
with `syn` and asserts `Ingress` has zero variants, `SinkOutput` has exactly
the one `Discard` variant, exactly one `Stdio::null` and one `Stdio::piped`
appear, and no read path, forbidden method, output descriptor field, or
boolean field exists. RUN 2026-09-06:

```bash
cargo test --test zcode_notify_adapter_e2e the_adapter_has_no_way_to_read_what_a_sink_says -- --exact
```
```
test the_adapter_has_no_way_to_read_what_a_sink_says ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 10 filtered out; finished in 0.01s
```

---

## 7. `kanban-codex-queue-adapter`

### Talks to
The installed `codex` CLI, three separate invocations per delivery
(`rust/codex_queue_adapter.rs:39-53`): `--version`, then `queue --help`, then
`queue --thread <name> --message <json>`. The message is queued for a session
that is already open; the turn happens afterwards.

Target pair: `codex.queue` / `enqueue-turn` (`:19-21`, enforced `:326-340`).

### Argument surface
```
kanban-codex-queue-adapter --codex PATH --codex-home PATH --thread NAME --required-version VER
```
(`:15`.)

| Flag | Rule | Citation |
|---|---|---|
| `--codex` | Required, absolute. Canonicalised, ancestor chain and inode identity validated. | `:170-176`, `:213-289`, `:462-492` |
| `--codex-home` | Required, absolute. Becomes the child's only environment variable, `CODEX_HOME`. Identity validated. | `:435-445`, `:494-521` |
| `--thread` | Required. 1..=128 ASCII printable characters; must not start `-`; must not be whitespace-only. | `:178-198` |
| `--required-version` | Required. 1..=32 chars, ASCII digits and dots only, no leading/trailing/repeated dots. | `:200-211` |

No timeout flag: this adapter relies entirely on the dispatcher's own
`timeoutMs` supervision.

### Failure codes
**There are none.** No `FailureClass`, no `exit_code` function; every failure
exits `1` (`rust/lib.rs:4347-4356`), and the dispatcher records
`adapter_exit`. The stderr message is your only diagnostic, so re-running by
hand is not optional here — it is the whole diagnostic path. The distinct
refusals, all exit `1`:

| stderr message | Meaning | Your next move |
|---|---|---|
| `codex version probe returned an unexpected version` | Installed `codex --version` is not `codex-cli <required-version>` (`:549-563`). | Codex was upgraded under you. Update `--required-version` in host config, deliberately. |
| `codex queue help probe returned an unexpected help layout` | `codex queue --help` no longer matches the pinned five-line prefix, or `--thread`/`--message` are missing, duplicated, or reordered (`:565-609`). | Codex changed its CLI surface. Fail-closed by design; do not loosen it. |
| `codex version probe wrote to stderr` / `codex queue help probe wrote to stderr` | A probe produced stderr at all (`:550-555`, `:566-571`). | Usually a wrapper script or shell profile writing to stderr. |
| `codex queue invocation failed` | The `queue` invocation exited non-zero (`:621-628`). | The named thread/session probably does not exist. |
| `adapter target consumer ID must be codex.queue` | The subscription points here with the wrong action or consumer (`:326-340`). | Fix the host allow-list binding. |
| `adapter render exceeds 65536 bytes` | The rendered queue message passed the cap (`:643-649`, `:18`). | The event is too large for this bridge. |

### What it explicitly does not do
- Does not pass a timeout to Codex.
- Does not read `CODEX_HOME` from its own environment; the child gets exactly
  one variable, the configured one, and no `PATH` (`:435-445`).
- Does not give the child stdin — `Stdio::null()` (`:440`).
- Does not classify its failures. See above.

### The one thing to know about this adapter's security posture
**It is the only adapter that puts the ledger event in a child's argv.** The
queue message — instruction, idempotency key, subscription, event ID,
attempt, and the whole `event` object — is passed as the value of `--message`
(`:539-547`, `:651-664`). argv is world-readable in the process table for the
lifetime of the invocation. The Cursor adapter's module doc calls this out as
the thing it was written to avoid (`rust/cursor_worker_adapter.rs:3-8`).

Verified by receipt below: `argv` length 5, `argv[4]` is the full JSON
including the event.

### Smoke receipt — RUN 2026-09-06

```bash
E=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
mkdir -p "$SMOKE/codexq/codex-home"; chmod 700 "$SMOKE/codexq" "$SMOKE/codexq/codex-home"
rustc --edition=2024 tests/fixtures/codex_queue_adapter_fake_codex.rs -o "$SMOKE/codexq/fake-codex"
cat > "$SMOKE/codexq/delivery.json" <<JSON
{"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":"$E","attempt":2,"createdAt":1720000000},"target":{"consumerID":"codex.queue","actionID":"enqueue-turn"},"event":{"eventID":"$E","eventHash":"$E","timestamp":1720000000}}
JSON
./target/debug/kanban-codex-queue-adapter --codex "$SMOKE/codexq/fake-codex" \
  --codex-home "$SMOKE/codexq/codex-home" --thread queue-thread \
  --required-version 1.2.3 < "$SMOKE/codexq/delivery.json" 2>&1; echo " exit=$?"
# One line per child invocation, as the fake recorded it:
python3 -c "
import json
for l in open('$SMOKE/codexq/capture.ndjson'):
    r=json.loads(l); print(r['mode'],'env=',r['env'],'argv=',r['argv'])"
./target/debug/kanban-codex-queue-adapter --codex "$SMOKE/codexq/fake-codex" \
  --codex-home "$SMOKE/codexq/codex-home" --thread queue-thread \
  --required-version 9.9.9 < "$SMOKE/codexq/delivery.json" 2>&1; echo " exit=$?"
```

Observed:
```
{"protocolVersion":1,"subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","createdAt":1720000000,"replay":true} exit=0
version env= [['CODEX_HOME', '/private/tmp/kanban-adapter-smoke/codexq/codex-home']] argv= ['--version']
queue-help env= [['CODEX_HOME', '/private/tmp/kanban-adapter-smoke/codexq/codex-home']] argv= ['queue', '--help']
queue env= [['CODEX_HOME', '/private/tmp/kanban-adapter-smoke/codexq/codex-home']] argv= ['queue', '--thread', 'queue-thread', '--message', '{"instruction":"At-least-once delivery; deduplicate by idempotency key.","idempotencyKey":"sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","attempt":2,"event":{"eventHash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","timestamp":1720000000}}']
Error: codex version probe returned an unexpected version
 exit=1
```

The third line is the disclosure: five argv elements, the fifth carrying the
whole event. Nothing else in this document reads like that.

---

## 8. `kanban-codex-app-server-adapter`

### Talks to
The installed `codex` CLI in `app-server --listen stdio://` mode — stdio
only, **no socket is bound and no port is opened** (`:1097-1103`). Four
invocations per delivery (`:525-540`): `--version`, `app-server --help`,
`app-server generate-json-schema --out <tmp>`, then the interactive
app-server itself, driven through a fail-closed state machine.

Target pair: `codex.app-server` / `start-readonly-turn` (`:44-45`, enforced
`:1330-1344`). The turn is requested with `approvalPolicy: "never"` and
sandbox `read-only` / `readOnly`
(`rust/codex_app_server_messages.rs:269-270`, `:304-306`).

### Argument surface
```
kanban-codex-app-server-adapter --codex PATH --codex-home PATH --cwd PATH \
  --required-version VER --client-request-sha256 HEX --protocol-schema-sha256 HEX \
  --protocol-timeout-ms N
```
(`:24`.)

| Flag | Rule | Citation |
|---|---|---|
| `--codex` | Required, absolute. Canonical, ancestor chain and inode identity validated, re-validated immediately before every spawn. | `:637-643`, `:707-805`, `:854-874`, `:919-925` |
| `--codex-home` | Required, absolute. Private directory. Becomes the child's `CODEX_HOME`. | `:679-697`, `:876-895` |
| `--cwd` | Required, absolute. Private **and empty**, re-checked before each spawn. | `:699-705`, `:897-917` |
| `--required-version` | Required. Digits and dots. | `:645-656` |
| `--client-request-sha256` | Required. Lowercase 64-hex. | `:658-667` |
| `--protocol-schema-sha256` | Required. Lowercase 64-hex. | `:658-667` |
| `--protocol-timeout-ms` | Required. Integer `1000..=300000`. One shared deadline for probes, schema generation and the turn. | `:669-677`, `:529` |

Child environment is cleared to `CODEX_HOME` plus a compile-time
`PATH=/usr/bin:/bin`, in its own process group, with termination signals
reset before exec (`:927-942`). The `PATH` is there so the child resolves the
root-owned system `/usr/bin/bwrap`; the comment at `:28-43` records the
accepted asymmetry that binaries reached *through* that `PATH` are not
identity-checked the way `--codex` is, because the adapter never execs them
directly and so cannot honestly pin them.

### Failure codes
**There are none.** Every failure exits `1` (`rust/lib.rs:4359-4370`); the
dispatcher records `adapter_exit`. Stderr always begins `Error:` and is kept
under 8 KiB (`tests/codex_app_server_adapter_e2e.rs:684-694`). Categories,
all exit `1`:

- Version or `app-server --help` drift (`:1105-1139`).
- Regenerated schema not matching `--protocol-schema-sha256` or
  `--client-request-sha256` (`:1160-1193`).
- An `initialize` opt-out notification method the generated schema does not
  declare — moved from a silent late failure to a startup one (`:1236-1256`).
- Any protocol, policy, identity, status, schema or size drift inside the
  turn; a tool-ish item; malformed output; child stderr; a wrong, extra or
  duplicate acknowledgement.
- Any stdout after completion (`:1723-1725`).
- `--cwd` no longer empty before a spawn (`:897-917`).

### What it explicitly does not do
- Does not open a socket. `--listen stdio://` only (`:1097-1103`).
- Does not enable the experimental API when regenerating the schema
  (ADR-031 lines 504-507).
- Does not leave its schema scratch directory behind; it cleans only that
  directory (ADR-031 lines 504-507;
  `tests/codex_app_server_adapter_e2e.rs:2162`).
- Does not write anything to `--cwd`; the e2e harness asserts the directory
  listing is unchanged across every run
  (`tests/codex_app_server_adapter_e2e.rs:615` and `:628`).
- Does not ship an active declarative subscription. The host allow-list
  binding exists; the bridge stays experimental and opt-in (ADR-031 lines
  492-519).

### Smoke receipt — RUN 2026-09-06

This adapter's argument surface requires two SHA-256 digests **of the
installed binary's own generated schema**, and its fake needs a multi-stage
transcript scenario file built by a Rust helper
(`tests/codex_app_server_adapter_e2e.rs:437-511`, `:710-720`, `:769-770`).
Reconstructing that in shell would be a re-implementation, not a receipt, so
the reproducible command here is the compiled-process contract test itself,
which drives the same dependency-free fake from `tests/fixtures/`:

```bash
cargo test --test codex_app_server_adapter_e2e compiled_process_happy_path_reaches_the_exact_transcript
```

Observed:
```
    Finished `test` profile [unoptimized + debuginfo] target(s) in 29.08s
     Running tests/codex_app_server_adapter_e2e.rs (target/debug/deps/codex_app_server_adapter_e2e-c0ce6f8ec6940263)
running 1 test
test compiled_process_happy_path_reaches_the_exact_transcript ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 16 filtered out; finished in 0.99s
```

That test asserts the exact 18-invocation transcript, the exact
`AdapterResponse` on stdout, empty stderr, and an unchanged `--cwd`
(`tests/codex_app_server_adapter_e2e.rs:777-823`).

The argument refusals *are* hand-runnable with no fake at all, and are the
common 03:00 case (a hash or path typo'd in host config):

```bash
H64=$(printf 'a%.0s' {1..64})
./target/debug/kanban-codex-app-server-adapter --codex /bin/echo --codex-home /tmp --cwd /tmp \
  --required-version 0.150.1 --client-request-sha256 nothex --protocol-schema-sha256 "$H64" \
  --protocol-timeout-ms 4000 </dev/null; echo "exit=$?"
./target/debug/kanban-codex-app-server-adapter --codex relative/path --codex-home /tmp --cwd /tmp \
  --required-version 0.150.1 --client-request-sha256 "$H64" --protocol-schema-sha256 "$H64" \
  --protocol-timeout-ms 4000 </dev/null; echo "exit=$?"
```
Observed:
```
Error: --client-request-sha256 must be a lowercase 64-hex string
exit=1
Error: --codex must be an absolute path
exit=1
```

---

## 9. `kanban-claude-print-adapter`

### Talks to
The installed Claude Code CLI, three invocations per delivery
(`rust/claude_print_adapter.rs:32-51`): `--version`, `--help`, then one fresh
safe-mode `--print` worker. It is not resuming a foreground session.

Target pair: `claude.print` / `start-readonly-turn` (`:20-21`, enforced
`:439-441`).

### Argument surface
```
kanban-claude-print-adapter --claude ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --required-version VERSION
```
(`:15`.)

| Flag | Rule | Citation |
|---|---|---|
| `--claude` | Required, absolute. Not a symlink; regular file; executable; no group/other write; owned by this euid or root; every ancestor trusted; inode identity captured and **re-validated before each of the three spawns**. | `:179-211`, `:228-239`, `:167-178` |
| `--home` | Required, absolute. Private directory (`0o077` clear). Becomes the child's `HOME`. | `:179-211` |
| `--cwd` | Required, absolute. Private **and empty**. Becomes the child's working directory. | `:212-217`, `:218-227` |
| `--required-version` | Required. Digits and dots. Probe output must equal exactly `<version> (Claude Code)`. | `:147-158`, `:333-338` |

No timeout flag; the dispatcher's `timeoutMs` supervises.

Child argv for the turn is fixed (`:308-323`):
`--safe-mode --print <prompt> --output-format json --tools "" --disallowedTools mcp__* --no-session-persistence --permission-mode dontAsk`.
Child environment is cleared to exactly `HOME` and `PATH=/usr/bin:/bin`, and
stdin is `Stdio::null()` (`:241-252`).

### Failure codes
**There are none.** Every failure exits `1` (`rust/lib.rs:4373-4381`);
`adapter_exit` on the board. Distinct stderr messages, all exit `1`:

| stderr message | Meaning | Your next move |
|---|---|---|
| `claude version probe returned an unexpected version` | `claude --version` ≠ `<required> (Claude Code)` (`:333-338`). | Claude was upgraded. Update host config deliberately. |
| `claude help probe returned an unexpected help layout` | One of the seven pinned flags does not appear exactly once in `--help` (`:22-30`, `:339-345`). | CLI surface changed. Fail-closed by design. |
| `claude print invocation failed` | The print turn exited non-zero (`:324-327`). | Usually auth. Check `HOME`. |
| `claude print invocation wrote to stderr` | The turn produced any stderr (`:328-330`). | Stricter than the Cursor bridge, and only workable because this adapter pins the program's output format exactly (`rust/cursor_worker_adapter.rs:259-263`). |
| `claude result did not contain the exact acknowledgement` | The final result object is not `type: "result"` with `result` equal to `ACK <sub>:<event>`, **or** carries failure evidence (`error`, `apiError`, `is_error: true`), **or** carries tool evidence (`:366-411`). An API/auth error and a wrong reply are the same message. | Distinguish by reading the child's own logs; this adapter deliberately does not quote the payload. |
| `claude result array is invalid` | Array output that is empty, or has tool or failure evidence anywhere in it (`:419-423`). | The turn used a tool. It must not. |
| `trailing characters at line 1 column N` | Trailing JSON after the result document (`:414-416`). | Output is not a single JSON value. |
| `--cwd must be empty` | The private cwd is not empty (`:212-217`). | Clear it. Something wrote there. |
| `--claude identity is no longer trusted` | The executable's inode, type or mode changed between probes (`:228-239`). | Something replaced the binary mid-delivery. Investigate before retrying. |

### What it explicitly does not do
- Does not put event content in the child's argv. Only the bounded
  subscription and event IDs enter the acknowledgement prompt (`:346-356`,
  `:357-365`). Verified by receipt.
- Does not give the child stdin (`:248`).
- Does not resume or persist a session (`--no-session-persistence`, `:319`).
- Does not allow tools (`--tools ""`) or MCP (`--disallowedTools mcp__*`).
- Does not accept the acknowledgement if the turn used a tool, even when the
  final text is exactly right (`:405`).

### Smoke receipt — RUN 2026-09-06

```bash
E=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
mkdir -p "$SMOKE/claude/home" "$SMOKE/claude/cwd"
chmod 700 "$SMOKE/claude" "$SMOKE/claude/home" "$SMOKE/claude/cwd"
rustc --edition=2024 tests/fixtures/claude_print_adapter_fake_claude.rs -o "$SMOKE/claude/fake-claude"
cat > "$SMOKE/claude/delivery.json" <<JSON
{"protocolVersion":1,"delivery":{"subscriptionID":"sub-test","eventID":"$E","attempt":2,"createdAt":1720000000},"target":{"consumerID":"claude.print","actionID":"start-readonly-turn"},"event":{"eventID":"$E","eventHash":"$E","timestamp":1720000000,"body":"must not reach Claude"}}
JSON
for S in object api-error tool mismatch trailing nonzero; do
  printf '%s' "$S" > "$SMOKE/claude/home/scenario.txt"
  printf '%-11s' "$S"
  ./target/debug/kanban-claude-print-adapter --claude "$SMOKE/claude/fake-claude" \
    --home "$SMOKE/claude/home" --cwd "$SMOKE/claude/cwd" \
    --required-version 2.1.236 < "$SMOKE/claude/delivery.json" 2>&1; echo " exit=$?"
done
python3 -c "
import json
p=[json.loads(l) for l in open('$SMOKE/claude/capture.ndjson') if json.loads(l)['mode']=='print'][0]
print('argv=',p['argv']); print('env=',p['env']); print('cwd=',p['cwd']); print('stdin=',repr(p['stdin']))
print('event body in argv:', any('must not reach' in a for a in p['argv']))"
```

Observed:
```
object     {"protocolVersion":1,"subscriptionID":"sub-test","eventID":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","createdAt":1720000000,"replay":true} exit=0
api-error  Error: claude result did not contain the exact acknowledgement
 exit=1
tool       Error: claude result array is invalid
 exit=1
mismatch   Error: claude result did not contain the exact acknowledgement
 exit=1
trailing   Error: trailing characters at line 1 column 108
 exit=1
nonzero    Error: claude print invocation failed
 exit=1
argv= ['--safe-mode', '--print', 'Reply with exactly this acknowledgement and nothing else: ACK sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '--output-format', 'json', '--tools', '', '--disallowedTools', 'mcp__*', '--no-session-persistence', '--permission-mode', 'dontAsk']
env= [['HOME', '/private/tmp/kanban-adapter-smoke/claude/home'], ['PATH', '/usr/bin:/bin']]
cwd= /private/tmp/kanban-adapter-smoke/claude/cwd
stdin= ''
event body in argv: False
```

The last five lines are the fake's record of the print turn: the fixed argv,
an environment of exactly `HOME` and `PATH`, the private cwd, empty stdin,
and the event body absent from argv.

`api-error` and `mismatch` producing the identical message is by design
(`:399-409` folds failure evidence, tool evidence and a wrong acknowledgement
into one `bail!`), but it costs you a diagnosis at 03:00: a revoked OAuth
token and a model that answered wrongly look the same from here.

---

## 10. Security

The dispatcher spawns every adapter with a **cleared environment**
(`rust/adapter_process.rs:245`) plus at most one configured secret variable
(`:254-256`), in its own process group (`:249`), with stdin/stdout/stderr on
pipes (`:246-248`). This is a host trust boundary, not an OS sandbox — stated
in `rust/adapter_process.rs:21-25` and ADR-031 lines 326-327. An adapter
trusted to receive the configured secret is already inside that boundary.

### Off-host reachability, all seven

**None of the seven adapters listens on a socket.** Six speak only to child
processes over pipes. The seventh, OpenCode, is an HTTP *client* whose
destination is constrained to a loopback literal at parse time
(`rust/opencode_adapter.rs:327-329`). No adapter binds a port; the Codex
app-server child runs `--listen stdio://`, not a URL
(`rust/codex_app_server_adapter.rs:1097-1103`). So none of them is reachable
from off-host at all, and none of them can send a ledger event off-host: the
only network syscall in the whole set is a loopback `connect`, receipt-proven
to refuse `10.0.0.1` (§3).

### Per adapter: trusts, refuses, and the hostile ceiling

**`kanban-opencode-adapter`**
- *Trusts:* its `--endpoint` argument, and an HTTP answer only insofar as it
  is a 2xx with a declared `Content-Length` whose body is an `AdapterResponse`
  naming this exact delivery (`rust/adapter_protocol.rs:130-138`).
- *Refuses:* non-loopback hosts, hostnames, userinfo, query, fragment,
  relative path segments, missing/duplicate/malformed `Content-Length`,
  chunked answers, bodies over 64 KiB, redirects, and any `consumerID`/
  `actionID` other than `opencode.server`/`enqueue-turn`.
- *Off-host:* not reachable; cannot transmit off-host.
- *A hostile or misconfigured listener on that loopback port achieves at
  most:* (a) it reads the delivery document — the ledger event, already
  refused if it carries a secret-shaped key (`rust/adapter_protocol.rs:91-93`);
  and (b) it can **forge success for this exact delivery** by echoing the
  identity triple back, marking the delivery acked without any turn having
  run. It cannot forge success for a *different* delivery, cannot cause the
  adapter to execute anything, and cannot obtain a credential — the request
  carries no `Authorization` header and there is no way to add one
  (`rust/opencode_adapter.rs:447-459`). **Corollary the operator must act
  on:** an OpenCode endpoint's authorization rests entirely on who can bind
  that loopback port. It is not token-authenticated.

**`kanban-kimi-acp-adapter`**
- *Trusts:* the pinned `--kimi` inode, and one frame that is a JSON-RPC 2.0
  response for id `1` whose `result` is an `AdapterResponse` naming this
  delivery.
- *Refuses:* a symlinked, non-file, non-executable, group/other-writable, or
  foreign-owned peer path; an untrusted ancestor; an inode that changed
  between the stat and the open (`rust/kimi_acp_adapter.rs:399-423`); a
  non-private `--home`; a non-empty `--cwd`; a truncated frame; an answer
  over 64 KiB (refused *unread*); an unrecognised JSON-RPC envelope member;
  and — before inspecting any payload — an answer naming another request id
  (`:686-696`).
- *Off-host:* not reachable; pipes only.
- *A hostile peer achieves at most:* forging success for this exact delivery
  by echoing the identity triple. It cannot get the adapter to read a second
  frame (`:24-26`), cannot make its stderr appear anywhere (it is
  `/dev/null`, `:769`), cannot exceed a bounded read, and cannot reach the
  dispatcher's environment (cleared to `HOME` + fixed `PATH`, `:755-774`). A
  peer that outlives its acknowledgement is given a 2 s window then has its
  private process group terminated (`:783-804`).

**`kanban-cursor-worker-adapter`**
- *Trusts:* the `--worker` path's mode and owner at validation time, and an
  acknowledgement naming this delivery.
- *Refuses:* a symlinked, non-file, non-executable, group/other-writable, or
  foreign-owned worker; a `--state-dir` that is a symlink, not a directory,
  or accessible to others. The `0o077` rule on `--state-dir` is not
  cosmetic: an account that can create files there could create a second
  lock file and defeat the serialization (`:590-596`). Verified by receipt —
  a `0755` state dir is refused with `cursor_worker_unavailable`.
- *Off-host:* not reachable; pipes only.
- *A hostile worker achieves at most:* forging success for this exact
  delivery. Notably it **cannot** silently succeed for another delivery —
  that is `cursor_worker_response_mismatched`, kept apart from the invalid
  case precisely so a healthy-looking answer for someone else is never
  recorded as delivered (`:239-246`). No event content is available to it via
  argv (`:51-54`), so a process-table observer learns nothing about the
  event. A worker killed by any means, `SIGKILL` included, releases the turn
  slot through the kernel with no cleanup — receipt-proven in §5.
- *Accepted residual:* one spawn happens immediately after validation, so
  there is no atomic open-time guarantee; the adapter says so rather than
  claiming one (`:557-565`). Worker descendants are not chased here and rely
  on the dispatcher's outer group kill (`:454-459`).

**`kanban-zcode-notify-adapter`**
- *Trusts:* the `--sink` text, and one exit code.
- *Refuses:* a relative path, a path with `.`/`..` segments (judged on the
  raw text, not on normalised components, `:333-342`), any flag other than
  `--sink` and `--notify-timeout-ms`, and any `consumerID`/`actionID` other
  than `zcode.notify`/`post-notification`.
- *Off-host:* not reachable; one-way pipe only.
- *Ingress is structurally impossible.* `Ingress` is uninhabited so no
  payload is representable (`:95-105`); `SinkOutput` has one variant and is
  the only place a `Stdio` for the sink's output is built, and it is
  `Stdio::null()`, so `Child::stdout`/`stderr` are `None` from spawn
  (`:107-129`, `:452-453`); the acknowledgement is computed from the request
  and `decode_response` is not imported (`:68-71`, `:376-385`). Enforced by
  `tests/zcode_notify_adapter_e2e.rs:504-559`, run in §6.
- *A hostile sink achieves at most:* choosing whether this delivery is
  recorded as delivered, refused, or malformed — via its exit status and
  nothing else. It cannot influence the acknowledgement's content, cannot
  return a payload, cannot read the dispatcher's secret (receipt:
  `sawSecret: false`), and gets no argv (receipt: `argumentCount: 1`).
  Receipt-proven: a sink that wrote 220 bytes of an actionable payload to
  both its streams changed nothing — `reply-bytes-read=0 acted-on=none`, and
  the acknowledgement's `replay` came from the request, contradicting the
  payload.
- *Accepted residual, stated in the code:* the sink's inode, owner and mode
  are **not** validated, unlike the Claude bridge, because swapping the
  binary buys no influence over a process that discards its output
  (`:317-324`). If you do not trust who can write the sink path, that
  reasoning does not cover *what the sink itself does on the host* — it
  covers only what it can do to this adapter.

**`kanban-codex-queue-adapter`**
- *Trusts:* `--codex` and `--codex-home` inode identity and ancestor chain
  (`:213-289`, `:462-521`); the installed CLI's exact `--version` string and
  the exact layout of `codex queue --help`.
- *Refuses:* version drift, help-layout drift, any probe stderr, a thread
  name starting `-` or containing non-printable ASCII, a render over 64 KiB.
- *Off-host:* not reachable; pipes only.
- *A hostile `codex` achieves at most:* causing this delivery to fail — the
  acknowledgement is synthesised from the request (`:666-674`), so the child
  cannot forge an identity. Exiting 0 from `queue` is enough to mark the
  delivery delivered without the message being queued.
- **The event is disclosed to the process table** for the duration of the
  `queue` invocation (`:539-547`; receipt in §7). On a multi-account host,
  treat any `codex.queue` subscription's events as readable by every local
  account.

**`kanban-codex-app-server-adapter`**
- *Trusts:* `--codex`/`--codex-home`/`--cwd` identity, re-validated before
  every spawn (`:919-925`); the exact version; the exact `app-server --help`
  markers; and two SHA-256 digests of the regenerated protocol schema
  (`:1160-1193`). Opt-out notification method names are checked against the
  generated schema so a misspelling fails at startup instead of silently at
  a notification (`:1236-1256`).
- *Refuses:* every drift above; any tool-ish item; any child stderr; any
  stdout after completion (`:1723-1725`); a `--cwd` that stopped being empty.
  The turn itself is `approvalPolicy: never` with a read-only sandbox
  (`rust/codex_app_server_messages.rs:269-270`, `:304-306`).
- *Off-host:* not reachable. `--listen stdio://`, no port.
- *A hostile `codex` achieves at most:* causing failure, or exiting the
  handshake cleanly enough to have the delivery recorded — the response is
  again synthesised from the request (`:1346-1354`).
- *Stated residual:* binaries the child reaches through the compile-time
  `PATH=/usr/bin:/bin` (notably `/usr/bin/bwrap`) are **not** identity-checked
  the way `--codex` is. The comment at `:28-43` records this as a deliberate,
  dated acceptance and is explicit that the measurement was of one binary on
  one host, not a guarantee about every binary on that `PATH`.

**`kanban-claude-print-adapter`**
- *Trusts:* `--claude` inode identity, captured once and re-validated before
  each of three spawns (`:228-239`) — the strictest path discipline of the
  seven, and the reason cited elsewhere is that this adapter *parses the
  child's stdout and acts on it* (`rust/zcode_notify_adapter.rs:317-320`).
- *Refuses:* any version or help-marker drift; any stderr from any of the
  three invocations; tool evidence anywhere in the output; failure evidence
  (`error`, `apiError`, `is_error: true`) anywhere; trailing JSON; output over
  64 KiB; a `result` that is not the exact `ACK <sub>:<event>` string; a
  non-empty `--cwd`.
- *Off-host:* not reachable; pipes only. Child stdin is `/dev/null`.
- *A hostile Claude achieves at most:* returning the exact acknowledgement
  string, which marks the delivery delivered. It cannot leak the event
  through argv (receipt: `body leaked into argv: False`), cannot use tools or
  MCP by configuration (`:315-318`) and is refused if it did anyway
  (`:366-387`), cannot persist a session (`:319`), and inherits only `HOME`
  and a fixed `PATH` (receipt-confirmed).

### The ceiling common to all seven, stated plainly
For every adapter that accepts an acknowledgement from its peer — OpenCode,
Kimi, Cursor — **a peer that can echo `(subscriptionID, eventID, createdAt)`
can mark a delivery delivered without doing the work.** For the four that
synthesise the acknowledgement themselves — ZCode, Codex queue, Codex
app-server, Claude print — **exit status 0 is sufficient to mark a delivery
delivered.** No adapter obtains proof of work. `subscription_deliveries.status
= 'acked'` means "the configured peer said yes", never "the turn ran".

---

## 11. Findings: where the code disagreed with what I expected

Reported rather than fixed; this task changes no Rust source.

1. **The adapters' error codes never reach the board.** The batch brief for
   this document stated that "the dispatcher stores the code in
   `subscription_deliveries.last_error_code`". It does not. It stores the
   literal `"adapter_exit"` (`rust/adapter_process.rs:373-375` →
   `rust/dispatcher.rs:731-734` → `rust/store.rs:2924-2931`), discards the
   adapter's stderr (`rust/adapter_process.rs:381-387`), and never records
   the numeric exit status. Every adapter's own doc comment says so
   correctly — e.g. `rust/opencode_adapter.rs:50-53` — so the code and its
   comments agree; the brief did not. §1.1 exists because of this.

2. **`retryable` / `terminal` has no effect on retry behaviour.** Retry
   versus dead-letter is `delivery.attempts > subscription.max_retries` and
   nothing else (`rust/store.rs:2912`). The dispositions the adapters
   compute, and their doc comments' reasoning about "spending the retry
   budget", describe an intent no code consumes. A `terminal` failure still
   burns every attempt.

3. **There are seven adapters, not six.** The brief listed six. `Cargo.toml`
   lines 28-54 declares seven adapter binaries, and `rust/lib.rs:4347-4442`
   wires seven entrypoints. This document covers all seven and records seven
   receipts.

4. **Only four of seven adapters classify anything.** The brief said "Each
   classifies failures into distinct codes with distinct exit statuses".
   `kanban-codex-queue-adapter`, `kanban-codex-app-server-adapter` and
   `kanban-claude-print-adapter` have no `FailureClass` and no `exit_code`
   function; they exit `1` for every failure
   (`rust/lib.rs:4347-4356`, `:4359-4370`, `:4373-4381`). For those three the
   stderr message is the entire diagnostic, which raises the value of the
   hand-run receipt considerably.

5. **`zcode_acknowledgement_malformed` for an unread notice is size-dependent.**
   A sink that exits 0 without reading is only caught when the notice exceeds
   the pipe buffer. With a 449-byte notice the write completes into the
   kernel buffer and the delivery is reported as **delivered** (receipt,
   §6). The e2e test pads to 256 KiB precisely to make the class reachable
   (`tests/zcode_notify_adapter_e2e.rs:426-429`). The doc comment at
   `rust/zcode_notify_adapter.rs:535-537` reads as an unconditional
   guarantee; it is conditional on notice size. This is the one place I would
   have expected the code to be stronger than it is.

6. **The Claude bridge folds three different diagnoses into one message.**
   A revoked OAuth token, a tool-using turn on the object path, and a wrong
   reply all produce `claude result did not contain the exact acknowledgement`
   (`rust/claude_print_adapter.rs:399-409`). Confirmed by receipt: the
   `api-error` and `mismatch` scenarios are indistinguishable from the
   adapter's output.

7. **`codex.queue` is the only adapter that discloses the event via argv**
   (`rust/codex_queue_adapter.rs:539-547`), which the Cursor adapter's module
   doc explicitly frames as the thing to avoid
   (`rust/cursor_worker_adapter.rs:3-8`). Receipt-confirmed in §7. Not a
   defect — `codex queue` has no stdin ingress for a message — but a real
   asymmetry an operator on a shared host must know.

### What I could not verify

- **Any live-host behaviour.** No real OpenCode server, Kimi peer, Cursor
  worker, ZCode sink, `codex-cli`, or Claude Code was involved. Every peer in
  every receipt is a fake compiled from `tests/fixtures/`. In particular the
  version and help-layout pins (`codex-cli 0.150.1`, Claude Code `2.1.236`)
  were exercised only against fakes that print the expected strings.
- **The end-to-end dispatcher path.** I did not stand up a board, a
  `dispatchers.json`, and a dispatcher run, so `last_error_code =
  'adapter_exit'` is established by reading the three call sites cited in
  §1.1, not by observing a row. Everything else in this document is
  receipt-backed.
- **`cursor_worker_busy` under real contention from two dispatcher workers.**
  My receipt contends two hand-run adapter processes over one state
  directory, which exercises the same `flock`, but not the dispatcher's
  per-consumer host lock above it.
- **Non-macOS pipe-buffer sizes** for finding 5. The 449-byte-vs-256-KiB
  boundary was observed on Darwin 25.6.0 (arm64); the threshold is the
  platform's pipe buffer, so the exact size at which the class becomes
  reachable will differ on Linux.
