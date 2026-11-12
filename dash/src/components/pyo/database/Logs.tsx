import { CaretRightIcon, CopyIcon, PauseIcon, PlayIcon } from "@phosphor-icons/react";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { z } from "zod";
import { copyLines, useWrapLines, WrapSwitch } from "~/components/pyo/LogViewer";
import { Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Segmented, Select } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { ZoomButton, Zoomable } from "~/components/ui/Zoom";
import { fetchDatabaseLogs, type DbEvent, type DbLogLine, type DbRole } from "~/lib/pyo/backend";
import { clockMs, dateTime } from "~/lib/pyo/format";
import { senderColor } from "~/lib/pyo/logColors";
import { stripAnsi } from "~/lib/pyo/ansi";
import { AnsiText } from "~/components/pyo/AnsiText";
import type { LogLevel, Database } from "~/lib/pyo/types";
import { useDatabase } from "./DatabaseLayout";

export const logsSearch = z.object({ view: z.enum(["logs", "events"]).optional().catch(undefined) });

export function DatabaseLogsPage() {
  const d = useDatabase();
  return d ? <DatabaseLogs key={d.id} d={d} /> : null;
}

const LEVEL_CLASS: Record<LogLevel, string> = {
  debug: "text-muted-foreground",
  info: "text-primary",
  warn: "text-warning",
  error: "text-destructive",
};

const ROLE_LABEL: Record<DbRole, string> = {
  primary: "Primary",
  replica: "Replicas",
  read: "Read replicas",
  pooler: "Pooler",
  "read-pooler": "Read pooler",
  sentinel: "Sentinels",
  proxy: "Proxy",
  backup: "Backups",
  import: "Import",
  other: "Other",
};

const RANGES = [
  { value: "live", label: "Live" },
  { value: "15m", label: "Last 15 minutes" },
  { value: "1h", label: "Last hour" },
  { value: "6h", label: "Last 6 hours" },
  { value: "24h", label: "Last 24 hours" },
];

/** The parts this database has, for the member filter. */
function rolesOf(d: Database): DbRole[] {
  const ha = d.instances > 1;
  const reads = (d.readReplicas?.instances ?? 0) > 0;
  const imported = d.import ? (["import"] as const) : [];
  // MySQL: its backup and restore jobs too.
  if (d.engine === "mysql") return ["primary", ...(ha ? (["replica"] as const) : []), "backup", ...imported];
  return d.engine === "postgres"
    ? ["primary", ...(ha ? (["replica"] as const) : []), ...(reads ? (["read"] as const) : []), ...(d.pooler ? (["pooler"] as const) : []), ...(reads ? (["read-pooler"] as const) : []), ...imported]
    : ["primary", ...(ha ? (["replica", "sentinel"] as const) : []), ...(reads ? (["read"] as const) : []), "proxy"];
}

type Line = DbLogLine & { id: string };

function DatabaseLogs({ d }: { d: Database }) {
  const navigate = useNavigate();
  const view = (useSearch({ strict: false }) as z.infer<typeof logsSearch>).view ?? "logs";
  const [range, setRange] = useState("live");
  const [lines, setLines] = useState<Line[]>([]);
  const [events, setEvents] = useState<DbEvent[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [frozen, setFrozen] = useState<Line[] | null>(null);
  const paused = !!frozen;
  const [query, setQuery] = useState("");
  const [role, setRole] = useState<"all" | DbRole>("all");
  const [level, setLevel] = useState<"all" | LogLevel>("all");
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [zoomed, setZoomed] = useState(false);
  const [wrap, setWrap] = useWrapLines();

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setLoading(true);
    const load = async () => {
      try {
        const r = await fetchDatabaseLogs(d.id, range);
        if (cancelled) return;
        setLines(r.logs.map((l, i) => ({ ...l, id: `${l.source}:${l.at}:${i}` })));
        setEvents(r.events);
        setError("");
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : "Couldn't load logs.");
      } finally {
        if (!cancelled) {
          setLoading(false);
          // Live: keep following. A time range is read once.
          if (range === "live") timer = setTimeout(load, 4000);
        }
      }
    };
    void load();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [d.id, range]);

  const roles = rolesOf(d);
  const q = query.toLowerCase();
  const shown = useMemo(
    () =>
      (frozen ?? lines).filter(
        (l) =>
          (role === "all" || l.role === role) &&
          (level === "all" || l.level === level) &&
          (!q || stripAnsi(l.message).toLowerCase().includes(q) || l.source.includes(q)),
      ),
    [lines, frozen, role, level, q],
  );
  const waiting = frozen ? lines.filter((l) => l.at > (frozen.at(-1)?.at ?? 0)).length : 0;
  const shownEvents = events.filter((e) => !q || `${e.reason} ${e.message} ${e.object ?? ""}`.toLowerCase().includes(q));

  return (
    <Panel>
      <Zoomable zoomed={zoomed} onClose={() => setZoomed(false)} label="Database logs">
        <div className={zoomed ? "flex min-h-0 flex-1 flex-col" : ""}>
          <div className="flex flex-wrap items-center gap-2 border-b border-border px-4 py-2.5">
            <Segmented
              label="View"
              value={view}
              onChange={(v) => void navigate({ to: ".", search: { view: v === "logs" ? undefined : v }, replace: true })}
              options={[
                { value: "logs", label: "Logs" },
                { value: "events", label: "Events" },
              ]}
            />
            <SearchInput value={query} onChange={setQuery} placeholder={view === "logs" ? "Search logs" : "Search events"} className="w-full sm:w-56" />
            {view === "logs" ? (
              <>
                <Select value={role} onChange={(e) => setRole(e.target.value as "all" | DbRole)} aria-label="Member" className="w-auto">
                  <option value="all">All members</option>
                  {roles.map((r) => (
                    <option key={r} value={r}>
                      {ROLE_LABEL[r]}
                    </option>
                  ))}
                </Select>
                <Select value={level} onChange={(e) => setLevel(e.target.value as "all" | LogLevel)} aria-label="Level" className="w-auto">
                  <option value="all">All levels</option>
                  <option value="error">Error</option>
                  <option value="warn">Warning</option>
                  <option value="info">Info</option>
                  <option value="debug">Debug</option>
                </Select>
                <Select
                  value={range}
                  onChange={(e) => {
                    setFrozen(null);
                    setRange(e.target.value);
                  }}
                  aria-label="Time range"
                  className="w-auto"
                >
                  {RANGES.map((r) => (
                    <option key={r.value} value={r.value}>
                      {r.label}
                    </option>
                  ))}
                </Select>
              </>
            ) : null}
            {view === "logs" ? (
              <div className="ml-auto flex items-center gap-2">
                {range === "live" ? (
                  <button
                    type="button"
                    aria-pressed={paused}
                    aria-label={paused ? "Resume" : "Pause"}
                    title={paused ? "Resume" : "Pause"}
                    onClick={() => setFrozen(paused ? null : lines)}
                    className={`inline-flex h-7 items-center gap-1.5 rounded-md border px-2 text-[12px] font-medium transition-colors ${paused ? "border-warning/40 bg-warning/15 text-warning hover:bg-warning/25" : "border-border text-muted-foreground hover:bg-accent hover:text-foreground"}`}
                  >
                    {paused ? <PlayIcon size={12} weight="fill" /> : <PauseIcon size={12} weight="fill" />}
                    {paused && waiting ? <span className="tabular-nums">{waiting} new</span> : null}
                  </button>
                ) : null}
                <Button variant="secondary" size="xs" disabled={!shown.length} onClick={() => copyLines(shown)}>
                  <CopyIcon size={12} />
                  Copy visible
                </Button>
                <ZoomButton zoomed={zoomed} onClick={() => setZoomed((z) => !z)} />
              </div>
            ) : null}
          </div>
          {view === "logs" ? (
            <Lines
              lines={shown}
              query={q}
              wrap={wrap}
              follow={range === "live" && !paused}
              open={open}
              onToggle={(id) =>
                setOpen((s) => {
                  const n = new Set(s);
                  if (n.has(id)) n.delete(id);
                  else n.add(id);
                  return n;
                })
              }
              height={zoomed ? "min-h-0 flex-1" : "h-[480px]"}
              empty={error || (loading ? "Loading logs from the cluster…" : lines.length ? "No lines match the filter." : "No output yet.")}
            />
          ) : (
            <Events events={shownEvents} empty={error || (loading ? "Loading events…" : "Nothing yet.")} />
          )}
          {view === "logs" ? (
            <div className="flex items-center gap-3 border-t border-border px-4 py-2">
              <p className="text-[11.5px] text-muted-foreground">
                {range === "live" ? "Live." : "Up to 3,000 lines."} Click a line with ▸ for its full record.
              </p>
              <WrapSwitch wrap={wrap} onChange={setWrap} />
            </div>
          ) : null}
        </div>
      </Zoomable>
    </Panel>
  );
}

function Lines({
  lines,
  query,
  wrap,
  follow,
  open,
  onToggle,
  height,
  empty,
}: {
  lines: Line[];
  query: string;
  wrap: boolean;
  follow: boolean;
  open: Set<string>;
  onToggle: (id: string) => void;
  height: string;
  empty: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && follow && atBottom.current) el.scrollTop = el.scrollHeight;
  }, [lines, follow]);
  return (
    <div
      ref={ref}
      onScroll={(e) => {
        const el = e.currentTarget;
        atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
      }}
      className={`${height} overflow-auto bg-background py-2 font-mono text-[12px] leading-[20px]`}
      role="log"
      aria-live="off"
    >
      {lines.length ? (
        lines.map((l) => {
          const expanded = open.has(l.id);
          return (
            <div key={l.id}>
              <div
                onClick={l.raw ? () => onToggle(l.id) : undefined}
                className={`flex items-start gap-3 px-4 ${wrap ? "whitespace-pre-wrap [overflow-wrap:anywhere]" : "whitespace-pre-wrap break-all sm:whitespace-pre sm:break-normal"} ${l.raw ? "cursor-pointer" : ""} hover:bg-accent/40`}
              >
                <span className="w-3 shrink-0 select-none pt-[3px] text-muted-foreground/60">
                  {l.raw ? <CaretRightIcon size={10} className={`transition-transform ${expanded ? "rotate-90" : ""}`} /> : null}
                </span>
                <span className="shrink-0 select-none text-muted-foreground/70">{clockMs(l.at)}</span>
                <span className={`w-11 shrink-0 select-none uppercase ${LEVEL_CLASS[l.level]}`}>{l.level}</span>
                <span className={`hidden w-36 shrink-0 truncate sm:inline ${senderColor(l.source)}`} title={`${l.source} · ${ROLE_LABEL[l.role]}`}>
                  {l.source}
                </span>
                <span className={`text-foreground ${wrap ? "min-w-0 flex-1" : ""}`}>
                  <AnsiText text={l.message} query={query} />
                </span>
              </div>
              {expanded ? (
                <pre className="mx-4 my-1 overflow-x-auto rounded-md border border-border bg-muted/40 px-3 py-2 text-[11.5px] leading-[18px] text-muted-foreground">
                  {JSON.stringify(l.raw, null, 2)}
                </pre>
              ) : null}
            </div>
          );
        })
      ) : (
        <p className="px-4 py-6 font-sans text-[12.5px] text-muted-foreground">{empty}</p>
      )}
    </div>
  );
}

function Events({ events, empty }: { events: DbEvent[]; empty: string }) {
  return events.length ? (
    <ul className="list-rows max-h-[560px] overflow-auto">
      {events.map((e, i) => (
        <li key={`${e.at}:${i}`} className="flex items-start gap-3 px-4 py-2.5 text-[12.5px]">
          <Dot tone={e.type === "Warning" ? "warning" : "neutral"} className="mt-1.5" />
          <div className="min-w-0 flex-1">
            <p className="flex flex-wrap items-baseline gap-x-2">
              <span className="font-medium">{e.reason}</span>
              {e.object ? <span className="font-mono text-[11.5px] text-muted-foreground">{e.object}</span> : null}
              {e.count && e.count > 1 ? <span className="text-[11.5px] text-muted-foreground">×{e.count}</span> : null}
            </p>
            <p className="text-muted-foreground [overflow-wrap:anywhere]">{e.message}</p>
          </div>
          <span className="shrink-0 text-[11.5px] tabular-nums text-muted-foreground">{dateTime(e.at)}</span>
        </li>
      ))}
    </ul>
  ) : (
    <p className="px-4 py-6 text-[12.5px] text-muted-foreground">{empty}</p>
  );
}
