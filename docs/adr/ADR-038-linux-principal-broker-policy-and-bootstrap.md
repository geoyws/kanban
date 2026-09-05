# ADR-038: Linux principal, broker, policy, and bootstrap

**Status:** Proposed
**Date:** 2026-09-05
**Deciders:** Proposed by claude@driver under George's 2026-09-05 standing goal; George accepts or amends
**Extends:** [ADR-033](ADR-033-principals-are-frozen-username-plus-uid-and-minted-through-a-peer-credential-broker.md), which it does not supersede; where this document and ADR-033 could be read differently, ADR-033 wins and the difference is named here

## Context

[ADR-033](ADR-033-principals-are-frozen-username-plus-uid-and-minted-through-a-peer-credential-broker.md)
froze the target contract for managed multi-user mode: one broker that owns the
data, authenticates the connecting process by kernel peer credential, mints a
`PrincipalContext` it never lets out, evaluates a closed capability vocabulary
over a closed scope vocabulary, and journals every policy change as an
append-only, hash-chained event. It says of itself that it "does not claim that
the broker, policy schema, filesystem ownership, or host cutover exists yet",
and it describes the policy schema in prose.
[ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) settled that a
surface is described once, as data, and that adapters are generated from that
description rather than written beside it. The policy surface has no such data
yet, so an adapter, a test, or the `/kb` skill has nothing to be generated from.

Epic e-b098ffe7 asks for the remaining decisions to be frozen *with their
reasoning* and for the schema to be written down as this repository would emit
it. That is this document. It adds four things ADR-033 states but does not
derive: the capability lattice as a partial order with a default, the policy
epoch as a compare-and-set that every operation participates in, the trust
statement for a database opened directly while enforcement is still `direct`,
and the JSON shapes. It reconciles two boundaries it must not move:
[ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md)'s stdio contract
and the web edge's identity assertion.

The trust boundary today, measured from source at commit 53e6ed1:

- `--as` is a claimed actor. Nothing checks it against anything; ADR-029 made
  the claim tamper-evident without making it true.
- `rust/serve.rs` `ServeConfig::actor_for_write` returns either the constant
  `OPERATOR_ACTOR` — the literal `geoyws` from `rust/model.rs` — or the single
  value of one edge-configured header (`--actor-header NAME`). Its module
  comment is honest that the proxy "must strip any client-supplied copy": the
  header is an edge label, trusted because the edge is, and it names an
  email-sized audit identity (`MAX_ACTOR_BYTES = 254`), not a principal.
- `rust/lib.rs` `store_path` resolves `--db PATH` to that file and
  `Store::open` opens it under the calling process's own UID. The registry is
  consulted only to refuse a retired board. No policy participates because no
  policy exists.

None of those is a defect in the code that has them; each is documented as the
label it is. The defect would be to put a policy beside them and let a reader
believe the label was authenticated. This document fixes what changes and,
just as carefully, what does not.

It is **Proposed** because it is a binding security architecture for the
operator's estate. The lane that wrote it does not accept it on George's
behalf.

## Decision

Twelve clauses. Each states the decision, then the reasoning.

### 1. A principal is one frozen `{username, uid}` pair behind an opaque `p-*` ID

A principal is a broker-minted `p-*` identifier bound to exactly one
`{username, uid}` pair. Both halves are required, both are frozen at `bind`,
and neither is updated in place. Grants and SSO mappings attach to the `p-*`
ID. Changing either half is `rebind`: a new principal, an explicit transfer,
the old one disabled, all in one event. This is ADR-033's rule restated so the
reasoning can be attached.

Why both halves and not one. A username alone is reused: `useradd` after
`userdel` hands the same name to a different person and every grant on the name
would follow it silently. A UID alone is reused the same way and additionally
hides continuity: a rename of the same person would look like a new account.
Freezing both makes every divergence detectable at authentication time — the
kernel says UID `1001`, passwd says `1001` is now `mallory`, the principal says
`1001` is `alice` — and ADR-033's two-way check (`getpwuid(uid).pw_name ==
username` and `getpwnam(username).pw_uid == uid`) denies rather than guesses.
Why frozen rather than updated: an in-place update is a policy mutation with no
event of its own, which is exactly the shape ADR-029 exists to make impossible.
Why an opaque ID: so that a grant, a mapping, an audit row, and a rebind
succession can name a principal without naming a username or UID that may be
reused, and so `explain` can answer for a disabled principal without resolving
passwd.

### 2. Peer-credential authentication: the kernel says who connected, and only the kernel

Every request crosses a Unix-domain socket. The broker calls
`getsockopt(fd, SOL_SOCKET, SO_PEERCRED)` on the accepted connection and reads
`struct ucred { pid, uid, gid }`. `uid` is the principal evidence. `pid` and
`gid` are recorded, never authorized on.

What `SO_PEERCRED` proves. The kernel records the connecting process's
credentials at the instant of `connect(2)` and reports them on the accepting
side; an unprivileged process cannot set, forge, or relay them, and they cannot
be sent as data. It therefore proves exactly this: *a process whose effective
UID was `N` connected to this socket*. It does not prove which human is at a
keyboard, which terminal or session the process belongs to, which binary
connected (the client's binary digest in ADR-033's negotiation is a
compatibility identity that the client reports about itself, not
authentication), or that the process still has UID `N` a second later. The
broker does not need any of those: it authorizes the connect-time UID for the
one short-lived connection in front of it, and a process that changes
credentials must reconnect and be seen again.

`getpeereid(2)` and `LOCAL_PEERCRED` are the BSD and macOS equivalents. They
return the peer's effective UID and GID without a PID. Version 1 names them so
the authenticator compiles and its unit tests run on the macOS devbox against
the real primitive, but they are not a managed-mode evidence source: ADR-033's
audit matrix admits only `kernel_so_peercred*` and
`kernel_peercred_unavailable`, and a host whose kernel offers neither is denied
under `kernel_peercred_unavailable`, not accommodated. Adding a second evidence
source is a new audit schema version, per ADR-033.

Why a Unix socket and not stdio, a TCP loopback port, or a token. Stdio has no
peer credential; a pipe carries bytes and the bytes can say anything. Loopback
TCP has no peer credential either — `SO_PEERCRED` is Unix-domain only — and
would reintroduce the port and authentication story ADR-011 refused. A token is
a bearer: whoever holds it is the principal, which is precisely the
"identity chosen by the caller" that ADR-033's context section rules out.

### 3. `PrincipalContext` is sealed: built once at the boundary, immutable, never rebuilt from request data

The authenticator module constructs one `PrincipalContext` per accepted
connection, after `SO_PEERCRED` and the two-way passwd check succeed (or, for
the proxy-only SSO route, after the mapping resolves). It contains the
principal ID, the frozen pair, the authentication evidence, the policy epoch
and policy state hash at mint, the request ID, and the client kind. Its fields
and constructor are private to that module. It does not implement
serialization or deserialization, it has no test-only constructor, and it never
appears in the socket protocol. It is passed by shared reference to policy and
store code and dropped when the connection closes.

Why sealed. Every field a request could supply — `--as`, headers, JSON-RPC
metadata, environment, `SUDO_USER`, a principal ID typed on the command line —
is a claim. If the context could be built from any of them, or amended after
construction, the claim would become the decision. Immutability is what lets a
long-running operation trust that the identity it started with is the identity
it commits with; the epoch check in clause 8 is what tells it when that trust
has expired. The only observable projection of a context is the actor and
evidence block of an audit row, and the generated-schema section gives that
shape precisely so a contract test can assert it without a constructor.

### 4. Registry policy rows are projections; policy events and access-audit events are the truth

The registry owns three append-only journals — `policy_events`,
`policy_epochs`, and `access_audit` — and a set of materialized tables
(principals, grants, SSO mappings, enforcement state, unconsumed proofs) that
are rebuildable from the first two. There is no update or delete on any
journal. A successful policy mutation commits its allowed access-audit row, its
policy event, its epoch row, and its projection update in one registry
transaction; a failed or denied attempt appends only a denied access-audit row.
All three journals join ADR-029's registry hash chain and backup manifest, so
truncation, reordering, or substitution fails `audit verify` and refuses policy
load.

Why events, not rows, are authoritative. A row answers "what is true now" and
cannot answer "how did it become true"; the second question is the one an
operator asks after an incident. Replaying `policy_events` from epoch 0 must
reproduce every principal, succession, grant, revocation, mapping, and the
one-way enforcement state, and the state hash at each epoch must match the
`policy_epochs` row. A projection that disagrees with replay is wrong by
definition and is rebuilt, never trusted. Why access-audit is a separate
stream: authorization decisions happen orders of magnitude more often than
policy changes, they do not advance the epoch, and mixing them into the policy
journal would make replay slow and the policy history noisy. They share the
hash chain because a denied attempt is evidence too.

### 5. The capability lattice: a positive-only product of three-element chains, default `none`

Let `C` be the chain `none < read < write < admin` and let `T` be the closed
set of normalized scope tuples from ADR-033:
`{registry}`, `{board:<id>}`, `{board:<id>, tag:<slug>}`, and `{board:<id>, *}`.
A principal's authority is a function `A : T -> C`. **The default is `none`
at every tuple**; `none` is never stored, it is the absence of a grant row.
Each active grant row contributes one point `(t, c)`; authority is the
pointwise join: `A(t) = max { c : (t, c) is an active grant }`. Tuples are
incomparable with each other, so the lattice is the product of one chain per
tuple: `admin` on `{registry}` implies nothing on any board tuple, `admin` on
`{board:b}` implies nothing on `{board:b, tag:s}`, and no tuple implies another
board.

One requirement rule, and only one, crosses tuples: a requirement
`({board:b, tag:s}, c)` is satisfied when `A({board:b, tag:s}) >= c` **or**
`A({board:b, *}) >= c`. The wildcard tuple is an alternative
satisfier for every current and future registered tag on that one board, not an
ordering above the tag tuples and not a claim over untagged rows. The same
satisfaction predicate defines what a grantor "holds" for non-escalation:
`grant` and `revoke` of `(t, c)` require registry `admin` and that the actor
satisfies `(t, c)` by the rule above.

Grant rows are exact triples `(principal, tuple, capability)`. Two levels on
one tuple are two rows and the effective level is their join. `revoke` names
the exact triple and retires exactly that row; it does not lower a different
level on the same tuple, and it refuses when no such active row exists.

Why positive-only. There are no deny rows, so evaluation is monotone in the
grant set, "conflict" cannot arise, `explain` is a lookup and not a solver, and
non-escalation reduces to a pointwise comparison. Why the default is `none`
rather than `read`: a fresh principal that could read every board before anyone
decided it should would make `bind` a grant, and ADR-033 says `bind` creates a
principal "with no grants". Why tuples do not imply each other: registry
authority is about principals and enforcement, board authority is about work
rows, and tag authority is the unit a board owner delegates by; collapsing any
two would make the smallest useful grant larger than intended. Why the wildcard
is a satisfier and not a parent: making `{board:b, *}` a parent of every tag
tuple would let a grant on `*` be *exceeded* by a later tag grant and vice
versa in ways the explain output could not render as a single matched row.

### 6. Empty-policy bootstrap: a fresh registry allows exactly one thing, and it is not break-glass

At policy epoch 0 — no `policy_events`, no `policy_epochs`, enforcement state
`direct` — the registry allows no policy operation to any authenticated peer.
No principal resolves, so every `access` command from a non-root peer, reads
included, fails with the generic `denied or not found`. From a peer whose
`SO_PEERCRED` UID is 0, exactly one command is accepted:

```text
kanban access bootstrap --username USERNAME --uid UID --as ACTOR --reason TEXT --confirm empty-policy
```

It verifies the requested **non-root** pair in both passwd directions, mints
the first principal, and in the same transaction seeds `admin` on `{registry}`
and `admin` on `{board:<id>}` and `{board:<id>, *}` for every board registered
at that moment. The event records the complete registered-board result set. The
result is epoch 1. It refuses at any epoch other than 0, and it refuses when
either journal is non-empty, when enforcement state is not `direct`, or when
registry integrity or the audit chain fails. Every `breakglass` command refuses
at epoch 0. Bootstrap binds no root principal and leaves UID 0 with no stored
authority.

The task body describes this as "fail closed except for the explicit root
break-glass". ADR-033 keeps the two names apart, and so does this document:
`bootstrap` presupposes there is no policy to repair, uses its own confirm
literal (`empty-policy`), its own evidence source
(`kernel_so_peercred_root_bootstrap`), and its own event kind; `breakglass`
presupposes a policy that exists and is being recovered. Both are root-only,
one-shot, and audited; neither is a policy row. Calling bootstrap a break-glass
would suggest it could be used twice.

Why root and not "first caller wins". Trust on a host already lives with UID 0:
whoever can be root can replace the registry file, so requiring root for the
one operation that seeds the registry adds no trust that did not exist and
removes the race where the first process to reach a fresh socket owns the
estate. Why the first principal is non-root: root authority is the host's, and
clause 7 explains why it never becomes a row. Why every registered board is
seeded, rather than only `{registry}`: registry `admin` implies nothing on any
board (clause 5), so a bootstrap that seeded only the registry would leave the
administrator unable to grant on any board without a second root operation.
Why `enforcement show` is not the exception: it requires registry `read`, and
at epoch 0 nobody holds it. The operator learns the pre-bootstrap state from
`doctor --json`, which this document extends with a `policy` block (`epoch`,
`enforcementState`, `journalHead`) — a registry integrity read that already
exists and already runs before any board opens.

### 7. Explicit root break-glass is a command family, not a policy row

Three commands, each root-only, each one-shot, each with the literal
`--confirm root-breakglass`:

- `breakglass principal-rebind` performs one rebind without a source proof;
- `breakglass map-sso` performs one Google-subject link without a subject proof
  and without the equivalent-scope check;
- `breakglass registry-admin` appends one `admin` grant on `{registry}` to an
  existing, enabled principal.

Each succeeds only when the socket peer UID is 0, the policy epoch is greater
than 0, and registry integrity, policy replay, and the audit chain verify. Each
skips exactly the named ordinary checks and retains every other one. Each
records its complete authority delta in one `breakglass_*` policy event with
the matching epoch row and an allowed root access-audit row whose evidence
source is `kernel_so_peercred_root_breakglass`, `actorPrincipalID` null,
`actorUsername` `root`, `actorUID` `0`. A refused attempt appends a
`breakglass_denied` access-audit row with the same evidence fields. Nothing
opens a root session, returns a token, or authorizes a second operation.

Why not a policy row for root. First, a row is reusable authority evaluated by
the lattice, and root's authority is the host's, not the registry's: a
`{registry: admin}` row for UID 0 would let the registry claim to govern
something that can overwrite the registry. Second, a row is revocable, and
revoking root would be a lie the audit trail would then carry. Third, a row has
a principal, and a principal can be the source of a `rebind` transfer or the
grantor of a grant — a path by which root's authority could flow into an
ordinary principal without the explicit delta a break-glass event records.
Fourth, ADR-033's replay invariant: reconstructing every epoch from epoch 0
never needs a root principal, because every break-glass result lands on an
ordinary principal and is expressible entirely in ordinary rows again. Root is
outside the application threat model (ADR-029, ADR-033); making its use narrow,
explicit, and audited is the most the application can honestly do.

### 8. Policy epochs are monotonic, bumped by policy events only, and compared for equality at commit

`policy_epochs` is a sequence of rows `0, 1, 2, ...` with no gaps and no
reuse. Epoch 0 is the empty state. Exactly one thing advances the epoch: a
committed policy event of one of the mutating kinds — `bootstrap`,
`principal_bound`, `principal_rebound`, `principal_disabled`, `grant_added`,
`grant_revoked`, `sso_mapped`, `sso_unmapped`, `board_seeded`,
`enforcement_prepared`, `enforcement_activated`,
`breakglass_principal_rebound`, `breakglass_registry_admin`,
`breakglass_sso_mapped` — and each advances it by exactly one. Authorization
decisions, proof issuance, consumption and expiry, root attempts, and denials
of any kind do not advance it. Each epoch row records the event sequence and
hash it was produced by, the previous state hash, and the resulting state hash.

A `PrincipalContext` carries the epoch and resulting state hash at the moment
it was minted. Every operation that reads or writes under that context rereads
the live epoch and state hash under the same transaction or lock immediately
before commit, and requires **equality** of both. A mismatch is a stale
context: the operation is denied with the generic shape, the access-audit row
records `decisionStage: "epoch"`, nothing is written, and the client's fix is
to reconnect — a new connection mints a new context at the live epoch. A
long-lived read (`watch`, a subscription, a streaming search) rechecks before
each emitted row or heartbeat, so revocation lands without waiting for a
reconnect (ADR-033).

`enforcement prepare` and `enforcement activate` additionally take an
operator-supplied `--expected-epoch EPOCH`, an explicit compare-and-set on top
of the context's own. For an actor who already holds registry `admin`, an
`--expected-epoch` mismatch is refused by naming the live epoch — the refusal
is its own fix (ADR-008), and a registry administrator learns nothing they
could not read. For anyone else it is the generic denial.

Why equality and not "at least". A restore (ADR-029) can legitimately produce
an epoch numerically lower than one a client saw, and can also produce the
*same* number with different contents. Ordering cannot distinguish either case;
equality on epoch plus state hash refuses both. Why the epoch is not on
`grant`/`revoke` too: their receipts carry the resulting epoch, an
administrator who wants compare-and-set reads `explain` first and the context
check already refuses a decision made against a superseded state; adding the
flag would be a grammar change to ADR-033's frozen surface for a race the
context check already closes. Why denials do not bump: a denial changes no
authority, and an epoch that advanced on every failed probe would turn a
denial into a way to invalidate every other client's context.

### 9. Direct-database isolation: `--db` bypasses the broker by construction, so nothing done through it is a policy decision

While enforcement state is `direct`, `--db PATH` (and `KANBAN_DB`, and a
scratch `KANBAN_DATA_DIR`) resolves to a file the calling process opens itself,
under its own UID, with no broker between. That is not a hole in the policy;
it is the absence of one, and the isolation rule is that the two never
masquerade as each other:

- A directly opened board is authorized by POSIX file permission and nothing
  else. Its events carry `--as` as the actor exactly as today, the ADR-029
  chain makes that claim tamper-evident, and the claim is still a claim. No
  `access_audit` row is written for a direct open, no `actorPrincipalID` can be
  attached to its events, and every receipt that reports enforcement state
  reports `direct`. A policy row that happens to exist in the registry has
  **no effect** on a direct open; it is not consulted, so it cannot be
  reported as having allowed anything.
- The broker exists in `direct` state for the policy journals only: bootstrap,
  binding, granting, and `enforcement prepare` all run inside it, because
  ADR-033's `prepare` preconditions require administrators to exist before the
  cutover. Board data is not yet its concern.
- In `managed` state the same command line means something else. `--db` is an
  addressing input (ADR-007, ADR-028): the broker requires it to name an
  already registered board and executes the operation itself. The direct open
  is impossible rather than forbidden — the file and its directory are owned
  by the broker's UID and unreadable to the caller — so an old binary, a shell
  `sqlite3`, or a hand-written adapter fails on `open(2)` before any code of
  ours runs. That is what "by construction" buys: the property does not depend
  on every client being well-behaved.

What this means for trust, stated plainly: the truth value of "this row was
authorized" is read from `enforcementState` on the row, never from whether a
grant existed at the time. A `direct` registry is a single-user tool with an
honest audit label; a `managed` registry is a multi-user system with an
authenticated actor; there is no state in which a label is reported as an
authentication. ADR-033's one-way `direct -> prepared -> managed` transition is
what guarantees the two worlds never share a registry.

### 10. The SSO mapping boundary: the edge maps an email once; the broker never sees one

Browser identity has two boundaries, and each maps exactly once.

At the web edge, the oauth2-proxy in front of `kanban serve` verifies the
Google session and applies its allowlist. That allowlist is where an email
lives, and it is consulted once, at session establishment, to decide
admission. The edge then forwards, over a proxy-only Unix socket that direct
HTTP and loopback clients cannot reach, exactly two values: the literal
provider `google` and the verifier's immutable OIDC `sub`, under ADR-033's byte
grammar `^[A-Za-z0-9._~-]{1,255}$`. It strips every caller-supplied identity
header first.

At the broker, the durable binding `(google, sub) -> p-*` was created once by
an administrator with `access map-sso`, using a broker-minted subject proof
(or, without one, by `breakglass map-sso`). Per request the broker resolves
that binding and mints a fresh `PrincipalContext`; it never sees, stores,
compares, or logs an email, a display name, a hosted domain, or a cookie. An
unmapped subject is denied. Event attribution is the mapped principal.

`--actor-header` is unchanged by this document and stays what
`rust/serve.rs` says it is: an edge label threaded into the audit actor of a
narrow write surface. Under managed enforcement it is recorded as
`claimedActor` on the access-audit row, beside the authenticated principal,
and it never resolves to a principal or affects authorization. It is not
retired because the label is useful in the audit row and because retiring it
would change a shipped surface for no security gain: it was never authority.

Why the subject and not the email. Emails change, are reassigned, and are
case- and provider-normalized inconsistently; `sub` is the one value Google
promises is stable and unique per account. Why the mapping is administrative
and not automatic: an automatic first-login mapping would let whoever the edge
admits mint their own principal, which is the caller-chosen identity ADR-033
exists to prevent. Why the broker never sees an email: a value that never
crosses the boundary cannot be logged, compared, or spoofed there.

### 11. The MCP-to-broker hop: ADR-011's stdio contract is unchanged, and this is why

An MCP client still spawns `kanban mcp`; the server still speaks
newline-delimited JSON-RPC over stdio, still generates its tool list from the
ADR-010 manifest, still spawns a real command process per `tools/call`, still
replaces itself in place between requests, and is still stdio-only. That
command process — not the MCP server — opens one short-lived connection to
the broker socket, negotiates the ADR-033 identities, and is authenticated by
its own `SO_PEERCRED` UID. `kanban mcp` never touches the socket and never
opens managed data.

The MCP server is therefore a client of the broker's clients, and not a policy
authority: it cannot assert an identity, forward one, or act "on behalf of"
anyone, because there is no field on the stdio pipe the broker would read as
authority. Stdin, the MCP parent's identity, tool arguments, JSON-RPC
metadata, `--as`, environment, and selectors carry none. A shared MCP service
account receives that Linux principal's grants and nothing more. The manifest's
`readOnly` flag remains what ADR-010 made it — a harness-side withholding a
client applies where it configures its tools — and the broker does not consult
it.

Why unchanged rather than amended to carry identity. First, stdio has no peer
credential, so the only way to "carry" identity on it is a bearer, and a
bearer is a caller-chosen identity. Second, the process boundary ADR-011 chose
for a different reason — one code path, one refusal — is already the place a
kernel identity exists: the spawned command's UID. Authenticating there costs
nothing new. Third, the in-place reload rests on the server holding nothing
between requests; a per-command broker connection that closes before the tool
result returns keeps that true, where a long-lived broker connection held by
the server would not. Fourth, ADR-011's "stdio only" decision refused a socket
transport because it would need "a listener, a port, an authentication story
and a lifetime nobody currently owns"; the broker socket is that story, owned,
but it is an internal authorization hop and not an MCP transport, and keeping
those two roles in two processes is what stops the refusal from being quietly
reversed.

### 12. The access-command grammar

The binary installs as both `kanban` and `kb`. Flags may appear in any order;
the grammar fixes the set, not the order. Every flag is single-valued except
`--scope` and `--replaces`, which are repeatable. Unknown flags, extra
positionals, short aliases, and positional identity values are refused
(ADR-008). Every `access` operation lists all three board selectors in
`ignoredSelectors`: it acts on the registry, never on a board.

```ebnf
access       = bin "access" ( bootstrap | principal | grant | revoke | sso
                            | explain | audit | breakglass | enforcement ) ;
bin          = "kanban" | "kb" ;
json         = [ "--json" ] ;
ctx          = "--as" ACTOR "--reason" TEXT ;
who          = "--principal" PRINCIPAL ;
pair         = "--username" USERNAME "--uid" UID ;
replaces     = { "--replaces" PRINCIPAL } ;
cap          = "--capability" CAPABILITY ;
scopes       = "--scope" SCOPE { "--scope" SCOPE } ;
google       = "--provider" "google" "--subject" SUBJECT ;

bootstrap    = "bootstrap" pair ctx "--confirm" "empty-policy" json ;

principal    = "principal" ( "bind"         pair replaces ctx json
                           | "prove-rebind" who pair replaces ctx json
                           | "rebind"       who pair replaces "--source-proof" REBIND_PROOF ctx json
                           | "disable"      who ctx json
                           | "show"         who json
                           | "list"         [ "--disabled" ] json ) ;

grant        = "grant"  who cap scopes ctx json ;
revoke       = "revoke" who cap scopes ctx json ;

sso          = "map-sso"   google "--subject-proof" SUBJECT_PROOF who ctx json
             | "unmap-sso" google ctx json ;

explain      = "explain" who cap scopes json ;
audit        = "audit" [ who ] [ "--actor-principal" PRINCIPAL ] [ "--kind" KIND ]
                       [ cap ] { "--scope" SCOPE } [ "--after-epoch" EPOCH ]
                       [ "--limit" LIMIT ] json ;

breakglass   = "breakglass" ( "principal-rebind" who pair replaces ctx glass
                            | "map-sso"          google who ctx glass
                            | "registry-admin"   who ctx glass ) ;
glass        = "--confirm" "root-breakglass" json ;

enforcement  = "enforcement" ( "show" json
                             | "prepare"  "--expected-epoch" EPOCH ctx "--confirm" "prepared" json
                             | "activate" "--expected-epoch" EPOCH "--prepare-receipt" RECEIPT
                                          ctx "--confirm" "no-direct-fallback" json ) ;

CAPABILITY   = "read" | "write" | "admin" ;
SCOPE        = "registry" | "board:" BOARD_ID | "tag:" TAG_SLUG | "*" ;
PRINCIPAL    = "p-" HEX8 ;
KIND         = (* one of the twenty-two event kinds listed in ADR-033 *) ;
EPOCH        = (* nonnegative integer *) ;
LIMIT        = (* integer 1..1000 *) ;
```

There is no bare `access show`. The read of a principal is `principal show`
(registry `read`); the read of effective authority is `explain` (registry
`admin`, so it is not an existence oracle); the read of enforcement is
`enforcement show` (registry `read`). Adding a fourth would be a second way to
say one of those three. The `--scope` set on `grant`, `revoke`, and `explain`
must normalize to exactly one valid tuple from clause 5; on `audit` it is a
filter that must itself form one valid tuple when present.

`BOARD_ID` is the registry's stable per-board identity, not the board's
display name. ADR-033 says "an unknown board ID" is refused and ADR-028 says
roots are not identity; a name is not identity either, because ADR-035 lets a
name be retired and a new board take it, and a grant keyed on the name would
follow it. The registry today exposes a board by `name` and `boardPath` only,
so this clause requires the registry to mint and expose an immutable
`boardID` for every board at registration or adoption (ADR-032 already
publishes adopted boards under a fresh UUID directory), to carry it in
`workspace list --json` and `board_seeded`, and never to reuse it after
retirement.

## Generated-schema contract

ADR-010: the schema follows the surface, as data, and an adapter or test reads
it rather than restating it. Storage column names are ADR-033's snake_case;
every `--json` projection below uses the camelCase and `…ID` casing this
binary already emits (`taskID`, `parentID`, `sessionID` in `rust/model.rs`).
Timestamps are Unix milliseconds, as everywhere else. Identifier prefixes are
`p-` for principals (fixed by ADR-033) and, proposed here and disjoint from
every board-row prefix in `rust/model.rs`: `pg-` grant, `pe-` policy event,
`pa-` access-audit event, `ps-` SSO mapping, `pf-` proof, `pc-` prepare
receipt, `rq-` request. The implementation may change a proposed prefix
without amending this document; it may not change a field name or nullability
below without a new policy-schema version.

### An `access` operation as `kanban schema --json` emits it

```json
{
  "name": "access grant",
  "command": "access",
  "subcommand": "grant",
  "longRunning": false,
  "flags": [
    { "name": "principal",  "kind": "value" },
    { "name": "capability", "kind": "value" },
    { "name": "scope",      "kind": "list"  },
    { "name": "as",         "kind": "value" },
    { "name": "reason",     "kind": "value" }
  ],
  "positionals": [],
  "readOnly": false,
  "createsBoard": false,
  "ignoredSelectors": ["db", "project", "workspace"]
}
```

`principal show`, `principal list`, `explain`, `audit`, and `enforcement show`
are `readOnly: true`; every other `access` operation is `readOnly: false`.

That classification needs one word of ADR-010 narrowed, and the narrowing is
named here rather than slipped in. ADR-010 says read-only means the operation
writes "not the board, not the registry, not a file". ADR-033 says every
brokered decision — a `task list` included — appends an access-audit row to
the registry. Read together literally, no brokered operation would be
read-only and the flag a harness uses to withhold mutation would say `false`
for everything, which is a flag that means nothing. This document reads
ADR-010's "registry" as the registry's *work state*: the audit row is the
broker's record of its own decision, written by the broker about the caller,
and the caller cannot cause, shape, or suppress it. A harness withholding
mutation wants to know what the caller can change, and the caller cannot
change an audit row. ADR-010's own E2E is unaffected: it measures the board
file's bytes, and a read-only operation still leaves them identical. Registry
recency touches remain outside the audit contract exactly as ADR-029 states,
and `store_path_readonly` keeps refusing to write them for a read.

### A principal, as `principal show --json` emits it

```json
{
  "id": "p-8a41d0c7",
  "username": "alice",
  "uid": 1001,
  "enabled": true,
  "boundAtEpoch": 3,
  "boundByEventID": "pe-00000003",
  "disabledAtEpoch": null,
  "disabledByEventID": null,
  "successorID": null,
  "predecessorID": "p-2b9e7710",
  "replaces": ["p-2b9e7710"],
  "grants": [ /* policy rows, below */ ],
  "ssoMappings": [
    { "id": "ps-c41d02aa", "provider": "google", "subject": "1083…", "mappedAtEpoch": 5, "mappedByEventID": "pe-00000005" }
  ]
}
```

### A policy row (grant), as `principal show --json` and `explain --json` emit it

```json
{
  "id": "pg-3f1c9a2e",
  "principalID": "p-8a41d0c7",
  "capability": "write",
  "scope": ["board:b-6f2e01aa", "tag:kanban"],
  "state": "active",
  "origin": "grant",
  "grantedAtEpoch": 7,
  "grantedByPrincipalID": "p-1c0ffee0",
  "grantedByEventID": "pe-00000007",
  "retiredAtEpoch": null,
  "retiredByEventID": null,
  "transferredFromGrantID": null
}
```

`scope` is the normalized tuple in canonical order (`registry` alone; else
`board:` first, then `tag:` or `*`). `state` is `active`, `revoked`, or
`retired` (superseded by a rebind transfer). `origin` is `bootstrap`,
`board_seed`, `grant`, `rebind_transfer`, or `breakglass_registry_admin`.
`grantedByPrincipalID` is null for `bootstrap` and `breakglass_registry_admin`
origins, whose events carry `actorUsername: "root"` instead.

### An `explain --json` receipt

```json
{
  "principalID": "p-8a41d0c7",
  "capability": "write",
  "requiredScopes": [["board:b-6f2e01aa", "tag:kanban"]],
  "policyEpoch": 9,
  "policyStateHash": "sha256:…",
  "enforcementState": "managed",
  "outcome": "allowed",
  "matchedGrantIDs": ["pg-3f1c9a2e"],
  "denialReason": null
}
```

`denialReason` is the generic `denied or not found` when `outcome` is
`denied`; the internal stage and code are on the audit row only.

### A policy event, as `access audit --json` emits it

```json
{
  "seq": 7,
  "id": "pe-00000007",
  "kind": "grant_added",
  "occurredAt": 1788940800000,
  "previousHash": "sha256:…",
  "eventHash": "sha256:…",
  "beforeEpoch": 6,
  "afterEpoch": 7,
  "accessAuditEventID": "pa-91be44f0",
  "actorPrincipalID": "p-1c0ffee0",
  "actorUsername": "geoyws",
  "actorUID": 1000,
  "context": {
    "authnKind": "socket_peer",
    "peerUID": 1000,
    "realUID": null,
    "effectiveUID": null,
    "clientKind": "cli",
    "requestID": "rq-5e0a7c3d",
    "claimedActor": "geoyws",
    "reason": "kanban lane needs write on :kanban",
    "provider": null,
    "subject": null
  },
  "targetPrincipalID": "p-8a41d0c7",
  "targetMappingID": null,
  "source": null,
  "successor": null,
  "delta": {
    "seededGrantIDs": [],
    "grantedGrantIDs": ["pg-3f1c9a2e"],
    "revokedGrantIDs": [],
    "activatedGrantIDs": [],
    "retiredGrantIDs": [],
    "mappedMappingIDs": [],
    "unmappedMappingIDs": []
  }
}
```

`source` and `successor` are non-null only on `principal_rebound` and
`breakglass_principal_rebound`, each carrying the complete frozen principal
value (`id`, `username`, `uid`) and, on the successor, `replaces`.
`actorPrincipalID` is null exactly on `bootstrap` and `breakglass_*` kinds,
where `actorUsername` is `root` and `actorUID` is `0`. `enforcement_prepared`
and `enforcement_activated` add an `enforcement` object: `fromState`,
`toState`, `expectedEpoch`, `manifestDigest`, `prepareReceiptID`, `brokerBinaryID`,
`brokerProtocolVersion`, `commandSchemaHash`, `policySchemaVersion`,
`preconditions` (ordered `{name, passed}`), and `clientProbes`
(`{clientKind, clientBinaryID, routeProbe, directRefusalProbe}`).

### A policy epoch row

```json
{
  "epoch": 7,
  "policyEventSeq": 7,
  "policyEventHash": "sha256:…",
  "previousStateHash": "sha256:…",
  "resultingStateHash": "sha256:…",
  "occurredAt": 1788940800000
}
```

### An access-audit event, as `access audit --json` emits it

```json
{
  "schemaVersion": 1,
  "seq": 4412,
  "id": "pa-91be44f0",
  "previousHash": "sha256:…",
  "eventHash": "sha256:…",
  "occurredAt": 1788940800000,

  "operation": "task move",
  "outcome": "denied",
  "decisionStage": "scope",
  "decisionCode": "missing_tuple",
  "policyEpoch": 9,
  "enforcementState": "managed",

  "actorPrincipalID": "p-8a41d0c7",
  "actorUsername": "alice",
  "actorUID": 1001,

  "authnKind": "socket_peer",
  "evidenceSource": "kernel_so_peercred",
  "peerPID": 48213,
  "peerUID": 1001,
  "peerGID": 1001,
  "realUID": null,
  "effectiveUID": null,
  "provider": null,
  "subject": null,
  "verifierID": null,
  "proxyRouteID": null,
  "sourceProofID": null,
  "subjectProofID": null,

  "requestID": "rq-5e0a7c3d",
  "clientKind": "mcp_command",
  "claimedActor": "claude@driver",
  "reason": null,
  "clientBinaryID": "sha256:…",
  "brokerBinaryID": "sha256:…",
  "brokerProtocolVersion": 1,
  "commandSchemaHash": "sha256:…",
  "policySchemaVersion": 1,
  "boardSchemaVersions": [{ "boardID": "b-6f2e01aa", "schemaVersion": 14 }],

  "requestedCapability": "write",
  "requiredScopes": [["board:b-6f2e01aa"], ["board:b-6f2e01aa", "tag:kanban"]],
  "matchedGrantIDs": ["pg-0a11ee42"],
  "visibleTargetIDs": ["t-37b81c83"],
  "redactedTargetDigests": []
}
```

Field groups and nullability follow ADR-033's version-1 matrix exactly:
`realUID` and `effectiveUID` are always null in version 1; `peer*` are
required once `SO_PEERCRED` succeeded; `provider`/`subject`/`verifierID`/
`proxyRouteID` are required for SSO-allowed rows and conditional for SSO-denied
rows by stage; proof IDs are null except on issuance, consumption, or a request
that presented that proof; `reason` and `claimedActor` are null only where the
grammar does not accept them; `boardSchemaVersions` is empty when no board was
opened. `authnKind` is `socket_peer`, `sso_proxy`, `root_bootstrap`, or
`root_breakglass`. `clientKind` is `cli`, `mcp_command`, `web`, `dispatcher`,
`adapter`, `backup`, `restore`, `archive`, or `search`. `decisionStage` is
`negotiation`, `peercred`, `principal`, `epoch`, `enforcement`, `scope`,
`nonEscalation`, `proof`, or `integrity`; `decisionCode` is a closed
implementation vocabulary under each stage. Both are visible only through the
administrator-gated `audit` command; the public refusal is `denied or not
found`.

### The `PrincipalContext`, as the audit row projects it

The type itself does not serialize (clause 3). This is the shape of the actor
and evidence block a contract test asserts on the access-audit row, which is
the only place a context is ever observable:

```json
{
  "principalID": "p-8a41d0c7",
  "username": "alice",
  "uid": 1001,
  "authnKind": "socket_peer",
  "evidenceSource": "kernel_so_peercred",
  "peerPID": 48213,
  "peerUID": 1001,
  "peerGID": 1001,
  "policyEpoch": 9,
  "policyStateHash": "sha256:…",
  "requestID": "rq-5e0a7c3d",
  "clientKind": "mcp_command"
}
```

Every field is set once at mint. There is no field a request can supply and
no field that changes after construction; `policyEpoch` and `policyStateHash`
are the values compared for equality at commit (clause 8).

### `doctor --json` gains a `policy` block

```json
{ "policy": { "epoch": 0, "enforcementState": "direct", "journalHead": null } }
```

Present on every registry, epoch 0 and `direct` on one that has never been
bootstrapped. It is how an operator reads pre-bootstrap state without holding a
grant (clause 6).

## Consequences

- Every decision ADR-033 states now carries the reason it was made, so an
  implementer or a reviewer can tell a deliberate constraint from an
  accidental one.
- The lattice is small enough to hold in one line — three levels, four tuple
  shapes, one cross-tuple rule, default `none` — and positive-only, so
  `explain` stays a lookup and non-escalation stays a comparison.
- A stale policy decision is impossible to commit: equality on epoch and state
  hash at commit closes the window between decision and write, including
  across a restore.
- A directly opened board and a brokered one can never be confused: the
  enforcement state is on every receipt and audit row, a direct open writes no
  access-audit row, and in `managed` state the direct open fails on `open(2)`.
- ADR-011's stdio contract, reload, and stdio-only decision stand unchanged;
  the broker socket is an internal hop owned by the spawned command process.
- The web edge keeps its email allowlist and `--actor-header` exactly as
  shipped; the broker gains a subject-keyed mapping and never learns an email.
- Root's two roles — seeding an empty registry and repairing a live one — stay
  distinct commands with distinct confirm literals, and neither becomes a row.
- The registry must mint and expose an immutable `boardID`; scope atoms key on
  it, not on the name. This is new registry surface.
- `schema --json` grows the `access` operations; `doctor --json` grows a
  `policy` block; the `/kb` skill and the MCP tool list inherit both from the
  manifest, not from a restatement.

## Implementation and evidence status

This document is design and generated-schema contract only. It implements no
broker, mints no principal, changes no file ownership, enforces no policy,
alters no proxy, and provides no runtime, integration, E2E, or live-tier
evidence. The `access` operations are not in `COMMANDS` and `doctor --json`
has no `policy` block at commit 53e6ed1.

Implementation inherits ADR-033's evidence list unchanged and adds: a
generated-schema contract test that the `access` entries of `schema --json`
match clause 12 flag-for-flag and kind-for-kind; unit coverage that the lattice
of clause 5 is positive-only and that the wildcard satisfies exactly the tag
tuples of its own board; a compiled-binary E2E in `tests/e2e.rs` on Linux
(ADR-006) that a context minted at epoch `N` is refused after a grant lands at
`N+1`, that `bootstrap` refuses at epoch 1 and every `breakglass` refuses at
epoch 0, and that a `--db` open in `direct` state writes no `access_audit` row
while the same command in `managed` state fails before `open(2)` returns.
Those layers keep their exact names; none is E2E unless it crosses the real
binary process boundary, and there is no live acceptance until George accepts
this document and the enforcement cutover runs.

## References

Measured 2026-09-05 at commit 53e6ed1. The authoring tree's realpath was
`/Users/geoyws/work/src/.kanban-worktrees/kanban-t-37b81c83-w11-8727`; every
path below is relative to that tree, and every line number is as of that
commit.

- `docs/adr/ADR-033-principals-are-frozen-username-plus-uid-and-minted-through-a-peer-credential-broker.md` — the accepted contract this document extends: broker ownership and `SO_PEERCRED` (lines 23–57), frozen principals (59–89), sealed context (91–103), CLI and MCP as clients (105–123), enforcement states (125–221), negotiation (223–248), capabilities and tuples (250–295), preimage authorization (297–315), grammar and event kinds (317–450), journals and audit shape (452–557), bootstrap and break-glass (559–604), SSO boundary (606–629)
- `docs/adr/ADR-011-in-binary-mcp-server-and-in-place-reload.md` — the stdio contract left unchanged: tool call runs the binary (31–63), the 2026-09-03 amendment (37–46), in-place reload conditions (65–105), stdio-only decision (135–151)
- `docs/adr/ADR-010-adapters-generated-from-the-command-surface.md` — schema follows the surface; `readOnly` strict reading (44–55)
- `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — refusals name their fix; the `--db` re-permission incident (37–41)
- `docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md` — the chain the policy journals join; root outside the threat model (11, 54); restore and anchors (38–42)
- `docs/adr/ADR-027-rules-are-one-tag-scoped-kb-document.md` — registry-owned rules and `ONLY:<board>` selectors as the precedent for board-scoped registry rows
- `docs/adr/ADR-028-roots-are-hints-not-board-identity.md` — roots are not identity, cited by ADR-033 for `--db` as addressing
- `docs/adr/ADR-032-workspace-adopt-copies-boards-into-registry-owned-storage.md` — adopted boards published under a fresh UUID directory (56), the basis for a minted `boardID`
- `docs/adr/ADR-035-workspace-retire-and-unretire.md` — a retired name can be taken by a new board, the reason scope atoms do not key on names
- `docs/adr/ADR-036-the-kb-skill-is-a-pinned-submodule-of-the-public-package.md` and `docs/adr/ADR-037-truncated-listings-refuse-a-default-limit-they-exceed.md` — house style
- `docs/adr/ADR-006-rust-runtime-and-compiled-binary-e2e.md` — release-gate evidence crosses the binary boundary
- `rust/serve.rs` — module comment on the edge trust boundary (1–29), `MAX_ACTOR_BYTES` (64), `ServeConfig::actor_for_write` (84–89), `configured_actor` (367–380), `normalize_actor_header_name` (382–390)
- `rust/model.rs` — `OPERATOR_ACTOR = "geoyws"` (651), camelCase and `…ID` JSON casing (`Task` 331–338, `Claim` 362–370), `ProjectRecord` with `name` and `boardPath` and no board ID (549–563)
- `rust/lib.rs` — `GLOBAL_FLAGS` and `BOARD_SELECTORS` (280–289), selector table with `ignoredSelectors` (337–430), `schema()` manifest shape (1665–1718), `store_path` direct open of `--db` (1937–1989)
- `rust/store.rs` — `Store::open` opens the board file under the caller's UID (1685)
- `rust/registry.rs` — `data_root` and `KANBAN_DATA_DIR` (385–389), `Registry::open` (1675)
- Kanban board `kanban` rules table of contents, read 2026-09-05 via `kb-host hax r ls --json`: r-cf2b2b9f (release gates cross the binary boundary), g-ffbd95f5 (name test layers honestly), g-bab3d977 (verify before claiming), r-98df1376 (actor, board, and typed IDs stay separate identities)
- Kanban board: epic e-b098ffe7; task t-37b81c83 (this decision)
