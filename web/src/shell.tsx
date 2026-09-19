/**
 * LOCAL CONTRACT COPY — writer A (`feat/t-bf255880-a-core`) owns this file.
 *
 * Written here, to the interface A ruled on, only so this branch's pages can
 * be driven at their real addresses before A's branch merges. A's copy wins
 * at the merge and this one is deleted. The exports and their shapes are
 * A's: `useRefreshSignal` bumps on every `/live` `refresh` frame, and
 * `useProjection` reads one JSON route, refetching when the signal moves.
 */

import { createContext, useCallback, useContext, useEffect, useState } from "react";

/** How many times the boards have moved since this document loaded. */
export const RefreshSignal = createContext(0);

export function useRefreshSignal(): number {
  return useContext(RefreshSignal);
}

/** One JSON route, read again whenever the boards move or the page asks. */
export function useProjection<T>(path: string): {
  data: T | null;
  error: string | null;
  reload: () => void;
} {
  const signal = useRefreshSignal();
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [asked, setAsked] = useState(0);
  const reload = useCallback(() => setAsked((count) => count + 1), []);
  // biome-ignore lint/correctness/useExhaustiveDependencies: the refresh signal and the manual ask ARE the refetch triggers
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const response = await fetch(path, {
          credentials: "same-origin",
          headers: { Accept: "application/json" },
        });
        if (!response.ok) {
          throw new Error(`${path} ${response.status}`);
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
  }, [path, signal, asked]);
  return { data, error, reload };
}
