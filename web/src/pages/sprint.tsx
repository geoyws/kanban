import type { ReactElement } from "react";
import { stamp, statusLabel } from "../format";
import type { Route } from "../router";
import { useProjection } from "../shell";
import { AppLink, daysRemaining, Goal, PageState, type Sprint } from "./sprints";

/**
 * One sprint: what it is for, when it runs, what is in it, and — once it is
 * closed — the deployment that proved the version was actually served.
 *
 * The page reads top to bottom as the sprint's own life: the goal, then the
 * dates, then the proof, then the scope. It carries no control at all; a
 * sprint is started and closed from the CLI, and a read page that grew a
 * button would be claiming otherwise.
 */

/** Enough of `rust/model.rs`'s `Task` for the scope list. */
interface ScopeTask {
  id: string;
  title: string;
  status: string;
}

/** The attempt that closed the sprint, as ADR-045's proof reads it. */
interface ClosingDeployment {
  id: string;
  servedVersion?: string;
  servedCommit?: string;
}

interface SprintDetail {
  board: string;
  sprint: Sprint;
  openTasks: number;
  doneTasks: number;
  goalHtml: string | null;
  tasks: ScopeTask[];
  closingDeployment: ClosingDeployment | null;
}

/** The served-version proof, or the fact that a closed sprint has none. */
function Proof({
  board,
  detail,
}: {
  board: string;
  detail: SprintDetail;
}): ReactElement | null {
  if (detail.closingDeployment === null) {
    return detail.sprint.status === "closed" ? (
      <p className="empty" data-no-sprint-proof>
        No served deployment proof is attached.
      </p>
    ) : null;
  }
  const attempt = detail.closingDeployment;
  return (
    <section className="sprint-card" data-sprint-deployment-proof>
      <h2>Served deployment proof</h2>
      <p>
        <a
          href={`/deployment/${encodeURIComponent(board)}/${encodeURIComponent(attempt.id)}`}
          data-deployment-link={attempt.id}
          data-ref
          target="_blank"
          rel="noopener"
        >
          <code>{attempt.id}</code>
        </a>
      </p>
      <dl>
        <dt>Served version</dt>
        <dd data-sprint-served-version>{attempt.servedVersion ?? "not recorded"}</dd>
        <dt>Served commit</dt>
        <dd data-sprint-served-commit>
          {attempt.servedCommit === undefined ? (
            "not recorded"
          ) : (
            <code>{attempt.servedCommit}</code>
          )}
        </dd>
      </dl>
    </section>
  );
}

export default function SprintPage({ route }: { route: Route }): ReactElement {
  const board = route.params.project ?? "";
  const id = route.params.id ?? "";
  const { data, error } = useProjection<SprintDetail>(
    `/api/v1/sprint/${encodeURIComponent(board)}/${encodeURIComponent(id)}`,
  );
  if (data === null) {
    return (
      <main id="main" data-page data-route="sprint" data-testid="app-root">
        <h1>Sprint</h1>
        <PageState error={error} />
      </main>
    );
  }
  const { sprint } = data;
  return (
    <main id="main" data-page data-route="sprint" data-testid="app-root">
      <h1 data-sprint-detail={sprint.id} data-testid="sprint-page">
        <span data-sprint-title>{sprint.title}</span>
      </h1>
      <p className="meta">
        Release <code data-sprint-version>{sprint.targetVersion}</code> on {data.board}.
      </p>
      <p>
        <AppLink
          href={`/sprints/${encodeURIComponent(data.board)}`}
          data-board-sprints-back
        >
          Back to {data.board} sprints
        </AppLink>
      </p>
      <div className="sprint-card is-current">
        <Goal html={data.goalHtml} />
        <dl>
          <dt>State</dt>
          <dd>
            <span className={`pill status-${sprint.status}`} data-sprint-state>
              {sprint.status}
            </span>
          </dd>
          <dt>Scheduled start</dt>
          <dd data-sprint-scheduled-start>{stamp(sprint.scheduledStart)}</dd>
          <dt>Scheduled end</dt>
          <dd data-sprint-scheduled-end>{stamp(sprint.scheduledEnd)}</dd>
          <dt>Actual start</dt>
          {/* Epoch zero is not a moment anyone can have started at, so it
              reads as the sprint not having started (`rust/model.rs`). */}
          <dd data-sprint-actual-start>
            {sprint.startsAt === 0 ? "not started" : stamp(sprint.startsAt)}
          </dd>
          <dt>Actual end</dt>
          <dd data-sprint-actual-end>
            {sprint.endsAt === null ? "not ended" : stamp(sprint.endsAt)}
          </dd>
          <dt>Days remaining</dt>
          <dd data-sprint-days-remaining>{daysRemaining(sprint.scheduledEnd)}</dd>
          <dt>Open tasks</dt>
          <dd data-sprint-open>{data.openTasks}</dd>
          <dt>Done tasks</dt>
          <dd data-sprint-done>{data.doneTasks}</dd>
          <dt>Archived</dt>
          <dd data-sprint-archived>{sprint.archived ? "yes" : "no"}</dd>
        </dl>
      </div>
      <Proof board={data.board} detail={data} />
      <h2>
        Attached tasks <span className="count">{data.tasks.length}</span>
      </h2>
      {data.tasks.length === 0 ? (
        <p className="empty" data-no-sprint-tasks>
          No visible tasks are attached.
        </p>
      ) : (
        <ul className="rows" data-sprint-tasks>
          {data.tasks.map((task) => (
            <li key={task.id} data-task={task.id}>
              <a
                href={`/task/${encodeURIComponent(data.board)}/${encodeURIComponent(task.id)}`}
                data-task-link={task.id}
                data-ref
                target="_blank"
                rel="noopener"
                data-task-title
              >
                {task.title}
              </a>
              <p className="meta">
                Attached as <code>{task.id}</code> in{" "}
                <span className={`pill status-${task.status}`} data-task-state>
                  {statusLabel(task.status)}
                </span>
              </p>
            </li>
          ))}
        </ul>
      )}
    </main>
  );
}
