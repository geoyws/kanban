/**
 * Lanes: where every lane stands, newest first.
 *
 * The counterpart to Needs you — that page is what waits on the operator,
 * this is what the agents are doing. A lane that has been posting is legible
 * here without anyone opening a terminal or waiting for a handoff.
 *
 * Nothing deletes a sitrep, so a lane whose driver is long gone keeps its
 * rows forever; the ordering is most-recently-active first, which is what
 * sinks it rather than an alphabet that would park it at the top.
 *
 * Each update's body arrives as HTML the server's one markdown renderer
 * produced. It is the only `dangerouslySetInnerHTML` on this page and it is
 * deliberate: a second renderer in the client would be a second sanitiser,
 * and the second one is always the one that is wrong (SPA-41).
 */

import type { ReactElement } from "react";
import type { Listing } from "../api";
import { ago } from "../format";
import { useProjection } from "../shell";

/** One sitrep, with the body typeset by the server (`projection::LaneUpdate`). */
interface LaneUpdate {
  id: string;
  lane: string;
  taskID?: string | null;
  author: string;
  body: string;
  bodyHtml: string;
  branch?: string | null;
  createdAt: number;
}

/** One `(board, lane)` group (`projection::LaneSummary`). */
interface LaneSummary {
  board: string;
  lane: string;
  updates: LaneUpdate[];
}

export default function LanesPage(): ReactElement {
  const { data, error } = useProjection<Listing<LaneSummary>>("/api/v1/lanes");
  const groups = data?.items ?? [];
  return (
    <main id="main" data-page data-route="lanes" data-testid="app-root">
      <h1>Lanes</h1>
      {error === null ? null : <p className="error">{error}</p>}
      {data !== null && groups.length === 0 ? (
        <p className="empty" data-testid="lanes-empty">
          No lane has posted a sitrep. <code>kb sr new "…" --as AGENT --lane LANE</code>{" "}
          writes one — no task and no lease required, which is the point of it.
        </p>
      ) : null}
      {groups.map((group) => (
        <article
          key={`${group.board}/${group.lane}`}
          className="item"
          data-lane={group.lane}
          data-testid="lanes-group"
        >
          <h2>
            {group.lane}{" "}
            <span className="count">
              <a
                href={`/board/${encodeURIComponent(group.board)}`}
                data-ref
                target="_blank"
                rel="noopener"
              >
                {group.board}
              </a>
            </span>
          </h2>
          {group.updates.map((update) => (
            <div key={update.id}>
              <p className="meta">
                {update.author} wrote this {ago(update.createdAt)}
                {update.taskID === null || update.taskID === undefined ? null : (
                  <>
                    , about{" "}
                    <a
                      href={`/task/${encodeURIComponent(group.board)}/${encodeURIComponent(update.taskID)}`}
                      data-task-link={update.taskID}
                      data-ref
                      target="_blank"
                      rel="noopener"
                    >
                      {update.taskID}
                    </a>
                  </>
                )}
                {update.branch === null || update.branch === undefined
                  ? null
                  : `, on ${update.branch}`}
              </p>
              <div
                className="body md"
                data-lane-body
                // biome-ignore lint/security/noDangerouslySetInnerHtml: server-typeset, server-sanitised
                dangerouslySetInnerHTML={{ __html: update.bodyHtml }}
              />
            </div>
          ))}
        </article>
      ))}
      {data?.truncated === true ? (
        <p className="count" data-testid="lanes-truncated">
          A lane posted more than {data.limit} updates; the newest are shown.
        </p>
      ) : null}
    </main>
  );
}
