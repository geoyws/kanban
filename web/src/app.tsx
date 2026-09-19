import type { ReactElement } from "react";
import { useEffect, useState } from "react";
import { Deck } from "./deck";
import { connectLive } from "./live";
import BoardPage from "./pages/board";
import BoardSprintsPage from "./pages/board-sprints";
import BoardsPage from "./pages/boards";
import DecidedPage from "./pages/decided";
import DeploymentPage from "./pages/deployment";
import DeploymentsPage from "./pages/deployments";
import LanesPage from "./pages/lanes";
import PlansPage from "./pages/plans";
import SearchPage from "./pages/search";
import SprintPage from "./pages/sprint";
import SprintsPage from "./pages/sprints";
import SubscriptionsPage from "./pages/subscriptions";
import TaskPage from "./pages/task";
import { bindPreviews, dismissPreviews } from "./previews";
import { type Route, setMountedRoutes, useRoute } from "./router";
import { Chrome, RefreshProvider } from "./shell";

/**
 * Every page that is not the deck, under one bar, one drawer and one
 * socket.
 *
 * The deck keeps its own of each because it is the one page with a state
 * worth protecting — an answer half written, a card mid-advance — and
 * mounting it under a component that swaps pages would be one more thing
 * able to throw that away. Everything else is a read page: it fetches its
 * projection, it re-fetches when the boards move, and swapping one for
 * another costs nothing.
 *
 * The socket is here rather than in each page so that walking the drawer
 * does not open and drop a `/live` connection per destination.
 */
function ReadPages({ route }: { route: Route }): ReactElement {
  const [socketUp, setSocketUp] = useState<boolean | null>(null);
  const [refreshes, setRefreshes] = useState(0);
  const [drawerOpen, setDrawerOpen] = useState(false);

  useEffect(() => {
    const stopLive = connectLive({
      onStatus: (status) => setSocketUp(status === "live"),
      // A read page renders no notice strip, exactly as the served read
      // pages rendered none: the strip is the deck's, where a decision is
      // being made. Nothing was shown, so nothing is claimed.
      onNotice: () => false,
      onRefresh: () => setRefreshes((count) => count + 1),
    });
    const stopPreviews = bindPreviews();
    return () => {
      stopLive();
      stopPreviews();
    };
  }, []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") {
        return;
      }
      if (dismissPreviews()) {
        event.preventDefault();
        return;
      }
      if (drawerOpen) {
        event.preventDefault();
        setDrawerOpen(false);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [drawerOpen]);

  return (
    <RefreshProvider value={refreshes}>
      <Chrome
        current={route.name}
        live={socketUp === null ? "connecting" : socketUp ? "live" : "reconnecting"}
        drawerOpen={drawerOpen}
        onDrawer={setDrawerOpen}
        covered={drawerOpen}
        onUncover={() => setDrawerOpen(false)}
      />
      <Page route={route} />
    </RefreshProvider>
  );
}

/**
 * Every read page this bundle renders, by the route name that reaches it.
 *
 * One table rather than a switch and a second list of names: this is also
 * what `navigate` is told the application can draw, so a page cannot be
 * added and left unroutable, and a route cannot be routed to before there
 * is a page for it.
 */
const PAGES: Record<string, (route: Route) => ReactElement> = {
  decided: (route) => <DecidedPage route={route} />,
  boards: () => <BoardsPage />,
  board: (route) => <BoardPage route={route} />,
  lanes: () => <LanesPage />,
  subscriptions: (route) => <SubscriptionsPage route={route} />,
  deployments: () => <DeploymentsPage />,
  deployment: (route) => <DeploymentPage route={route} />,
  sprints: (route) => <SprintsPage route={route} />,
  "board-sprints": (route) => <BoardSprintsPage route={route} />,
  sprint: (route) => <SprintPage route={route} />,
  plans: (route) => <PlansPage route={route} />,
  search: (route) => <SearchPage route={route} />,
  task: (route) => <TaskPage route={route} />,
};

/**
 * The two surfaces that ANSWER rather than read, by route name.
 *
 * Both are the deck component: `/` shows one card at a time, `/all` shows
 * the whole queue (`web/src/deck.tsx`). Neither is in `PAGES`, because a
 * page that holds a half-written answer, a notice strip and this sitting's
 * receipts cannot be swapped under `ReadPages`'s shared socket without
 * being able to lose all three.
 */
const ANSWERING: Record<string, () => ReactElement> = {
  "needs-you": () => <Deck />,
  all: () => <Deck layout="list" />,
};

setMountedRoutes([...Object.keys(ANSWERING), ...Object.keys(PAGES)]);

/** One route to one page. A name with no page is a page that says so. */
function Page({ route }: { route: Route }): ReactElement {
  const page = PAGES[route.name];
  if (page !== undefined) {
    return page(route);
  }
  return (
    <main id="main" data-page data-route="not-found" data-testid="app-root">
      <h1>Not found</h1>
      <p>
        No page at that address. <a href="/">Start over</a>.
      </p>
    </main>
  );
}

/**
 * The application: one document, and the address bar says which page it is
 * showing (ADR-048 §1).
 */
export function App(): ReactElement {
  const route = useRoute();
  const answering = ANSWERING[route.name];
  return answering === undefined ? <ReadPages route={route} /> : answering();
}
