import { useSyncExternalStore } from "react";

// Tiny toast queue. Callable from anywhere (including the mock store), read
// by <Toaster /> in the app shell.
//
//   toast({ title, description, tone, action })
//   toast.error("Couldn't save", { description })
//   const id = toast.loading("Deploying…"); toast.success("Deployed", { id })
//   toast.promise(work, { loading, success, error })
//
// Each toast runs its own clock; the clocks stop while anything holds them
// (the stack is hovered or focused, the tab is hidden) and resume with the
// time they had left.

export type ToastTone = "success" | "error" | "info" | "warning" | "loading";

export type ToastAction = {
  label: string;
  /** A link; the toast closes when it's followed. */
  href?: string;
  onClick?: () => void;
};

export type Toast = {
  id: number;
  tone: ToastTone;
  title: string;
  description?: string;
  action?: ToastAction;
  /** Auto-dismiss delay in ms; null stays until updated or dismissed. */
  timeout: number | null;
  /** Fading out; removed after TOAST_LEAVE_MS. */
  leaving: boolean;
  /** Bumped on every update. */
  rev: number;
};

export type ToastInput = {
  title: string;
  description?: string;
  tone?: ToastTone;
  action?: ToastAction;
  timeout?: number | null;
  /** Replace this toast's content instead of adding one. */
  id?: number;
};

type Options = Omit<ToastInput, "title" | "tone">;

/** How long a dismissed toast stays mounted for its exit. */
export const TOAST_LEAVE_MS = 200;
const LIMIT = 5;

let list: Toast[] = [];
let nextId = 1;
const subscribers = new Set<() => void>();
function publish(next: Toast[]) {
  list = next;
  subscribers.forEach((fn) => fn());
}

type Clock = { left: number; since: number; handle?: ReturnType<typeof setTimeout> };
const clocks = new Map<number, Clock>();
const holds = new Set<string>();

function run(id: number) {
  const c = clocks.get(id);
  if (!c || c.handle || holds.size) return;
  c.since = Date.now();
  c.handle = setTimeout(() => dismissToast(id), Math.max(c.left, 0));
}

function halt(id: number) {
  const c = clocks.get(id);
  if (!c?.handle) return;
  clearTimeout(c.handle);
  c.handle = undefined;
  c.left -= Date.now() - c.since;
}

function stopClock(id: number) {
  halt(id);
  clocks.delete(id);
}

/** Stops (or restarts) every toast's clock while `reason` holds. */
export function holdToasts(reason: string, on: boolean) {
  const before = holds.size > 0;
  if (on) holds.add(reason);
  else holds.delete(reason);
  const after = holds.size > 0;
  if (before === after) return;
  for (const id of clocks.keys()) (after ? halt : run)(id);
}

function put(input: ToastInput) {
  const tone = input.tone ?? "success";
  const id = input.id ?? nextId++;
  nextId = Math.max(nextId, id + 1);
  const prev = list.find((t) => t.id === id);
  const entry: Toast = {
    id,
    tone,
    title: input.title,
    description: input.description,
    action: input.action,
    timeout:
      input.timeout !== undefined
        ? input.timeout
        : tone === "loading"
          ? null
          : tone === "error"
            ? 8000
            : 5000,
    leaving: false,
    rev: (prev?.rev ?? 0) + 1,
  };
  // Newest first.
  publish(prev ? list.map((t) => (t.id === id ? entry : t)) : [entry, ...list]);

  stopClock(id);
  if (entry.timeout !== null) {
    clocks.set(id, { left: entry.timeout, since: 0 });
    run(id);
  }
  for (const old of list.filter((t) => !t.leaving).slice(LIMIT)) dismissToast(old.id);
  return id;
}

export function dismissToast(id: number) {
  if (!list.some((t) => t.id === id && !t.leaving)) return;
  stopClock(id);
  publish(list.map((t) => (t.id === id ? { ...t, leaving: true } : t)));
  setTimeout(() => {
    // An update may have brought it back meanwhile.
    if (list.some((t) => t.id === id && t.leaving))
      publish(list.filter((t) => t.id !== id));
  }, TOAST_LEAVE_MS);
}

const toned =
  (tone: ToastTone) =>
  (title: string, options: Options = {}) =>
    put({ ...options, title, tone });

const message = <A,>(m: string | ((arg: A) => string), arg: A) =>
  typeof m === "function" ? m(arg) : m;

export const toast = Object.assign((input: ToastInput) => put(input), {
  success: toned("success"),
  error: toned("error"),
  info: toned("info"),
  warning: toned("warning"),
  loading: toned("loading"),
  /** A loading toast that turns into success or error when `work` settles. */
  promise<T>(
    work: Promise<T>,
    m: {
      loading: string;
      success: string | ((value: T) => string);
      error: string | ((err: unknown) => string);
    },
  ) {
    const id = put({ title: m.loading, tone: "loading" });
    work.then(
      (v) => put({ id, tone: "success", title: message(m.success, v) }),
      (e) => put({ id, tone: "error", title: message(m.error, e) }),
    );
    return work;
  },
  dismiss: dismissToast,
});

const NONE: Toast[] = [];

export function useToasts() {
  return useSyncExternalStore(
    (fn) => {
      subscribers.add(fn);
      return () => subscribers.delete(fn);
    },
    () => list,
    () => NONE,
  );
}
