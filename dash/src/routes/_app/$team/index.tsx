import { createFileRoute, Link } from "@tanstack/react-router";
import { AppLink } from "~/components/ui/AppLink";
import { Layered } from "~/components/ui/Layered";
import { currentSlug } from "~/lib/pyo/paths";
import { parseLocation } from "~/lib/pyo/location";
import { Flag } from "~/components/ui/Flag";
import {
  ArrowRightIcon,
  CheckIcon,
  MagnifyingGlassIcon,
  PlusIcon,
} from "@phosphor-icons/react";
import { ProjectIcon } from "~/components/ui/ProjectIcon";
import { useState } from "react";
import { DeploymentList } from "~/components/pyo/lists";
import { RemoteAccessNote } from "~/components/pyo/RemoteAccess";
import { DomainStatus } from "~/components/pyo/DomainState";
import { ProjectCard } from "~/components/pyo/ProjectCard";
import {
  ClusterStatusBadge,
  NodeStatusBadge,
  Dot,
} from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Page, Skeleton } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { Sparkline } from "~/components/charts/Charts";
import { projectSubtitle } from "~/components/pyo/ProjectCard";
import {
  fetchAnalytics,
  fetchMetrics,
  type MetricsEntity,
} from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { compact, cpu, mem, pct, plural, timeAgo } from "~/lib/pyo/format";
import {
  buildersIn,
  servicesIn,
  clustersIn,
  clusterSummary,
  currentTeam,
  deploymentsWhere,
  projectsIn,
  projectStats,
  servicePath,
  usePyo,
} from "~/lib/pyo/store";
import { seo } from "~/lib/seo";
import type { Builder, Cluster } from "~/lib/pyo/types";

export const Route = createFileRoute("/_app/$team/")({
  head: () => ({ meta: seo({ title: "Overview" }) }),
  component: OverviewPage,
});

// The overview answers three questions, in order: is anything broken, how are
// my projects doing, and what shipped recently. Everything else is a click away.

function OverviewPage() {
  const s = usePyo();
  const ws = currentTeam(s);
  const [query, setQuery] = useState("");
  if (!ws) return null;

  const apps = servicesIn(s);
  const clusters = clustersIn(s);
  const projects = projectsIn(s)
    .map((p) => ({ p, st: projectStats(s, p) }))
    .sort((a, b) => b.st.lastActivity - a.st.lastActivity);
  // The most recently active ones; the Projects page has the rest. A search
  // looks through all of them.
  const ql = query.trim().toLowerCase();
  const shown = ql
    ? projects.filter(
        ({ p, st }) =>
          p.name.toLowerCase().includes(ql) ||
          p.namespace.includes(ql) ||
          projectSubtitle(st).toLowerCase().includes(ql),
      )
    : projects.slice(0, 12);
  const deployments = deploymentsWhere(s, { teamId: ws.id }).slice(0, 5);
  // Newest first, like the deployments beside them.
  const domains = apps
    .flatMap((a) => a.domains.map((d) => ({ d, a })))
    .sort((x, y) => y.d.createdAt - x.d.createdAt);

  if (!projects.length)
    return (
      <Page width="max-w-3xl">
        <RemoteAccessNote />
        <GetStarted
          name={ws.name}
          server={
            clusters.find((c) => c.id === ws.defaultClusterId) ?? clusters[0]
          }
          builder={buildersIn(s).find((b) => b.status !== "removing")}
        />
      </Page>
    );

  const running = apps.filter((a) => a.status === "running").length;
  const lastDeploy = deployments[0];
  const nodes = clusters
    .map((c) => clusterSummary(s, c))
    .reduce(
      (a, x) => ({ total: a.total + x.servers, ready: a.ready + x.readyServers }),
      { total: 0, ready: 0 },
    );

  return (
    <Page>
      <RemoteAccessNote />
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[24px] font-semibold tracking-tight">
            {ws.name}
          </h1>
          {/* Problems live in the Issues section; this line is the at-a-glance state. */}
          <p className="mt-2 flex flex-wrap items-center gap-x-5 gap-y-1 text-[13px] text-muted-foreground">
            <span className="inline-flex items-center gap-2">
              <Dot tone={running < apps.length ? "warning" : "success"} />
              <span>
                <span className="text-foreground">{running}</span> of{" "}
                {plural(apps.length, "service")} running
              </span>
            </span>
            <span className="inline-flex items-center gap-2">
              <Dot tone={nodes.ready < nodes.total ? "warning" : "success"} />
              <span>
                <span className="text-foreground">{nodes.ready}</span> of{" "}
                {plural(nodes.total, "server")} online
              </span>
            </span>
            {lastDeploy ? (
              <span>
                Last deploy{" "}
                <span className="text-foreground">
                  {timeAgo(lastDeploy.createdAt)}
                </span>
              </span>
            ) : null}
          </p>
        </div>
        <ButtonLink href="/projects/new" size="sm" variant="secondary">
          <PlusIcon size={13} weight="bold" /> New project
        </ButtonLink>
      </div>

      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_320px]">
        <div className="min-w-0 space-y-5">
          <Layered
            id="projects-h"
            title={
              <span className="inline-flex items-center gap-2">
                Projects
                {projects.length ? (
                  <span className="rounded-full bg-foreground/[0.06] px-1.5 text-[11.5px] font-medium tabular-nums text-muted-foreground">
                    {projects.length}
                  </span>
                ) : null}
              </span>
            }
            bare
            footer={
              !ql && projects.length > shown.length ? (
                <AppLink
                  href="/projects"
                  className="group inline-flex items-center gap-1 font-medium text-foreground/85 hover:text-foreground"
                >
                  View all {projects.length} projects
                  <ArrowRightIcon
                    size={12}
                    className="transition-transform group-hover:translate-x-0.5"
                  />
                </AppLink>
              ) : ql && shown.length ? (
                <span>
                  {plural(shown.length, "project")} match
                  {shown.length === 1 ? "es" : ""} “{query.trim()}”
                </span>
              ) : !ql && projects.length ? (
                <AppLink
                  href="/projects"
                  className="group inline-flex items-center gap-1 font-medium text-foreground/85 hover:text-foreground"
                >
                  Open Projects
                  <ArrowRightIcon
                    size={12}
                    className="transition-transform group-hover:translate-x-0.5"
                  />
                </AppLink>
              ) : undefined
            }
          >
            {projects.length > 1 ? (
              <SearchInput
                value={query}
                onChange={setQuery}
                placeholder="Search projects…"
                shortcut="/"
                className="mb-1.5 [&_input]:rounded-[10px] [&_input]:pl-9 [&_svg]:left-3.5"
                label="Search projects"
              />
            ) : null}
            {shown.length ? (
              <div className="grid gap-1.5 sm:grid-cols-2">
                {shown.map(({ p, st }) => (
                  <ProjectCard key={p.id} p={p} st={st} />
                ))}
              </div>
            ) : (
              <div className="flex flex-col items-center rounded-[14px] border border-dashed border-card-border bg-card px-6 py-12 text-center">
                <span className="flex h-10 w-10 items-center justify-center rounded-full border border-border text-muted-foreground">
                  {ql ? (
                    <MagnifyingGlassIcon size={17} />
                  ) : (
                    <ProjectIcon size={17} />
                  )}
                </span>
                <p className="mt-3 text-[13.5px] font-medium">
                  {ql
                    ? `No projects match “${query.trim()}”`
                    : "No projects yet"}
                </p>
                <p className="mt-1 text-[12.5px] text-muted-foreground">
                  {ql
                    ? "Try another name or domain."
                    : "Group the services you deploy."}
                </p>
                {ql ? (
                  <Button
                    size="xs"
                    variant="secondary"
                    className="mt-4"
                    onClick={() => setQuery("")}
                  >
                    Clear search
                  </Button>
                ) : (
                  <ButtonLink href="/projects/new" size="xs" className="mt-4">
                    <PlusIcon size={12} weight="bold" /> New project
                  </ButtonLink>
                )}
              </div>
            )}
          </Layered>
        </div>

        <aside className="space-y-5">
          <Layered id="deploys-h" title="Recent deployments" bare>
            <DeploymentList deployments={deployments} s={s} />
          </Layered>
          {domains.length ? (
            <Layered
              id="domains-h"
              title="Domains"
              bare
              footer={
                <span className="flex items-center justify-between gap-2">
                  <span>{plural(domains.length, "domain")}</span>
                  <AppLink
                    href="/domains"
                    className="group inline-flex items-center gap-1 font-medium text-foreground/85 hover:text-foreground"
                  >
                    View all domains
                    <ArrowRightIcon
                      size={12}
                      className="transition-transform group-hover:translate-x-0.5"
                    />
                  </AppLink>
                </span>
              }
            >
              <ul className="overflow-hidden rounded-[14px] border border-card-border bg-card">
                {domains.slice(0, 5).map(({ d, a }, i) => (
                  <li
                    key={`${a.id}:${d.id}`}
                    className={`flex items-center justify-between gap-3 px-4 py-2.5 ${i ? "border-t border-border" : ""}`}
                  >
                    <span className="min-w-0">
                      <AppLink
                        href={servicePath(a.id, "domains")}
                        className="block truncate font-mono text-[12.5px] text-foreground hover:text-primary"
                      >
                        {d.hostname}
                      </AppLink>
                      <span className="block truncate text-[11.5px] text-muted-foreground">
                        {a.name}
                      </span>
                    </span>
                    <span className="shrink-0 text-[12px]">
                      <DomainStatus
                        d={d}
                        certs={s.certificates}
                        notDeployed={!a.currentDeploymentId}
                      />
                    </span>
                  </li>
                ))}
              </ul>
            </Layered>
          ) : null}
          {clusters.length ? (
            <Layered
              id="servers-h"
              title="Clusters"
              bare
              footer={
                <span className="flex items-center justify-between gap-2">
                  <span>
                    {nodes.ready} of {plural(nodes.total, "server")} online
                  </span>
                  <AppLink
                    href="/clusters"
                    className="group inline-flex items-center gap-1 font-medium text-foreground/85 hover:text-foreground"
                  >
                    View clusters
                    <ArrowRightIcon
                      size={12}
                      className="transition-transform group-hover:translate-x-0.5"
                    />
                  </AppLink>
                </span>
              }
            >
              <ServersGlance s={s} clusters={clusters} />
            </Layered>
          ) : null}
        </aside>
      </div>

      {s.observabilitySettings.enabled ? (
        <Pulse
          team={ws.id}
          hasSites={apps.some((a) =>
            a.domains.some((d) => d.status === "active"),
          )}
        />
      ) : null}
    </Page>
  );
}

/** Each cluster's servers and whether they're up: cluster status beside its name, server status beside each. */
function ServersGlance({
  s,
  clusters,
}: {
  s: ReturnType<typeof usePyo>;
  clusters: Cluster[];
}) {
  return (
    <div className="overflow-hidden rounded-[14px] border border-card-border bg-card">
      {clusters.map((c, i) => {
        const nodes = s.servers.filter((n) => n.clusterId === c.id);
        return (
          <div key={c.id} className={i ? "border-t border-border" : ""}>
            <div className="flex items-center justify-between gap-3 bg-muted/30 px-4 py-2">
              <span className="inline-flex min-w-0 items-center gap-2 text-[12px] font-medium text-muted-foreground">
                <span className="truncate">{c.name}</span>
              </span>
              <span className="shrink-0 text-[12px]">
                <ClusterStatusBadge status={c.status} />
              </span>
            </div>
            {nodes.length ? (
              <ul className="list-rows">
                {nodes.map((n) => (
                  <li key={n.id}>
                    <Link
                      to="/$team/clusters/$clusterId/$serverId"
                      params={{
                        team: currentSlug(),
                        clusterId: c.name,
                        serverId: n.name,
                      }}
                      className="flex items-center justify-between gap-3 px-4 py-2.5 transition-colors hover:bg-accent/40"
                    >
                      <span className="inline-flex min-w-0 items-center gap-2">
                        <Flag
                          country={
                            parseLocation(n.location ?? c.location).country
                          }
                          size={14}
                        />
                        <span className="truncate text-[13px] font-medium">
                          {n.name}
                        </span>
                      </span>
                      <span className="shrink-0 text-[12px]">
                        <NodeStatusBadge
                          status={n.status}
                          schedulable={n.schedulable}
                          maintenance={n.maintenance}
                        />
                      </span>
                    </Link>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="px-4 py-2.5 text-[12.5px] text-muted-foreground">
                No servers yet
              </p>
            )}
          </div>
        );
      })}
    </div>
  );
}

/** A glance at traffic and load, each linking to its analytics view. */
function Pulse({ team, hasSites }: { team: string; hasSites: boolean }) {
  const web = useQuery(
    `pulse-web:${team}:${hasSites}`,
    () =>
      hasSites
        ? fetchAnalytics({ team, project: "all", site: "all", range: "7d" })
        : Promise.resolve(null),
    { refreshMs: 60_000 },
  );
  const srv = useQuery(
    `pulse-srv:${team}`,
    () => fetchMetrics({ team, cluster: "all", range: "24h" }),
    { refreshMs: 60_000 },
  );
  const w = web.status === "success" ? web.data : null;
  const m = srv.status === "success" ? srv.data : null;
  // Totals over servers; null where none reported (a gap).
  const sumOver = (pick: (n: MetricsEntity) => (number | null)[]) =>
    m
      ? m.times.map((_, i) => {
          const vs = m.servers
            .map((n) => pick(n)[i])
            .filter((v): v is number => v != null);
          return vs.length ? vs.reduce((a, v) => a + v, 0) : null;
        })
      : [];
  const cpuCap = m?.servers.reduce((a, n) => a + n.cpuCapacity, 0) ?? 0;
  const memCap = m?.servers.reduce((a, n) => a + n.memoryCapacityMi, 0) ?? 0;
  const cpuSeries = sumOver((n) => n.cpu).map((v) =>
    v == null ? null : cpuCap ? v / cpuCap : 0,
  );
  const memSeries = sumOver((n) => n.memoryMi).map((v) =>
    v == null ? null : memCap ? v / memCap : 0,
  );
  // A share's scale fits its peak (as the usage charts do), so 8% isn't flat.
  const shareTop = (v: (number | null)[]) => {
    const peak = Math.max(0, ...v.filter((x): x is number => x != null));
    return [0.04, 0.08, 0.2, 0.4, 0.6, 0.8].find((x) => peak <= x * 0.9) ?? 1;
  };
  const lastOf = (v: (number | null)[]) => {
    for (let i = v.length - 1; i >= 0; i--)
      if (v[i] != null) return v[i] as number;
    return 0;
  };
  const change = (now: number, before: number) =>
    before > 0 ? (now - before) / before : NaN;

  const tiles: {
    label: string;
    period: string;
    value: string;
    sub?: string;
    trend?: number;
    values: (number | null)[];
    server: boolean;
    loading: boolean;
  }[] = [
    {
      label: "Visitors",
      period: "7 days",
      value: w
        ? compact(w.summary.estimatedVisitors)
        : hasSites
          ? "–"
          : "No sites",
      trend: w
        ? change(w.summary.estimatedVisitors, w.previous.estimatedVisitors)
        : undefined,
      values: w?.series.map((p) => p.visitors) ?? [],
      server: false,
      loading: hasSites && web.status === "loading",
    },
    {
      label: "Requests",
      period: "7 days",
      value: w ? compact(w.summary.requests) : "–",
      sub: w ? `${pct(w.summary.errorRate, 1)} errors` : undefined,
      values: w?.series.map((p) => p.requests) ?? [],
      server: false,
      loading: hasSites && web.status === "loading",
    },
    {
      label: "CPU",
      period: "24 hours",
      value: m?.times.length ? pct(lastOf(cpuSeries)) : "–",
      sub: m?.times.length ? `of ${cpu(cpuCap * 1000)}` : undefined,
      values: cpuSeries,
      server: true,
      loading: srv.status === "loading",
    },
    {
      label: "Memory",
      period: "24 hours",
      value: m?.times.length ? pct(lastOf(memSeries)) : "–",
      sub: m?.times.length ? `of ${mem(memCap)}` : undefined,
      values: memSeries,
      server: true,
      loading: srv.status === "loading",
    },
  ];

  return (
    <Layered
      id="pulse-h"
      title="Analytics"
      bare
      footer={
        <span className="flex items-center justify-between gap-2">
          <span>Traffic over the last 7 days, usage over 24 hours.</span>
          <AppLink
            href="/observability/analytics"
            className="group inline-flex items-center gap-1 font-medium text-foreground/85 hover:text-foreground"
          >
            View analytics
            <ArrowRightIcon
              size={12}
              className="transition-transform group-hover:translate-x-0.5"
            />
          </AppLink>
        </span>
      }
    >
      <div className="grid grid-cols-2 overflow-hidden rounded-[14px] border border-card-border bg-card lg:grid-cols-4">
        {tiles.map((t, i) => (
          <Link
            key={t.label}
            to="/$team/observability/analytics"
            params={{ team: currentSlug() }}
            search={t.server ? { view: "server" } : {}}
            className={`group flex min-w-0 flex-col gap-2 overflow-hidden border-border px-4 py-4 transition-colors hover:bg-accent/40 ${i % 2 === 0 ? "border-r" : ""} ${i < 2 ? "border-b lg:border-b-0" : ""} lg:border-r lg:last:border-r-0`}
          >
            <span className="flex items-baseline justify-between gap-2 text-[12.5px]">
              <span className="font-medium text-muted-foreground">
                {t.label}
              </span>
              <span className="text-[11.5px] text-muted-foreground/80">
                {t.period}
              </span>
            </span>
            {t.loading ? (
              <Skeleton className="h-7 w-20" />
            ) : (
              <span className="flex flex-wrap items-baseline gap-x-2">
                <span className="text-[22px] font-semibold tabular-nums tracking-tight">
                  {t.value}
                </span>
                {t.trend !== undefined && Number.isFinite(t.trend) ? (
                  <span
                    className={`text-[11.5px] font-medium tabular-nums ${Math.abs(t.trend) < 0.005 ? "text-muted-foreground" : t.trend > 0 ? "text-success" : "text-destructive"}`}
                  >
                    {t.trend > 0 ? "+" : ""}
                    {Math.round(t.trend * 100)}%
                  </span>
                ) : null}
                {t.sub ? (
                  <span className="text-[12px] tabular-nums text-muted-foreground">
                    {t.sub}
                  </span>
                ) : null}
              </span>
            )}
            <span className="block h-7">
              {t.values.length > 1 ? (
                <Sparkline
                  values={t.values}
                  width={240}
                  height={28}
                  fill
                  label={`${t.label} trend`}
                  domain={t.server ? [0, shareTop(t.values)] : undefined}
                />
              ) : null}
            </span>
          </Link>
        ))}
      </div>
    </Layered>
  );
}

function GetStarted({
  name,
  server,
  builder,
}: {
  name: string;
  server?: Cluster;
  builder?: Builder;
}) {
  const steps = [
    {
      title: "Server",
      body: server ? server.name : "Where your services run.",
      done: !!server,
      action: server ? null : (
        <ButtonLink href="/clusters/new" size="sm">
          Add cluster
        </ButtonLink>
      ),
    },
    {
      title: "Builder",
      body: builder
        ? builder.status === "ready"
          ? builder.name
          : `Starting ${builder.name}…`
        : "Turns your code into an image.",
      done: builder?.status === "ready",
      action: builder ? null : (
        <ButtonLink href="/builders/new" size="sm" variant="secondary">
          Add builder
        </ButtonLink>
      ),
    },
    {
      title: "Deploy something",
      body: "From a template, Git or an image.",
      done: false,
      action: (
        <ButtonLink href="/deploy/new" size="sm">
          New deploy
        </ButtonLink>
      ),
    },
  ];
  return (
    <div className="py-6">
      <h1 className="text-[24px] font-semibold tracking-tight">
        Welcome to {name}
      </h1>
      <p className="mt-1.5 text-[14px] text-muted-foreground">
        Your first deploy.
      </p>
      <div className="mt-8">
        <Layered>
          <ol className="list-rows">
            {steps.map((st, i) => (
              <li
                key={st.title}
                className="flex flex-wrap items-center gap-4 px-5 py-4"
              >
                <span
                  className={`flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-[11.5px] font-semibold ${st.done ? "bg-success/15 text-success" : "bg-secondary text-muted-foreground"}`}
                >
                  {st.done ? (
                    <CheckIcon size={12} weight="bold" aria-label="Done" />
                  ) : (
                    i + 1
                  )}
                </span>
                <div className="min-w-0 flex-1">
                  <p className="text-[14px] font-medium">{st.title}</p>
                  <p className="text-[13px] text-muted-foreground">{st.body}</p>
                </div>
                {st.action}
              </li>
            ))}
          </ol>
        </Layered>
      </div>
    </div>
  );
}
