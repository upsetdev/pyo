import { ObservabilityOff, useObservabilityOn } from "~/components/pyo/ObservabilityOff";
import { Segmented } from "~/components/ui/Field";
import {
  CopyIcon,
  ListBulletsIcon,
  PauseIcon,
  PlayIcon,
  XIcon,
} from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { z } from "zod";
import { Layered, TOOLBAR } from "~/components/ui/Layered";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Select } from "~/components/ui/Field";
import {
  EmptyState,
  ErrorState,
  Metric,
  MetricStrip,
  Panel,
  Skeleton,
} from "~/components/ui/Panel";
import { ZoomButton, Zoomable } from "~/components/ui/Zoom";
import { SearchInput } from "~/components/ui/Table";
import { useWrapLines, WrapSwitch } from "~/components/pyo/LogViewer";
import { toast } from "~/lib/pyo/toast";
import { senderColor } from "~/lib/pyo/logColors";
import { compact } from "~/lib/pyo/format";
import {
  fetchAccessLogs,
  type AccessLog,
  type AccessLogSummary,
} from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { servicesIn, usePyo } from "~/lib/pyo/store";
import type { TimeRange } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";
import { copyText } from "~/lib/pyo/clipboard";
import { stripAnsi } from "~/lib/pyo/ansi";

export const Route = createFileRoute("/_app/$team/observability/logs")({
  validateSearch: z.object({
    site: z.string().optional(),
    range: z.enum(["1h", "24h", "7d", "30d"]).optional().catch(undefined),
    status: z.enum(["2xx", "3xx", "4xx", "5xx"]).optional().catch(undefined),
    method: z.string().optional(),
    q: z.string().optional(),
  }),
  head: () => ({ meta: seo({ title: "Logs" }) }),
  component: LogsPage,
});

const RANGES: TimeRange[] = ["1h", "24h", "7d", "30d"];
const METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

const size = (b: number) =>
  b < 1024
    ? `${b} B`
    : b < 1024 * 1024
      ? `${(b / 1024).toFixed(1)} KB`
      : `${(b / 1024 / 1024).toFixed(1)} MB`;

/**
 * Every request the web server (each cluster's ingress) handled for the
 * team's domains, newest first. The first page refreshes on its own;
 * "Load more" pages back in time.
 */
function LogsPage() {
  const s = usePyo();
  const search = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const sites = servicesIn(s).flatMap((a) =>
    a.domains.map((d) => ({ id: d.id, host: d.hostname })),
  );
  const site = sites.some((x) => x.id === search.site) ? search.site! : "all";
  const range = search.range ?? "24h";
  const [text, setText] = useState(search.q ?? "");
  // Search as you type, a moment after the last key.
  useEffect(() => {
    const t = setTimeout(() => {
      if ((search.q ?? "") !== text) setFilter({ q: text || undefined });
    }, 350);
    return () => clearTimeout(t);
  }, [text]);
  const setFilter = (next: Partial<typeof search>) =>
    void navigate({ search: (prev) => ({ ...prev, ...next }), replace: true });

  const filter = {
    team: s.currentTeamId,
    site,
    range,
    status: search.status ?? "all",
    method: search.method ?? "all",
    q: search.q ?? "",
  };
  const on = useObservabilityOn();
  const key = on ? JSON.stringify(filter) : "off";
  const q = useQuery(key, () => (on ? fetchAccessLogs(filter) : Promise.resolve(null)), {
    refreshMs: 10_000,
  });
  // The last answer stays on screen while new filters load, so a search
  // doesn't swap the console (zoomed or not) for a placeholder.
  const [kept, setKept] = useState(q.data);
  useEffect(() => {
    if (q.data) setKept(q.data);
  }, [q.data]);
  const data = q.data ?? kept;
  // Older pages, added below the live first page.
  const [older, setOlder] = useState<{
    key: string;
    logs: AccessLog[];
    next: number | null;
  }>({ key, logs: [], next: null });
  const [loadingMore, setLoadingMore] = useState(false);
  const extra = older.key === key ? older : { key, logs: [], next: null };
  const next = extra.logs.length ? extra.next : (data?.next ?? null);
  const loadMore = async () => {
    if (!next) return;
    setLoadingMore(true);
    try {
      const r = await fetchAccessLogs({ ...filter, before: next });
      setOlder({ key, logs: [...extra.logs, ...r.logs], next: r.next });
    } finally {
      setLoadingMore(false);
    }
  };
  const showIp = !!data && data.ipMode !== "none";
  const logs = [...(data?.logs ?? []), ...extra.logs];
  const filtered =
    site !== "all" ||
    range !== "24h" ||
    !!search.status ||
    !!search.method ||
    !!search.q;

  if (!on)
    return (
      <Panel>
        <ObservabilityOff />
      </Panel>
    );
  if (!sites.length)
    return (
      <Panel>
        <EmptyState
          icon={ListBulletsIcon}
          title="No websites yet"
          description="Add a domain to a service."
          action={
            <ButtonLink href="/projects" size="sm">
              Go to projects
            </ButtonLink>
          }
        />
      </Panel>
    );

  return (
    <div className="space-y-6">
      <Layered bare>
        <div className={TOOLBAR.replace("mb-1.5 ", "")}>
          {sites.length > 1 ? (
            <div className="w-[calc(50%-3px)] sm:w-48">
              <Select
                value={site}
                onChange={(e) =>
                  setFilter({
                    site: e.target.value === "all" ? undefined : e.target.value,
                  })
                }
                aria-label="Website"
                heading="Website"
              >
                <option value="all">All websites</option>
                {sites.map((x) => (
                  <option key={x.id} value={x.id}>
                    {x.host}
                  </option>
                ))}
              </Select>
            </div>
          ) : null}
          <div className="w-[calc(50%-3px)] sm:w-32">
            <Select
              value={search.status ?? "all"}
              onChange={(e) =>
                setFilter({
                  status:
                    e.target.value === "all"
                      ? undefined
                      : (e.target.value as "2xx"),
                })
              }
              aria-label="Status"
              heading="Status"
            >
              <option value="all">Any status</option>
              {["2xx", "3xx", "4xx", "5xx"].map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </Select>
          </div>
          <div className="w-[calc(50%-3px)] sm:w-32">
            <Select
              value={search.method ?? "all"}
              onChange={(e) =>
                setFilter({
                  method: e.target.value === "all" ? undefined : e.target.value,
                })
              }
              aria-label="Method"
              heading="Method"
            >
              <option value="all">Any method</option>
              {METHODS.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </Select>
          </div>
          {filtered ? (
            <button
              type="button"
              onClick={() => {
                setText("");
                setFilter({
                  site: undefined,
                  range: undefined,
                  status: undefined,
                  method: undefined,
                  q: undefined,
                });
              }}
              className="inline-flex h-10 items-center gap-1.5 rounded-xl px-3 text-[12.5px] font-medium text-muted-foreground transition-colors hover:bg-card/60 hover:text-foreground"
            >
              <XIcon size={12} weight="bold" /> Reset
            </button>
          ) : null}
          <Segmented
            label="Time range"
            className="shrink-0 sm:ml-auto"
            value={range}
            onChange={(r) => setFilter({ range: r === "24h" ? undefined : r })}
            options={RANGES.map((r) => ({ value: r, label: r }))}
          />
        </div>
      </Layered>

      {!data && q.status === "loading" ? (
        <div className="space-y-6">
          <Skeleton className="h-[92px] rounded-xl" />
          <Skeleton className="h-[480px] rounded-xl" />
        </div>
      ) : !data && q.status === "error" ? (
        <Panel>
          <ErrorState
            title="Couldn't load logs"
            description={q.error}
            onRetry={q.retry}
          />
        </Panel>
      ) : (
        <>
          <Summary s={data?.summary ?? null} />
          <Tail
            logs={logs}
            showIp={showIp}
            search={text}
            onSearch={setText}
            onStatus={(c) => setFilter({ status: c })}
            serviceOf={(host) =>
              servicesIn(s).find((a) => a.domains.some((d) => d.hostname === host))
                ?.name
            }
            empty={
              filtered
                ? "No matching requests. Try other filters or a longer range."
                : "No requests yet."
            }
            older={next ? () => void loadMore() : undefined}
            loadingOlder={loadingMore}
          />
        </>
      )}
    </div>
  );
}

const bandwidth = (b: number) =>
  b >= 1024 ** 3
    ? `${(b / 1024 ** 3).toFixed(2)} GB`
    : b >= 1024 ** 2
      ? `${(b / 1024 ** 2).toFixed(1)} MB`
      : size(b);

function Summary({ s }: { s: AccessLogSummary | null }) {
  if (!s) return null;
  const errors = s.serverErrors + s.clientErrors;
  const rate = s.requests ? s.serverErrors / s.requests : 0;
  return (
    <MetricStrip cols="grid-cols-2 lg:grid-cols-4">
      <Metric label="Requests" value={compact(s.requests)} />
      <Metric label="Bandwidth" value={bandwidth(s.bytes)} />
      <Metric
        label="Errors"
        value={compact(errors)}
        sub={`${compact(s.serverErrors)} 5xx · ${compact(s.clientErrors)} 4xx`}
        tone={rate > 0.05 ? "danger" : rate > 0.01 ? "warning" : undefined}
      />
      <Metric
        label="Response time"
        value={`${s.p50} ms`}
        sub={`p95 ${s.p95} ms`}
      />
    </MetricStrip>
  );
}

const pad = (n: number) => String(n).padStart(2, "0");
const MONTHS = "Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec".split(" ");

/** "Oct 31 01:50:38", syslog style, in your time zone. */
const stamp = (ms: number) => {
  const d = new Date(ms);
  return `${MONTHS[d.getMonth()]} ${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
};

/** Request fields are the client's: shown and copied as plain text. */
const pathOf = (l: AccessLog) =>
  stripAnsi(l.url.replace(/^https?:\/\/[^/]+/, "") || l.path);

const statusColor = (s: number) =>
  s >= 500
    ? "text-destructive"
    : s >= 400
      ? "text-warning"
      : s >= 300
        ? "text-muted-foreground"
        : "text-success";

const statusClass = (s: number) =>
  `${Math.trunc(s / 100)}xx` as "2xx" | "3xx" | "4xx" | "5xx";

const lineText = (l: AccessLog, service?: string) =>
  `${service ?? "web"}: ${l.ip ? `${l.ip} ` : ""}"${l.method} ${pathOf(l)}" ${size(l.bytes)} ${l.durationMs}ms${l.country ? ` ${l.country}` : ""}`;

/**
 * The requests as a live tail, in the service logs' style with Papertrail's
 * ways: oldest at the top, following the newest at the bottom unless you
 * scroll up or pause; a host or status filters by it; a line opens to its
 * details; older events load above.
 */
function Tail({
  logs,
  showIp,
  search,
  onSearch,
  onStatus,
  serviceOf,
  empty,
  older,
  loadingOlder,
}: {
  logs: AccessLog[];
  showIp: boolean;
  search: string;
  onSearch: (q: string) => void;
  onStatus: (c: "2xx" | "3xx" | "4xx" | "5xx") => void;
  /** The service a host belongs to: the "program" of each line. */
  serviceOf: (host: string) => string | undefined;
  empty: string;
  older?: () => void;
  loadingOlder: boolean;
}) {
  const [zoomed, setZoomed] = useState(false);
  const [wrap, setWrap] = useWrapLines();
  // Paused: the lines on screen stay put while new ones keep arriving.
  const [frozen, setFrozen] = useState<AccessLog[] | null>(null);
  const [open, setOpen] = useState<number | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const fromBottom = useRef(0);
  const shown = frozen ?? logs;
  const lines = useMemo(() => [...shown].reverse(), [shown]);
  const waiting = frozen ? Math.max(0, logs.length - frozen.length) : 0;

  // Keep to the bottom (the newest line) unless you've scrolled up; then
  // keep your place as lines arrive or older ones load above. Again on the
  // next frame, once fonts and layout have settled.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const place = () => {
      el.scrollTop = atBottom.current
        ? el.scrollHeight
        : el.scrollHeight - fromBottom.current;
    };
    place();
    const f = requestAnimationFrame(place);
    return () => cancelAnimationFrame(f);
  }, [lines, zoomed, wrap]);
  // A new search starts at the bottom again.
  useEffect(() => {
    atBottom.current = true;
  }, [search]);

  const copy = () => {
    const text = lines
      .map(
        (l) =>
          `${new Date(l.at).toISOString()} ${l.status} ${l.host} ${lineText(l, serviceOf(l.host))}`,
      )
      .join("\n");
    copyText(text)
      .then(() =>
        toast({
          title: `Copied ${lines.length} line${lines.length === 1 ? "" : "s"}`,
        }),
      )
      .catch(() => toast({ tone: "error", title: "Clipboard unavailable" }));
  };

  return (
    <Panel>
      <Zoomable
        zoomed={zoomed}
        onClose={() => setZoomed(false)}
        label="Logs"
      >
        <div className={zoomed ? "flex min-h-0 flex-1 flex-col" : ""}>
          <div className="flex flex-wrap items-center gap-2 border-b border-border px-4 py-2.5">
            <SearchInput
              value={search}
              onChange={onSearch}
              placeholder="Search logs"
              className="w-full sm:w-64"
            />
            <div className="ml-auto flex items-center gap-2">
              <button
                type="button"
                aria-pressed={!!frozen}
                aria-label={frozen ? "Resume" : "Pause"}
                title={frozen ? "Resume" : "Pause"}
                onClick={() => {
                  if (frozen) atBottom.current = true;
                  setFrozen(frozen ? null : logs);
                }}
                className={`inline-flex h-7 items-center gap-1.5 rounded-md border px-2 text-[12px] font-medium transition-colors ${frozen ? "border-warning/40 bg-warning/15 text-warning hover:bg-warning/25" : "border-border text-muted-foreground hover:bg-accent hover:text-foreground"}`}
              >
                {frozen ? (
                  <PlayIcon size={12} weight="fill" />
                ) : (
                  <PauseIcon size={12} weight="fill" />
                )}
                {frozen && waiting ? (
                  <span className="tabular-nums">{waiting} new</span>
                ) : null}
              </button>
              <Button
                variant="secondary"
                size="xs"
                disabled={!lines.length}
                onClick={copy}
              >
                <CopyIcon size={12} /> Copy visible
              </Button>
              <ZoomButton
                zoomed={zoomed}
                onClick={() => setZoomed((z) => !z)}
              />
            </div>
          </div>
          <div
            ref={ref}
            onScroll={(e) => {
              const el = e.currentTarget;
              fromBottom.current = el.scrollHeight - el.scrollTop;
              atBottom.current = fromBottom.current - el.clientHeight < 24;
            }}
            className={`${zoomed ? "min-h-0 flex-1" : "h-[560px]"} overflow-auto bg-background py-2 font-mono text-[12px] leading-[20px]`}
            role="log"
            aria-live="off"
          >
            {older ? (
              <div className="px-4 pb-1">
                <button
                  type="button"
                  disabled={loadingOlder}
                  onClick={older}
                  className="font-sans text-[12px] font-medium text-muted-foreground hover:text-foreground disabled:opacity-60"
                >
                  {loadingOlder ? "Loading…" : "Load older events"}
                </button>
              </div>
            ) : null}
            {lines.length ? (
              lines.map((l, i) => {
                const service = serviceOf(l.host);
                return (
                  <div key={`${l.at}-${i}`}>
                    <div
                      onClick={() => setOpen(open === i ? null : i)}
                      className={`flex cursor-pointer items-start gap-3 px-4 ${wrap ? "whitespace-pre-wrap [overflow-wrap:anywhere]" : "whitespace-pre-wrap break-all sm:whitespace-pre sm:break-normal"} ${open === i ? "bg-primary/15" : "hover:bg-accent/40"}`}
                    >
                      <span className="shrink-0 select-none text-muted-foreground/70">
                        {stamp(l.at)}
                      </span>
                      <button
                        type="button"
                        title={`Show only ${statusClass(l.status)}`}
                        onClick={(e) => {
                          e.stopPropagation();
                          onStatus(statusClass(l.status));
                        }}
                        className={`w-9 shrink-0 select-none text-left hover:opacity-70 ${statusColor(l.status)}`}
                      >
                        {l.status}
                      </button>
                      <button
                        type="button"
                        title={`Show only ${l.host}`}
                        onClick={(e) => {
                          e.stopPropagation();
                          onSearch(l.host);
                        }}
                        className={`hidden w-56 shrink-0 truncate text-left sm:inline hover:opacity-70 ${senderColor(l.host)}`}
                      >
                        {l.host}
                      </button>
                      <span
                        className={`text-foreground ${wrap ? "min-w-0 flex-1" : ""}`}
                      >
                        <span className={senderColor(service ?? "web", 3)}>
                          {service ?? "web"}:
                        </span>{" "}
                        {showIp && l.ip ? (
                          <span className="text-muted-foreground">{l.ip} </span>
                        ) : null}
                        &quot;{l.method} {pathOf(l)}&quot;{" "}
                        <span className="text-muted-foreground">
                          {size(l.bytes)} {l.durationMs}ms
                          {l.country ? ` ${l.country}` : ""}
                        </span>
                      </span>
                    </div>
                    {open === i ? (
                      <div className="whitespace-pre-wrap px-4 pb-1.5 pl-[11.5rem] text-muted-foreground">
                        {[
                          `time     ${new Date(l.at).toISOString()}`,
                          `url      ${l.host}${pathOf(l)}`,
                          l.ip && `ip       ${l.ip}`,
                          (l.city || l.countryName) &&
                            `location ${[l.city, l.countryName].filter(Boolean).join(", ")}`,
                          l.referrer && `referrer ${stripAnsi(l.referrer)}`,
                          (l.userAgent || l.browser || l.device) &&
                            `ua       ${stripAnsi(l.userAgent || [l.browser, l.device].filter(Boolean).join(" · "))}`,
                        ]
                          .filter(Boolean)
                          .join("\n")}
                      </div>
                    ) : null}
                  </div>
                );
              })
            ) : (
              <p className="px-4 py-6 font-sans text-[12.5px] text-muted-foreground">
                {search ? "No lines match." : empty}
              </p>
            )}
          </div>
          <div className="flex items-center gap-3 border-t border-border px-4 py-2">
            <p className="text-[11.5px] text-muted-foreground">
              {lines.length} {lines.length === 1 ? "event" : "events"}. Click a
              status or host to filter, a line for details.
            </p>
            <WrapSwitch wrap={wrap} onChange={setWrap} />
          </div>
        </div>
      </Zoomable>
    </Panel>
  );
}
