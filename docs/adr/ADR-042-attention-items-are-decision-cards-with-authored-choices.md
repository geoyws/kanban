# ADR-042: An attention item is a decision card with authored choices

**Status:** Accepted
**Date:** 2026-09-08
**Amended:** 2026-09-11 (§5's undo copy: the receipt now offers a web Undo; see ADR-016's 2026-09-11 amendment)
He named the shape of the problem three times in one hour — "kanban needs to
have multiple choice questions about what to do, not just approve or reject,
with one additional choice being the free text choice that also allows for
approve/reject/etc."; "this is so that kb.geoy.ws can easily let me make
decisions quickly"; and "the language has to be clear to give me a good context
to make my decision". He ruled on the shape, not on field names, storage,
composition or refusal text; those are decided here and he may supersede any of
them by a later ADR.
**Supersedes:** nothing.
[ADR-012](ADR-012-session-handoffs-and-durable-attention.md) (attention is
durable and only the owner retires it) and
[ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md) (the UI has exactly two
write verbs) stay in force unchanged; this ADR changes the *shape* of one of
those verbs and adds no third.

## Context

**Citation convention.** A bare `<file>:NNN` below is a path under the
repository root, read at commit `1d8d069` on `kanban-geoyws-driver`. Every claim
about current behaviour cites the line that has it.

An attention row today is prose plus a state machine. The stored record is
`body`, `kind`, `raised_by`, `priority`, `status`, and — once settled — a single
free-text `resolution` (`rust/model.rs:822`–`rust/model.rs:841`,
`rust/db.rs:747`–`rust/db.rs:769`). Nothing on the row says what the choices
*are*.

So the choices live in prose, and every consumer parses them back out. The `kb`
skill tells a raiser to write a "verdict-first, ≤2 sentences, with the concrete
next action" body (`skills/kb/SKILL.md:259`). The `kb-att` walkthrough then asks
a clerk session to read those bodies and reconstruct "the actual choices,
recommended first" from them
(`/Users/geoyws/.agents/skills/kb-att/SKILL.md:110`), and its draft classifier
is a 81-line jq program whose whole job is regex-matching English dependency
clauses — `gate`, `blocked by`, `depends on`, `wait`, `until`, `promote when` —
out of body text
(`/Users/geoyws/.agents/skills/kb-att/scripts/lane-draft-classify.jq:28`–`:34`).
That is a parser for a language nobody defined.

The web card offers exactly three buttons and all three resolve. `Approve` and
`Reject` write two fixed sentences — `Decision: Approved. Proceed.` and
`Decision: Declined. Do not proceed.` (`rust/serve.rs:1282`,
`rust/serve.rs:1286`) — and `Comment and Resolve` writes `Comment: <text>` with
no verdict at all (`rust/serve.rs:1293`, label at `rust/serve.rs:77`, rendered
at `rust/serve.rs:1445`). All three land through the same call
(`rust/serve.rs:584`). The card itself shows the priority, the kind, the raiser,
the age and then the whole body (`rust/serve.rs:1421`–`rust/serve.rs:1433`), and
the two quick buttons relabel themselves to "Comment and Approve" / "Comment and
Reject" the moment the textarea has text (`rust/serve.rs:1446`–`:1449`,
`rust/serve.rs:2767`–`rust/serve.rs:2774`).

Put together: geoyws reads a wall of text, reconstructs the options in his head,
and then either presses a button whose meaning is one of two fixed sentences, or
types a comment that closes the item with **no machine-readable verdict at
all**. A lane reading the trail afterwards gets `Comment: do it after the pin
lands` and has to guess whether that was a yes.

The volume makes this the bottleneck rather than an annoyance. Measured
2026-09-08 with `attention list --status open --limit 500` per board: **133 open
items across seven boards** — kanban 6, geoyws 6, dotfiles 21, hax 17, atmux 37,
unum 3, px 43. At one wall of prose apiece, "decide quickly" is not available at
any speed of reading.

## Decision

An attention item is a **decision card**: a question, the context needed to
answer it, and two to four authored choices with a marked recommendation, plus
an always-present custom answer that still carries a verdict. The body stays
exactly where it is and keeps doing exactly what it does — it is the long form,
and the card is what geoyws reads first.

### 1. The card

Four authored parts, all optional on the row and all bounded:

| part | shape | bound | rule |
| --- | --- | --- | --- |
| `question` | one sentence, ends in `?` | ≤ 160 characters | what geoyws is deciding, in his terms |
| `context` | 2–5 plain sentences | ≤ 800 characters | what is true now, what is blocked, what waiting costs |
| `choices` | 2–4 objects | see below | exactly one carries `recommended: true` |
| `custom` | implicit, never stored | — | free text plus an outcome; always offered |

A choice is:

```json
{
  "key": "assign-and-login",
  "label": "Assign a Claude seat to hax and log in",
  "consequence": "You buy or free one Claude seat, ssh to hax and finish the browser login: about ten minutes of your time plus the seat's monthly cost, and the receipt is retried the same day.",
  "outcome": "approve",
  "recommended": true
}
```

- `key` — a slug, `[a-z0-9][a-z0-9-]{0,31}`, unique within the item. It is what
  the CLI, the MCP tool, the form POST and the ledger all name, so it must be
  typeable and stable; it is never shown to geoyws.
- `label` — a verb phrase, ≤ 60 characters, no `|` (see §4 for why). It is the
  button text.
- `consequence` — one sentence, ≤ 200 characters: **what happens if this is
  picked, including its cost or risk**. Required on every authored choice.
- `outcome` — one of `approve`, `reject`, `defer`, `other`. This is the
  machine-readable verdict a lane acts on; the label is for the human.
- `recommended` — `true` on exactly one choice, absent or `false` on the rest.

`ATTENTION_OUTCOMES: [&str; 4] = ["approve", "reject", "defer", "other"]` goes in
`rust/model.rs` beside `ATTENTION_KINDS` (`rust/model.rs:814`) and
`ATTENTION_STATUSES` (`rust/model.rs:818`), for the same reason those are there:
a closed set the schema publishes rather than a convention.

**The custom choice is implicit and reserved.** Every item offers it; it is
never stored in `choices`, and `custom` is a refused key for an authored choice
(§4). Choosing it requires **both** a note and an explicit `outcome` from the
same four values — that is the whole of geoyws's "one additional choice being
the free text choice that **also allows for approve/reject/etc.**". A free-text
answer with no verdict is the thing this ADR exists to remove.

**Every choice resolves the row, `defer` included.** A deferral is a verdict —
"not now, and here is why" — and leaving the row open with a decision attached
would put a decided item back in a list whose entire purpose is undecided items.
`reopen` (`rust/store.rs:5228`) is the way back. The corollary is a writing rule,
not a machine rule, and it is in §7: a `defer` consequence must name what brings
the question back, because "later" with no trigger is how a row is deferred into
oblivion.

**The body is untouched.** `body` stays required and stays the long form:
receipts, SHA256s, paths, the `RESOLVE-WHEN` line. The card is the top of the
item and the body is folded beneath it (§6). Nothing in this ADR rewrites a
body, and the Phase 4 backfill (§8) explicitly does not.

### 2. Storage: four columns on the attention row, schema 24 → 25

`BOARD_V25`, appended to `BOARD_MIGRATIONS` (`rust/db.rs:2178`–`rust/db.rs:2182`),
with `BOARD_SCHEMA_VERSION` moving 24 → 25 (`rust/db.rs:1722`). The two stay
equal because `every_board_migration_is_in_the_ladder` asserts it
(`rust/db.rs:2874`, assertion at `rust/db.rs:2901`), and `open_board` refuses a
file whose `user_version` is not the ladder's length (`rust/db.rs:2322`).

```sql
ALTER TABLE attention ADD COLUMN question TEXT
  CHECK(question IS NULL OR length(question) BETWEEN 1 AND 160);
ALTER TABLE attention ADD COLUMN context TEXT
  CHECK(context IS NULL OR length(context) BETWEEN 1 AND 800);
ALTER TABLE attention ADD COLUMN choices TEXT
  CHECK(choices IS NULL OR (json_valid(choices) AND json_type(choices)='array'
        AND json_array_length(choices) BETWEEN 2 AND 4));
ALTER TABLE attention ADD COLUMN decision TEXT
  CHECK(decision IS NULL OR (json_valid(decision) AND json_type(decision)='object'));
```

**Columns on the row, not a new table.** The array is at most four objects, is
always read with its row, is never queried across rows and is never joined. A
child table would add a write per raise, a delete per archive, and a join to
every listing — the cost `attach_attention_tags` (`rust/store.rs:428`) already
pays for tags, which unlike choices *are* cross-row queryable
(`rust/store.rs:5049`). There is direct precedent for a JSON array in a column
with a `json_valid` CHECK: `rules.task_tags` (`rust/db.rs:728`–`rust/db.rs:729`).

**`question` and `context` are plain TEXT, not keys inside one `card` blob.**
Two reasons. `question` is what the "Needs you" list renders and what search must
index, and the search view is plain column concatenation
(`rust/db.rs:535`–`rust/db.rs:537`) — a blob would put `json_extract` into a view
that has none. And a length bound is expressible as a CHECK on a scalar column
and is not expressible on a key inside a JSON document without `json_extract` in
the CHECK.

**`ALTER TABLE ADD COLUMN` rather than a table rebuild.** `attention` was rebuilt
once already, in `BOARD_V17` (`rust/db.rs:741`–`rust/db.rs:794`), to widen a
CHECK that could not be altered in place; that migration also has to drop and
recreate three search triggers (`rust/db.rs:742`–`rust/db.rs:744`,
`rust/db.rs:782`). Four added columns need none of that. The price is that
cross-field invariants — the question/context pairing, exactly-one-recommended,
key uniqueness, the reserved `custom`, a consequence per choice — cannot be
expressed as column CHECKs. **They are enforced in the store, and only in the
store**, deliberately: a CHECK failure says `CHECK constraint failed`, and
ADR-008 requires a refusal that names the fix. A rule half-enforced by the schema
reads as though the schema were authoritative when it is not.

**Search.** `BOARD_V25` also redefines `search_source_rows` so the attention arm
indexes the card. Today that arm is
`a.raised_by || char(10) || a.body || char(10) || COALESCE(a.resolution,'')`
(`rust/db.rs:537`); it gains `question`, `context` and the choices' labels and
consequences. The three triggers reference the view by name
(`rust/db.rs:782`–`rust/db.rs:793`) and are unchanged. Existing
`search_documents` rows carry the old text until the row is next written, so
Phase 2 runs `kanban search-rebuild` in its migration acceptance case and the
Phase 5 release runs it per board. This is what makes the kb-att precedent grep
("has geoyws already answered this class of question") find a decision by its
*choice text* and not only by its body.

**Existing items read as the default pair.** A row with `choices IS NULL` is
served — by the CLI, the MCP tool and the web — as if it carried:

```json
[
  {"key": "approve", "label": "Approve - proceed",
   "consequence": "The work the body describes goes ahead as written.",
   "outcome": "approve"},
  {"key": "reject",  "label": "Reject - do not proceed",
   "consequence": "The work the body describes does not happen; whoever raised it needs a new plan.",
   "outcome": "reject"}
]
```

with **no recommendation** — nobody authored this pair, so nothing may be
marked — and with no `question` and no `context`: the body serves as both. This
is the only place in the model where zero recommendations is legal, and it is
legal precisely because the pair is synthesized rather than chosen. The
materialization happens on read, in one function beside `attention_row`
(`rust/store.rs:1037`), so `NULL` stays `NULL` in storage and a backfilled row is
distinguishable from a never-authored one for as long as any remain.

Labels are ASCII (`Approve - proceed`, not an em dash) because they travel
through argv, a form POST and a terminal.

### 3. The decision record, and why it is not `decisions[]`

Resolving writes a `decision` object on the row:

```json
{"choice": "keep-parked", "outcome": "defer", "note": null,
 "by": "geoyws", "at": 1788805112431}
```

`choice` is the key, or the literal `custom`. `note` is `null` when an authored
choice was taken with no note, and required for `custom`. `by` is the audit
actor and `at` is `now_ms()` — the same two values the row's `resolved_by` and
`resolved_at` already take (`rust/store.rs:5200`).

The same object goes into the `attention_resolved` event's payload
(`rust/store.rs:5205`–`rust/store.rs:5216`), beside the `previousResolvedAt` /
`previousResolvedBy` / `previousResolution` keys already there
(`rust/store.rs:5210`–`rust/store.rs:5212`), and gains a `previousDecision`
sibling. `attention_reopened`'s payload
(`rust/store.rs:5257`–`rust/store.rs:5265`), which already carries the
`resolution` it is undoing (`rust/store.rs:5263`), gains the `decision` it is
undoing.

**`resolution` stays, and is composed rather than passed.** The composition is:

```
Decision: <label>. <consequence>
Note: <note>                          ← only when a note was given
```

and for the custom answer:

```
Decision: Custom answer, recorded as <outcome>.
Note: <note>                          ← always present; custom requires it
```

**The composer moves inside `resolve_attention_with_authorization`**
(`rust/store.rs:5154`), and the store's parameter changes from
`resolution: Option<&str>` to the decision. Today `serve.rs` composes its own
text in `compose_resolution_note` (`rust/serve.rs:1278`) and the CLI passes
`--note` straight through (`rust/lib.rs:6625`), which is two composers for one
string. One composer inside the write path means no caller can produce a
different trail, and `resolution` becomes derived state rather than an
independent input. `compose_resolution_note` and `reply_button_labels`
(`rust/serve.rs:1270`) are deleted.

**What changes in the trail, named.** The second line becomes `Note:` where the
web wrote `Comment:` (`rust/serve.rs:1282`, `rust/serve.rs:1293`), because the
flag is `--note`, the store's own refusal calls it a note
(`rust/store.rs:5192`), and kb-att's helper is `resolve a-id "note"`. The default
pair's sentence becomes `Decision: Approve - proceed. The work the body
describes goes ahead as written.` where the web wrote `Decision: Approved.
Proceed.`. **No historical resolution is rewritten** — every settled row keeps
its exact bytes, the way pre-2026-09-05 rows keep `resolvedBy: geo`. What is
preserved is the shape: a resolution still begins `Decision: ` and a second line
still names the free text, so a precedent grep over the resolved trail keeps
working. Anything grepping the literal `Comment: ` finds pre-2026-09-08 rows only,
and that is stated here rather than discovered.

**The resolve gate is unchanged.** Only `geoyws` (`rust/model.rs:807`) or the
raiser may resolve (`rust/store.rs:5181`–`rust/store.rs:5190`); a second resolve
of a settled row is refused (`rust/store.rs:5175`–`rust/store.rs:5180`); the web
edge keeps its own authorization path
(`resolve_attention_from_trusted_edge`, `rust/store.rs:5145`, called at
`rust/serve.rs:584`) with the actor still defaulting to `OPERATOR_ACTOR`
(`rust/serve.rs:96`–`rust/serve.rs:98`). Reopen keeps its gate too
(`rust/store.rs:5243`).

**Reopen keeps the previous decision as ledger history, not as a `decisions[]`
array on the row.** This was the open question; it is decided against the array,
for four reasons in order of weight.

1. **The ledger already holds it, hash-chained.** Every resolution's text and
   resolver are already preserved across a reopen–reresolve cycle by the two
   event payloads above (`rust/store.rs:5210`–`rust/store.rs:5212`,
   `rust/store.rs:5261`–`rust/store.rs:5263`). Adding `decisions[]` would make
   the row a second, *unchained* copy of a record ADR-029 deliberately put behind
   a hash chain. Two copies of a history drift, and the one that drifts is the
   one nothing verifies.
2. **The row's contract is "what is true now".** Nothing else on `attention`
   accumulates: there is one `resolution`, one `resolved_by`, one `reopen_note`
   (`rust/model.rs:835`–`rust/model.rs:838`), and `BOARD_V17`'s CHECK is written
   around exactly one settlement plus one reopen
   (`rust/db.rs:759`–`rust/db.rs:768`). An array would be the first place in this
   schema where a row carries its own history, and it would need its own
   growth bound, its own ordering rule and its own archive behaviour.
3. **Nothing reads it.** The web card shows the open question, not its past
   answers. The CLI's `attention list` is a queue view. The one consumer that
   wants prior decisions is precedent hunting, which is a *cross-row* search
   over resolved items — `kb ev`, `context` and the search index, all of which
   read the ledger.
4. **It would grow the listing.** `attention list` already ships bodies for up
   to 500 rows in the kb-att digest; an unbounded per-row decision history rides
   along on every one of them.

**The cost, stated because it is the reason someone would want the array.** A
reader who wants "what did geoyws decide the *first* time" must read the event
trail rather than the row: one extra call (`kanban events --task <id>` or
`kanban context`). That is accepted. If it turns out to be a daily need, the
answer is a read-only projection that assembles the history from the ledger — not
a second copy on the row.

**"Comment and Resolve" is removed.** The constant (`rust/serve.rs:77`), its
rendered button (`rust/serve.rs:1445`), the `reply` decision value and its
default (`rust/serve.rs:553`, `rust/serve.rs:559`), and the `"reply"` arm of the
composer (`rust/serve.rs:1289`–`rust/serve.rs:1294`) all go. A custom answer
carries an outcome, so after this ADR **nothing closes an attention item without
a verdict** — which is the single sentence this whole design has to be able to
say.

### 4. CLI: the flags, and every refusal by name

```bash
kanban attention raise "<body>" --as AGENT [--kind K] [--task T] [--priority N] [--tag T]... \
  --question TEXT --context TEXT \
  --choice KEY=LABEL|OUTCOME  (repeatable, 2–4) \
  --consequence KEY=TEXT      (repeatable, one per choice) \
  --recommend KEY

kanban attention update ID --as AGENT [--body TEXT|--body-file P] [--tag T|--clear-tags] \
  [--question TEXT] [--context TEXT] [--choice …]... [--consequence …]... [--recommend KEY] \
  [--clear-card]

kanban attention resolve ID --as geoyws --choice KEY [--note TEXT]
kanban attention resolve ID --as geoyws --choice custom --outcome OUTCOME --note TEXT
```

**`--choice KEY=LABEL|OUTCOME` is one token, not three parallel flags.** Three
repeatables (`--key`, `--label`, `--outcome`) can arrive in mismatched counts and
there is no way to bind the third `--label` to the second `--key`; one token binds
them by construction. The value is split on its **first** `=` and its **last**
`|`, which is why a label may not contain `|` and may contain `=`. Note that
`--choice=a=b|approve` already works: the argument parser splits an inline value
on the first `=` of the whole token (`rust/lib.rs:1723`–`rust/lib.rs:1724`), so
the flag name is `choice` and the value is `a=b|approve`.

**`--consequence KEY=TEXT` is separate** because a consequence is a sentence that
will contain `|`, `=`, commas and colons; the `KEY=` prefix binds it to its
choice without an escaping rule.

**`--clear-card`, not `--clear-choices`.** Question, context, choices and
recommendation are one card, and clearing half of it leaves the pairing rule
below violated. One flag clears all four columns and returns the item to the
default pair, mirroring `--clear-tags` (`rust/lib.rs:1099`).

**`--choice` is required on `resolve`, and `--note` becomes conditional.** Today
`--note` is required (`rust/store.rs:5191`–`rust/store.rs:5195`) and there is no
`--choice` (`rust/lib.rs:1106`, `rust/lib.rs:6625`). After this ADR: `--choice` is
always required, `--note` is optional when the choice is an authored key (the
label and consequence are the auditable record), and required for `custom`. This
is a clean cutover with exactly two callers — the web edge, rewritten in Phase 3,
and kb-att's `resolve` helper
(`/Users/geoyws/.agents/skills/kb-att/SKILL.md:196`), rewritten in Phase 4 — and
the Phase 4 backfill touches every open row anyway.

**COMMANDS rows** (`rust/lib.rs:1080`–`rust/lib.rs:1110`) become:

```rust
("attention", Some("raise"),
 &["as","kind","task","priority","tag","question","context","choice","consequence","recommend"],
 &["text"], false),
("attention", Some("update"),
 &["as","body","body-file","tag","clear-tags","question","context","choice","consequence",
   "recommend","clear-card"],
 &["id"], false),
("attention", Some("resolve"), &["as","note","choice","outcome"], &["id"], false),
```

`outcome` joins `ENUM_ARGUMENTS` (`rust/lib.rs:1394`) as a flag of
`attention resolve` with `values: &ATTENTION_OUTCOMES`, so `schema --json`
publishes the four values (`rust/lib.rs:2363`–`rust/lib.rs:2366`) and the MCP
tool gets an enum. `clear-card` joins `BOOLEAN` (`rust/lib.rs:272`).

**The repeatable predicate becomes subcommand-aware.** `choice` and
`consequence` repeat on `attention raise` and `attention update` and must **not**
repeat on `attention resolve`, where a second `--choice` is two answers to one
question. The existing machinery cannot express that: `REPEATABLE` is global
(`rust/lib.rs:316`) and the three exceptions are keyed on the *command* only
(`rust/lib.rs:2352`–`rust/lib.rs:2355`, mirrored at `rust/mcp.rs:277`–`:278` and
`rust/mcp.rs:376`–`rust/mcp.rs:377`), and `attention raise` and `attention
resolve` share the command `attention`. Phase 2 replaces those four call sites
with one `repeatable(command, sub, flag)` predicate; the existing `watch`,
`subscription` and `access` sets become entries in it with `sub: None`. The
alternative — two names for one concept, `--choice` on raise and `--pick` on
resolve — is what ADR-041 §1 already refused, and `schema --json` publishing
`kind: "list"` for a flag one operation takes exactly once is a lie the generated
adapters would inherit. `every_list_valued_flag_is_declared_repeatable`
(`rust/lib.rs:8197`, assertion at `rust/lib.rs:8216`) is the drift guard and
extends to the new predicate.

**Every refusal, named.** All are store-level so the CLI, MCP and web share them,
and each names the fix, per ADR-008:

| # | condition | refusal |
| --- | --- | --- |
| 1 | `--consequence` or `--recommend` names a key no `--choice` declared | `attention: no choice named <key>; declared keys are <k1>, <k2>` |
| 2 | two `--choice` share a key | `attention: choice key <key> is given twice; keys must be unique within an item` |
| 3 | any choice given and the count is not 2–4 | `attention: an item carries 2 to 4 choices; <n> were given` |
| 4 | choices given and `--recommend` names two, or none | `attention: exactly one choice is recommended; <n> were` |
| 5 | a declared choice has no `--consequence` | `attention: choice <key> has no --consequence; every choice must say what happens if it is picked` |
| 6 | `--question` without `--context`, or the reverse | `attention: --question and --context are one card; give both or neither` |
| 7 | `--choice custom` on resolve without `--outcome`, or without `--note` | `attention: a custom answer needs --outcome (approve, reject, defer, other) and --note` |
| 8 | any card flag on a resolved item | `attention <id> is resolved history; its card cannot be rewritten` (widened from `rust/store.rs:5093`, which today says "its tags") |
| 9 | `--choice custom=…` on raise or update | `attention: custom is reserved for the free-text answer and cannot be a choice key` |
| 10 | `--outcome` on resolve without `--choice custom` | `attention: --outcome applies only to --choice custom; an authored choice carries its own outcome` |
| 11 | `--recommend` with no `--choice` | `attention: --recommend needs choices; give --choice or drop it` |
| 12 | label over 60 chars or containing `|`; consequence over 200; question over 160; context over 800; key not `[a-z0-9][a-z0-9-]{0,31}` | each names the field, the bound and the length or character found |
| 13 | resolve `--choice KEY` naming a key the row does not carry | `attention <id> has no choice <key>; its choices are <k1>, <k2>` |
| 14 | resolve with no `--choice` | `attention resolve requires --choice KEY or --choice custom --outcome X --note TEXT` |

Refusal 13 is the one that makes a stale card safe: if geoyws's browser is
holding a card that a later `attention update` replaced, the click names a key
that no longer exists and is refused by name rather than mapped to whatever now
sits in that position.

**MCP mirrors the flags one to one with no hand-written tool.** `tools()` builds
every tool from `COMMANDS` (`rust/mcp.rs:243`), the flag kind comes from the same
repeatable predicate (`rust/mcp.rs:277`), `readOnlyHint` from the same row
(`rust/mcp.rs:317`), and `arguments_for` coerces the object into the argv the CLI
would have taken, arrays included (`rust/mcp.rs:327`, `rust/mcp.rs:376`). So
`attention_raise` takes `{"choice": ["k=L|approve", …], "consequence": ["k=…"],
"recommend": "k", "question": …, "context": …}` with no new code, which is
ADR-010's whole point.

**`kanban schema` lists them** because it is generated from the same table
(`rust/lib.rs:2333`), and `ATTENTION_FIELDS` (`rust/lib.rs:3747`) grows from 17
to 21 with `question`, `context`, `choices`, `decision`, guarded by
`the_field_lists_name_exactly_the_keys_a_row_carries` (`rust/lib.rs:8722`,
assertion at `rust/lib.rs:8746`).

**The skill's table is guarded by the alias-drift test.** `skills/kb/SKILL.md` is
read back into the test binary with
`include_str!("../skills/kb/SKILL.md")` in
`the_skill_documents_aliases_that_actually_resolve` (`rust/lib.rs:8120`, the
include at `rust/lib.rs:8126`), which walks the skill's four-cell tables and
fails on a documented command that does not resolve — the `att`/`attn` row it
checks is `skills/kb/SKILL.md:136`. That is the named guard for the Attention
section's new flag table (`skills/kb/SKILL.md:250`–`skills/kb/SKILL.md:311`):
Phase 4 must write the flags into that section, and a flag documented for a
command that does not accept it fails there rather than in a user's terminal.

### 5. The web card

Order on the card, top to bottom:

1. **The question**, as the card's heading. This replaces the body as the first
   thing rendered (`rust/serve.rs:1433`).
2. **The context**, one paragraph.
3. **The recommended choice**, first and visibly marked (`Recommended`), as a
   one-click button carrying its label; its consequence sits beneath it.
4. **The alternatives**, in the order the raiser declared them, same shape.
5. **The custom answer**: an outcome picker over the four values plus the
   existing textarea, still bounded by `MAX_REPLY_BYTES` (`rust/serve.rs:74`,
   enforced at `rust/serve.rs:527`).
6. **The body**, folded into a closed `<details>`. It is one click away, never
   gone.
7. The existing meta line — priority, kind, board, raiser, age, tags
   (`rust/serve.rs:1421`–`rust/serve.rs:1432`) — stays, and the `about <task>`
   line with it (`rust/serve.rs:1434`–`rust/serve.rs:1439`).

**One click resolves. No confirmation dialog.** A confirmation would double every
decision's cost, which is the opposite of the ask, and the undo already exists
and is audited: `reopen` (`rust/store.rs:5228`), gated to geoyws or the resolver
(`rust/store.rs:5243`), preserving the resolution it undoes
(`rust/store.rs:5263`). The card names reopen as the undo in one line of copy so
the absence of a confirmation is a stated property rather than an oversight.

**Amended 2026-09-11.** The card still names reopen as the undo, but it no
longer quotes a CLI command: the receipt carries an Undo button, `u` reopens
the newest decision on the page, and `/decided` lists recent decisions with
an undo per row. Same gates, same store operation; the web shape is
ADR-016's 2026-09-11 amendment.

**Keyboard: `1`–`4` pick the nth listed choice, `c` focuses the custom
textarea.** Because the recommendation is always listed first, `1` is always the
recommendation — that is the muscle memory that makes 133 items tractable. Keys
are inert while the textarea has focus.

**The "Needs you" list shows `question` plus the recommended choice's label**,
where it shows the whole body today (`rust/serve.rs:1433`). For an item with no
authored card it shows the body's first line and the default pair, unchanged in
substance.

**Deletions.** `reply_button_labels` (`rust/serve.rs:1270`),
`compose_resolution_note` (`rust/serve.rs:1278`), `COMMENT_RESOLVE_LABEL`
(`rust/serve.rs:77`) and its unit assertion (`rust/serve.rs:3258`), the
`data-empty-label`/`data-comment-label` swap
(`rust/serve.rs:1446`–`rust/serve.rs:1449`) and `bindQuickReplies`'s relabelling
(`rust/serve.rs:2760`, `rust/serve.rs:2770`). The buttons no longer change
meaning when the textarea has text, because they are choices rather than comment
modifiers. `bindQuickReplies`'s other job — blocking the live swap while a draft
reply is being typed (`rust/serve.rs:2759`, `rust/serve.rs:2778`) — is kept and
extended to the custom textarea.

**Write scope is unchanged.** ADR-016's two verbs stay two verbs; this changes
the shape of the resolve verb's payload from `{decision, reply}` to
`{choice, outcome?, note?}` and adds nothing. Origin-equals-Host, the strict form
decoding and the trusted-edge actor rule
(`rust/serve.rs:96`–`rust/serve.rs:101`, `rust/serve.rs:584`) are untouched.

### 6. Acceptance for Phase 3: two real-Chrome cases

Both are compiled-process cases in `tests/e2e.rs` against a spawned `kanban
serve`, driven through a real Chrome tab in the style of
`needs_you_comment_buttons_and_resolve_flow_work_in_real_chrome`
(`tests/e2e.rs:22503`) and `assert_reply_recorded` (`tests/e2e.rs:676`), with the
actor header configured as the existing browser tests do:

1. `a_recommended_choice_resolves_in_one_click_in_real_chrome_and_records_its_outcome`
   — an item raised with three choices renders the recommendation first and
   marked, one click on it redirects to `/?replied=<id>`, and the row afterwards
   reads `status: resolved`, `decision.choice` the key, `decision.outcome` its
   outcome, `decision.by` the header actor, and `resolution` exactly
   `Decision: <label>. <consequence>`.
2. `a_custom_answer_in_real_chrome_requires_an_outcome_and_records_one`
   — submitting the custom textarea with no outcome selected is refused and the
   item is still open; selecting `defer`, typing a note and submitting resolves
   it with `decision.choice: "custom"`, `decision.outcome: "defer"`, the note on
   the row, and `resolution` reading
   `Decision: Custom answer, recorded as defer.` followed by `Note: <text>`.

A third, cheaper case belongs to the same phase and does not need Chrome:
`needs_you_replies_and_live_revisions_cross_the_real_server_process`
(`tests/e2e.rs:22183`) is extended to assert the card's server-rendered order and
the default-pair rendering for an item with no choices.

### 7. The writing standard

This section is the reason geoyws said "the language has to be clear to give me a
good context to make my decision". A card that is machine-readable but unreadable
is a worse wall of text than the body, because it is a shorter one that hides the
long one.

**The question.**
- One sentence, present tense, ending in `?`, at most 160 characters.
- It names the thing being decided, not the row: "hax has no logged-in Claude
  account — assign a seat, or drop that receipt?", never "Please decide on
  a-347ff24c".
- It admits every listed choice. A question that only one option answers is a
  request for approval wearing a question mark.
- No verdict-first shorthand. `BLOCKED —`, `RESOLVE-WHEN`, `PARKED -` belong in
  the body.

**The context, in this order, 2–5 sentences, at most 800 characters.**
1. **What is true now** — the measured state, not the history of how it got there.
2. **What is blocked** — named in plain words, with how many things are waiting
   and what waits on those.
3. **What waiting costs** — time, money, risk, or what ships unproven. If waiting
   costs nothing, say that; it is a legitimate answer and it changes the decision.

**Banned in question and context:** "the lane", "the row", "the executor", "the
driver", "the item", and any other word for the machinery. geoyws is deciding
about the world, not about the board. Typed ids — `a-*`, `t-*`, `e-*`, `d-*` —
appear only in a trailing `References:` clause, never mid-sentence.

**Required:** numbers with units (`ten minutes`, `43 items`, `2.4 GB`, `HTTP
401`); absolute dates (`2026-09-05`, never "Friday" or "last week"); full nouns
in place of pronouns whose referent could drift; and second person for anything
only geoyws can do ("only you can finish the browser login").

**Each choice.**
- The label is a verb phrase starting with the verb: "Assign a Claude seat to hax
  and log in", not "hax seat assignment". At most 60 characters, because it is a
  button.
- The consequence says **what happens and what it costs**, in that order, in one
  sentence. "Approved" is not a consequence. "The receipt is retried the same day;
  about ten minutes of your time plus the seat's monthly cost" is.
- A `defer` consequence must name **what brings the question back** — a date, an
  event, or a task that will be filed. "Later" with no trigger is how a row is
  deferred into oblivion, which is exactly what a-347ff24c did between 2026-09-05
  and today.
- Never put a credential value in a card; name the store entry.

**The recommendation is the raiser's opinion and is always exactly one.** An
agent that cannot pick one has not finished thinking about the question, and
should raise a smaller one.

#### The worked example: a-347ff24c, converted

Read from the live kanban board on 2026-09-08. The row: `kind: blocking`,
`priority: 0` (P0), `raisedBy: codex@driver`, `taskID: t-8c656910`, `createdAt`
2026-09-05 02:29 MYT, tags `pubsub`, `testing`, `status: open`. Its task
`t-8c656910`, "Install Claude Code on HAX for the pubsub adapter live receipt",
is `blocked`, P3, lane `driver`. Its body today, in full:

> PARKED - George 2026-09-05 walkthrough (via claude@driver): until an account is
> assigned to @@hax. Not engineering. No workaround. Gates only t-8c656910 (P1, no
> dependents).
>
> PREVIOUS BODY FOLLOWS, UNCHANGED:
> BLOCKED — HAX Claude Code 2.1.236 is installed at canonical root-owned
> executable /root/.local/share/claude/versions/2.1.236 (SHA256
> 6c8818fa22187aa555c242be4abbacc44d6b71a32ac9631ee7b2b5d12f51f752). Kanban commit
> c19ffe4b2ebcd89dbbf048db0872fe095b30b9d5 is live as verified deployment
> d-b49c3b23 with six binaries. Host dispatchers.json SHA256
> 608685d8a3837be23202f3f1c66477fb027afc9724a1d1b644dcd542fe1f1a17 binds
> claude.print/start-readonly-turn to the exact immutable release adapter, exact
> Claude executable/version, /root HOME, and private empty cwd; dispatcher no-op
> proves the config loads, and no active subscription exists. The prior real
> serialized no-tools Claude turn failed HTTP 401 because the stored OAuth access
> token is revoked. George must `ssh hax`, run `/root/.local/bin/claude auth
> login`, complete the browser flow, then tell driver to retry t-8c656910. Resolve
> only after the installed adapter returns its exact live AdapterResponse for a
> real Claude acknowledgement; do not work around authentication.

That is 1,235 characters, two SHA256s, a deployment id, a commit sha and an
absolute executable path, and it is a P0 that geoyws already parked once and that
is still sitting at the top of "Needs you" three days later. The card:

**question** (133 characters)

> hax has no logged-in Claude account, so the pubsub adapter cannot record one
> real Claude reply - assign a seat, or drop that receipt?

**context** (588 characters)

> Claude Code 2.1.236 is installed on hax and its dispatcher config loads, but the
> saved login is revoked and a real turn answers HTTP 401. You parked this on
> 2026-09-05 until an account was assigned to hax; three days later no account has
> been assigned. Only you can finish it: it needs a paid seat and a browser login
> nobody else can complete. One task is waiting - install Claude Code on hax for
> the pubsub adapter's live receipt - and nothing is waiting on that task. Until it
> moves, the pubsub adapter ships with every provider proven except Claude.
> References: a-347ff24c, t-8c656910.

**choices**

| key | label | consequence | outcome | |
| --- | --- | --- | --- | --- |
| `assign-and-login` | Assign a Claude seat to hax and log in | You buy or free one Claude seat, ssh to hax and finish the browser login: about ten minutes of your time plus the seat's monthly cost, and the receipt is retried the same day. | `approve` | **recommended** |
| `keep-parked` | Keep it parked until a seat frees up | Nothing changes and nobody waits on you; the pubsub adapter keeps shipping with the Claude path unproven, and a task is filed to re-raise this the day a seat frees up. | `defer` | |
| `drop-receipt` | Drop the live-Claude receipt from the adapter | The adapter is proven against the other providers only, the Claude path stays untested in production, and the install task closes as cancelled. | `reject` | |

Bounds: question 133 ≤ 160; context 588 ≤ 800; labels 38, 36 and 45 ≤ 60;
consequences 175, 167 and 143 ≤ 200; three choices, within 2–4; exactly one
recommended.

`assign-and-login` is the recommendation because the row is P0 and its own
resolve condition is a live receipt that no workaround may produce ("do not work
around authentication"); the cost is one seat, which is the smallest price on the
card. `keep-parked` is offered honestly rather than omitted, because it is what
geoyws chose on 2026-09-05 and it may still be right — but under §7 its
consequence names the trigger that brings the question back, which the 2026-09-05
parking did not, and which is why the item is still open.

The command that records it, in the shape Phase 4 will emit:

```bash
kanban attention update a-347ff24c --as claude@driver \
  --question "hax has no logged-in Claude account, so the pubsub adapter cannot record one real Claude reply - assign a seat, or drop that receipt?" \
  --context "Claude Code 2.1.236 is installed on hax and its dispatcher config loads, but the saved login is revoked and a real turn answers HTTP 401. You parked this on 2026-09-05 until an account was assigned to hax; three days later no account has been assigned. Only you can finish it: it needs a paid seat and a browser login nobody else can complete. One task is waiting - install Claude Code on hax for the pubsub adapter's live receipt - and nothing is waiting on that task. Until it moves, the pubsub adapter ships with every provider proven except Claude. References: a-347ff24c, t-8c656910." \
  --choice "assign-and-login=Assign a Claude seat to hax and log in|approve" \
  --consequence "assign-and-login=You buy or free one Claude seat, ssh to hax and finish the browser login: about ten minutes of your time plus the seat's monthly cost, and the receipt is retried the same day." \
  --choice "keep-parked=Keep it parked until a seat frees up|defer" \
  --consequence "keep-parked=Nothing changes and nobody waits on you; the pubsub adapter keeps shipping with the Claude path unproven, and a task is filed to re-raise this the day a seat frees up." \
  --choice "drop-receipt=Drop the live-Claude receipt from the adapter|reject" \
  --consequence "drop-receipt=The adapter is proven against the other providers only, the Claude path stays untested in production, and the install task closes as cancelled." \
  --recommend assign-and-login --json
```

The body is not touched by that command; it stays exactly as quoted above, folded
beneath the card. If geoyws presses `2`, the row afterwards carries
`decision: {"choice":"keep-parked","outcome":"defer","note":null,"by":"geoyws","at":…}`
and `resolution: "Decision: Keep it parked until a seat frees up. Nothing changes
and nobody waits on you; the pubsub adapter keeps shipping with the Claude path
unproven, and a task is filed to re-raise this the day a seat frees up."`

### 8. The backfill contract (Phase 4, `t-3377e301`)

**133 open items across seven boards**, measured 2026-09-08 with
`attention list --status open --limit 500 --fields id` per board:

| order | board | open items |
| --- | --- | --- |
| 1 | kanban | 6 |
| 2 | geoyws | 6 |
| 3 | dotfiles | 21 |
| 4 | hax | 17 |
| 5 | atmux | 37 |
| 6 | unum | 3 |
| 7 | px | 43 |
| | **total** | **133** |

**That order is the decided order.** kanban first because it is this change's own
board: a badly written card there is read within the hour by the people who wrote
the model, and the writing standard gets corrected on six items instead of
forty-three. px last because it is the largest and the most externally visible —
by the time the pass reaches it, the standard has been exercised on ninety items.

**The method.**

- **One subagent per open item**, drafting `question`, `context` and `choices`
  from that item's body plus its task's notes and checkpoints. One item per
  subagent because a card is a piece of writing, and a subagent given ten items
  writes ten cards in the same voice with the same guessed choices.
- **The subagent drafts; it never writes.** Per the standing delegation
  boundary, no subagent mutates the board. Each returns the draft card as
  structured data plus the evidence it used.
- **The main loop reviews every card before it lands**, against §7, and rejects
  any card whose choices are "approve / reject" wearing new labels — that item
  did not need a card and should keep the default pair.
- **`attention update` records it**, question, context, choices and
  recommendation in one call. The body is never rewritten by the backfill.
- **Batched through `transact`.** `transact` is shipped (`rust/lib.rs:1040`) and
  bounded at 32 items (`rust/mcp.rs:462`, enforced for the CLI at
  `rust/lib.rs:4809`), and a batch runs against one open
  board, so the 133 updates are **9 transactions** — one each for kanban,
  geoyws, dotfiles, hax and unum, two for atmux and two for px — instead of 133
  process starts, and a board's batch either lands whole or not at all
  (ADR-041).

**Three rules for the drafting.**

1. An item whose body already carries prose options — the `kb-att` convention —
   is a **conversion**: the options become choices in the order written, and the
   body's own recommendation becomes `recommended`. Nothing is invented.
2. An item with no options in its body gets a minimum of two authored choices,
   and if the honest set is exactly "do it / do not do it", the item keeps
   `choices: NULL` and gets only `question` and `context`. A default pair that was
   never authored is more honest than two invented ones.
3. An item whose only real answer is a fact only geoyws holds — which product was
   promised, whether a credential was rotated at its source — gets a two-choice
   card whose second choice is `other`, and its context says plainly that the
   answer is a fact rather than a preference. This is the kb-att "fact exception"
   (`/Users/geoyws/.agents/skills/kb-att/SKILL.md:159`–`:163`) expressed in the
   model instead of in a skill's prose.

**What Phase 4 also owns.** `skills/kb/SKILL.md`'s Attention section
(`skills/kb/SKILL.md:250`) gains the flag table and the writing standard, guarded
by `rust/lib.rs:8120`. The `kb-att` skill's bucket table
(`/Users/geoyws/.agents/skills/kb-att/SKILL.md:106`–`:114`) reads `choices`
instead of reconstructing them from prose, and
`lane-draft-classify.jq`'s dependency-clause regex
(`/Users/geoyws/.agents/skills/kb-att/scripts/lane-draft-classify.jq:30`) keeps
its job for **task drafts**, which this ADR does not change — it loses its job
only for attention rows.

## Consequences

**Phase 2 owns the model, store, CLI, MCP and migration.** Four columns and
`BOARD_V25`, `ATTENTION_OUTCOMES`, the default-pair materialization beside
`attention_row` (`rust/store.rs:1037`), the fourteen refusals, the composer moving
inside `resolve_attention_with_authorization` (`rust/store.rs:5154`), the
`decision` payload on both event kinds, the subcommand-aware repeatable predicate,
`ATTENTION_FIELDS` 17 → 21, and the search view. It is the largest phase and the
refusals are most of it.

**Phase 3 owns the card**, via `/frontend-design`. The deletions in §5 are as much
of the work as the additions.

**Phase 4 owns the skills and the 133-item backfill** (§8). **Phase 5 releases to
hax and hig**; the served proof is geoyws deciding one real item in one click.

**What geoyws gets.** A list whose rows read as questions with answers attached,
and a keyboard that answers them. Nothing about who may resolve changes, and the
one-click resolve has no confirmation because reopen is the undo.

**What a lane gets.** `decision.outcome` — a value from a closed set — instead of
a sentence to interpret. `Comment: do it after the pin lands` becomes
`{"choice":"custom","outcome":"approve","note":"do it after the pin lands"}`, and
the lane can branch on the first field and read the third.

**What breaks, named.** `attention resolve` without `--choice` is refused, which
breaks every existing caller: the web edge (Phase 3), kb-att's `resolve` helper
(Phase 4), and any ad-hoc script. This is deliberate and is the point — a resolve
with no verdict is what the ADR removes — but it is a hard cutover with no
compatibility mode, and a board on schema 25 served by a binary that predates it
is refused at open (`rust/db.rs:2322`), so Phase 5 must move binary and board
together.

**What is deliberately not built.** No new table, no `decisions[]` history on the
row (§3), no deferral scheduler, no seven-day reversal window, no change to the
resolve gate, and no GraphQL. A `defer` with a date would be the obvious V2; it
needs a sweeper, an idea of "due", and a decision about what a lapsed deferral
does, none of which is smuggled in here.

**The one asymmetry to watch.** Cards make items *look* answered — a well-written
question with three plausible choices reads as though someone thought hard about
it. A subagent can produce that appearance from a body that says nothing. The
main-loop review in §8 is the only thing standing between the backfill and 133
confident-looking guesses, and it is not optional.

## Acceptance: the compiled-process cases Phase 2 must pin

Named so the implementer cannot ship a narrower proof. All are compiled-process
e2e cases in `tests/e2e.rs` driven through the real binary, and the MCP ones
through a real stdio `Session`.

1. `an_attention_raised_with_choices_round_trips_every_field_through_list_and_json`
2. `an_attention_raised_without_choices_reads_as_the_default_approve_reject_pair_with_no_recommendation`
3. `resolving_with_a_choice_records_the_decision_object_and_composes_the_resolution_text`
   — `decision.{choice,outcome,note,by,at}` on the row and
   `resolution == "Decision: <label>. <consequence>"`, with `\nNote: <note>` only
   when a note was given.
4. `resolving_with_custom_requires_an_outcome_and_a_note_and_records_both`
5. `resolve_without_a_choice_is_refused_and_the_item_stays_open`
6. `every_card_refusal_names_its_fix` — all fourteen rows of §4's table, each
   asserted on its message and each leaving the board byte-identical.
7. `a_resolved_item_refuses_every_card_flag` — the widened
   `rust/store.rs:5093`.
8. `reopening_keeps_the_previous_decision_in_the_ledger_and_clears_it_from_the_row`
   — the §3 decision, asserted both ways: `attention_reopened`'s payload carries
   the decision, and the reopened row carries none.
9. `re_resolving_a_reopened_item_records_the_new_decision_and_the_previous_one_survives_in_the_ledger`
10. `a_board_migrates_from_schema_24_to_25_and_its_existing_attention_rows_read_as_the_default_pair`
    — a v24 board with resolved and open rows, migrated, every historical
    `resolution` byte-identical afterwards.
11. `the_audit_chain_stays_healthy_across_the_card_migration` —
    `kanban audit verify --json` healthy, `max(seq)` unchanged by the migration
    itself.
12. `search_finds_an_item_by_its_question_and_by_a_choice_consequence` — after
    `kanban search-rebuild`, and for a freshly raised row without one.
13. `the_schema_publishes_the_card_flags_with_the_right_kinds` — `choice` and
    `consequence` are `list` on `attention raise` and `attention update` and
    **absent** from `attention resolve`, whose `choice` is `value` and whose
    `outcome` carries the four enum values.
14. `attention_resolve_refuses_a_second_choice_flag` — the property case 13's
    schema claim rests on.
15. `the_mcp_attention_tools_mirror_every_card_flag_one_to_one` — `attention_raise`
    and `attention_update` accept arrays for `choice`/`consequence`;
    `attention_resolve` accepts scalars; the results equal the CLI's byte for
    byte.
16. `a_transact_of_thirty_two_attention_updates_lands_or_rolls_back_whole` — the
    Phase 4 backfill's batch shape, proven before 133 rows depend on it.
17. `the_attention_field_list_names_the_four_new_keys` — `rust/lib.rs:8722`
    extended.

Phase 3's two real-Chrome cases are named in §6.

## References

- `rust/model.rs:807`, `rust/model.rs:814`, `rust/model.rs:818` — `OPERATOR_ACTOR`
  and the two closed sets `ATTENTION_OUTCOMES` joins
- `rust/model.rs:822`–`rust/model.rs:841` — the `Attention` struct the four fields
  are added to
- `rust/store.rs:1037` — `attention_row`, where the default pair is materialized
- `rust/store.rs:4878`, `rust/store.rs:4901`, `rust/store.rs:4910` —
  `raise_attention`, its INSERT and its event payload
- `rust/store.rs:5066`, `rust/store.rs:5074`, `rust/store.rs:5093`,
  `rust/store.rs:5098` — `update_attention`, its "give me something" refusal, its
  resolved-history refusal and its body write
- `rust/store.rs:5132`, `rust/store.rs:5145`, `rust/store.rs:5154` — the three
  resolve entry points, including the trusted edge the web uses
- `rust/store.rs:5175`–`rust/store.rs:5195` — already-resolved, the geoyws-only
  gate, and the note requirement that becomes conditional
- `rust/store.rs:5205`–`rust/store.rs:5216`, `rust/store.rs:5257`–`rust/store.rs:5265`
  — the two event payloads that already carry resolution history and now carry
  the decision
- `rust/store.rs:5228`, `rust/store.rs:5243` — `reopen_attention`, the undo, and
  its gate
- `rust/serve.rs:74`, `rust/serve.rs:77`, `rust/serve.rs:96`–`rust/serve.rs:98` —
  the reply bound, the removed label, the trusted-edge actor
- `rust/serve.rs:527`, `rust/serve.rs:553`, `rust/serve.rs:559`,
  `rust/serve.rs:569`, `rust/serve.rs:584` — the POST path the choice payload
  replaces
- `rust/serve.rs:1270`–`rust/serve.rs:1297` — `reply_button_labels` and
  `compose_resolution_note`, both deleted
- `rust/serve.rs:1419`–`rust/serve.rs:1457` — the card as it stands
- `rust/serve.rs:2760`–`rust/serve.rs:2775` — the relabelling script, deleted; the
  draft-reply swap guard, kept
- `rust/lib.rs:316`, `rust/lib.rs:2352`, `rust/mcp.rs:277`, `rust/mcp.rs:376` —
  the four sites the subcommand-aware repeatable predicate replaces
- `rust/lib.rs:1040` — `transact`, which the backfill batches through
- `rust/lib.rs:1080`–`rust/lib.rs:1110` — the five attention `COMMANDS` rows
- `rust/lib.rs:1394`–`rust/lib.rs:1414` — `ENUM_ARGUMENTS`, where `outcome` is
  registered
- `rust/lib.rs:1723`–`rust/lib.rs:1724` — the inline `--flag=value` split that
  makes `--choice=k=L|approve` unambiguous
- `rust/lib.rs:2333`, `rust/lib.rs:2363` — `schema()` and its enum publication
- `rust/lib.rs:3747` — `ATTENTION_FIELDS`, 17 keys becoming 21
- `rust/lib.rs:6571`–`rust/lib.rs:6635` — the five CLI dispatch arms
- `rust/lib.rs:8120`, `rust/lib.rs:8126` —
  `the_skill_documents_aliases_that_actually_resolve` and its `include_str!` of
  `skills/kb/SKILL.md`: the named guard for the skill's table
- `rust/lib.rs:8197`, `rust/lib.rs:8216` —
  `every_list_valued_flag_is_declared_repeatable`
- `rust/lib.rs:8722`, `rust/lib.rs:8746` —
  `the_field_lists_name_exactly_the_keys_a_row_carries`
- `rust/db.rs:535`–`rust/db.rs:537` — the attention arm of `search_source_rows`
- `rust/db.rs:728`–`rust/db.rs:729` — `rules.task_tags`, the JSON-array-column
  precedent
- `rust/db.rs:741`–`rust/db.rs:794` — `BOARD_V17`, the one attention rebuild, and
  the CHECK written around a single settlement
- `rust/db.rs:1722`, `rust/db.rs:2178`–`rust/db.rs:2182`, `rust/db.rs:2322`,
  `rust/db.rs:2874`, `rust/db.rs:2901` — the version constant, the ladder, the
  open-time refusal, and the test that keeps them equal
- `rust/mcp.rs:243`, `rust/mcp.rs:317`, `rust/mcp.rs:327` — tools generated from
  `COMMANDS`, the `readOnlyHint` annotation, argument coercion
- `tests/e2e.rs:676`, `tests/e2e.rs:22183`, `tests/e2e.rs:22503` — the reply
  assertion helper and the two existing serve cases the Phase 3 cases extend
- `skills/kb/SKILL.md:136`, `skills/kb/SKILL.md:250`–`skills/kb/SKILL.md:311` —
  the guarded alias row and the Attention section Phase 4 rewrites
- `/Users/geoyws/.agents/skills/kb-att/SKILL.md:106`–`:114`, `:159`–`:163`,
  `:196` — the bucket table, the fact exception, and the `resolve` helper the CLI
  change breaks
- `/Users/geoyws/.agents/skills/kb-att/scripts/lane-draft-classify.jq:28`–`:34` —
  the prose-options parser this ADR retires for attention rows
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — why
  every §4 refusal names its fix
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — why the MCP
  surface needs no hand-written tool
- [ADR-012](ADR-012-session-handoffs-and-durable-attention.md) — durable
  attention, unchanged
- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md) — the UI's two write
  verbs, still two
- [ADR-029](ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md) —
  the chain that makes the event trail, not the row, the place for decision
  history
- [ADR-037](ADR-037-truncated-listings-refuse-a-default-limit-they-exceed.md) —
  the `--limit 500` the 133-item measurement had to pass
- [ADR-041](ADR-041-transact-is-one-atomic-ordered-write-batch.md) — `transact`,
  which the backfill batches through, and §1's precedent for refusing two
  spellings of one concept
- Kanban board: epic `e-fddce629`; this ADR `t-a105d87f`; backfill `t-3377e301`
- Measured at commit `1d8d069` on `kanban-geoyws-driver`; board counts read live
  on 2026-09-08
