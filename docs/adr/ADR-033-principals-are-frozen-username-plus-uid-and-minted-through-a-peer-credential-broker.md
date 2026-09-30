# ADR-033: Principals are frozen username-plus-UID bindings minted by a peer-credential broker

**Status:** Accepted
**Date:** 2026-09-03
**Deciders:** George

## Context

Kanban's `--as` values identify audit claims, not authenticated users. Board
selectors identify data, not authority. Stdio also has no peer-credential
primitive: the process on the other end of an MCP pipe cannot prove which
human caused a tool call. Giving those fields authority would let a caller
choose both its identity and the data against which that identity is checked.

Managed multi-user mode therefore needs one process boundary that authenticates
local callers, owns the data, constructs authorization context, evaluates
policy, and executes the operation. This ADR freezes that target contract. It
does not claim that the broker, policy schema, filesystem ownership, or host
cutover exists yet.

## Decision

### The broker owns managed data and authenticates the connecting process

The access broker is the exclusive filesystem owner and exclusive opener of
managed registry, board, backup, restore, archive, and search-index files.
Those files and their parent directories are unreadable and unwritable by
human CLI users, MCP processes, web processes, dispatchers, and adapters.
Clients receive result data and audit receipts, never database paths, open file
descriptors, backup handles, or index handles.

Every local request crosses a Unix-domain socket. The broker calls
`getsockopt(SO_PEERCRED)` on the accepted connection and derives the peer's
numeric UID from the kernel. It resolves the UID through the host passwd
database and requires both of these equalities before policy evaluation:

```text
getpwuid(peer_uid).pw_name == stored_username
getpwnam(stored_username).pw_uid == stored_uid == peer_uid
```

Request fields, `--as`, `SUDO_USER`, environment variables, JSON-RPC fields,
headers, cookies, selectors, real-UID claims, and parent-process metadata do
not replace that evidence. A process launched through `sudo` is authorized as
the resulting kernel peer UID. An unresolved, deleted, renamed, or mismatched
account is denied after broker-only registry resolution and before any board,
backup, restore, archive, or index file is opened.

The broker resolves a board in the registry and authorizes it before opening
its database. Existing selectors remain addressing inputs, subject to
[ADR-007](ADR-007-global-project-addressing.md) and
[ADR-028](ADR-028-roots-are-hints-not-board-identity.md), but none is an access
path around the broker. In managed mode `--db` must resolve to an already
registered board file; `--project`, `--workspace`, root paths, environment
defaults, and direct file paths cannot make an unauthorized or unregistered
file eligible. Offline maintenance is a broker operation under an exclusive
registry-admin lock, not a direct-SQL compatibility path.

### Each principal is one frozen username-plus-UID pair

A principal is an opaque broker-minted `p-*` ID plus exactly one frozen
`{username, uid}` pair. Both values are required and neither is updated in
place. Grants and SSO mappings attach to the principal ID, never to a username
or UID.

`bind` creates a new principal ID with no grants or SSO mappings. `rebind`
never edits or reactivates its source principal: in one policy transaction it
disables the source, mints a successor principal with the requested frozen
pair, and explicitly transfers the source's then-active grants and SSO
mappings to new versioned rows targeting the successor. The event records the
source ID, successor ID, and every transferred or retired row. `bind` verifies
that its requested pair currently resolves in both passwd directions. Ordinary
`rebind` instead verifies that the old pair still resolves in both directions
and that its distinct requested successor pair does not yet resolve or belong
to another active principal; it is a pre-change reservation, not a repair after
the passwd change. An active username or UID may belong to only one principal.

A username or UID found on a disabled principal may be reused only when every
colliding disabled principal is named by repeatable `--replaces`; the source
named by `rebind --principal` is implicitly acknowledged. An omitted, active,
or unrelated collision fails closed. A planned rename or UID change uses the
ordinary proof/rebind sequence below while the old pair still authenticates,
then changes passwd only after the successful rebind receipt. An account that
was already renamed, deleted, or stopped resolving cannot use ordinary rebind,
even if the same username or UID later returns; only
`breakglass principal-rebind` can recover its authority. A reused UID assigned
to a different person uses `bind --replaces OLD_ID`, receives a new principal
ID, and inherits nothing. Disabling a principal makes every grant and SSO
mapping ineffective without deleting or changing the principal.

### `PrincipalContext` exists only inside the broker

After authenticating the socket peer or verified SSO identity, the broker
mints an internal `PrincipalContext` containing the principal ID, immutable
username-plus-UID pair, authentication evidence, policy epoch, request ID, and
client kind. Its constructor and fields are private to the broker's
authenticator module. The type does not implement serialization or
deserialization, has no public constructor or test-only production bypass, and
is absent from the socket protocol.

The context is passed only by reference to broker-internal policy and store
code. It is not a bearer token, JSON object, environment value, header, CLI
argument, or result field. A client cannot submit, cache, replay, or amend one.

### CLI and stdio MCP are clients of the same broker

All data-bearing CLI operations connect to the broker socket. The CLI parses
and normalizes arguments locally, but the broker resolves selectors,
authorizes the complete operation, opens the stores, and executes it. A
client-side refusal is an additional validation layer, not authorization.

[ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md) remains the
client-facing MCP contract: an MCP client spawns `kanban mcp` and exchanges
newline-delimited JSON-RPC over stdio. Each tool call still uses the generated
CLI surface and a real command process. That command process makes a separate,
short-lived connection to the broker socket; the broker authenticates that
connection's peer UID. Stdin, the MCP parent's identity, tool arguments, and
JSON-RPC metadata carry no authority. `kanban mcp` and its children never open
managed data. A shared MCP service account receives only that Linux
principal's grants and cannot impersonate several humans.

The same rule covers web, dispatcher, backup, restore, archive, search, and
adapter paths. No projection or helper is a second file-opening authority.

### Enforcement moves one way from direct to prepared to managed

The registry enforcement state is exactly `direct`, `prepared`, or `managed`.
The only transitions are:

```text
direct -> prepared -> managed
```

`direct` is the migration-only legacy state: existing binaries may still open
managed candidates directly, and policy is not yet an enforcement claim.
`prepared` means policy is bootstrapped, every managed path is inventoried and
staged under broker ownership, every intended client has passed a broker-route
probe, and a verified cutover receipt exists; direct compatibility is still a
known migration exposure. `managed` means every data-bearing path is enforced
through the broker and direct opening is permanently unsupported.

The exact activation grammar is:

```text
kanban access enforcement show [--json]
kanban access enforcement prepare --expected-epoch EPOCH --as ACTOR --reason TEXT --confirm prepared [--json]
kanban access enforcement activate --expected-epoch EPOCH --prepare-receipt RECEIPT --as ACTOR --reason TEXT --confirm no-direct-fallback [--json]
```

There is no `disable`, `rollback`, `direct`, `unprepare`, `--force`, or mode
environment variable. Both mutations run inside the broker, require registry
`admin`, and require an exact live epoch. `show` reports replayed state, epoch,
the prepare receipt ID if present, and broker/client protocol identities; it
does not infer state from file permissions.

`prepare` acquires the exclusive registry and managed-path lock and evaluates
these preconditions in this order:

1. state is exactly `direct`, `--expected-epoch` equals the replayed epoch,
   registry integrity/audit verification passes, and no policy event is
   missing or unreplayable;
2. bootstrap has produced at least one enabled non-root registry administrator,
   every registered board has an enabled administrator holding both its board
   and board-local `*` tuples, and every inventoried service UID has an exact
   enabled binding and its declared least-privilege grants;
3. the candidate broker passes protocol, command-schema, policy-schema, and
   board-schema negotiation; the proxy-only SSO route and every CLI, MCP, web,
   dispatcher, adapter, backup, restore, archive, and index client pass a
   broker-route probe; staged permissions prove unprivileged direct selectors
   cannot open data, while prepared state still records privileged legacy
   direct access as an exposure to eliminate at activation;
4. a canonical manifest names every managed file and directory, current owner,
   target broker owner, mode, digest, backup anchor, and open-handle result;
   paths with symlinks, escaping hard links, unknown writers, or non-broker
   handles are refused; and
5. ownership and modes are staged from that manifest, verified, and only then
   does one registry transaction append `enforcement_prepared`, append its
   epoch row, store the immutable prepare receipt, and project state as
   `prepared`.

A failure in steps 1 through 4 changes nothing except a hash-covered denied
access-audit record. Before step 5 changes the first path, the broker fsyncs a
recovery marker containing the manifest and pre-change metadata. A step-5
filesystem failure restores every already-staged path from that marker and
leaves state `direct`; failure to restore is a hard operator incident and still
cannot write a prepared event. A registry transaction failure likewise restores
staged ownership and leaves state `direct`. After a crash, broker startup sees
the marker with no matching committed `enforcement_prepared` event, restores
the manifest before accepting work, and appends the denied attempt; it never
infers `prepared` from partial permissions. The marker is retired only after
the committed event, epoch, state projection, filesystem ownership, modes, and
manifest all agree, and only then is the receipt returned.

`activate` takes the same exclusive lock and checks, in order, exact state
`prepared`, exact epoch, the named receipt and every manifest digest, policy
replay, all administrator/service invariants, compatible running broker
identity, zero non-broker managed-file handles, and fresh broker-route/direct-
refusal probes for every inventoried client. Any failure appends a denied
access-audit record and leaves `prepared` unchanged. The final registry
transaction appends `enforcement_activated`, appends the next epoch row, and
projects `managed` atomically. No external action occurs between those three
writes.

Once that transaction commits, failure ordering reverses: state remains
`managed` through a broker crash, policy-load failure, client mismatch, failed
restart, failed restore, or unavailable socket. Every affected request denies;
none opens data directly or falls back to `direct` or permissive policy. Old
unprivileged binaries fail on the enforcement-state/protocol check and
filesystem ownership; UID 0 remains outside the application threat model, not
a supported direct mode. Restore refuses a snapshot whose replayed enforcement
history would move `managed` backward. Recovery is forward through a compatible
broker or the narrow break-glass commands below, never by deleting state,
lowering a mode, or using selectors.

`enforcement_prepared` and `enforcement_activated` are full `policy_events`.
Each records old/new state, expected and resulting epoch, actor and context,
broker/protocol/schema identities, the manifest and prepare-receipt digests,
the ordered precondition results, and every client probe identity. Their
matching `policy_epochs` rows commit in the same transaction. Replay from
epoch 0 determines the enforcement state; a projection or filesystem mode may
confirm that state but cannot replace it.

### Every client negotiates the broker contract

Before any operation, the client and broker exchange an exact broker protocol
version, generated command-schema hash, policy-schema version, supported board
schema range, client binary identity, and broker binary identity. A mismatch
is a generic denial before a board is opened. Every result and access-audit
record carries those identities so a stale broker cannot be reported as fresh
client execution.

The first protocol value is literal integer `1`. The client sends protocol
`1`, its exact generated command-schema hash, its supported policy-schema
range, its supported board-schema range, and its binary digest. The broker
must answer protocol `1`, the identical command-schema hash, its exact live
policy-schema version, its supported board-schema range, and its binary digest.
Protocol and command hash require equality; the live policy version and every
target board version must lie in both advertised ranges. There is no downgrade,
best-effort field omission, or retry with an older protocol.

The broker is a separately managed long-running service. Installing a CLI/MCP
binary does not upgrade or reload it. A broker upgrade preflights replay and
schema compatibility, stops accepting new requests, drains accepted requests,
restarts the service under its owner, negotiates the new identity, and only
then resumes clients. A request not yet accepted may reconnect; an accepted
request is never automatically replayed unless its operation has an explicit
idempotency key. Upgrade or negotiation failure leaves policy enforcement
closed and never enables a direct path.

### Capabilities and scope tuples are a closed vocabulary

The only capabilities are `read`, `write`, and `admin`, ordered
`admin >= write >= read`. Capability inheritance applies within one exact
scope tuple only. Registry authority does not imply board-content authority,
board authority does not imply tag authority, and authority on one tag or board
does not imply another.

The only scope atoms are exactly:

```text
registry
board:<id>
tag:<slug>
*
```

The only valid normalized scope tuples are exactly:

```text
{registry}
{board:<id>}
{board:<id>, tag:<slug>}
{board:<id>, *}
```

`tag:<slug>` and literal `*` are invalid without exactly one `board:<id>` in
the same tuple. `registry` cannot be combined with another atom; two board
atoms, duplicate atoms, an unknown board ID, and an unregistered tag slug are
refused. Literal `*` means every current and future registered tag on that one
board. It does not mean every board and it does not cover an untagged row.

An operation over a board requires the requested capability on
`{board:<id>}`. Every tagged row additionally requires that capability on each
`{board:<id>, tag:<slug>}` tuple, or on `{board:<id>, *}`. An untagged row
requires only the board tuple. Multi-row, multi-tag, multi-board, relation,
history, search, rule, deployment, subscription, backup, restore, and batch
operations require all applicable tuples; there is no any-of or partial-success
authorization. A write requires every tuple from both the complete old state
and the complete resulting state.

Grantors cannot grant a capability level or scope tuple they do not themselves
hold. The empty-policy bootstrap is the sole initial-seeding exception. A new
board-registration transaction gives its authenticated creator `admin` on the
new board tuple and its board-local `*` tuple so later delegation is possible;
the event records that seeding explicitly.

### Authorization uses the stored preimage and fails without an oracle

For a read or mutation, the broker resolves the complete old result set under
the same transaction or lock used for the authorization decision. Required
boards, rows, tags, relations, ancestors, descendants, history, and indirect
targets come from database state, not only from caller-supplied IDs or tags.
The broker then computes the complete resulting scope set for a mutation and
requires all old and new tuples before writing. The policy epoch is rechecked
under that transaction or lock immediately before commit.

An unauthorized board is never opened. Unauthorized data is excluded before
payloads, totals, facets, rankings, cursors, or existence-dependent errors are
computed. Explicit unauthorized and nonexistent board, tag, row, principal,
mapping, and grant requests return the same generic `denied or not found`
shape. The contract does not claim constant-time execution.

Long-lived reads reauthorize when the policy epoch changes, before emitting
another row or heartbeat. Revocation does not wait for a reconnect. A stale
decision fails closed and is retried through a newly minted context.

### The command grammar is exact

`PRINCIPAL` is a broker-minted `p-*` ID. `CAPABILITY` is exactly
`read|write|admin`. Each repeatable `--scope` supplies one atom from the closed
scope vocabulary above and the complete set must normalize to one valid tuple.
Every `--replaces` value names a disabled principal whose frozen username or
UID collides with the requested pair; extra and missing acknowledgements are
refused. The source of `rebind --principal` is implicit and must not also be
passed to `--replaces`. `REBIND_PROOF`, `SUBJECT_PROOF`, and `RECEIPT` are
opaque broker-minted IDs; none is caller-chosen or a bearer identity outside
the one operation described below.

The complete v1 management surface is:

```text
kanban access bootstrap --username USERNAME --uid UID --as ACTOR --reason TEXT --confirm empty-policy [--json]

kanban access principal bind --username USERNAME --uid UID [--replaces PRINCIPAL ...] --as ACTOR --reason TEXT [--json]
kanban access principal prove-rebind --principal PRINCIPAL --username USERNAME --uid UID [--replaces PRINCIPAL ...] --as ACTOR --reason TEXT [--json]
kanban access principal rebind --principal PRINCIPAL --username USERNAME --uid UID [--replaces PRINCIPAL ...] --source-proof REBIND_PROOF --as ACTOR --reason TEXT [--json]
kanban access principal disable --principal PRINCIPAL --as ACTOR --reason TEXT [--json]
kanban access principal show --principal PRINCIPAL [--json]
kanban access principal list [--disabled] [--json]

kanban access grant --principal PRINCIPAL --capability CAPABILITY --scope SCOPE [--scope SCOPE ...] --as ACTOR --reason TEXT [--json]
kanban access revoke --principal PRINCIPAL --capability CAPABILITY --scope SCOPE [--scope SCOPE ...] --as ACTOR --reason TEXT [--json]

kanban access map-sso --provider google --subject SUBJECT --subject-proof SUBJECT_PROOF --principal PRINCIPAL --as ACTOR --reason TEXT [--json]
kanban access unmap-sso --provider google --subject SUBJECT --as ACTOR --reason TEXT [--json]

kanban access explain --principal PRINCIPAL --capability CAPABILITY --scope SCOPE [--scope SCOPE ...] [--json]
kanban access audit [--principal PRINCIPAL] [--actor-principal PRINCIPAL] [--kind KIND] [--capability CAPABILITY] [--scope SCOPE ...] [--after-epoch EPOCH] [--limit LIMIT] [--json]

kanban access breakglass principal-rebind --principal PRINCIPAL --username USERNAME --uid UID [--replaces PRINCIPAL ...] --as ACTOR --reason TEXT --confirm root-breakglass [--json]
kanban access breakglass map-sso --provider google --subject SUBJECT --principal PRINCIPAL --as ACTOR --reason TEXT --confirm root-breakglass [--json]
kanban access breakglass registry-admin --principal PRINCIPAL --as ACTOR --reason TEXT --confirm root-breakglass [--json]

kanban access enforcement show [--json]
kanban access enforcement prepare --expected-epoch EPOCH --as ACTOR --reason TEXT --confirm prepared [--json]
kanban access enforcement activate --expected-epoch EPOCH --prepare-receipt RECEIPT --as ACTOR --reason TEXT --confirm no-direct-fallback [--json]
```

There are no short aliases, positional identity values, bulk files,
`--username`/`--uid` alternatives for principal-targeting commands, or generic
policy-edit command. Unknown flags and extra positionals are refused.

`principal show` and `principal list` require registry `read`; policy mutations
require registry `admin` plus grantor non-escalation where applicable.
`explain` and `audit` require registry `admin`, including when the target is the
caller, so neither becomes an existence oracle. `explain` is read-only and
returns the epoch, required tuples, matched grant IDs, and generic denial
reason; it never creates authority.

`KIND` is exactly one of `bootstrap`, `principal_bound`,
`principal_rebound`, `principal_disabled`, `grant_added`, `grant_revoked`,
`sso_mapped`, `sso_unmapped`, `board_seeded`, `enforcement_prepared`,
`enforcement_activated`, `rebind_proof_issued`, `rebind_proof_consumed`,
`sso_subject_proof_issued`, `sso_subject_proof_consumed`, `proof_expired`,
`breakglass_principal_rebound`, `breakglass_registry_admin`,
`breakglass_sso_mapped`, `authorization_allowed`, `authorization_denied`, or
`breakglass_denied`. An audit scope filter, when present, must itself form one
valid normalized tuple. `EPOCH` is a nonnegative integer and `LIMIT` is an
integer from 1 through 1000. Audit filters combine with AND and never relax the
administrator gate.

`map-sso` accepts only an already-existing, enabled principal ID. It does not
accept username or UID, mint a principal, create or change a Linux binding, or
reactivate a disabled principal. Provider and immutable provider subject form
one unique mapping. `unmap-sso` retires that mapping; neither operation deletes
history.

Ordinary rebind is a transfer of effective authority, not identity maintenance.
`prove-rebind` must run while the source pair still authenticates: the broker
requires the socket peer to be that exact enabled source principal and mints a
single-use proof bound to source ID, requested successor pair, normalized
`--replaces` set, current epoch, complete active grant-and-mapping state hash,
and a fixed five-minute expiry. The requested successor pair must be distinct
and not yet resolve in passwd. The proof returns no principal context.

A planned OS rename has this exact order:

1. while the old frozen pair still resolves and authenticates, that source
   principal runs `prove-rebind` for the exact future pair;
2. before changing passwd, an eligible administrator submits `rebind` with the
   proof; under one policy transaction the broker rechecks the old pair in both
   passwd directions, requires the future pair still not to resolve, replays
   the exact proof-bound epoch/grant/mapping state, consumes the proof, disables
   the source, mints the successor, transfers authority, and commits the policy
   event, epoch, projection, and allowed audit row; and
3. only after receiving that committed rebind receipt may the operator perform
   the OS rename or UID change. The successor cannot authenticate before its
   frozen pair begins resolving exactly, and the retired source cannot
   authenticate after step 2.

Any policy-epoch change, grant or mapping change, source-state change, passwd
change, target collision, expiry, or failed transaction before step 2 commits
invalidates the proof without transferring authority. The operator must mint
and consume a new proof while the old pair still authenticates. If the passwd
change has already happened, the old pair has stopped resolving, or the old
pair cannot authenticate for any other reason, ordinary `prove-rebind` and
ordinary `rebind` are both unavailable; only
`breakglass principal-rebind` can recover the principal. A stale proof is never
accepted as a post-change recovery credential.

The authenticated actor performing ordinary `rebind` must hold registry
`admin` and an equal-or-higher capability on every exact scope tuple carried by
every source grant that would transfer. This all-of check uses the source's
complete active grant result set from the old policy database. The transaction
fails rather than partially transferring grants or mappings. Source proof is
consent, not authority: it never substitutes for the actor's equivalent grants.

Ordinary SSO linking likewise requires proof of the subject. After the Google
verifier accepts an otherwise unmapped proxy-only request, the broker may mint
a single-use `SUBJECT_PROOF` bound to literal provider `google`, the exact
immutable subject bytes, verified proxy session, current epoch, and a fixed
five-minute expiry. `map-sso` consumes it atomically. A proof is never accepted
from a header, CLI field, non-proxy connection, different subject, changed
epoch, expired session, or second use.

Because mapping lets the SSO subject exercise the target principal's current
authority, the actor must hold registry `admin` and an equal-or-higher
capability on every exact tuple in the target principal's complete active grant
result set. The target must still be enabled and the grant set and epoch must
still match when the transaction commits. Subject proof does not satisfy
non-escalation. If subject proof cannot be produced, only
`breakglass map-sso` may create the mapping. `unmap-sso` removes an
authentication route and requires registry `admin`, but transfers no grant.

`--as` is mandatory audit context on management mutations and remains
non-authoritative. It is stored as `context.claimed_actor`, separate from the
broker-derived actor. `--reason` is mandatory, nonblank, bounded text and must
not contain credentials. The broker does not accept a caller-supplied actor
principal, actor UID, authentication kind, request ID, client kind, or policy
epoch.

### Policy events and epochs replay the complete authorization state

The registry owns append-only `policy_events` and `policy_epochs` journals.
There is no supported update or delete. Every policy mutation appends exactly
one canonical event and exactly one epoch row in the same transaction. Epoch 0
is the empty state; the successful bootstrap event creates epoch 1. Failed and
denied attempts do not advance the epoch.

Each `policy_events` row stores at least:

- sequence, event ID, kind, timestamp, previous hash, and event hash;
- `before_epoch` and `after_epoch`;
- the matching `access_audit_event_id`;
- broker-derived `actor_principal_id`, `actor_username`, and `actor_uid`;
  `actor_principal_id` alone is null on the two root-only paths, while those
  paths still record `actor_username` as `root` and `actor_uid` as `0`;
- a context object containing `authn_kind`, `peer_uid`, `real_uid`,
  `effective_uid`, `client_kind`, `request_id`, `claimed_actor`, and `reason`,
  plus verified provider and subject for SSO requests;
- target principal or mapping IDs;
- complete immutable source and successor principal values where applicable,
  including the rebind succession relation; and
- the exact normalized capability and scope-tuple delta, including seeded,
  granted, revoked, activated, and retired grant IDs.

Each `policy_epochs` row stores the epoch, policy-event sequence and hash,
previous state hash, resulting state hash, and timestamp. Replaying
`policy_events` from epoch 0 must reconstruct frozen principals, rebind
succession, active/disabled state, mappings, grants, revocations, the one-way
enforcement state and prepare receipt, and the exact state hash at every epoch.
Materialized policy tables are rebuildable projections and never the sole
source of truth. The journals join ADR-029's
registry hash chain and backup manifest; truncation, rewriting, sequence gaps,
or a state-hash mismatch fail `audit verify` and policy load.

Authorization decisions, generic denials, proof issuance/consumption, root
attempts, and break-glass use also append hash-covered `access_audit` records.
They do not advance the policy epoch unless they perform a policy mutation.
For a successful policy mutation, its allowed access-audit row, policy event,
epoch row, and materialized policy projection commit in one registry
transaction; none may exist without the others.
Unconsumed proof IDs are a materialized projection of proof-issued minus
proof-consumed/expired audit events; only a hash of the opaque proof is stored,
and broker restart does not make a used proof reusable. `access audit` reads
both streams without exposing records outside its administrator gate.

Every `access_audit` row has this exact version-1 shape:

- chain fields: `schema_version` (literal `1`), `seq`, `event_id`,
  `previous_hash`, `event_hash`, and `occurred_at`;
- decision fields: `operation`, `outcome` (`allowed|denied`), internal
  `decision_stage`, internal `decision_code`, `policy_epoch`, and
  `enforcement_state`;
- actor fields: `actor_principal_id`, `actor_username`, and `actor_uid`;
- evidence fields: `authn_kind`, `evidence_source`, `peer_pid`, `peer_uid`,
  `peer_gid`, `real_uid`, `effective_uid`, `provider`, `subject`,
  `verifier_id`, `proxy_route_id`, `source_proof_id`, and `subject_proof_id`;
- request context: `request_id`, `client_kind`, `claimed_actor`, `reason`,
  `client_binary_id`, `broker_binary_id`, `broker_protocol_version`,
  `command_schema_hash`, `policy_schema_version`, and
  `board_schema_versions`;
  and
- authorization context: normalized required scope tuples, requested
  capability, matched grant IDs, target IDs already visible to the actor, and
  redacted target digests for targets the actor may not discover.

The public refusal remains only `denied or not found`; internal decision stage,
code, unmatched target, and proof failure are visible only through the
registry-admin audit command. Audit payloads never contain cookies, tokens,
proof secret bytes, headers, credentials, or unredacted secret values. Failure
to append a required audit row denies the operation. If registry damage makes
the append impossible, the broker emits a host-service diagnostic and remains
closed; it does not claim a durable receipt or fall back to direct access.
`claimed_actor` and `reason` are null only for operations whose exact grammar
does not accept them; `board_schema_versions` is an empty list when no board
was opened; each proof-ID field is null except for issuance, consumption, or a
request that presented that proof type.

The evidence-source and nullability matrix is binding. `R` means required,
`-` means always null, and `C` means required only after the evidence stage
named below succeeded:

| Authentication and outcome | Actor principal/name/UID | Peer PID/UID/GID | Real/effective UID | Provider/subject | Verifier and proxy route | Proof ID | `evidence_source` |
|---|---:|---:|---:|---:|---:|---:|---|
| socket peer allowed | R | R | - | - | - | C for rebind or SSO map | `kernel_so_peercred` |
| socket peer denied | C after principal resolution | C after `SO_PEERCRED` | - | - | - | C if parsed | `kernel_so_peercred` or `kernel_peercred_unavailable` |
| SSO allowed | R | R for proxy peer | - | R | R | C for linking | `google_verifier_proxy_socket` |
| SSO denied | C after mapping | C after proxy accept | - | C after verifier success and subject validation | C after each stage | C if parsed | one closed SSO evidence value below |
| bootstrap allowed | name `root`, UID 0; principal ID - | R with UID 0 | - | - | - | - | `kernel_so_peercred_root_bootstrap` |
| bootstrap denied | C after root resolution; principal ID - | C after `SO_PEERCRED` | - | - | - | - | `kernel_so_peercred_root_bootstrap` or `kernel_peercred_unavailable` |
| break-glass allowed | name `root`, UID 0; principal ID - | R with UID 0 | - | - | - | - | `kernel_so_peercred_root_breakglass` |
| break-glass denied | C after root resolution; principal ID - | C after `SO_PEERCRED` | - | C for SSO-map target syntax only | - | - | `kernel_so_peercred_root_breakglass` or `kernel_peercred_unavailable` |

Linux `SO_PEERCRED` supplies peer PID, UID, and GID; it does not supply the
peer's real UID or a separate effective UID. Version 1 does not infer those
fields from the peer UID, request input, `SUDO_USER`, or process ancestry, so
`real_uid` and `effective_uid` are always explicit nulls in this matrix. The
peer UID remains the sole local kernel identity value. A later independently
verified `/proc` evidence source would require a new audit schema version and
must not relabel historical peer UID evidence.

The closed SSO evidence values are `google_verifier_proxy_socket`,
`proxy_peercred_unavailable`, `proxy_peer_not_allowed`,
`google_verification_failed`, and `google_subject_invalid`. They identify the
last independently completed evidence stage and are not returned in a public
denial.

### Bootstrap and break-glass are different root-only operations

`access bootstrap` succeeds only when the socket peer UID is 0, policy epoch is
0, enforcement state is `direct`, both policy journals are empty, and registry
integrity and audit verification pass. It verifies the requested non-root
passwd pair, creates the first principal, and atomically seeds `admin` grants
for `{registry}` plus
`{board:<id>}` and `{board:<id>, *}` for every board then registered. Its event
records the complete registered-board result set. It refuses after epoch 1.

Bootstrap does not bind root, create a root principal, or give UID 0 an implicit
or reusable application grant.

Break-glass refuses at epoch 0 and uses its separate command prefix and literal
confirmation. It succeeds only for socket peer UID 0 after registry integrity
and audit verification. `principal-rebind` performs that one rebind without a
source proof; `map-sso` performs that one Google-subject link without a subject
proof or equivalent-scope actor; `registry-admin` appends that one `admin`
grant on `{registry}` to an existing, enabled principal. These are the only
exceptions to ordinary proof and non-escalation checks, and their complete
authority delta is recorded. Neither opens a general root session, returns a
capability token, grants root, bypasses the broker, or authorizes a second
operation. Every attempt, including a refusal, gets a dedicated root audit
record with kernel UID evidence, `claimed_actor`, reason, request ID, and
outcome.

`breakglass map-sso` substitutes verified root socket identity for the ordinary
registry-administrator actor. After that explicit break-glass authentication
substitution, it skips exactly two ordinary-link checks: `SUBJECT_PROOF`
presentation and equivalent-scope non-escalation. It retains every other check.
Under one exclusive registry transaction the broker
must require kernel socket peer UID 0; policy epoch greater than 0; healthy
registry integrity, policy replay, and audit chains; exact literal provider
`google`; a `SUBJECT` satisfying the byte grammar below; a unique provider/
subject pair with no active mapping; an already-existing enabled target
principal; and that target's exact current ID, enabled state, grant set, mapping
set, and policy epoch at commit. Success appends exactly one mapping delta, one
`breakglass_sso_mapped` policy event, the matching epoch row and projection,
and one allowed root access-audit row atomically. Failure appends only the
denied root audit row when the audit journal is writable and changes no policy.

`breakglass map-sso` cannot mint or rebind a principal, reactivate a disabled
principal, alter an existing mapping, grant or transfer any capability, bypass
provider/subject grammar or uniqueness, skip integrity/audit verification, or
authorize another operation. Those actions require their separately named
ordinary or break-glass command and receipt.

### Browser SSO is verified at a proxy-only boundary

Google SSO is the only version-1 browser identity provider. The provider token
is exactly lowercase ASCII `google`; aliases, case folding, trimming, Unicode
normalization, and another provider value are refused. `SUBJECT` is the
Google verifier's immutable OIDC `sub`, not email, hosted-domain, display name,
or a caller label. Its accepted byte grammar is exactly
`^[A-Za-z0-9._~-]{1,255}$`: non-ASCII UTF-8, empty input, leading or trailing
whitespace, embedded whitespace, ASCII controls, NUL, percent-decoding, and
values over 255 bytes are refused. Storage and comparison use the original
ASCII bytes exactly; there is no case, URL, or Unicode normalization step.

The reverse proxy strips caller-supplied identity headers, verifies the Google
session, and sends literal provider `google` and the verifier-issued subject
only over a proxy-only Unix socket that direct HTTP and loopback clients cannot
reach. The broker trusts an SSO assertion only from the allow-listed proxy peer
credential on that socket, maps the exact provider/subject to one existing
enabled principal, and mints a fresh `PrincipalContext` for every request.

The web service's process UID is not the browser principal. A cookie, header,
email label, display name, literal `geo`, unmapped identity, direct loopback
request, or value from a non-proxy socket fails closed. Event attribution uses
the mapped principal as actor; any on-behalf-of label is separate audit context
and cannot affect authorization.

## Consequences

- Managed data has one file-opening and authorization boundary.
- A username, UID, actor label, selector, or stdio caller cannot mint its own
  authority.
- UID reuse is an explicit choice between minting a successor with transferred
  authority and minting a new principal with no inherited authority.
- Board-local tag and wildcard grants cannot leak into another board.
- Complete old and resulting database state participates in all-of decisions.
- Policy state and every epoch are reproducible from append-only, hash-covered
  history.
- Root remains outside the application threat model, but bootstrap and
  break-glass use become narrow, explicit, and auditable instead of implicit.

## Implementation and evidence status

This ADR and the ADR-011 amendment are design and generated-command-schema
contracts only. They do not implement the broker, create Linux accounts,
change file ownership, migrate managed data, enforce policy, alter the SSO
proxy, or provide runtime, E2E, or live-tier evidence.

Implementation must add generated-schema contract tests for every accepted and
rejected grammar form, unit coverage for replay and scope algebra, API-level
integration against real registry and board databases with tenancy isolation,
and compiled-process E2E on Linux for peer credentials, UID reuse, selector and
actor forgery, source and subject proof replay, equivalent-scope transfer,
old/resulting scope checks, audit nullability, immediate revocation, exact SSO
bytes, stdio MCP plus the separately versioned broker hop, broker upgrade
failure, every direct/prepared/managed precondition and failure boundary,
permanent no-direct fallback, direct-database refusal, bootstrap, and
break-glass. Those layers must retain their exact names; integration is not E2E,
and there is no current live acceptance.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) - narrow operations, not arbitrary write SQL
- [ADR-007](ADR-007-global-project-addressing.md) - selector precedence and ambiguity refusal
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) - generic fail-closed behavior
- [ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md) - stdio remains the client-facing MCP transport
- [ADR-015](ADR-015-tags-are-a-per-board-master-file.md) - registered tag vocabulary
- [ADR-029](ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md) - hash chains and external anchors
