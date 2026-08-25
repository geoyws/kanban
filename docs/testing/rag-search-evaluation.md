# RAG search evaluation contract

The compiled-process E2E fixture creates records whose identifiers are stable
inside the test database, then drives the released CLI, MCP transport and HTTP
server. These queries are the minimum relevance corpus for
`kanban-semantic-lite-v1`.

| class | query | relevant fixture | gate |
| --- | --- | --- | --- |
| identifier | `t-search-handoff` | token-pressure handoff task | top 1 |
| exact phrase | `bounded context packet` | context-budget task | top 1 |
| paraphrase | `continue work after an agent runs out of context` | token-pressure handoff task | top 5 |
| paraphrase | `publish the new binary and restart the website` | release/install task | top 5 |
| paraphrase | `keep old completed items out of the hot working set` | settled-history archival task | top 5 |
| paraphrase | `who is allowed to sign in to the public board` | edge-authentication task | top 5 |
| paraphrase | `find overdue work whose owner stopped checking in` | stale-lease task | top 5 |
| filter | `release`, tag `ops` | tagged release task only | exact filter |
| cold history | `retired alias` | archived task | absent by default, present with `--all` |
| board isolation | `private alpha phrase` | alpha board only | absent from beta, present with `--all-boards` |

The suite records three rankings for the paraphrase rows: lexical-only,
semantic-only and hybrid. Hybrid must retrieve at least four of the five
relevant records in the top five and may not perform worse than both component
rankings on the same corpus.

Every returned row is also checked for:

- board, source type, canonical source ID and optional linked task ID;
- stable `kanban://<board>/<source-type>/<source-id>` citation;
- bounded snippet and numeric lexical/semantic/hybrid scores;
- source timestamp and explicit archived state;
- honest returned-result, character-budget and truncation accounting.

Performance is measured separately from correctness. The release receipt names
the binary hash, host, board/document count, cold or warm state, iteration count
and p50/p95. A single fast invocation is not a percentile.
