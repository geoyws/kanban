/**
 * SCRATCH COPY OF WRITER A's MODULE (branch `feat/t-bf255880-a-core`).
 *
 * Written to A's published contract so branch C's four pages and their two
 * real-Chrome journeys can be proved on their own. Delete this file at the
 * merge and keep A's.
 */

import { useCallback, useEffect, useState } from "react";
import { fetchJson } from "./api";
import { connectLive } from "./live";

let signal = 0;
const listeners = new Set<(value: number) => void>();
let stopLive: (() => void) | null = null;

export function useRefreshSignal(): number {
  const [value, setValue] = useState(signal);
  useEffect(() => {
    listeners.add(setValue);
    if (stopLive === null) {
      stopLive = connectLive({
        onStatus: () => undefined,
        onNotice: () => false,
        onRefresh: () => {
          signal += 1;
          for (const listener of listeners) {
            listener(signal);
          }
        },
      });
    }
    return () => {
      listeners.delete(setValue);
    };
  }, []);
  return value;
}

export function useProjection<T>(path: string): {
  data: T | null;
  error: string | null;
  reload: () => void;
} {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [asked, setAsked] = useState(0);
  const refreshes = useRefreshSignal();
  const reload = useCallback(() => setAsked((count) => count + 1), []);
  // biome-ignore lint/correctness/useExhaustiveDependencies: asked and refreshes are the re-read triggers
  useEffect(() => {
    let live = true;
    fetchJson<T>(path)
      .then((answer) => {
        if (live) {
          setData(answer);
          setError(null);
        }
      })
      .catch((failure: Error) => {
        if (live) {
          setError(failure.message);
        }
      });
    return () => {
      live = false;
    };
  }, [path, asked, refreshes]);
  return { data, error, reload };
}
