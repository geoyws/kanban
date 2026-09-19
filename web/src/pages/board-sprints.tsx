import type { ReactElement } from "react";
import type { Route } from "../router";
import { useProjection } from "../shell";
import { AppLink, BoardSprintState, type BoardSprints, PageState } from "./sprints";

/**
 * One board's sprints: the same state the index shows for every board, with
 * the board's own name as the heading and a way back to its work.
 */
export default function BoardSprintsPage({ route }: { route: Route }): ReactElement {
  const board = route.params.project ?? "";
  const { data, error } = useProjection<BoardSprints>(
    `/api/v1/sprints/${encodeURIComponent(board)}`,
  );
  return (
    <main id="main" data-page data-route="board-sprints" data-testid="app-root">
      <h1 data-board-sprints={board} data-testid="board-sprints-page">
        {board} sprints
      </h1>
      <p>
        <AppLink href={`/board/${encodeURIComponent(board)}`}>Back to board</AppLink>
      </p>
      {data === null ? (
        <PageState error={error} />
      ) : (
        <BoardSprintState state={data} level={2} />
      )}
    </main>
  );
}
