/**
 * Search: one field, and what every board knows about what was typed in it.
 *
 * Cross-board retrieval for people who should not need to know which board
 * owns a fact before they can find it. The ranking, the bound and the boards
 * searched are the store's own (`/api/v1/search`, which shares
 * `rust/serve.rs`'s `search_receipt` with the CLI's search); this page adds
 * the reading order and nothing else.
 *
 * A result is a title, one sentence saying where it came from and how well
 * it scored, the snippet the store cut, and the citation that retrieves the
 * authoritative row. Nothing here is a card: a search result is a pointer,
 * and a pointer that dressed as a record would invite deciding from it.
 */

import type { ReactElement } from "react";
import { aOrAn, tagSentence } from "../format";
import type { Route } from "../router";
import { navigate } from "../router";
import { useProjection } from "../shell";

/** `rust/model.rs`'s `SearchResult`, as the projection serialises it. */
interface SearchResult {
  board: string;
  sourceKind: string;
  sourceId: string;
  taskID?: string;
  title: string;
  snippet: string;
  status?: string | null;
  lane?: string | null;
  tags: string[];
  score: number;
  citation: string;
}

/** The bounded result set and the receipt that bounds it. */
interface SearchPage {
  items: SearchResult[];
  returned: number;
  limit: number | null;
  truncated: boolean;
  query: string;
  embeddingModel: string;
  boards: string[];
  missingBoards: string[];
}

export default function SearchPage({ route }: { route: Route }): ReactElement {
  const query = (route.query.get("q") ?? "").trim();
  const { data, error } = useProjection<SearchPage>(
    `/api/v1/search?q=${encodeURIComponent(query)}`,
  );

  // The root is the page WITH its answer in it: while the boards are still
  // being searched the page is mounted but has nothing to read, and a
  // reader who arrived then arrived early.
  const settled = query.length === 0 || error !== null || data !== null;
  return (
    <main
      id="main"
      data-page
      data-route="search"
      data-testid={settled ? "app-root" : "app-loading"}
    >
      <h1>Search</h1>
      <form
        className="search-page"
        action="/search"
        method="get"
        data-search-form
        onSubmit={(event) => {
          event.preventDefault();
          const typed = new FormData(event.currentTarget).get("q");
          navigate(
            `/search?q=${encodeURIComponent(typeof typed === "string" ? typed : "")}`,
          );
        }}
      >
        <input
          name="q"
          defaultValue={query}
          key={query}
          aria-label="Search Kanban"
          placeholder="Task, decision, handoff, rule…"
        />
        <button type="submit">Search</button>
      </form>
      {query.length === 0 ? (
        <p className="empty">
          Search every board, including tasks, notes, checkpoints, handoffs, attention,
          sitreps, rules, and their audit trail.
        </p>
      ) : error !== null ? (
        <p className="error">{error}</p>
      ) : data === null ? (
        <p className="meta">Searching every board…</p>
      ) : (
        <Results page={data} />
      )}
    </main>
  );
}

function Results({ page }: { page: SearchPage }) {
  return (
    <>
      <p className="count" data-search-count>
        {`${page.items.length} result${page.items.length === 1 ? "" : "s"} across `}
        {`${page.boards.length} board${page.boards.length === 1 ? "" : "s"}, model `}
        <code>{page.embeddingModel}</code>
        {page.truncated ? ", bounded" : ""}
      </p>
      {page.items.length === 0 ? (
        <p className="empty">No matching Kanban knowledge.</p>
      ) : null}
      {page.items.map((result) => {
        const kind = result.sourceKind.replace(/_/g, " ");
        return (
          <article
            className="search-result"
            data-search-result={result.citation}
            key={result.citation}
          >
            <h2>
              {result.taskID === undefined ? (
                result.title
              ) : (
                <a
                  href={`/task/${encodeURIComponent(result.board)}/${encodeURIComponent(result.taskID)}`}
                  data-task-link={result.taskID}
                  data-ref
                  target="_blank"
                  rel="noopener"
                >
                  {result.title}
                </a>
              )}
            </h2>
            <p className="meta">
              {`${aOrAn(kind)} ${kind} on ${result.board}, scoring ${result.score.toFixed(3)}`}
              {result.status === null || result.status === undefined
                ? ""
                : `, ${result.status.replace(/_/g, " ")}`}
              {result.lane === null || result.lane === undefined
                ? ""
                : `, in lane ${result.lane}`}
              {tagSentence(result.tags)}
            </p>
            <p className="body">{result.snippet}</p>
            <p className="citation">
              <code>{result.citation}</code>
            </p>
          </article>
        );
      })}
      {page.missingBoards.length === 0 ? null : (
        <p className="error">{`Missing board files: ${page.missingBoards.join(", ")}`}</p>
      )}
    </>
  );
}
