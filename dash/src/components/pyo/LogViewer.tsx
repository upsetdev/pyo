import { CopyIcon, PauseIcon, PlayIcon } from "@phosphor-icons/react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Button } from "~/components/ui/Button";
import { ZoomButton, Zoomable } from "~/components/ui/Zoom";
import { Select } from "~/components/ui/Field";
import { SearchInput } from "~/components/ui/Table";
import { fetchServiceLogs } from "~/lib/pyo/backend";
import { clockMs } from "~/lib/pyo/format";
import { toast } from "~/lib/pyo/toast";
import { senderColor } from "~/lib/pyo/logColors";
import type { Application, LogEntry, LogLevel } from "~/lib/pyo/types";
import { copyText } from "~/lib/pyo/clipboard";
import { stripAnsi } from "~/lib/pyo/ansi";
import { AnsiText } from "~/components/pyo/AnsiText";

const LEVEL_CLASS: Record<LogLevel, string> = {
  debug: "text-muted-foreground",
  info: "text-primary",
  warn: "text-warning",
  error: "text-destructive",
};

/** Monospaced log lines; click to select lines for copying. */
export function LogLines({
  entries,
  query = "",
  selected,
  onToggle,
  follow = true,
  height = "h-[420px]",
  empty = "No log lines.",
  wrap = false,
  compact = false,
}: {
  entries: LogEntry[];
  query?: string;
  selected?: Set<string>;
  onToggle?: (id: string) => void;
  follow?: boolean;
  height?: string;
  empty?: string;
  /** Wrap long lines on every screen size, not only on phones. */
  wrap?: boolean;
  /** A narrow panel: no sender column, lines wrap. */
  compact?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // Follow new lines while you're at the bottom; scrolled up, stay put.
  const atBottom = useRef(true);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || !follow || !atBottom.current) return;
    el.scrollTop = el.scrollHeight;
    const f = requestAnimationFrame(() => {
      el.scrollTop = el.scrollHeight;
    });
    return () => cancelAnimationFrame(f);
  }, [entries, follow]);
  const q = query.toLowerCase();
  return (
    <div
      ref={ref}
      onScroll={(e) => {
        const el = e.currentTarget;
        atBottom.current =
          el.scrollHeight - el.scrollTop - el.clientHeight < 24;
      }}
      className={`${height} overflow-auto bg-background py-2 font-mono text-[12px] leading-[20px]`}
      role="log"
      aria-live="off"
    >
      {entries.length ? (
        entries.map((e) => {
          const sel = selected?.has(e.id);
          return (
            <div
              key={e.id}
              onClick={onToggle ? () => onToggle(e.id) : undefined}
              className={`flex items-start ${compact ? "gap-2 px-3" : "gap-3 px-4"} ${wrap || compact ? "whitespace-pre-wrap [overflow-wrap:anywhere]" : "whitespace-pre-wrap break-all sm:whitespace-pre sm:break-normal"} ${onToggle ? "cursor-pointer" : ""} ${sel ? "bg-primary/15" : "hover:bg-accent/40"}`}
            >
              <span className="shrink-0 select-none text-muted-foreground/70">
                {clockMs(e.at)}
              </span>
              <span
                className={`w-11 shrink-0 select-none uppercase ${LEVEL_CLASS[e.level]}`}
              >
                {e.level}
              </span>
              {/* Phones: time, level and message only, wrapped. */}
              <span
                className={`hidden w-28 shrink-0 truncate ${compact ? "" : "sm:inline"} ${senderColor(e.source)}`}
              >
                {e.source}
              </span>
              <span
                className={`text-foreground ${wrap || compact ? "min-w-0 flex-1" : ""}`}
              >
                <AnsiText text={e.message} query={q} />
              </span>
            </div>
          );
        })
      ) : (
        <p className="px-4 py-6 font-sans text-[12.5px] text-muted-foreground">
          {empty}
        </p>
      )}
    </div>
  );
}

export function copyLines(entries: LogEntry[]) {
  const text = entries
    .map(
      (e) =>
        `${new Date(e.at).toISOString()} ${e.level.toUpperCase()} ${e.source} ${stripAnsi(e.message)}`,
    )
    .join("\n");
  copyText(text)
    .then(() =>
      toast({
        title: `Copied ${entries.length} line${entries.length === 1 ? "" : "s"}`,
      }),
    )
    .catch(() => toast({ tone: "error", title: "Clipboard unavailable" }));
}

/**
 * Recent container output of a service's pods, read from the cluster by its
 * agent. Re-fetched every few seconds while not paused.
 */
const WRAP_KEY = "pyo.logs.wrap";

/** Whether log lines wrap, remembered in this browser (on by default). */
export function useWrapLines() {
  const [wrap, setWrap] = useState(() => {
    try {
      return localStorage.getItem(WRAP_KEY) !== "0";
    } catch {
      return true;
    }
  });
  const set = (v: boolean) => {
    setWrap(v);
    try {
      localStorage.setItem(WRAP_KEY, v ? "1" : "0");
    } catch {
      // Storage blocked: the choice lasts for this page only.
    }
  };
  return [wrap, set] as const;
}

/** A small switch for it, for the bottom-right corner of a log. Phones always wrap, so it's hidden there. */
export function WrapSwitch({
  wrap,
  onChange,
}: {
  wrap: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={wrap}
      onClick={() => onChange(!wrap)}
      className="ml-auto hidden shrink-0 items-center gap-1.5 text-[11.5px] text-muted-foreground transition-colors hover:text-foreground sm:inline-flex"
    >
      <span
        className={`relative inline-flex h-3 w-[22px] items-center rounded-full transition-colors ${wrap ? "bg-primary" : "bg-muted-foreground/30"}`}
      >
        <span
          className={`absolute h-2 w-2 rounded-full bg-white shadow-sm transition-transform ${wrap ? "translate-x-3" : "translate-x-0.5"}`}
        />
      </span>
      Wrap lines
    </button>
  );
}

export function ServiceLogViewer({ app }: { app: Application }) {
  const running = app.status !== "stopped" && !!app.currentDeploymentId;
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const [loading, setLoading] = useState(running);
  const [error, setError] = useState("");
  // Paused: the screen holds still while new lines keep arriving.
  const [frozen, setFrozen] = useState<LogEntry[] | null>(null);
  const paused = !!frozen;
  const [query, setQuery] = useState("");
  const [level, setLevel] = useState<"all" | LogLevel>("all");
  const [selected, setSelected] = useState<Set<string>>(new Set());

  useEffect(() => {
    if (!running) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const load = async () => {
      try {
        const res = await fetchServiceLogs(app.id);
        if (cancelled) return;
        // Stable ids so a re-fetch keeps selection on the same lines.
        setEntries(
          res.logs.map((l, i) => ({ ...l, id: `${l.source}:${l.at}:${i}` })),
        );
        setError("");
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : "Couldn't load logs.");
      } finally {
        if (!cancelled) {
          setLoading(false);
          timer = setTimeout(load, 3000);
        }
      }
    };
    void load();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [app.id, running]);

  const lastFrozen = frozen?.at(-1)?.at ?? 0;
  const waiting = frozen ? entries.filter((e) => e.at > lastFrozen).length : 0;
  const shown = useMemo(
    () =>
      (frozen ?? entries).filter(
        (e) =>
          (level === "all" || e.level === level) &&
          (!query ||
            stripAnsi(e.message).toLowerCase().includes(query.toLowerCase()) ||
            e.source.includes(query)),
      ),
    [entries, frozen, level, query],
  );

  const toggle = (id: string) =>
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });

  const [zoomed, setZoomed] = useState(false);
  const [wrap, setWrap] = useWrapLines();
  return (
    <Zoomable zoomed={zoomed} onClose={() => setZoomed(false)} label="Logs">
      <div className={zoomed ? "flex min-h-0 flex-1 flex-col" : ""}>
        <div className="flex flex-wrap items-center gap-2 border-b border-border px-4 py-2.5">
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder="Search logs"
            className="w-full sm:w-64"
          />
          <Select
            value={level}
            onChange={(e) => setLevel(e.target.value as "all" | LogLevel)}
            aria-label="Level"
            className="w-auto"
          >
            <option value="all">All levels</option>
            <option value="error">Error</option>
            <option value="warn">Warning</option>
            <option value="info">Info</option>
            <option value="debug">Debug</option>
          </Select>
          <div className="ml-auto flex items-center gap-2">
            {running ? (
              <button
                type="button"
                aria-pressed={paused}
                aria-label={paused ? "Resume" : "Pause"}
                title={paused ? "Resume" : error ? `Pause · ${error}` : "Pause"}
                onClick={() => setFrozen(paused ? null : entries)}
                className={`inline-flex h-7 items-center gap-1.5 rounded-md border px-2 text-[12px] font-medium transition-colors ${paused ? "border-warning/40 bg-warning/15 text-warning hover:bg-warning/25" : "border-border text-muted-foreground hover:bg-accent hover:text-foreground"}`}
              >
                {paused ? (
                  <PlayIcon size={12} weight="fill" />
                ) : (
                  <PauseIcon size={12} weight="fill" />
                )}
                {paused && waiting ? (
                  <span className="tabular-nums">{waiting} new</span>
                ) : null}
              </button>
            ) : null}
            <Button
              variant="secondary"
              size="xs"
              disabled={!shown.length}
              onClick={() =>
                copyLines(
                  selected.size
                    ? entries.filter((e) => selected.has(e.id))
                    : shown,
                )
              }
            >
              <CopyIcon size={12} />
              {selected.size
                ? `Copy ${selected.size} selected`
                : "Copy visible"}
            </Button>
            <ZoomButton zoomed={zoomed} onClick={() => setZoomed((z) => !z)} />
          </div>
        </div>
        <LogLines
          entries={shown}
          query={query}
          wrap={wrap}
          height={zoomed ? "min-h-0 flex-1" : undefined}
          selected={selected}
          onToggle={toggle}
          follow={!paused}
          empty={
            !running
              ? "Not running. Deploy it to see logs."
              : error
                ? error
                : loading
                  ? "Loading logs from the cluster…"
                  : entries.length
                    ? "No lines match the filter."
                    : "No output yet."
          }
        />
        <div className="flex items-center gap-3 border-t border-border px-4 py-2">
          <p className="text-[11.5px] text-muted-foreground">
            Last 300 lines. Click lines to select them.
          </p>
          <WrapSwitch wrap={wrap} onChange={setWrap} />
        </div>
      </div>
    </Zoomable>
  );
}
