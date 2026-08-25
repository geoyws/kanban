# ADR-023: SQLite-native hybrid search supplies cited RAG context

**Status:** Accepted
**Date:** 2026-08-25
**Deciders:** George Yong

## Context

Kanban is now the durable work ledger across many projects and worktrees. It can
retrieve a row by identifier and list bounded operational views, but discovery
still means broad JSON reads and manual scanning. That is slow for a person and
wastes an agent's context window. Exact terms are not enough: the useful prior
record may describe a handoff while the caller asks how to resume work, or say
release while the caller asks what was deployed.

The board remains a private SQLite ledger. Search must preserve that boundary,
must cite the authoritative record behind every snippet, and must not turn an
ordinary read into a dependency on a hosted model or secret API key.

## Decision

### One derived search corpus in each board

Board schema V13 adds `search_documents`, one derived row per searchable task,
note, checkpoint, handoff, attention item, sitrep, project rule, and selected
audit event. Each row carries the source kind and identifier, linked task,
status, lane, tags, timestamps, archived state, source hash, embedding model,
and embedding bytes. No lease token, credential or session capability enters
the corpus.

An external-content FTS5 table indexes title, body and tags. SQLite triggers on
the authoritative tables keep the document and FTS rows current. Existing rows
are backfilled by the migration. The FTS index is always disposable: the board
rows remain authoritative and `search-rebuild` reconstructs all derived rows.

Global rules continue to live once in the registry. Search composes matching
global rules into the read result rather than copying them into every board.

### Local semantic-lite vectors, measured before ANN

Kanban ships a deterministic `kanban-semantic-lite-v1` embedding. It hashes
normalised words, stems, character n-grams, adjacent terms and a small explicit
Kanban-domain concept vocabulary into a fixed vector, then L2-normalises it.
This is not presented as a general neural language model. It supplies useful
paraphrase and concept recall locally, deterministically, and without network
or credential dependencies.

Vectors are compact little-endian `f32` BLOBs in SQLite. A search computes the
query vector and scans filtered document vectors with cosine similarity. Missing,
stale or corrupt cached vectors are recomputed in memory so a read remains
fresh and read-only. `search-rebuild --as ACTOR` persists the current model and
source hashes transactionally.

Brute-force cosine is the deliberate first implementation. `sqlite-vec` is
pre-1.0 and extension loading adds another installed artifact and failure mode.
An approximate-nearest-neighbour extension may replace the scan only after the
released corpus and latency receipts show that it is needed; the public result
contract does not expose the storage implementation.

### Hybrid ranking and cited output

`kb search QUERY` combines three signals:

1. exact source identifier and exact-title matches;
2. FTS5/BM25 lexical rank;
3. semantic vector similarity.

The ranks are fused rather than pretending their raw scores share a scale.
Each result names the board, source type, source ID, linked task ID, title,
bounded snippet, filters, timestamps, archived state, per-signal scores and a
stable `kanban://` citation. Active rows are the default; cold history requires
`--all` and remains visibly labelled.

The read is bounded twice: `--limit` caps result count and `--max-chars` caps
the serialized context payload. The receipt reports returned results, used
characters, searched and missing boards, model identity and truncation. Cache
freshness is reported by `doctor`.

### One command contract, three surfaces

- CLI: `kb search QUERY`, with source/status/tag/lane/time filters and explicit
  `--all-boards` cross-project scope.
- MCP: generated from the same command table and marked read-only. The explicit
  rebuild operation is a separate write tool.
- Web: one keyboard-friendly search form and a server-rendered results page.
  The handler calls the same retrieval function and never receives a lease.

`--all-boards` is explicit and cannot be combined with `--project`, `--workspace`
or `--db`. It opens only present registered boards. Missing boards are named in
the receipt rather than silently omitted.

## Consequences

- Exact and conceptual discovery become one bounded operation for people and
  agents, with source citations instead of generated assertions.
- Ordinary search works offline. A future optional neural embedding provider
  can be added behind a new model identifier without changing stored truth.
- Semantic-lite recall is intentionally narrower than a large embedding model;
  the checked-in evaluation corpus makes that limitation measurable.
- The derived corpus increases each board file. It remains inside the same
  online backup, restore and integrity boundary and can be rebuilt after model
  or schema changes.
- Brute-force vector scans are appropriate only while measured latency stays
  within the release target. That is a gate, not an assumption.

## Release gates

- Exact identifier and exact phrase queries are top-1 in the compiled-process
  fixture.
- At least 80% of the checked-in paraphrase queries place the relevant record
  in the top five, with lexical-only and semantic-only baselines printed by the
  test fixture.
- Every result has a valid source citation; active-only, archived, board and tag
  isolation have compiled-process coverage.
- `--max-chars` and `--limit` are enforced and report truncation.
- CLI and MCP response semantics match; the real loopback server exposes the
  same results and a real browser exercises the installed page.
- Warm p95 is below 250 ms for the current Kanban board and below one second for
  all present registered boards, with corpus size and host recorded.
- Backup/restore followed by `search-rebuild` produces the same source set and
  does not change authoritative ledger rows.

## References

- [SQLite FTS5](https://www.sqlite.org/fts5.html)
- [ADR-001](ADR-001-durable-agent-work-ledger.md)
- [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md)
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md)
- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md)
- [ADR-021](ADR-021-settled-history-leaves-operational-indexes.md)
