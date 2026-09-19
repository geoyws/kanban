/**
 * One board: its rows grouped by status in workflow order, under the rules
 * that frame it.
 *
 * Workflow order rather than count order, because the question this page
 * answers is "where is the work" and the answer is a pipeline. A status with
 * nothing in it is not a heading with a zero; it is absent, so the page is
 * as long as the board is busy.
 *
 * A board this principal may not read, one that is retired, and one that
 * does not exist all answer the same way — the projection's single
 * non-enumerating refusal — and so this page says the same sentence for all
 * three (SPA-08).
 */

import type { ReactElement } from "react";
import type { Listing } from "../api";
import { Priority } from "../card";
import { aOrAn, statusLabel, tagSentence } from "../format";
import type { Route } from "../router";
import { Link, useProjection } from "../shell";

/** `rust/model.rs`'s `TASK_STATUSES`: the pipeline, in its own order. */
const STATUSES = [
  "draft",
  "backlog",
  "todo",
  "in_progress",
  "blocked",
  "review",
  "done",
  "cancelled",
] as const;

interface Task {
  id: string;
  type: string;
  title: string;
  status: string;
  priority: number;
  priorityLevel?: string | null;
  lane?: string | null;
  tags: string[];
}

/** One row of the board, with the open attention standing against it. */
interface BoardTaskRow {
  task: Task;
  openAttention: number;
}

/** One applicable rule: the registry's summary, and the body it folds open. */
interface BoardRule {
  id: string;
  headline: string;
  hasMore: boolean;
  bytes: number;
  tags: string[];
  body: string;
}

interface BoardDetail {
  board: string;
  roots: string[];
  tasks: Listing<BoardTaskRow>;
  rules: BoardRule[];
}

export default function BoardPage({ route }: { route: Route }): ReactElement {
  const name = route.params.project ?? "";
  const { data, error } = useProjection<BoardDetail>(
    `/api/v1/board/${encodeURIComponent(name)}`,
  );
  const rows = data?.tasks.items ?? [];
  return (
    <main id="main" data-page data-route="board" data-testid="app-root">
      <h1>{data?.board ?? name}</h1>
      {error === null ? null : (
        <p className="error" data-testid="board-refusal">
          {error}
        </p>
      )}
      {data === null ? null : (
        <>
          <p>
            <Link
              href={`/sprints/${encodeURIComponent(data.board)}`}
              data-board-sprints-link={data.board}
            >
              Sprints for {data.board}
            </Link>
          </p>
          <p className="meta" data-testid="board-roots">
            Roots: {data.roots.length === 0 ? "Rootless" : data.roots.join(", ")},
            holding {rows.length} rows
          </p>
          {data.rules.length === 0 ? null : (
            <>
              <h2>
                Rules <span className="count">{data.rules.length}</span>
              </h2>
              {data.rules.map((rule) => (
                <details className="rule" key={rule.id} data-rule={rule.id}>
                  <summary>
                    <code>{rule.id}</code> {rule.headline}
                    {tagSentence(rule.tags)}
                  </summary>
                  <pre>{rule.body}</pre>
                </details>
              ))}
            </>
          )}
          {STATUSES.map((status) => {
            const group = rows.filter((row) => row.task.status === status);
            if (group.length === 0) {
              return null;
            }
            return (
              <section key={status}>
                <h2>
                  {statusLabel(status)} <span className="count">{group.length}</span>
                </h2>
                <ul className="rows">
                  {group.map(({ task, openAttention }) => (
                    <li key={task.id} data-task={task.id} data-testid="board-row">
                      <a
                        href={`/task/${encodeURIComponent(data.board)}/${encodeURIComponent(task.id)}`}
                        data-task-link={task.id}
                        data-ref
                        target="_blank"
                        rel="noopener"
                      >
                        {task.title}
                      </a>
                      <p className="meta">
                        {aOrAn(task.type)} {task.type} in{" "}
                        <span className={`pill status-${task.status}`}>
                          {statusLabel(task.status)}
                        </span>{" "}
                        at{" "}
                        <Priority priority={task.priority} level={task.priorityLevel} />
                        {task.lane === null || task.lane === undefined
                          ? null
                          : `, in lane ${task.lane}`}
                        {tagSentence(task.tags)}
                        {openAttention === 0 ? null : (
                          <>
                            , with{" "}
                            <span className="attention-count">
                              {openAttention} open attention
                            </span>
                          </>
                        )}
                        , filed as <code>{task.id}</code>
                      </p>
                    </li>
                  ))}
                </ul>
              </section>
            );
          })}
        </>
      )}
    </main>
  );
}
