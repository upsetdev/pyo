import { Layered, TOOLBAR } from "~/components/ui/Layered";
import {
  ArrowDownRightIcon,
  ArrowUpRightIcon,
  ChartLineUpIcon,
  CpuIcon,
  GlobeSimpleIcon,
  TreeStructureIcon,
  XIcon,
} from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { z } from "zod";
import { TimeSeriesChart } from "~/components/charts/Charts";
import { ServerMetrics } from "~/components/pyo/ServerMetrics";
import { ButtonLink } from "~/components/ui/Button";
import { Segmented, Select } from "~/components/ui/Field";
import { EmptyState, ErrorState, Panel, Skeleton } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { fetchAnalytics, type AnalyticsResponse } from "~/lib/pyo/backend";
import { compact, pct, plural } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import { servicesIn, clustersIn, projectsIn, usePyo } from "~/lib/pyo/store";
import type { TimeRange } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/observability/analytics")({
  validateSearch: z.object({
    project: z.string().optional(),
    site: z.string().optional(),
    range: z.enum(["1h", "24h", "7d", "30d"]).optional().catch(undefined),
    view: z.enum(["web", "server"]).optional().catch(undefined),
    cluster: z.string().optional(),
  }),
  head: () => ({ meta: seo({ title: "Analytics" }) }),
  component: AnalyticsPage,
});

/** "ID" -> 🇮🇩; unknown locations get a globe. */
const flag = (code: string) =>
  /^[A-Z]{2}$/.test(code)
    ? String.fromCodePoint(...[...code].map((c) => 0x1f1a5 + c.charCodeAt(0)))
    : "🌐";

const RANGE_LABEL: Record<TimeRange, string> = {
  "1h": "Last hour",
  "24h": "Last 24 hours",
  "7d": "Last 7 days",
  "30d": "Last 30 days",
};

/**
 * Two views of a team's services over time. Web analytics: every domain's
 * traffic, from ingress access logs; project and service links land here
 * pre-filtered. Server metrics: CPU and memory of servers and services, from the
 * agents' samples. Laid out like Vercel's: filters, one chart with metric
 * tabs, then lists.
 */
function AnalyticsPage() {
  const s = usePyo();
  const search = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const view = search.view ?? "web";
  const allSites = servicesIn(s).flatMap((a) =>
    a.domains
      .filter((d) => d.status === "active")
      .map((d) => ({
        id: d.id,
        host: d.hostname,
        app: a.name,
        projectId: a.projectId,
      })),
  );
  // Only projects that serve at least one website are worth filtering by.
  const projects = projectsIn(s).filter((p) =>
    allSites.some((x) => x.projectId === p.id),
  );
  const project = projects.some((p) => p.id === search.project)
    ? search.project!
    : "all";
  const sites =
    project === "all"
      ? allSites
      : allSites.filter((x) => x.projectId === project);
  const site = sites.some((x) => x.id === search.site) ? search.site! : "all";
  const clusters = clustersIn(s);
  const cluster = clusters.some((c) => c.id === search.cluster)
    ? search.cluster!
    : "all";
  // Server metrics default to a day: they only go back 30 days, sampled each minute.
  const range: TimeRange = search.range ?? (view === "server" ? "24h" : "7d");
  const setFilter = (next: {
    project?: string;
    site?: string;
    range?: TimeRange;
    view?: "web" | "server";
    cluster?: string;
  }) =>
    void navigate({ search: (prev) => ({ ...prev, ...next }), replace: true });
  const filter = { team: s.currentTeamId, project, site, range };
  // Refreshed every 30s; the API also returns the previous period for the change badges.
  const q = useQuery(
    view === "web" ? JSON.stringify(filter) : "off",
    () =>
      view === "web" && allSites.length
        ? fetchAnalytics(filter)
        : Promise.resolve(null),
    { refreshMs: 30_000 },
  );

  const rangePicker = (
    <Segmented
      label="Time range"
      className="shrink-0 sm:ml-auto"
      value={range}
      onChange={(r) => setFilter({ range: r })}
      options={(Object.keys(RANGE_LABEL) as TimeRange[]).map((r) => ({ value: r, label: r, ariaLabel: RANGE_LABEL[r], title: RANGE_LABEL[r] }))}
    />
  );
  const defaultRange = view === "server" ? "24h" : "7d";
  const filtered =
    (view === "web"
      ? project !== "all" || site !== "all"
      : cluster !== "all") || range !== defaultRange;
  const reset = filtered ? (
    <button
      type="button"
      onClick={() =>
        setFilter({
          project: undefined,
          site: undefined,
          cluster: undefined,
          range: undefined,
        })
      }
      className="inline-flex h-9 items-center gap-1.5 rounded-[10px] px-3 text-[12.5px] font-medium text-muted-foreground transition-colors hover:bg-card/60 hover:text-foreground"
    >
      <XIcon size={12} weight="bold" /> Reset
    </button>
  ) : null;
  const viewSwitch = (
    <Segmented
      label="Analytics view"
      value={view}
      onChange={(v) =>
        setFilter({
          view: v === "web" ? undefined : v,
          range: undefined,
        })
      }
      options={[
        {
          value: "web",
          label: (
            <span className="inline-flex items-center gap-1.5">
              <GlobeSimpleIcon size={13} /> Traffic
            </span>
          ),
        },
        {
          value: "server",
          label: (
            <span className="inline-flex items-center gap-1.5">
              <CpuIcon size={13} /> Servers
            </span>
          ),
        },
      ]}
    />
  );
  const bar = (controls: ReactNode) => (
    <Layered bare>
      <div className={TOOLBAR.replace("mb-1.5 ", "")}>
        {viewSwitch}
        {controls}
        {reset}
        {rangePicker}
      </div>
    </Layered>
  );

  return (
    <div className="space-y-6">
      {view === "server" ? (
        <>
          {bar(
            clusters.length > 1 ? (
              <div className="w-full sm:w-52">
                <Select
                  value={cluster}
                  onChange={(e) =>
                    setFilter({
                      cluster:
                        e.target.value === "all" ? undefined : e.target.value,
                    })
                  }
                  aria-label="Cluster"
                  heading="Cluster"
                >
                  <option value="all">All clusters</option>
                  {clusters.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))}
                </Select>
              </div>
            ) : clusters[0] ? (
              // One cluster: nothing to pick, so say what's shown.
              <span className="inline-flex h-9 items-center gap-2 px-2.5 text-[13px] text-muted-foreground">
                <TreeStructureIcon size={14} />
                <span className="font-medium text-foreground">
                  {clusters[0].name}
                </span>
                ·{" "}
                {plural(
                  s.servers.filter((n) => n.clusterId === clusters[0].id).length,
                  "server",
                )}
              </span>
            ) : null,
          )}
          <ServerMetrics
            team={s.currentTeamId}
            cluster={cluster}
            range={range}
          />
        </>
      ) : !allSites.length ? (
        <Panel>
          <EmptyState
            icon={ChartLineUpIcon}
            title="No websites to analyze yet"
            description="Add a domain to a service."
            action={
              <ButtonLink href="/projects" size="sm">
                Go to projects
              </ButtonLink>
            }
          />
        </Panel>
      ) : (
        <>
          {bar(
            <>
              {projects.length > 1 ? (
                <div className="w-[calc(50%-3px)] sm:w-44">
                  <Select
                    value={project}
                    onChange={(e) =>
                      setFilter({
                        project:
                          e.target.value === "all" ? undefined : e.target.value,
                        site: undefined,
                      })
                    }
                    aria-label="Project"
                    heading="Project"
                  >
                    <option value="all">All projects</option>
                    {projects.map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name}
                      </option>
                    ))}
                  </Select>
                </div>
              ) : null}
              <div
                className={
                  projects.length > 1
                    ? "w-[calc(50%-3px)] sm:w-64"
                    : "w-full sm:w-64"
                }
              >
                <Select
                  value={site}
                  onChange={(e) =>
                    setFilter({
                      site:
                        e.target.value === "all" ? undefined : e.target.value,
                    })
                  }
                  aria-label="Website"
                  heading="Website"
                >
                  <option value="all">All websites ({sites.length})</option>
                  {sites.map((x) => (
                    <option key={x.id} value={x.id}>
                      {x.host}
                    </option>
                  ))}
                </Select>
              </div>
            </>,
          )}

          {q.status === "loading" ? (
            <div className="space-y-4" aria-busy aria-label="Loading analytics">
              <Skeleton className="h-[380px] rounded-xl" />
              <div className="grid gap-4 lg:grid-cols-2">
                {[0, 1, 2, 3].map((i) => (
                  <Skeleton key={i} className="h-72 rounded-xl" />
                ))}
              </div>
            </div>
          ) : q.status === "error" ? (
            <Panel>
              <ErrorState
                title="Couldn't load analytics"
                description={q.error}
                onRetry={q.retry}
              />
            </Panel>
          ) : q.data ? (
            <Loaded data={q.data} />
          ) : null}
        </>
      )}
    </div>
  );
}

type MetricId = "visitors" | "pageviews" | "errors";

function Loaded({ data }: { data: AnalyticsResponse }) {
  const now = data;
  const prev = { summary: data.previous };
  if (!data.summary.requests)
    return (
      <Panel>
        <EmptyState
          icon={ChartLineUpIcon}
          title="No traffic in this range yet"
          description="Shown within seconds."
        />
      </Panel>
    );
  const [metric, setMetric] = useState<MetricId>("visitors");
  const a = now.summary;
  const viewsPerRequest = a.requests ? a.pageviews / a.requests : 0;
  const metrics: {
    id: MetricId;
    label: ReactNode;
    value: string;
    change: number;
    lowerIsBetter?: boolean;
    values: number[];
    format: (v: number) => string;
  }[] = [
    {
      id: "visitors",
      label: (
        <span title="Estimated from IP and browser per day, not exact people.">
          Visitors
        </span>
      ),
      value: compact(a.estimatedVisitors),
      change: change(a.estimatedVisitors, prev.summary.estimatedVisitors),
      values: now.series.map((p) => p.visitors),
      format: compact,
    },
    {
      id: "pageviews",
      label: "Page views",
      value: compact(a.pageviews),
      change: change(a.pageviews, prev.summary.pageviews),
      values: now.series.map((p) => Math.round(p.requests * viewsPerRequest)),
      format: compact,
    },
    {
      id: "errors",
      label: "Error rate",
      value: pct(a.errorRate, 2),
      change: change(a.errorRate, prev.summary.errorRate),
      lowerIsBetter: true,
      values: now.series.map((p) => (p.requests ? p.errors / p.requests : 0)),
      format: (v) => pct(v, 1),
    },
  ];
  const current = metrics.find((m) => m.id === metric)!;

  // Path view folds full URLs (with query strings) into their path. Pages
  // count views: visitors can't be summed across URLs without double counting.
  const paths = Object.values(
    a.pages.reduce<
      Record<string, { key: string; label: string; value: number }>
    >((acc, p) => {
      acc[p.path] = {
        key: p.path,
        label: p.path,
        value: (acc[p.path]?.value ?? 0) + p.views,
      };
      return acc;
    }, {}),
  ).sort((x, y) => y.value - x.value);

  return (
    <>
      <Layered bare>
        <section
          aria-label="Traffic"
          className="overflow-hidden rounded-[14px] border border-card-border bg-card"
        >
          <div
            role="tablist"
            aria-label="Metric"
            className="grid grid-cols-3 border-b border-border"
          >
            {metrics.map((m) => {
              const on = m.id === metric;
              const good = m.lowerIsBetter ? m.change <= 0 : m.change >= 0;
              return (
                <button
                  key={m.id}
                  type="button"
                  role="tab"
                  aria-selected={on}
                  onClick={() => setMetric(m.id)}
                  className={`relative border-r border-border px-4 py-4 text-left transition-colors last:border-r-0 sm:px-6 sm:py-5 ${on ? "bg-accent/40" : "hover:bg-accent/20"}`}
                >
                  <span className="block text-[12.5px] font-medium text-muted-foreground">
                    {m.label}
                  </span>
                  <span className="mt-1.5 flex flex-wrap items-center gap-x-2.5 gap-y-1">
                    <span className="text-[22px] font-semibold tabular-nums tracking-tight sm:text-[26px]">
                      {m.value}
                    </span>
                    {Number.isFinite(m.change) ? (
                      <span
                        title="Compared with the previous period"
                        className={`inline-flex items-center gap-0.5 rounded-full px-1.5 py-0.5 text-[11px] font-medium tabular-nums ${
                          Math.abs(m.change) < 0.005
                            ? "bg-muted text-muted-foreground"
                            : good
                              ? "bg-success/12 text-success"
                              : "bg-destructive/12 text-destructive"
                        }`}
                      >
                        {Math.abs(m.change) < 0.005 ? null : m.change > 0 ? (
                          <ArrowUpRightIcon size={10} weight="bold" />
                        ) : (
                          <ArrowDownRightIcon size={10} weight="bold" />
                        )}
                        {Math.abs(m.change * 100).toFixed(0)}%
                      </span>
                    ) : null}
                  </span>
                  {on ? (
                    <span className="absolute inset-x-0 bottom-0 h-0.5 bg-foreground" />
                  ) : null}
                </button>
              );
            })}
          </div>
          <div className="px-3 pb-3 pt-5 sm:px-5">
            <TimeSeriesChart
              label={`${typeof current.label === "string" ? current.label : "Visitors"} over time`}
              area
              times={now.series.map((p) => p.at)}
              series={[
                {
                  key: current.id,
                  label:
                    typeof current.label === "string"
                      ? current.label
                      : "Visitors",
                  color: "var(--series-1)",
                  values: current.values,
                },
              ]}
              format={current.format}
              height={240}
            />
          </div>
        </section>
      </Layered>

      <Layered bare>
        <div className="grid gap-1.5 lg:grid-cols-2">
          <TopList
            label="Pages"
            tabs={[
              {
                id: "pages",
                label: "Pages",
                rows: paths,
                mono: true,
                column: "Views",
              },
              {
                id: "urls",
                label: "Full URLs",
                mono: true,
                column: "Views",
                note: "Query strings are kept, with sensitive values redacted.",
                rows: a.pages.map((p) => ({
                  key: p.url,
                  label: <UrlLabel url={p.url} />,
                  text: p.url,
                  value: p.views,
                })),
              },
              {
                id: "params",
                label: "Query params",
                mono: true,
                column: "Requests",
                note: "Redacted values are never stored.",
                rows: a.queryParams.map((p) => ({
                  key: p.key,
                  label: p.redacted ? (
                    <>
                      {p.key} <Redacted />
                    </>
                  ) : (
                    p.key
                  ),
                  text: p.key,
                  value: p.requests,
                })),
              },
            ]}
          />
          <TopList
            label="Referrers"
            tabs={[
              {
                id: "referrers",
                label: "Referrers",
                rows: a.referrers.map((r) => ({
                  key: r.source,
                  label: r.source,
                  value: r.visitors,
                })),
              },
            ]}
          />
          <TopList
            label="Locations"
            tabs={[
              {
                id: "countries",
                label: "Countries",
                rows: a.countries.map((c) => ({
                  key: c.code,
                  label: `${flag(c.code)} ${c.name}`,
                  text: c.name,
                  value: c.visitors,
                })),
              },
              {
                id: "cities",
                label: "Cities",
                rows: a.cities.map((c) => ({
                  key: c.city,
                  label: `${flag(c.country)} ${c.city}`,
                  text: c.city,
                  value: c.visitors,
                })),
              },
              // Client IPs, when the analytics setting keeps them.
              ...(a.ipMode && a.ipMode !== "none"
                ? [
                    {
                      id: "ips",
                      label: "IPs",
                      column: "Requests",
                      mono: true,
                      rows: (a.clientIps ?? []).map((r) => ({
                        key: r.ip,
                        label: r.ip,
                        text: r.ip,
                        value: r.requests,
                      })),
                    },
                  ]
                : []),
            ]}
          />
          <TopList
            label="Technology"
            tabs={[
              {
                id: "devices",
                label: "Devices",
                rows: a.devices.map((d) => ({
                  key: d.label,
                  label: d.label,
                  value: d.visitors,
                })),
              },
              {
                id: "browsers",
                label: "Browsers",
                rows: a.browsers.map((d) => ({
                  key: d.label,
                  label: d.label,
                  value: d.visitors,
                })),
              },
            ]}
          />
        </div>
      </Layered>
    </>
  );
}

/** Relative change against the previous period; NaN when there's nothing to compare. */
const change = (now: number, before: number) =>
  before ? (now - before) / before : Number.NaN;

type Row = { key: string; label: ReactNode; text?: string; value: number };
type Tab = {
  id: string;
  label: string;
  rows: Row[];
  column?: string;
  mono?: boolean;
  note?: string;
};

const SHOWN = 7;

/** Vercel-style top list: tabs on the left, the column on the right, rows with a proportional bar. */
function TopList({ label, tabs }: { label: string; tabs: Tab[] }) {
  const [tabId, setTabId] = useState(tabs[0].id);
  const [all, setAll] = useState(false);
  const [query, setQuery] = useState("");
  const tab = tabs.find((t) => t.id === tabId) ?? tabs[0];
  const sorted = [...tab.rows].sort((x, y) => y.value - x.value);
  const filtered = sorted.filter(
    (r) =>
      !query ||
      (r.text ?? String(r.label)).toLowerCase().includes(query.toLowerCase()),
  );
  const rows = all ? filtered : filtered.slice(0, SHOWN);
  const top = Math.max(1, ...tab.rows.map((r) => r.value));

  return (
    <section
      aria-label={label}
      className="flex flex-col overflow-hidden rounded-[14px] border border-card-border bg-card"
    >
      <div className="flex items-center justify-between gap-3 border-b border-border px-4">
        <div
          role="tablist"
          aria-label={label}
          className="scrollbar-none -mb-px flex gap-4 overflow-x-auto"
        >
          {tabs.map((t) => (
            <button
              key={t.id}
              type="button"
              role="tab"
              aria-selected={t.id === tab.id}
              onClick={() => {
                setTabId(t.id);
                setQuery("");
              }}
              className={`relative h-11 shrink-0 text-[13px] font-medium transition-colors ${t.id === tab.id ? "text-foreground" : "text-muted-foreground hover:text-foreground"}`}
            >
              {t.label}
              {t.id === tab.id && tabs.length > 1 ? (
                <span className="absolute inset-x-0 bottom-0 h-0.5 rounded-full bg-foreground" />
              ) : null}
            </button>
          ))}
        </div>
        <span className="shrink-0 text-[11.5px] font-medium uppercase tracking-wider text-muted-foreground">
          {tab.column ?? "Visitors"}
        </span>
      </div>

      {all ? (
        <div className="border-b border-border px-3 py-2">
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder={`Filter ${tab.label.toLowerCase()}…`}
            label={`Filter ${tab.label}`}
            className="w-full"
          />
        </div>
      ) : null}

      {rows.length ? (
        <ul className="flex-1 space-y-0.5 p-2">
          {rows.map((r) => (
            <li
              key={r.key}
              className="group relative flex h-8 items-center justify-between gap-3 px-2.5 text-[13px]"
            >
              <span
                aria-hidden
                className="absolute inset-y-0 left-0 rounded-md bg-primary/10 transition-colors group-hover:bg-primary/18"
                style={{ width: `${(r.value / top) * 100}%` }}
              />
              <span
                className={`relative min-w-0 truncate ${tab.mono ? "font-mono text-[12px]" : ""}`}
                title={
                  r.text ?? (typeof r.label === "string" ? r.label : undefined)
                }
              >
                {r.label}
              </span>
              <span className="relative shrink-0 tabular-nums">
                {compact(r.value)}
              </span>
            </li>
          ))}
        </ul>
      ) : (
        <p className="flex-1 px-4 py-10 text-center text-[13px] text-muted-foreground">
          {query ? "Nothing matches" : "No data"}
        </p>
      )}

      {tab.note || tab.rows.length > SHOWN ? (
        <div className="flex min-h-10 items-center justify-between gap-3 border-t border-border px-4 py-1.5">
          <span className="min-w-0 truncate text-[12px] text-muted-foreground">
            {tab.note ?? ""}
          </span>
          {tab.rows.length > SHOWN ? (
            <button
              type="button"
              onClick={() => {
                setAll((v) => !v);
                setQuery("");
              }}
              aria-expanded={all}
              className="shrink-0 rounded-full px-2.5 py-1 text-[12.5px] font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
            >
              {all ? "Show less" : `View all ${tab.rows.length}`}
            </button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

function Redacted() {
  return (
    <span className="rounded bg-muted px-1 font-sans text-[10.5px] text-muted-foreground">
      redacted
    </span>
  );
}

/** A URL with its query string dimmed and redacted values marked. */
function UrlLabel({ url }: { url: string }) {
  const [path, query] = url.split("?");
  return (
    <>
      {path}
      {query ? (
        <span className="text-muted-foreground">
          ?
          {query.split("&").map((kv, i) => (
            <span key={kv}>
              {i ? "&" : ""}
              {kv.includes("[redacted]") ? (
                <>
                  {kv.split("=")[0]}=<Redacted />
                </>
              ) : (
                kv
              )}
            </span>
          ))}
        </span>
      ) : null}
    </>
  );
}
