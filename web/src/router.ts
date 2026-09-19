/**
 * The address bar, as one application reads it.
 *
 * Every page the server used to render is now a component of this one
 * document (ADR-048 §1), so the URL stops being a request and becomes state:
 * `matchRoute` turns a path into a name and its parameters, `navigate` moves
 * without a round trip, and `useRoute` is how a component hears about it.
 *
 * The shapes below are `rust/serve.rs`'s `ROUTE_SHAPES`, one for one and in
 * the same order, because the client's idea of what exists and the server's
 * have to be the same idea. Two of them are deliberately not handled here as
 * application routes:
 *
 * - `/all` keeps its server-rendered page and is reached as an ordinary
 *   link. It is the flat list the deck was laid over; nothing in this
 *   application renders it.
 * - `/preview/...` is a fragment fetched by the hover previews, never a
 *   destination, so it has no route name.
 *
 * An address that matches nothing is `not-found` rather than a throw: a typo
 * in the bar is a page that says so, not a blank document.
 *
 * **A route is only routed to once something renders it.** `t-bf255880`
 * moves the pages in waves, so at any commit some of the shapes below are
 * still server-rendered arms. `navigate` asks the registry the application
 * fills in at start-up and falls back to a real navigation for anything not
 * in it: a `pushState` to a page this bundle cannot draw would replace a
 * working server page with a blank one.
 */

import { useEffect, useState } from "react";

/** Where the application is: the name of a page, and what it is about. */
export interface Route {
  name: string;
  params: Record<string, string>;
  query: URLSearchParams;
}

/**
 * The event `navigate` fires. `popstate` covers the back button and nothing
 * else — a `pushState` is silent by design — so the two together are the
 * whole of "the address changed".
 */
const ROUTE_CHANGE = "routechange";

/**
 * The parameterless pages, by the single segment that names each.
 *
 * `app` is the deck under its own address: `/app` has served the shell
 * since the cutover and is what spec SPA-04 loads, so it names the same
 * page `/` does. Without it the bundle would answer its own entry point
 * with `not-found`.
 */
const PAGES: Record<string, string> = {
  app: "needs-you",
  all: "all",
  decided: "decided",
  boards: "boards",
  lanes: "lanes",
  sprints: "sprints",
  plans: "plans",
  deployments: "deployments",
  subscriptions: "subscriptions",
  search: "search",
};

/** The one-parameter pages, by their leading segment. */
const BOARD_PAGES: Record<string, string> = {
  board: "board",
  sprints: "board-sprints",
};

/** The two-parameter pages, by their leading segment. */
const ITEM_PAGES: Record<string, string> = {
  sprint: "sprint",
  deployment: "deployment",
  task: "task",
};

/**
 * The route names this bundle renders, declared by the application at
 * start-up. Empty until then, which makes every link a plain link — the
 * safe direction.
 */
let mounted: ReadonlySet<string> = new Set();

/** Declare what the application renders. Called once, from `app.tsx`. */
export function setMountedRoutes(names: Iterable<string>): void {
  mounted = new Set(names);
}

/** Read one address. Never throws; an unknown path is `not-found`. */
export function matchRoute(pathname: string, search: string): Route {
  const query = new URLSearchParams(search);
  const parts = pathname
    .split("/")
    .filter((segment) => segment.length > 0)
    .map((segment) => {
      try {
        return decodeURIComponent(segment);
      } catch {
        // A path that is not valid percent-encoding is still an address the
        // bar can hold; it names no page, and that is the whole answer.
        return segment;
      }
    });
  const at = (name: string, params: Record<string, string> = {}): Route => ({
    name,
    params,
    query,
  });
  const [head, second, third] = parts;
  if (head === undefined) {
    return at("needs-you");
  }
  if (parts.length === 1) {
    const page = PAGES[head];
    return page === undefined ? at("not-found") : at(page);
  }
  if (parts.length === 2 && second !== undefined) {
    const page = BOARD_PAGES[head];
    return page === undefined ? at("not-found") : at(page, { project: second });
  }
  if (parts.length === 3 && second !== undefined && third !== undefined) {
    const page = ITEM_PAGES[head];
    return page === undefined
      ? at("not-found")
      : at(page, { project: second, id: third });
  }
  return at("not-found");
}

/**
 * Go to `href` without a round trip.
 *
 * An address this application does not render is left to the browser: a
 * link out of the estate is a link out of the estate, and so is a shape
 * this bundle has no page for. Everything else is a history entry and one
 * event.
 */
export function navigate(href: string): void {
  const target = new URL(href, location.href);
  if (target.origin !== location.origin) {
    location.assign(target.href);
    return;
  }
  const route = matchRoute(target.pathname, target.search);
  if (!mounted.has(route.name)) {
    // A shape no page renders and an address that names nothing both
    // belong to the server, which answers each with its refusal document.
    location.assign(target.href);
    return;
  }
  if (target.href === location.href) {
    return;
  }
  history.pushState(null, "", target.href);
  window.dispatchEvent(new CustomEvent(ROUTE_CHANGE));
}

/** The current route, re-read on every back button and every `navigate`. */
export function useRoute(): Route {
  const [route, setRoute] = useState<Route>(() =>
    matchRoute(location.pathname, location.search),
  );
  useEffect(() => {
    const read = () => setRoute(matchRoute(location.pathname, location.search));
    window.addEventListener("popstate", read);
    window.addEventListener(ROUTE_CHANGE, read);
    // The address may have moved between the first render and this effect —
    // a redirect answered while the bundle was still parsing — so the state
    // is re-read once on mount rather than trusted from render time.
    read();
    return () => {
      window.removeEventListener("popstate", read);
      window.removeEventListener(ROUTE_CHANGE, read);
    };
  }, []);
  return route;
}
