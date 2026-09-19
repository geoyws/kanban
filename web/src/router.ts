/**
 * SCRATCH COPY OF WRITER A's MODULE (branch `feat/t-bf255880-a-core`).
 *
 * Written to A's published contract so branch C's four pages and their two
 * real-Chrome journeys can be proved on their own. Delete this file at the
 * merge and keep A's.
 */

export interface Route {
  name: string;
  params: Record<string, string>;
  query: URLSearchParams;
}

const ROUTES: [string, string[]][] = [
  ["needs-you", []],
  ["all", ["all"]],
  ["decided", ["decided"]],
  ["boards", ["boards"]],
  ["board", ["board", ":project"]],
  ["lanes", ["lanes"]],
  ["sprints", ["sprints"]],
  ["board-sprints", ["sprints", ":project"]],
  ["sprint", ["sprint", ":project", ":id"]],
  ["plans", ["plans"]],
  ["deployments", ["deployments"]],
  ["deployment", ["deployment", ":project", ":id"]],
  ["subscriptions", ["subscriptions"]],
  ["search", ["search"]],
  ["task", ["task", ":project", ":id"]],
];

export function matchRoute(pathname: string, search: string): Route {
  const query = new URLSearchParams(search);
  const segments = pathname
    .split("/")
    .filter((segment) => segment.length > 0)
    .map(decodeURIComponent);
  for (const [name, pattern] of ROUTES) {
    if (pattern.length !== segments.length) {
      continue;
    }
    const params: Record<string, string> = {};
    let matched = true;
    for (const [index, part] of pattern.entries()) {
      const segment = segments[index] ?? "";
      if (part.startsWith(":")) {
        params[part.slice(1)] = segment;
      } else if (part !== segment) {
        matched = false;
        break;
      }
    }
    if (matched) {
      return { name, params, query };
    }
  }
  return { name: "not-found", params: {}, query };
}

export function navigate(href: string): void {
  history.pushState(null, "", href);
  window.dispatchEvent(new CustomEvent("routechange"));
}

import { useEffect, useState } from "react";

export function useRoute(): Route {
  const [route, setRoute] = useState(() =>
    matchRoute(location.pathname, location.search),
  );
  useEffect(() => {
    const read = () => setRoute(matchRoute(location.pathname, location.search));
    window.addEventListener("popstate", read);
    window.addEventListener("routechange", read);
    return () => {
      window.removeEventListener("popstate", read);
      window.removeEventListener("routechange", read);
    };
  }, []);
  return route;
}
