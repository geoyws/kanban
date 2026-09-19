/**
 * What every read page needs from the shell: its projection, refetched when
 * the boards move.
 *
 * WRITER A OWNS THIS FILE. This copy is written to writer A's published
 * contract (`t-bf255880`, 2026-09-19) so the search and task pages can be
 * developed and proved before A's branch merges; when it does, A's copy is
 * the one that stays. One socket serves the whole application, so a page
 * never calls `connectLive` itself — it reads the signal below.
 */

import { useCallback, useEffect, useState } from "react";

/** The event the shell fires on every `/live` `refresh` frame. */
const REFRESH_SIGNAL = "kb:refresh";

/** Announce that the boards moved. Called by the shell's one socket. */
export function announceRefresh(): void {
  window.dispatchEvent(new CustomEvent(REFRESH_SIGNAL));
}

/** A counter that bumps on every `refresh` frame, for pages to depend on. */
export function useRefreshSignal(): number {
  const [signal, setSignal] = useState(0);
  useEffect(() => {
    const bump = () => setSignal((previous) => previous + 1);
    window.addEventListener(REFRESH_SIGNAL, bump);
    return () => window.removeEventListener(REFRESH_SIGNAL, bump);
  }, []);
  return signal;
}

/**
 * One page's projection: the rows, or the one sentence that says why there
 * are none. A refusal is the served surface's own status, never a stack: a
 * board that is unknown, retired or unreadable to this principal answers
 * `404` with the same body, and the page says so in its own words.
 */
export function useProjection<T>(path: string): {
  data: T | null;
  error: string | null;
  reload: () => void;
} {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const signal = useRefreshSignal();
  const reload = useCallback(() => setAttempt((previous) => previous + 1), []);
  // `attempt` and `signal` are not read inside the effect: they ARE the
  // reasons to run it again — a reload the page asked for, and a `refresh`
  // frame saying the boards moved.
  // biome-ignore lint/correctness/useExhaustiveDependencies: refetch triggers
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const response = await fetch(path, {
          credentials: "same-origin",
          headers: { Accept: "application/json" },
        });
        if (!response.ok) {
          throw new Error(
            response.status === 404
              ? "Nothing here to read. The board may be retired, or it may never have existed."
              : "The page could not be read. Reload to try again.",
          );
        }
        const body = (await response.json()) as T;
        if (live) {
          setData(body);
          setError(null);
        }
      } catch (failure) {
        if (live) {
          setError(failure instanceof Error ? failure.message : String(failure));
        }
      }
    })();
    return () => {
      live = false;
    };
  }, [path, attempt, signal]);
  return { data, error, reload };
}
