import type { MouseEvent, ReactElement } from "react";
import type { Listing } from "../api";
import { stamp } from "../format";
import type { Route } from "../router";
import { navigate } from "../router";
import { useProjection } from "../shell";

/**
 * Sprints: every board's release boundary, and the history either side of
 * it.
 *
 * A sprint is a version with a date on it, so the version is what the card
 * leads with — set in the mono stack, because it is an identifier a reader
 * matches against a deploy receipt rather than prose. The title explains
 * it, the goal is the sprint's own words, and the five numbers under it are
 * the only things an operator plans around: when it starts, when it is due,
 * how long is left, and how much of it is still open.
 *
 * This module also holds the pieces `/sprints/{project}` and
 * `/sprint/{project}/{id}` share with it, so one board's sprint state is
 * assembled once for the index and the board page alike — `serve.rs`'s
 * `board_sprints_content` had the same reason.
 */

/** `rust/model.rs`'s `Sprint`, as the projection serialises it. */
export interface Sprint {
  id: string;
  title: string;
  body: string | null;
  status: string;
  targetVersion: string;
  scheduledStart: number;
  scheduledEnd: number;
  startsAt: number;
  endsAt: number | null;
  closedByDeployment: string | null;
  createdAt: number;
  updatedAt: number;
  archived: boolean;
}

/** One sprint with the two counts its card shows, and its goal, typeset. */
export interface SprintCard {
  sprint: Sprint;
  openTasks: number;
  doneTasks: number;
  goalHtml: string | null;
}

/** One board's release boundary and everything either side of it. */
export interface BoardSprints {
  board: string;
  current: SprintCard | null;
  history: SprintCard[];
}

const DAY = 86_400_000;

/**
 * `rust/serve.rs`'s `sprint_days_remaining`: whole days left, rounded up,
 * and never negative — an overdue sprint has none left rather than minus
 * three.
 */
export function daysRemaining(scheduledEnd: number, now: number = Date.now()): number {
  const left = scheduledEnd - now;
  return left <= 0 ? 0 : Math.ceil(left / DAY);
}

/**
 * A link that stays inside the application: the address bar changes, the
 * document does not. Modified clicks and the middle button are left to the
 * browser, because a reader asking for a new tab means it.
 */
export function AppLink({
  href,
  children,
  ...rest
}: {
  href: string;
  children: React.ReactNode;
} & Record<string, unknown>): ReactElement {
  const follow = (event: MouseEvent<HTMLAnchorElement>) => {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
      return;
    }
    event.preventDefault();
    navigate(href);
  };
  return (
    <a href={href} onClick={follow} {...rest}>
      {children}
    </a>
  );
}

/** The sprint's own words, or the state of having written none. */
export function Goal({ html }: { html: string | null }): ReactElement {
  if (html === null) {
    return (
      <div className="body md" data-sprint-goal>
        <p className="empty">No goal or success criteria recorded.</p>
      </div>
    );
  }
  return (
    <div className="body md" data-sprint-goal>
      {/* The goal is agent-authored text typeset by the server's one
          markdown renderer, which is the one sanitiser (SPA-41). The client
          adds no parser of its own. */}
      <div
        className="typeset"
        // biome-ignore lint/security/noDangerouslySetInnerHtml: server-typeset, server-sanitised
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  );
}

/** One sprint as a card: the version, the goal, and the five numbers. */
export function SprintSummary({
  board,
  card,
  current,
  level,
}: {
  board: string;
  card: SprintCard;
  current: boolean;
  level: 3 | 4;
}): ReactElement {
  const { sprint } = card;
  const Heading = `h${level}` as "h3" | "h4";
  return (
    <article
      className={current ? "sprint-card is-current" : "sprint-card"}
      data-sprint-summary={sprint.id}
      data-testid="sprint-summary"
      {...(current ? { "data-sprint-current": "" } : {})}
    >
      <Heading className="sprint-version">
        <AppLink
          href={`/sprint/${encodeURIComponent(board)}/${encodeURIComponent(sprint.id)}`}
          data-sprint-link={sprint.id}
        >
          <code data-sprint-version>{sprint.targetVersion}</code>
        </AppLink>
      </Heading>
      <p>
        <strong data-sprint-title>{sprint.title}</strong>
      </p>
      <Goal html={card.goalHtml} />
      <p className="meta">
        <span className={`pill status-${sprint.status}`} data-sprint-state>
          {sprint.status}
        </span>
        {sprint.archived ? (
          <span className="meta" data-sprint-archived>
            archived
          </span>
        ) : null}
      </p>
      <dl>
        <dt>Scheduled start</dt>
        <dd data-sprint-scheduled-start>{stamp(sprint.scheduledStart)}</dd>
        <dt>Scheduled end</dt>
        <dd data-sprint-scheduled-end>{stamp(sprint.scheduledEnd)}</dd>
        <dt>Days remaining</dt>
        <dd data-sprint-days-remaining>{daysRemaining(sprint.scheduledEnd)}</dd>
        <dt>Open tasks</dt>
        <dd data-sprint-open>{card.openTasks}</dd>
        <dt>Done tasks</dt>
        <dd data-sprint-done>{card.doneTasks}</dd>
      </dl>
    </article>
  );
}

/**
 * One board's sprint state: the boundary it is on now, then everything
 * planned, closed or abandoned.
 *
 * The three empty states are three different facts and are named as such —
 * a board between sprints, a board with a boundary and no history, and a
 * board that has never had one.
 */
export function BoardSprintState({
  state,
  level,
}: {
  state: BoardSprints;
  level: 2 | 3;
}): ReactElement {
  const Heading = `h${level}` as "h2" | "h3";
  const cardLevel = (level + 1) as 3 | 4;
  return (
    <>
      <Heading>Current sprint</Heading>
      {state.current === null ? (
        <p className="empty" data-no-current-sprint>
          No current sprint. Planned and historical sprints remain below.
        </p>
      ) : (
        <SprintSummary
          board={state.board}
          card={state.current}
          current={true}
          level={cardLevel}
        />
      )}
      {state.history.length === 0 ? (
        state.current === null ? (
          <p className="empty" data-no-sprints>
            This board has no sprints.
          </p>
        ) : (
          <p className="empty" data-no-sprint-history>
            No planned or historical sprints.
          </p>
        )
      ) : (
        <section className="sprint-history" data-sprint-history>
          <Heading>
            Planned and history <span className="count">{state.history.length}</span>
          </Heading>
          {state.history.map((card) => (
            <SprintSummary
              key={card.sprint.id}
              board={state.board}
              card={card}
              current={false}
              level={cardLevel}
            />
          ))}
        </section>
      )}
    </>
  );
}

/** What a page says when the projection refused it or has not answered. */
export function PageState({ error }: { error: string | null }): ReactElement {
  return error === null ? (
    <p className="empty" data-testid="page-loading">
      Reading the ledger.
    </p>
  ) : (
    <p className="error" data-testid="page-error">
      The ledger did not answer this page: {error}. Reload to ask again.
    </p>
  );
}

export default function SprintsPage(_props: { route: Route }): ReactElement {
  const { data, error } = useProjection<Listing<BoardSprints>>("/api/v1/sprints");
  return (
    <main id="main" data-page data-route="sprints" data-testid="app-root">
      <h1 data-sprints-overview data-testid="sprints-page">
        Sprints
      </h1>
      {data === null ? (
        <PageState error={error} />
      ) : data.items.length === 0 ? (
        <p className="empty">No boards are registered.</p>
      ) : (
        data.items.map((state) => (
          <section key={state.board} data-sprint-board={state.board}>
            <h2>
              <AppLink
                href={`/sprints/${encodeURIComponent(state.board)}`}
                data-board-sprints-link
              >
                {state.board}
              </AppLink>
            </h2>
            <BoardSprintState state={state} level={3} />
          </section>
        ))
      )}
    </main>
  );
}
