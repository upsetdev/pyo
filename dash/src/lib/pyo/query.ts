import { useCallback, useEffect, useRef, useState } from "react";

// Reads that don't live in the store snapshot (analytics). Each key's latest
// result is kept in memory, so coming back to a page shows it at once and
// refreshes it in the background. Loading only shows the first time a key is
// fetched. Nothing is written to the browser's storage: a full page load,
// which signing out is, starts empty.

export type QueryState<T> =
  | { status: "loading"; data?: undefined }
  | { status: "error"; error: string; data?: undefined }
  | { status: "success"; data: T };

/** Newer than this, coming back to a page doesn't fetch again. */
const FRESH_MS = 10_000;
const MAX_ENTRIES = 100;

const cache = new Map<string, { data: unknown; at: number }>();

function remember(key: string, data: unknown) {
  // Oldest first: a Map keeps insertion order.
  cache.delete(key);
  cache.set(key, { data, at: Date.now() });
  if (cache.size > MAX_ENTRIES) cache.delete(cache.keys().next().value!);
}

function cached<T>(key: string): QueryState<T> {
  const hit = cache.get(key);
  return hit ? { status: "success", data: hit.data as T } : { status: "loading" };
}

export function useQuery<T>(
  key: string,
  fetcher: () => Promise<T>,
  opts: { refreshMs?: number } = {},
) {
  const [state, setState] = useState<QueryState<T>>(() => cached<T>(key));
  const [attempt, setAttempt] = useState(0);
  const retried = useRef(0);
  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const hit = cache.get(key);
    setState(cached<T>(key));
    const load = async () => {
      // With data on screen, a failed refresh keeps it there.
      const showErrors = !cache.has(key);
      try {
        const data = await fetcherRef.current();
        remember(key, data);
        if (!cancelled) setState({ status: "success", data });
      } catch (e) {
        if (!cancelled && showErrors)
          setState({
            status: "error",
            error: e instanceof Error ? e.message : "Couldn't load.",
          });
      }
      if (!cancelled && opts.refreshMs)
        timer = setTimeout(() => void load(), opts.refreshMs);
    };
    const forced = attempt !== retried.current;
    retried.current = attempt;
    const age = hit ? Date.now() - hit.at : Infinity;
    if (forced || age >= FRESH_MS) void load();
    else if (opts.refreshMs)
      timer = setTimeout(() => void load(), FRESH_MS - age);
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [key, attempt, opts.refreshMs]);

  const retry = useCallback(() => setAttempt((a) => a + 1), []);
  return { ...state, retry } as QueryState<T> & { retry: () => void };
}
