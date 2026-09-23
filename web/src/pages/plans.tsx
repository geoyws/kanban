import type { FormEvent, ReactElement } from "react";
import { useState } from "react";
import type { Listing } from "../api";
import { postForm } from "../api";
import { Priority } from "../card";
import { ago, statusLabel } from "../format";
import type { Route } from "../router";
import { navigate } from "../router";
import { useProjection } from "../shell";
import { TagChips } from "../tags";
import { AppLink, PageState } from "./sprints";

/**
 * Plans: the drafted epics, and the work each one is holding back.
 *
 * A draft plan is the only page in the read set that is also a decision, so
 * it is the only one with a control on it: the plan's body is the argument,
 * the children beneath it are what the argument costs, and "Open plan" is
 * the answer. That order is the page — the button sits after the case for
 * pressing it, never before.
 */

interface PlanTask {
  id: string;
  type: string;
  title: string;
  body: string | null;
  status: string;
  priority: number;
  priorityLevel?: string | null;
  createdAt: number;
  tags: string[];
}

interface PlanChild {
  task: PlanTask;
  openAttention: number;
}

interface PlanCard {
  board: string;
  task: PlanTask;
  bodyHtml: string | null;
  openAttention: number;
  children: PlanChild[];
}

/** `rust/serve.rs`'s `attention_count_badge`: silent at zero. */
function OpenAttention({ count }: { count: number }): ReactElement | null {
  return count === 0 ? null : (
    <span className="attention-count">{count} open attention</span>
  );
}

function PlanArticle({
  card,
  onOpen,
  refusal,
}: {
  card: PlanCard;
  onOpen: (card: PlanCard) => void;
  refusal: string | null;
}): ReactElement {
  const plan = card.task;
  const action = `/plan/${encodeURIComponent(card.board)}/${encodeURIComponent(plan.id)}/open`;
  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    onOpen(card);
  };
  return (
    <article className="plan" data-plan={plan.id} data-testid="plan">
      <h2>
        <a
          href={`/task/${encodeURIComponent(card.board)}/${encodeURIComponent(plan.id)}`}
          data-task-link={plan.id}
          data-ref
          target="_blank"
          rel="noopener"
        >
          {plan.title}
        </a>{" "}
        <OpenAttention count={card.openAttention} />
      </h2>
      <p className="meta">
        Drafted {ago(plan.createdAt)} on{" "}
        <a
          href={`/board/${encodeURIComponent(card.board)}`}
          data-ref
          target="_blank"
          rel="noopener"
        >
          {card.board}
        </a>{" "}
        as <code>{plan.id}</code> at{" "}
        <Priority priority={plan.priority} level={plan.priorityLevel} />
        <TagChips tags={plan.tags} />
      </p>
      {card.children.length === 0 ? null : (
        <>
          <p className="meta">
            Holds back {card.children.length}{" "}
            {card.children.length === 1 ? "row" : "rows"}, none claimable until this
            plan is opened:
          </p>
          <ul className="children">
            {card.children.map((child) => (
              <li key={child.task.id}>
                <a
                  href={`/task/${encodeURIComponent(card.board)}/${encodeURIComponent(child.task.id)}`}
                  data-ref
                  target="_blank"
                  rel="noopener"
                >
                  {child.task.title}
                </a>
                <p className="meta">
                  In{" "}
                  <span className={`pill status-${child.task.status}`}>
                    {statusLabel(child.task.status)}
                  </span>{" "}
                  at{" "}
                  <Priority
                    priority={child.task.priority}
                    level={child.task.priorityLevel}
                  />
                  , filed as <code>{child.task.id}</code>
                  {child.openAttention === 0 ? null : (
                    <>
                      , with <OpenAttention count={child.openAttention} />
                    </>
                  )}
                </p>
              </li>
            ))}
          </ul>
        </>
      )}
      {card.bodyHtml === null ? null : (
        <div className="body md plan-body" data-plan-body>
          {/* The plan is agent-authored text typeset by the server's one
              markdown renderer, which is the one sanitiser (SPA-41). */}
          <div
            className="typeset"
            // biome-ignore lint/security/noDangerouslySetInnerHtml: server-typeset, server-sanitised
            dangerouslySetInnerHTML={{ __html: card.bodyHtml }}
          />
        </div>
      )}
      {refusal === null ? null : (
        <p className="error" data-refusal="open">
          {refusal}
        </p>
      )}
      {/* The same form the server has always accepted, at the same address
          and with the same method: the client posts it rather than letting
          the browser replace the document, and re-reads the projection
          (SPA-10). */}
      <form method="post" action={action} onSubmit={submit}>
        <button type="submit" data-plan-open data-testid="plan-open">
          Open plan
        </button>
      </form>
      <p className="cmd">
        Equivalent:{" "}
        <code>
          kb t mv {plan.id} todo --as ACTOR --project {card.board}
        </code>
      </p>
    </article>
  );
}

export default function PlansPage({ route }: { route: Route }): ReactElement {
  const { data, error, reload } = useProjection<Listing<PlanCard>>("/api/v1/plans");
  const [refusals, setRefusals] = useState<Record<string, string>>({});
  const opened = route.query.get("opened");
  const open = (card: PlanCard) => {
    const id = card.task.id;
    const action = `/plan/${encodeURIComponent(card.board)}/${encodeURIComponent(id)}/open`;
    void postForm(action).then((result) => {
      if (result.recorded) {
        navigate(`/plans?opened=${encodeURIComponent(id)}`);
        reload();
        return;
      }
      // The board's own sentence, verbatim, or the status when it sent none.
      setRefusals((rows) => ({
        ...rows,
        [id]: result.refusal ?? `The board refused this with ${result.status}.`,
      }));
    });
  };
  return (
    <main id="main" data-page data-route="plans" data-testid="app-root">
      <h1 data-testid="plans-page">Plans</h1>
      {opened === null ? null : (
        <p className="success" data-plan-opened>
          Opened plan <code>{opened}</code> and its child work is now eligible for
          claims.
        </p>
      )}
      {data === null ? (
        <PageState error={error} />
      ) : data.items.length === 0 ? (
        <p className="empty">
          No drafted plans. A plan is an epic with <code>--status draft</code>; its body
          is the plan and its children are the work, gated until it is opened.
        </p>
      ) : (
        data.items.map((card) => (
          <PlanArticle
            key={`${card.board}/${card.task.id}`}
            card={card}
            onOpen={open}
            refusal={refusals[card.task.id] ?? null}
          />
        ))
      )}
      {/* The board index is one tap from here because a plan names the board
          it gates. */}
      {data === null || data.items.length === 0 ? null : (
        <p className="meta">
          <AppLink href="/boards">Every board</AppLink>
        </p>
      )}
    </main>
  );
}
