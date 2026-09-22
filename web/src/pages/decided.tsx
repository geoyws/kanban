/**
 * Recent decisions: what was decided, newest first, each one undoable.
 *
 * Deciding is one keypress, so the list it leaves behind has to be readable
 * at the same speed: the question, the verdict in the ledger's own words,
 * the note when one rode along, and one Undo. The rows carry the same
 * `data-item`/`data-project` contract the deck's receipts do, so `u` and the
 * undo button land on the same code path in both places (SPA-43).
 *
 * The previous decision is NOT re-shown from the row: a reopen clears it
 * (ADR-042 §3), so what is rendered here is whatever the row settled with,
 * read once while it was still resolved.
 *
 * The undo is still a form posting to `/attention/{board}/{id}/reopen` — the
 * same route, the same method, the same field-less body the served page
 * posted (SPA-10). What changed is that the submit is intercepted, so the
 * page stays where it is and the row leaves the projection instead of the
 * document being replaced.
 */

import type { ReactElement } from "react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { Attention, Card, CheckSummary, Listing } from "../api";
import { postReopen } from "../api";
import { Priority } from "../card";
import { ago, cardQuestion } from "../format";
import type { Route } from "../router";
import { Link, useProjection } from "../shell";

/** The decisions room's envelope: the merge, plus the per-board scan. */
interface Decided extends Listing<Card> {
  scanLimit: number;
  /** The one check summary block's data (ACC-19); absent on an empty page. */
  checkSummary?: CheckSummary | null;
}

/** The reserved key a free-text answer is recorded under. */
const CUSTOM_CHOICE = "custom";

/**
 * What the decision was, in the words the ledger keeps for it
 * (`rust/serve.rs`'s `decision_words`).
 *
 * Falls back to the composed resolution for a row settled before decisions
 * were recorded, so a pre-2026 decision still reads as itself rather than as
 * nothing.
 */
function decisionWords(item: Attention): string {
  const decision = item.decision;
  if (decision !== null && decision !== undefined) {
    if (decision.choice === CUSTOM_CHOICE) {
      return `Your own answer, recorded as ${decision.outcome}.`;
    }
    const chosen = item.choices.find((choice) => choice.key === decision.choice);
    if (chosen !== undefined) {
      return `${chosen.label}. ${chosen.consequence}`;
    }
  }
  return item.resolution ?? "Resolved.";
}

// biome-ignore-start lint/a11y/noNoninteractiveTabindex: `u` aims at the row the reader is on
export default function DecidedPage({ route }: { route: Route }): ReactElement {
  const { data, error, reload } = useProjection<Decided>("/api/v1/decided");
  const [undoing, setUndoing] = useState<Record<string, boolean>>({});
  const [refusals, setRefusals] = useState<Record<string, string>>({});
  const rows = useRef<Record<string, HTMLElement | null>>({});

  const undo = useCallback(
    async (board: string, id: string) => {
      setUndoing((open) => ({ ...open, [id]: true }));
      const result = await postReopen(board, id).catch(() => ({
        recorded: false as const,
        refusal: null,
        status: 0,
      }));
      if (!result.recorded) {
        setUndoing((open) => ({ ...open, [id]: false }));
        setRefusals((held) => ({
          ...held,
          [id]:
            result.status === 0
              ? "The undo did not reach the board. Try again."
              : (result.refusal ??
                `The board refused to bring back ${id} (${result.status}).`),
        }));
        return;
      }
      // Open again, so the projection may stop listing it. The row leaves
      // when the board says it has left, not when the click landed.
      reload();
    },
    [reload],
  );

  // `u` undoes the row the reader is on, which is the row `tabindex=0` let
  // them reach. The served page bound the same key to the same rows.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "u" || event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      const target = event.target instanceof HTMLElement ? event.target : null;
      if (target?.matches("input, textarea") === true) {
        return;
      }
      const row = target?.closest<HTMLElement>("article.decided");
      const board = row?.dataset.project;
      const id = row?.dataset.item;
      if (board === undefined || id === undefined) {
        return;
      }
      event.preventDefault();
      void undo(board, id);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [undo]);

  const undone = route.query.get("undone");
  const items = data?.items ?? [];
  return (
    <main
      id="main"
      data-page
      data-route="decided"
      data-testid="app-root"
      data-projection={data === null ? 0 : 1}
    >
      <div className="heading">
        <h1>Recent decisions</h1>
      </div>
      {undone === null ? null : (
        <p className="success">
          Brought back <code>{undone}</code> - it is open again on{" "}
          <Link href="/">Needs you</Link>.
        </p>
      )}
      {error === null ? null : <p className="error">{error}</p>}
      {data === null ? null : items.length === 0 ? (
        <p className="empty" data-testid="decided-empty">
          Nothing decided yet.
        </p>
      ) : (
        <>
          <p className="count" data-testid="decided-count">
            Newest {items.length} shown. Undoing one puts it back on Needs you.
          </p>
          {data?.checkSummary == null ? null : (
            <section
              className="check-summary"
              data-testid="decided-check-summary"
              aria-label="Check results by subject"
            >
              <p data-testid="decided-check-summary-text">
                Checks: {data.checkSummary.answered} answered,{" "}
                {data.checkSummary.missed} missed — worst:{" "}
                <a href={`#d-${data.checkSummary.worst.rowId}`}>
                  {data.checkSummary.worst.about}
                </a>{" "}
                ({data.checkSummary.worst.missed}/{data.checkSummary.worst.answered})
              </p>
            </section>
          )}
          {items.map((card) => {
            const item = card.attention;
            const outcome = item.decision?.outcome ?? "other";
            const note = item.decision?.note;
            return (
              <article
                key={`${card.board}/${item.id}`}
                className="decided"
                tabIndex={0}
                data-item={item.id}
                data-project={card.board}
                data-testid="decided-row"
                aria-labelledby={`d-${item.id}`}
                ref={(node) => {
                  rows.current[item.id] = node;
                }}
              >
                <h2 id={`d-${item.id}`}>{cardQuestion(item.question, item.body)}</h2>
                <p className="decision">
                  <span className={`pill status-${outcome}`}>{outcome}</span>{" "}
                  {decisionWords(item)}
                </p>
                {note === null || note === undefined || note.length === 0 ? null : (
                  <p className="note">{note}</p>
                )}
                {/* One sentence, not a chain: who decided it, when, and
                    where it lives. */}
                <p className="meta">
                  {item.resolvedBy ?? "someone"} decided this{" "}
                  {item.resolvedAt === null || item.resolvedAt === undefined
                    ? "at some point"
                    : ago(item.resolvedAt)}{" "}
                  on{" "}
                  <a
                    href={`/board/${encodeURIComponent(card.board)}`}
                    data-ref
                    target="_blank"
                    rel="noopener"
                  >
                    {card.board}
                  </a>
                  {card.task === undefined ? null : (
                    <>
                      , about the <span data-task-type>{card.task.taskType}</span>{" "}
                      <a
                        href={`/task/${encodeURIComponent(card.board)}/${encodeURIComponent(card.task.id)}`}
                        data-task-link={card.task.id}
                        data-ref
                        target="_blank"
                        rel="noopener"
                      >
                        {card.task.title}
                      </a>
                    </>
                  )}
                  , <Priority priority={item.priority} level={item.priorityLevel} />
                </p>
                <form
                  className="undo"
                  method="post"
                  action={`/attention/${encodeURIComponent(card.board)}/${encodeURIComponent(item.id)}/reopen`}
                  onSubmit={(event) => {
                    event.preventDefault();
                    void undo(card.board, item.id);
                  }}
                >
                  <button
                    type="submit"
                    className="undo-button"
                    data-undo
                    {...(undoing[item.id] === true ? { "data-pressed": "" } : {})}
                  >
                    {undoing[item.id] === true ? "Undoing…" : "Undo - open it again"}
                  </button>
                </form>
                {refusals[item.id] === undefined ? null : (
                  <p className="error" data-refusal="board">
                    {refusals[item.id]}
                  </p>
                )}
              </article>
            );
          })}
          <p className="keys">u undo</p>
        </>
      )}
    </main>
  );
}
// biome-ignore-end lint/a11y/noNoninteractiveTabindex: `u` aims at the row the reader is on
