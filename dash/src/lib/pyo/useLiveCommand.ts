import { useEffect, useRef, useState } from "react";

type Result<T> = { data: T } | { error: string };

/**
 * Asks for an install command as a form is filled in: `key` changes when an
 * input does, and once it's been still for a moment (and `ready`), `fetch`
 * runs. Answers to older keys are dropped, so the command shown always
 * matches the inputs.
 */
export function useLiveCommand<T>(
  key: string,
  ready: boolean,
  fetch: () => Promise<Result<T>>,
  delay = 450,
) {
  const [state, setState] = useState<{
    key: string;
    data?: T;
    error?: string;
  } | null>(null);
  const latest = useRef(key);
  latest.current = key;
  const run = useRef(fetch);
  run.current = fetch;

  useEffect(() => {
    if (!ready) return;
    const t = setTimeout(async () => {
      const r = await run.current();
      if (latest.current !== key) return;
      setState("data" in r ? { key, data: r.data } : { key, error: r.error });
    }, delay);
    return () => clearTimeout(t);
  }, [key, ready, delay]);

  const current = state?.key === key ? state : null;
  return {
    data: state?.data,
    /** The last answer was for other inputs: a new one is on its way. */
    stale: ready && !current,
    error: ready ? current?.error : undefined,
  };
}
