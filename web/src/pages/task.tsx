/**
 * One task in full: what it is, who holds it, and the trail.
 *
 * The page an operator lands on from every reference link in the product, so
 * it reads top-down in the order a person asks their questions: what is this
 * row, when did it move, what does it say, who has it now, what is still
 * being asked about it, what has been written on it, and — last, because it
 * is the record rather than the reading — the audit trail.
 *
 * Every agent-authored body arrives typeset by the server
 * (`/api/v1/task/{project}/{id}`'s `bodyHtml` fields): one markdown renderer
 * and one sanitiser on the served surface, never a second one here (SPA-41).
 */

import type { ReactElement } from "react";
import type { Attention } from "../api";
import { ago, aOrAn, cardQuestion, stamp, statusLabel } from "../format";
import type { Route } from "../router";
import { useProjection } from "../shell";
import { TagChips } from "../tags";

/** `rust/model.rs`'s `Task`, as the projection serialises it. */
export interface Task {
  id: string;
  type: string;
  parentID?: string | null;
  title: string;
  body?: string | null;
  assignee?: string | null;
  lane?: string | null;
  deliverable?: string | null;
  driverOnly: boolean;
  status: string;
  priority: number;
  priorityLevel?: string | null;
  createdAt: number;
  updatedAt: number;
  completedAt?: number | null;
  tags: string[];
  allowedModels: string[];
}

/** `rust/model.rs`'s `ClaimSummary`: the holder, never the lease token. */
interface ClaimSummary {
  agentID: string;
  claimedAt: number;
  expiresAt: number;
  model?: string | null;
}

interface TaskNote {
  seq: number;
  author: string;
  kind: string;
  body: string;
  createdAt: number;
}

interface Checkpoint {
  seq: number;
  author: string;
  state: string;
  summary: string;
  intent: string;
  nextAction: string;
  branch?: string | null;
  headSha?: string | null;
  rootHead?: string | null;
  dirtySummary?: string | null;
  createdAt: number;
}

interface Event {
  seq: number;
  kind: string;
  actor?: string | null;
  payload: unknown;
  createdAt: number;
}

interface Envelope<T> {
  items: T[];
  returned: number;
  limit: number | null;
  truncated: boolean;
}

interface TaskDetail {
  board: string;
  task: Task;
  bodyHtml: string | null;
  claim: ClaimSummary | null;
  openAttention: { attention: Attention; bodyHtml: string }[];
  notes: Envelope<{ note: TaskNote; bodyHtml: string }>;
  checkpoints: Envelope<Checkpoint>;
  events: Envelope<Event>;
}

/**
 * The priority, which is the only thing in a row's sentence that is not
 * prose (`rust/serve.rs`'s `priority_badge`).
 */
export function Priority({
  priority,
  level,
}: {
  priority: number;
  level: string | null | undefined;
}): ReactElement {
  if (level === null || level === undefined) {
    return (
      <span className="priority priority-legacy" title="legacy out-of-band priority">
        {priority}
      </span>
    );
  }
  return (
    <span
      className={`priority priority-${level.toLowerCase()}`}
      title={`stored priority ${priority}`}
    >
      {level}
    </span>
  );
}

/** One reference: it previews on hover and opens in its own tab. */
export function TaskLink({
  board,
  id,
  children,
}: {
  board: string;
  id: string;
  children: React.ReactNode;
}): ReactElement {
  return (
    <a
      href={`/task/${encodeURIComponent(board)}/${encodeURIComponent(id)}`}
      data-task-link={id}
      data-ref
      target="_blank"
      rel="noopener"
    >
      {children}
    </a>
  );
}

/** One labelled fact, as `rust/serve.rs`'s `row` writes it. */
function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

/**
 * What the ledger recorded, as a table: every event is one row, so a trail
 * is read down a column rather than across a paragraph.
 *
 * The payload is free-form JSON an agent wrote. It is rendered as TEXT — the
 * keys in sorted order, exactly as `rust/serve.rs`'s `compact` joins them —
 * because no element authored by an agent enters this document (SPA-41).
 */
function payloadText(payload: unknown): string {
  if (payload === null || payload === undefined) {
    return "";
  }
  if (typeof payload !== "object") {
    return JSON.stringify(payload) ?? String(payload);
  }
  if (Array.isArray(payload)) {
    return JSON.stringify(payload);
  }
  return Object.entries(payload as Record<string, unknown>)
    .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
    .map(
      ([key, value]) =>
        `${key}=${typeof value === "string" ? value : JSON.stringify(value)}`,
    )
    .join("  ");
}

export default function TaskPage({ route }: { route: Route }): ReactElement {
  const board = route.params.project ?? "";
  const id = route.params.id ?? "";
  const path = `/api/v1/task/${encodeURIComponent(board)}/${encodeURIComponent(id)}`;
  const { data, error } = useProjection<TaskDetail>(path);

  if (error !== null) {
    return (
      <main id="main" data-page data-route="task" data-testid="app-root">
        <h1>Not found</h1>
        <p className="error">{error}</p>
      </main>
    );
  }
  if (data === null) {
    // Not the application root yet: the root is the page with its answer
    // in it, and a reader -- or a test -- that found it here would have
    // found a page that says nothing about the task it names.
    return (
      <main id="main" data-page data-route="task" data-testid="app-loading">
        <h1>Task</h1>
        <p className="meta">Reading the board…</p>
      </main>
    );
  }

  const task = data.task;
  const attention = data.openAttention;
  return (
    <main id="main" data-page data-route="task" data-testid="app-root">
      <h1 data-task-detail={task.id} data-task-title>
        {task.title}
      </h1>
      <p className="meta">
        {`${aOrAn(task.type)} ${task.type} on `}
        <a
          href={`/board/${encodeURIComponent(data.board)}`}
          data-ref
          target="_blank"
          rel="noopener"
        >
          {data.board}
        </a>
        {", in "}
        <span className={`pill status-${task.status}`} data-task-status>
          {statusLabel(task.status)}
        </span>
        {" at "}
        <Priority priority={task.priority} level={task.priorityLevel} />
        <TagChips tags={task.tags} />
        {", filed as "}
        <code>{task.id}</code>
      </p>
      <dl className="facts">
        <Fact label="created">{stamp(task.createdAt)}</Fact>
        <Fact label="updated">{stamp(task.updatedAt)}</Fact>
        {task.completedAt === null || task.completedAt === undefined ? null : (
          <Fact label="completed">{stamp(task.completedAt)}</Fact>
        )}
        {task.parentID === null || task.parentID === undefined ? null : (
          <Fact label="parent">
            <TaskLink board={data.board} id={task.parentID}>
              {task.parentID}
            </TaskLink>
          </Fact>
        )}
        {(
          [
            ["assignee", task.assignee],
            ["lane", task.lane],
            ["deliverable", task.deliverable],
          ] as const
        ).map(([label, value]) =>
          value === null || value === undefined ? null : (
            <Fact key={label} label={label}>
              {value}
            </Fact>
          ),
        )}
        {task.driverOnly ? <Fact label="driver only">yes</Fact> : null}
        {task.allowedModels.length === 0 ? null : (
          <Fact label="allowed models">{task.allowedModels.join(", ")}</Fact>
        )}
      </dl>
      {data.bodyHtml === null ? null : (
        <>
          <h2>Body</h2>
          <div
            className="body md"
            data-task-body
            // biome-ignore lint/security/noDangerouslySetInnerHtml: server-typeset, server-sanitised
            dangerouslySetInnerHTML={{ __html: data.bodyHtml }}
          />
        </>
      )}
      {data.claim === null ? null : (
        <>
          <h2>Held by</h2>
          <dl data-task-claim>
            <Fact label="agent">{data.claim.agentID}</Fact>
            <Fact label="claimed">{stamp(data.claim.claimedAt)}</Fact>
            <Fact label="expires">{stamp(data.claim.expiresAt)}</Fact>
            {data.claim.model === null || data.claim.model === undefined ? null : (
              <Fact label="model">{data.claim.model}</Fact>
            )}
          </dl>
        </>
      )}
      {attention.length === 0 ? null : (
        <>
          <h2>
            Open attention <span className="count">{attention.length}</span>
          </h2>
          <ul className="rows" data-task-attention>
            {attention.map((row) => (
              <AttentionRow key={row.attention.id} board={data.board} row={row} />
            ))}
          </ul>
        </>
      )}
      {data.notes.items.length === 0 ? null : (
        <>
          <h2>Notes</h2>
          {data.notes.items.map((row) => (
            <article className="note" data-task-note key={row.note.seq}>
              <p className="meta">
                {`${aOrAn(row.note.kind)} ${row.note.kind} note by ${row.note.author} at ${stamp(row.note.createdAt)}`}
              </p>
              <div
                className="body md"
                // biome-ignore lint/security/noDangerouslySetInnerHtml: server-typeset, server-sanitised
                dangerouslySetInnerHTML={{ __html: row.bodyHtml }}
              />
            </article>
          ))}
        </>
      )}
      {data.checkpoints.items.length === 0 ? null : (
        <>
          <h2>Checkpoints</h2>
          {data.checkpoints.items.map((point) => (
            <article className="note" data-task-checkpoint key={point.seq}>
              <p className="meta">
                {`${aOrAn(point.state)} ${point.state} checkpoint by ${point.author} at ${stamp(point.createdAt)}`}
              </p>
              <dl>
                <Fact label="summary">{point.summary}</Fact>
                <Fact label="intent">{point.intent}</Fact>
                <Fact label="next">{point.nextAction}</Fact>
                {(
                  [
                    ["branch", point.branch],
                    ["HEAD", point.headSha],
                    ["root HEAD", point.rootHead],
                    ["tree", point.dirtySummary],
                  ] as const
                ).map(([label, value]) =>
                  value === null || value === undefined ? null : (
                    <Fact key={label} label={label}>
                      {value}
                    </Fact>
                  ),
                )}
              </dl>
            </article>
          ))}
        </>
      )}
      {data.events.items.length === 0 ? null : (
        <>
          <h2>Trail</h2>
          <table data-task-trail>
            <thead>
              <tr>
                <th>When</th>
                <th>What</th>
                <th>Who</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {data.events.items.map((event) => (
                <tr data-event-kind={event.kind} key={event.seq}>
                  <td className="when">{stamp(event.createdAt)}</td>
                  <td>
                    <code>{event.kind}</code>
                  </td>
                  <td>{event.actor ?? "—"}</td>
                  <td className="payload">{payloadText(event.payload)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="meta">
            {`Newest ${data.events.limit ?? data.events.returned} shown, and `}
            <code>{`kb ev --task ${task.id} --project ${data.board} --json`}</code>
            {" prints the rest."}
          </p>
        </>
      )}
    </main>
  );
}

/** One open ask against this task: a title and one sentence (SPA-35). */
function AttentionRow({
  board,
  row,
}: {
  board: string;
  row: { attention: Attention; bodyHtml: string };
}) {
  const item = row.attention;
  const kind = item.kind.replace(/_/g, " ");
  return (
    <li data-item={item.id}>
      <span className="title">{cardQuestion(item.question, item.body)}</span>
      <p className="meta">
        {`${aOrAn(kind)} ${kind} ask, raised by ${item.raisedBy} ${ago(item.createdAt)} at `}
        <Priority priority={item.priority} level={item.priorityLevel} />
        <TagChips tags={item.tags} />
        {item.taskID === null || item.taskID === undefined ? null : (
          <>
            {", about "}
            <TaskLink board={board} id={item.taskID}>
              {item.taskID}
            </TaskLink>
          </>
        )}
      </p>
      <div
        className="body md"
        // biome-ignore lint/security/noDangerouslySetInnerHtml: server-typeset, server-sanitised
        dangerouslySetInnerHTML={{ __html: row.bodyHtml }}
      />
    </li>
  );
}
