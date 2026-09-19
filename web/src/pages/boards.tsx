/**
 * Boards: every readable board at a glance, most urgent first.
 *
 * A table, because these are seven counts per board and a count compares
 * down a column or not at all. The order is the projection's — the board
 * with the most urgent unanswered thing leads — and it is not re-derived
 * here; `/api/v1/boards` serves the rows already ranked.
 *
 * A retired board is not a row with a note; it is absent, because absence is
 * the whole denial (SPA-08).
 */

import type { ReactElement } from "react";
import type { Listing } from "../api";
import { useProjection } from "../shell";

/** One row of the index, as `projection::BoardSummary` serialises it. */
interface BoardSummary {
  board: string;
  openAttention: number;
  todo: number;
  inProgress: number;
  stale: number;
  handoffs: number;
  tasks: number;
}

export default function BoardsPage(): ReactElement {
  const { data, error } = useProjection<Listing<BoardSummary>>("/api/v1/boards");
  const rows = data?.items ?? [];
  return (
    <main id="main" data-page data-route="boards" data-testid="app-root">
      <h1>Boards</h1>
      {error === null ? null : <p className="error">{error}</p>}
      {data !== null && rows.length === 0 ? (
        <p className="empty" data-testid="boards-empty">
          No board is registered here yet. <code>kb init --name NAME</code> makes the
          first one.
        </p>
      ) : (
        // Seven columns do not fit a 390px screen, and a column squeezed off
        // the edge is a count the operator cannot read. The table scrolls
        // inside its own band instead, so the document never does (SPA-37).
        <div className="scroller">
          <table>
            <thead>
              <tr>
                <th>Board</th>
                <th className="n">Open attention</th>
                <th className="n">To do</th>
                <th className="n">In progress</th>
                <th className="n">Stale</th>
                <th className="n">Handoffs</th>
                <th className="n">Tasks</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((summary) => (
                <tr
                  key={summary.board}
                  data-board={summary.board}
                  data-testid="boards-row"
                >
                  <td>
                    {/* A reference link: it previews on hover and opens the
                      board in its own tab, exactly as every other `data-ref`
                      in this estate does (SPA-40). */}
                    <a
                      href={`/board/${encodeURIComponent(summary.board)}`}
                      data-board-link={summary.board}
                      data-ref
                      target="_blank"
                      rel="noopener"
                    >
                      {summary.board}
                    </a>
                  </td>
                  {/* The one cell that is ever emphasised is the one an
                    operator is answering: a board with nothing waiting is
                    quiet in the same column. */}
                  <td className={summary.openAttention === 0 ? "n" : "n waiting"}>
                    {summary.openAttention}
                  </td>
                  <td className="n">{summary.todo}</td>
                  <td className="n">{summary.inProgress}</td>
                  <td className="n">{summary.stale}</td>
                  <td className="n">{summary.handoffs}</td>
                  <td className="n">{summary.tasks}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <p className="meta">
        Counts come from the same projection <code>kb dash</code> reads. Integrity is
        what <code>kb doctor</code> checks, not what this page claims.
      </p>
    </main>
  );
}
