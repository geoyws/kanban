/**
 * LOCAL CONTRACT COPY — writer A (`feat/t-bf255880-a-core`) owns this file.
 *
 * It is written here, to the interface agreed in the batch brief, only so
 * that this branch's pages can be driven at their real addresses by their
 * real-Chrome journeys before A's branch merges. At the merge A's copy wins
 * and this one is deleted; nothing in `web/src/pages/` depends on anything
 * beyond the agreed exports.
 */

import { useMemo, useSyncExternalStore } from "react";

/** Where the address bar says we are, resolved once per navigation. */
export interface Route {
  name: string;
  params: Record<string, string>;
  query: URLSearchParams;
}

/** The event `navigate` raises so a `pushState` reaches the subscribers. */
const ROUTE_CHANGE = "routechange";

/** One address shape: its name, and the names of its path parameters. */
const SHAPES: ReadonlyArray<readonly [string, readonly string[], string]> = [
  ["", [], "needs-you"],
  ["all", [], "all"],
  ["decided", [], "decided"],
  ["boards", [], "boards"],
  ["lanes", [], "lanes"],
  ["sprints", [], "sprints"],
  ["plans", [], "plans"],
  ["deployments", [], "deployments"],
  ["subscriptions", [], "subscriptions"],
  ["search", [], "search"],
  ["board/:project", ["project"], "board"],
  ["sprints/:project", ["project"], "board-sprints"],
  ["sprint/:project/:id", ["project", "id"], "sprint"],
  ["deployment/:project/:id", ["project", "id"], "deployment"],
  ["task/:project/:id", ["project", "id"], "task"],
];

/** Resolve one address to its route, or to the unknown one. */
export function matchRoute(pathname: string, search: string): Route {
  const query = new URLSearchParams(search);
  const segments = pathname.split("/").filter((segment) => segment.length > 0);
  for (const [shape, names, name] of SHAPES) {
    const parts = shape.split("/").filter((segment) => segment.length > 0);
    if (parts.length !== segments.length) {
      continue;
    }
    const params: Record<string, string> = {};
    let matched = true;
    for (let at = 0; at < parts.length; at += 1) {
      const part = parts[at] ?? "";
      const segment = segments[at] ?? "";
      if (part.startsWith(":")) {
        params[part.slice(1)] = decodeURIComponent(segment);
        continue;
      }
      if (part !== segment) {
        matched = false;
        break;
      }
    }
    if (matched && names.every((needed) => needed in params)) {
      return { name, params, query };
    }
  }
  return { name: "not-found", params: {}, query };
}

/** Go somewhere without leaving the document. */
export function navigate(href: string): void {
  history.pushState(null, "", href);
  window.dispatchEvent(new CustomEvent(ROUTE_CHANGE));
}

/** Subscribe to both ways an address can change: the browser's, and ours. */
function watch(onChange: () => void): () => void {
  window.addEventListener("popstate", onChange);
  window.addEventListener(ROUTE_CHANGE, onChange);
  return () => {
    window.removeEventListener("popstate", onChange);
    window.removeEventListener(ROUTE_CHANGE, onChange);
  };
}

/** The current route, re-read whenever the address changes. */
export function useRoute(): Route {
  const address = useSyncExternalStore(
    watch,
    () => `${location.pathname}${location.search}`,
    () => "/",
  );
  return useMemo(() => {
    const at = address.indexOf("?");
    return at === -1
      ? matchRoute(address, "")
      : matchRoute(address.slice(0, at), address.slice(at));
  }, [address]);
}
