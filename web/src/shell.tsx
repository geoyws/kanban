/**
 * What every page of this application has in common: the bar, the drawer,
 * the one socket, and the way a page gets its rows.
 *
 * The deck and the read pages share the bar and the drawer rather than each
 * rendering their own, because "which destination am I on" is one question
 * with one answer and two navigations that drifted would be two products
 * (SPA-38). They share the socket for a blunter reason: a second `/live`
 * connection per mounted page is a second connection the server has to
 * fan out to, opened and dropped on every route change.
 *
 * The bar, the backdrop and the drawer are rendered as SIBLINGS of the
 * page's own `<main>`, never inside it. A read page carries no controls, and
 * a drawer search field inside `main` would make every read page look like
 * one that does.
 */

import type { ReactElement, ReactNode } from "react";
import { createContext, useCallback, useContext, useEffect, useState } from "react";
import { fetchJson } from "./api";
import { navigate } from "./router";

/**
 * Every destination the drawer lists, as `[href, route name, label]`.
 *
 * The route names are `router.ts`'s, so "which one is current" is a
 * comparison rather than a second path-matching rule. `/all` is the one
 * server-rendered page left in the list and is followed as an ordinary
 * link.
 */
const DESTINATIONS: readonly (readonly [string, string, string])[] = [
  ["/", "needs-you", "Needs you"],
  ["/all", "all", "All open"],
  ["/decided", "decided", "Recent decisions"],
  ["/lanes", "lanes", "Lanes"],
  ["/boards", "boards", "Boards"],
  ["/sprints", "sprints", "Sprints"],
  ["/plans", "plans", "Plans"],
  ["/deployments", "deployments", "Deployments"],
  ["/subscriptions", "subscriptions", "Subscriptions"],
];

/**
 * How many times the boards have moved since this document loaded.
 *
 * A number rather than an event, because a page that mounts after a refresh
 * has already missed the event and would go on showing rows from before it.
 */
const RefreshSignal = createContext(0);

export const RefreshProvider = RefreshSignal.Provider;

/** The refresh counter, for a page that reloads something of its own. */
export function useRefreshSignal(): number {
  return useContext(RefreshSignal);
}

/** A projection being read: the rows, or why they are not here. */
export interface Projection<T> {
  data: T | null;
  error: string | null;
  reload: () => void;
}

/**
 * Read one projection, and read it again whenever the boards move.
 *
 * The refusal is kept as a sentence rather than thrown: a page whose data
 * did not arrive has to say so on screen, and a board that refused to name
 * itself (`404`, the one non-enumerating denial) is a legitimate answer a
 * page must render rather than crash on.
 */
export function useProjection<T>(path: string): Projection<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [asked, setAsked] = useState(0);
  const refreshes = useRefreshSignal();
  const reload = useCallback(() => setAsked((count) => count + 1), []);
  // biome-ignore lint/correctness/useExhaustiveDependencies: the two counters ARE the trigger — a read is re-run on them, not derived from them
  useEffect(() => {
    let live = true;
    fetchJson<T>(path)
      .then((rows) => {
        if (live) {
          setData(rows);
          setError(null);
        }
      })
      .catch((failure: unknown) => {
        if (!live) {
          return;
        }
        // A refused read leaves the rows that are on screen alone: the
        // operator is reading them, and replacing a page with a sentence
        // because one poll failed loses their place.
        setError(
          failure instanceof Error
            ? `This page could not be read (${failure.message}).`
            : "This page could not be read.",
        );
      });
    return () => {
      live = false;
    };
  }, [path, asked, refreshes]);
  return { data, error, reload };
}

/** What a page tells the shell about itself. */
export interface ChromeProps {
  /** The route name, which is how the drawer marks where you are. */
  current: string;
  /** The connection line's one word. */
  live: string;
  drawerOpen: boolean;
  onDrawer: (open: boolean) => void;
  /** Whether anything at all is over the page — the drawer, or a panel. */
  covered: boolean;
  onUncover: () => void;
}

/**
 * The bar, the backdrop and the drawer, in that document order.
 *
 * `[data-primary-nav]` is the bar; the destinations live behind the
 * hamburger because eight of them across the top of a 390px screen was the
 * whole top edge spent on navigation nobody uses while deciding (George,
 * 2026-09-17).
 */
export function Chrome(props: ChromeProps): ReactElement {
  const { current, live, drawerOpen, onDrawer, covered, onUncover } = props;
  return (
    <>
      <nav aria-label="Primary" data-primary-nav>
        <button
          type="button"
          className="menu"
          data-menu
          data-testid="nav-menu"
          aria-label="Menu"
          aria-expanded={drawerOpen}
          aria-controls="nav-drawer"
          onClick={() => onDrawer(!drawerOpen)}
        >
          <span className="bars" aria-hidden="true" />
        </button>
        <a
          className="brand"
          href="/"
          aria-label="Kanban home"
          onClick={(event) => {
            if (event.metaKey || event.ctrlKey || event.shiftKey) {
              return;
            }
            event.preventDefault();
            navigate("/");
          }}
        >
          kb
        </a>
        {/* The page's one status region: `<output>` IS `role=status`, so
            the attribute the server-rendered span writes is left off here
            rather than repeated. One status, one log, no third. */}
        <output className="live" data-live data-testid="deck-live" aria-live="polite">
          {live}
        </output>
      </nav>
      {/* The backdrop is the pointer's way out of the menu and the history;
          the keyboard has `Escape` and each panel's own toggle. */}
      {/* biome-ignore lint/a11y/noStaticElementInteractions: pointer shortcut, keyboard has Escape */}
      {/* biome-ignore lint/a11y/useKeyWithClickEvents: pointer shortcut, keyboard has Escape */}
      <div className="backdrop" data-backdrop hidden={!covered} onClick={onUncover} />
      <nav
        className="drawer"
        id="nav-drawer"
        data-drawer
        data-testid="nav-drawer"
        aria-label="Destinations"
        hidden={!drawerOpen}
      >
        <form action="/search" method="get" data-nav-search>
          <input name="q" aria-label="Search Kanban" placeholder="Search" />
        </form>
        <div className="nav-links">
          {DESTINATIONS.map(([href, name, label]) => (
            <a
              key={name}
              href={href}
              data-nav={name}
              // Which destination this is, marked by a rule rather than a
              // fill, and read off the route rather than the address bar:
              // one shell serves every page.
              {...(name === current ? { "aria-current": "page" as const } : {})}
              onClick={(event) => {
                onDrawer(false);
                // `/all` is still a served page, and a modified click is the
                // reader asking the browser for a tab of their own.
                if (
                  name === "all" ||
                  event.metaKey ||
                  event.ctrlKey ||
                  event.shiftKey
                ) {
                  return;
                }
                event.preventDefault();
                navigate(href);
              }}
            >
              {label}
            </a>
          ))}
        </div>
      </nav>
    </>
  );
}

/** One in-application link: the same anchor, without the round trip. */
export function Link(props: {
  href: string;
  children: ReactNode;
  className?: string;
  [key: `data-${string}`]: string | boolean | undefined;
}): ReactElement {
  const { href, children, className, ...rest } = props;
  return (
    <a
      href={href}
      {...(className === undefined ? {} : { className })}
      {...rest}
      onClick={(event) => {
        if (event.metaKey || event.ctrlKey || event.shiftKey) {
          return;
        }
        event.preventDefault();
        navigate(href);
      }}
    >
      {children}
    </a>
  );
}
