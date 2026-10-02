import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * Minimal async data hook.
 *
 * No react-query-style dependency: this project has small data volumes and low
 * refresh rates, so a hook with a `reload` is enough — and it avoids adding
 * "state management" cognitive load.
 */
function depsEqual(a: unknown[], b: unknown[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (!Object.is(a[i], b[i])) return false;
  }
  return true;
}

export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fetching, setFetching] = useState(false);
  const mounted = useRef(true);
  // The deps of the most recent *successful* fetch. `loading` is derived from this plus `fetching`,
  // so it never depends on the timing of a setState during render — which is what removes the
  // empty-state flash on first paint and when switching perspective.
  const fetchedDeps = useRef<unknown[] | null>(null);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  /**
   * @param silent Silent refresh: does not set `fetching`, so no spinner flashes and the table is not masked.
   *   Used for **polling** — the spinner next to "indexing" in the list should mean "really making
   *   progress", not "mask the whole table every 3 seconds".
   */
  const run = useCallback(
    async (silent = false) => {
      if (!silent) setFetching(true);
      setError(null);
      try {
        const result = await fn();
        if (mounted.current) setData(result);
        return result;
      } catch (e) {
        if (mounted.current) setError(e instanceof Error ? e.message : String(e));
        return null;
      } finally {
        if (mounted.current) {
          // Mark this round of deps as settled either way: success → data ready; failure → stop the
          // spinner and show the error, so it can never spin forever.
          fetchedDeps.current = deps;
          if (!silent) setFetching(false);
        }
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    deps,
  );

  useEffect(() => {
    void run();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [run]);

  // First frame (fetchedDeps null) or deps changed → data not ready → loading true (spinner);
  // ORed with `fetching` (an active pull). Derived synchronously, so the very first frame is
  // already true and an empty state is never shown.
  const ready = fetchedDeps.current !== null && depsEqual(fetchedDeps.current, deps);
  const loading = !ready || fetching;

  const reload = useCallback(() => run(false), [run]);
  const silentReload = useCallback(() => run(true), [run]);

  return { data, loading, error, reload, silentReload };
}

/** Polling: used for graph-build progress. */
export function usePolling(fn: () => Promise<unknown>, intervalMs: number, active: boolean) {
  const saved = useRef(fn);
  useEffect(() => {
    saved.current = fn;
  }, [fn]);

  useEffect(() => {
    if (!active) return;
    const id = window.setInterval(() => {
      void saved.current();
    }, intervalMs);
    void saved.current();
    return () => window.clearInterval(id);
  }, [intervalMs, active]);
}
