/**
 * The client router: one address bar, one page component.
 *
 * WRITER A OWNS THIS FILE. This copy is written to writer A's published
 * contract (`t-bf255880`, 2026-09-19) so the search and task pages can be
 * developed and proved before A's branch merges; when it does, A's copy is
 * the one that stays.
 */

import { useEffect, useState } from "react";

export interface Route {
  name: string;
  params: Record<string, string>;
  query: URLSearchParams;
}

/** The shapes the application answers, most specific segment count first. */
const ROUTES: { name: string; segments: string[] }[] = [
  { name: "needs-you", segments: [] },
  { name: "all", segments: ["all"] },
  { name: "decided", segments: ["decided"] },
  { name: "boards", segments: ["boards"] },
  { name: "board", segments: ["board", "{project}"] },
  { name: "lanes", segments: ["lanes"] },
  { name: "sprints", segments: ["sprints"] },
  { name: "board-sprints", segments: ["sprints", "{project}"] },
  { name: "sprint", segments: ["sprint", "{project}", "{id}"] },
  { name: "plans", segments: ["plans"] },
  { name: "deployments", segments: ["deployments"] },
  { name: "deployment", segments: ["deployment", "{project}", "{id}"] },
  { name: "subscriptions", segments: ["subscriptions"] },
  { name: "search", segments: ["search"] },
  { name: "task", segments: ["task", "{project}", "{id}"] },
];

/** The event a `navigate` fires so the mounted page hears its own push. */
const ROUTE_CHANGE = "routechange";

export function matchRoute(pathname: string, search: string): Route {
  const query = new URLSearchParams(search);
  const segments = pathname
    .split("/")
    .filter((segment) => segment.length > 0)
    .map((segment) => decodeURIComponent(segment));
  for (const route of ROUTES) {
    if (route.segments.length !== segments.length) {
      continue;
    }
    const params: Record<string, string> = {};
    let matched = true;
    for (const [at, shape] of route.segments.entries()) {
      const segment = segments[at] ?? "";
      if (shape.startsWith("{")) {
        params[shape.slice(1, -1)] = segment;
        continue;
      }
      if (shape !== segment) {
        matched = false;
        break;
      }
    }
    if (matched) {
      return { name: route.name, params, query };
    }
  }
  return { name: "not-found", params: {}, query };
}

export function navigate(href: string): void {
  const url = new URL(href, location.href);
  history.pushState(null, "", url);
  window.dispatchEvent(new CustomEvent(ROUTE_CHANGE));
}

export function useRoute(): Route {
  const [route, setRoute] = useState(() =>
    matchRoute(location.pathname, location.search),
  );
  useEffect(() => {
    const read = () => setRoute(matchRoute(location.pathname, location.search));
    window.addEventListener("popstate", read);
    window.addEventListener(ROUTE_CHANGE, read);
    return () => {
      window.removeEventListener("popstate", read);
      window.removeEventListener(ROUTE_CHANGE, read);
    };
  }, []);
  return route;
}
