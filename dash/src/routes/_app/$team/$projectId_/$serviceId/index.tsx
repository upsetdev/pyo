import { Segmented } from "~/components/ui/Field";
import { fetchPreview } from "~/lib/pyo/backend";
import { Layered } from "~/components/ui/Layered";
import {
  ArrowCounterClockwiseIcon,
  CircleNotchIcon,
  GlobeSimpleIcon,
  HardDrivesIcon,
  LockSimpleIcon,
  MinusCircleIcon,
  WarningCircleIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { servicePlaces, SourcePill } from "~/components/pyo/ProjectCard";
import { PlaceFlags } from "~/components/ui/Flag";
import { locationLabel } from "~/lib/pyo/location";
import { TrafficCanvas } from "~/components/pyo/TrafficCanvas";
import { ServerRole } from "~/components/pyo/infra";
import { ConnectionsCard, hasPublicPorts } from "~/components/pyo/ConnectionsCard";
import { useState, type ReactNode } from "react";
import { TimeSeriesChart } from "~/components/charts/Charts";
import {
  DeploymentsTable,
  previousVersion,
  RollbackDialog,
} from "~/components/pyo/DeploymentsTable";
import { DeploymentStatusBadge, Dot } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Meter, Panel, Skeleton } from "~/components/ui/Panel";
import { StatusNote, type NoteTone } from "~/components/ui/StatusNote";
import { CopyButton } from "~/components/ui/Table";
import { fetchAnalytics } from "~/lib/pyo/backend";
import {
  compact,
  cpu,
  domainUrl,
  duration,
  mem,
  pct,
  rate,
  timeAgo,
} from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import {
  activeDeploymentFor,
  desiredReplicas,
  usePyo,
  type PyoState,
  clusterRef,
  serviceParams,
  servicePath,
  serviceIdOf,
} from "~/lib/pyo/store";
import type { Application, Deployment } from "~/lib/pyo/types";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/")({
  component: AppOverview,
});

// Like a project page on Vercel: the live version with a preview (and a
// one-click rollback), then its traffic, how hard it's working, and the
// latest deployments.

function AppOverview() {
  const serviceId = serviceIdOf(Route.useParams());
  const s = usePyo();
  const navigate = useNavigate();
  const [rollbackTo, setRollbackTo] = useState<Deployment | null>(null);
  const app = s.services.find((a) => a.id === serviceId);
  if (!app) return null;
  const project = s.projects.find((p) => p.id === app.projectId);
  const current = s.deployments.find((d) => d.id === app.currentDeploymentId);
  const active = activeDeploymentFor(s, app.id);
  const deployments = s.deployments
    .filter((d) => d.serviceId === app.id)
    .sort((a, b) => b.createdAt - a.createdAt);
  const port = app.ports[0]?.containerPort;
  const host = `${app.name}.${project?.namespace ?? "default"}.svc.cluster.local${port ? `:${port}` : ""}`;

  const previous = previousVersion(s, app.id);
  const latest = deployments[0];

  return (
    <div className="space-y-6">
      <Attention
        app={app}
        activeId={active?.id}
        current={current}
        latest={latest}
        s={s}
        onRollback={setRollbackTo}
      />
      <Production
        app={app}
        current={current}
        host={host}
        previous={active ? undefined : previous}
        onRollback={setRollbackTo}
      />
      <div className="grid gap-5 lg:grid-cols-[minmax(0,1.8fr)_minmax(0,1fr)]">
        {app.workloadKind === "CronJob" ? null : <AnalyticsCard app={app} />}
        <Resources app={app} s={s} />
      </div>
      {app.workloadKind !== "CronJob" && hasPublicPorts(app) ? (
        <ConnectionsCard app={app} />
      ) : null}
      {app.workloadKind === "CronJob" ? null : <TrafficCanvas app={app} />}
      <DeploymentsTable
        title="Deployments"
        action={
          <Link
            to="/$team/$projectId/$serviceId/deployments"
            params={{ team: currentSlug(), ...serviceParams(app.id) }}
            className="text-[12.5px] font-medium text-muted-foreground transition-colors hover:text-foreground"
          >
            View all {deployments.length > 5 ? `(${deployments.length})` : ""}
          </Link>
        }
        deployments={deployments.slice(0, 5)}
        s={s}
        showService={false}
        filters={false}
      />
      <RollbackDialog
        s={s}
        to={rollbackTo}
        onClose={() => setRollbackTo(null)}
        onDone={(id) =>
          void navigate({
            to: "/$team/deployments/$deploymentId",
            params: { team: currentSlug(), deploymentId: id },
          })
        }
      />
    </div>
  );
}

// ── Production ───────────────────────────────────────────────────────────────

function Production({
  app,
  current,
  host,
  previous,
  onRollback,
}: {
  app: Application;
  current?: Deployment;
  host: string;
  previous?: Deployment;
  onRollback: (d: Deployment) => void;
}) {
  const s = usePyo();
  const team = currentSlug();
  const stopped = app.status === "stopped";
  const cron = app.workloadKind === "CronJob";
  const want = stopped ? 0 : desiredReplicas(app);
  const ready = app.runtime.readyReplicas;
  const domains = app.domains;
  const live = domains.find((d) => d.status === "active");
  const up = app.status === "running" || app.status === "degraded";
  const src = current?.source ?? app.source;

  return (
    <Layered bare>
      <section
        aria-label="Production"
        className="overflow-hidden rounded-[14px] border border-card-border bg-card"
      >
        <div
          className={`grid ${cron ? "" : "md:grid-cols-[minmax(0,5fr)_minmax(0,7fr)]"}`}
        >
          {cron ? null : (
            <div className="border-b border-border p-4 md:border-b-0 md:border-r">
              <Preview
                serviceId={app.id}
                url={live && up ? domainUrl(live) : undefined}
                name={app.name}
              />
            </div>
          )}
          <div className="grid content-start gap-5 p-5 sm:grid-cols-2">
            <div className="space-y-5">
              <Field label="Deployment">
                {current ? (
                  <Link
                    to="/$team/deployments/$deploymentId"
                    params={{ team, deploymentId: current.id }}
                    className="font-mono text-[12.5px] hover:text-primary"
                  >
                    {current.id}
                  </Link>
                ) : (
                  <span className="text-muted-foreground">None yet</span>
                )}
              </Field>
              {cron ? null : (
              <Field label={domains.length > 1 ? "Domains" : "Domain"}>
                {domains.length ? (
                  <span className="flex flex-col gap-1">
                    {domains.slice(0, 2).map((d) => (
                      <a
                        key={d.id}
                        href={domainUrl(d)}
                        target="_blank"
                        rel="noreferrer"
                        className="inline-flex min-w-0 items-center gap-1.5 hover:text-primary"
                      >
                        <GlobeSimpleIcon
                          size={13}
                          className="shrink-0 text-muted-foreground"
                        />
                        <span className="truncate">{d.hostname}</span>
                        {d.status !== "active" ? (
                          <span className="shrink-0 text-[12px] text-warning">
                            {d.status === "error"
                              ? "DNS error"
                              : "Waiting for DNS"}
                          </span>
                        ) : null}
                      </a>
                    ))}
                    {domains.length > 2 ? (
                      <Link
                        to="/$team/$projectId/$serviceId/domains"
                        params={{ team, ...serviceParams(app.id) }}
                        className="w-fit text-[12.5px] text-muted-foreground hover:text-foreground"
                      >
                        See more ({domains.length - 2})
                      </Link>
                    ) : null}
                  </span>
                ) : (
                  <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
                    <span className="inline-flex min-w-0 items-center gap-1.5 text-muted-foreground">
                      <LockSimpleIcon size={13} className="shrink-0" />
                      Private
                    </span>
                    <Link
                      to="/$team/$projectId/$serviceId/domains"
                      params={{ team, ...serviceParams(app.id) }}
                      className="text-[12.5px] text-muted-foreground hover:text-foreground"
                    >
                      Add domain
                    </Link>
                  </span>
                )}
              </Field>
              )}
              <Field label="Source">
                {/* One label: repo and deployed commit, or image and tag. */}
                <SourcePill
                  source={src}
                  revision={
                    src.type === "git"
                      ? src.commit ||
                        (current?.revisionLabel &&
                        current.revisionLabel !== src.branch
                          ? current.revisionLabel
                          : src.branch)
                      : src.type === "image" || src.type === "template"
                        ? src.reference
                        : undefined
                  }
                />
              </Field>
            </div>
            <div className="space-y-5">
              <Field label="Status">
                {current ? (
                  <span className="inline-flex items-center gap-2">
                    <DeploymentStatusBadge status={current.status} />
                    <span className="text-muted-foreground">
                      {timeAgo(current.finishedAt ?? current.createdAt)}
                    </span>
                  </span>
                ) : (
                  <span className="text-muted-foreground">Not deployed</span>
                )}
              </Field>
              {app.workloadKind === "CronJob" ? (
                <Field label="Schedule">
                  <span className="font-mono text-[12.5px]">
                    {app.schedule} UTC
                  </span>
                </Field>
              ) : null}
              {app.workloadKind === "CronJob" ? (
                <Field label="Last run">
                  <LastRun app={app} />
                </Field>
              ) : (
              <Field label="Replicas">
                <span className="flex items-center gap-2">
                  <span className="flex gap-0.5" aria-hidden>
                    {Array.from(
                      { length: Math.min(Math.max(want, 1), 12) },
                      (_, i) => (
                        <span
                          key={i}
                          className={`h-3.5 w-2 rounded-[2px] ${want === 0 ? "bg-muted" : i < ready ? "bg-success" : "bg-destructive/70"}`}
                        />
                      ),
                    )}
                  </span>
                  <span className="tabular-nums">
                    {ready}{" "}
                    <span className="text-muted-foreground">
                      of {want} ready
                      {app.autoscale
                        ? ` · autoscaling ${app.autoscale.min}–${app.autoscale.max}`
                        : ""}
                    </span>
                  </span>
                </span>
              </Field>
 )}
              {cron ? null : (
              <Field label="Uptime">
                {app.runtime.since && !stopped ? (
                  <span className="tabular-nums">
                    {duration(Date.now() - app.runtime.since)}
                  </span>
                ) : (
                  <span className="text-muted-foreground">Not running</span>
                )}
              </Field>
              )}
              <Field label={servicePlaces(s, app).length > 1 ? "Regions" : "Region"}>
                {(() => {
                  const places = servicePlaces(s, app);
                  if (!places.length)
                    return (
                      <span className="text-muted-foreground">Unknown</span>
                    );
                  return (
                    <span className="flex items-center gap-2">
                      <PlaceFlags places={places} size={24} />
                      {places.length === 1 ? (
                        <span className="truncate">
                          {locationLabel(places[0])}
                        </span>
                      ) : (
                        <span className="text-muted-foreground">
                          {places.length} regions
                        </span>
                      )}
                    </span>
                  );
                })()}
              </Field>
            </div>
            {current ? (
              <div className="flex flex-wrap gap-2 sm:col-span-2">
                <ButtonLink
                  href={`/deployments/${current.id}`}
                  size="xs"
                  variant="secondary"
                >
                  Build logs
                </ButtonLink>
                <ButtonLink
                  href={servicePath(app.id, "logs")}
                  size="xs"
                  variant="secondary"
                >
                  Runtime logs
                </ButtonLink>
                {previous ? (
                  <Button
                    size="xs"
                    variant="secondary"
                    onClick={() => onRollback(previous)}
                    title={`Go back to ${previous.revisionLabel || previous.id}, deployed ${timeAgo(previous.createdAt)}`}
                  >
                    <ArrowCounterClockwiseIcon size={12} /> Instant rollback
                  </Button>
                ) : null}
              </div>
            ) : null}
          </div>
        </div>
      </section>
    </Layered>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <p className="mb-1 text-[12px] text-muted-foreground">{label}</p>
      <div className="text-[13px]">{children}</div>
    </div>
  );
}

/** The preview's frame when there's nothing to show yet: theme surfaces only. */
function PreviewBlank({ children, pulse = false }: { children: ReactNode; pulse?: boolean }) {
  return (
    <div className={`absolute inset-0 flex flex-col items-center justify-center gap-2 bg-muted text-center ${pulse ? "motion-safe:animate-pulse" : ""}`}>
      <span className="flex h-10 w-10 items-center justify-center rounded-full border border-border bg-card text-muted-foreground shadow-card">
        <GlobeSimpleIcon size={18} />
      </span>
      {children}
    </div>
  );
}

/** The live site, shrunk into a card; a placeholder when there's nothing public. */
function Preview({ serviceId, url, name }: { serviceId: string; url?: string; name: string }) {
  const [loaded, setLoaded] = useState(false);
  // Many sites refuse to be framed; then the browser would show its own grey error page.
  const check = useQuery(`preview:${serviceId}:${url ?? ""}`, () =>
    url ? fetchPreview(serviceId) : Promise.resolve({ frameable: false, reason: "none" }),
  );
  const frameable = check.status === "success" && check.data.frameable;
  const host = url?.replace(/^https?:\/\//, "") ?? "";
  if (!url)
    return (
      <div className="relative aspect-[16/10] overflow-hidden rounded-lg border border-border">
        <PreviewBlank>
          <p className="px-6 text-[12.5px] text-muted-foreground">No public preview of {name}. Add a domain to see the site here.</p>
        </PreviewBlank>
      </div>
    );
  return (
    <a
      href={url}
      target="_blank"
      rel="noreferrer"
      aria-label={`Open ${url}`}
      className="group relative block aspect-[16/10] overflow-hidden rounded-lg border border-border bg-muted"
    >
      {!frameable || !loaded ? (
        <PreviewBlank pulse={check.status === "loading" || (frameable && !loaded)}>
          <p className="px-6 font-mono text-[12px] text-foreground/80">{host}</p>
          {check.status !== "loading" && !frameable ? (
            <p className="px-6 text-[12px] text-muted-foreground">
              {check.status === "success" && check.data.reason === "refused"
                ? "This site doesn't allow previews. Open it to see it."
                : "No preview right now. Open the site to see it."}
            </p>
          ) : null}
        </PreviewBlank>
      ) : null}
      {/* Rendered at 3x and scaled down, so it reads like a desktop screenshot. */}
      {frameable ? (
      <iframe
        src={url}
        title={`Preview of ${name}`}
        sandbox="allow-scripts"
        loading="lazy"
        tabIndex={-1}
        onLoad={() => setLoaded(true)}
        className={`pointer-events-none absolute left-0 top-0 h-[300%] w-[300%] origin-top-left scale-[0.3333] border-0 bg-white transition-opacity duration-300 ${loaded ? "opacity-100" : "opacity-0"}`}
      />
      ) : null}
      <span className="absolute inset-0 transition-colors group-hover:bg-black/5" />
    </a>
  );
}

// ── Analytics (requests and visitors) ───────────────────────────────────────

const RANGES = ["24h", "7d", "30d"] as const;

function AnalyticsCard({ app }: { app: Application }) {
  const team = currentSlug();
  const [range, setRange] = useState<(typeof RANGES)[number]>("24h");
  const [metric, setMetric] = useState(0);
  const hasDomain = app.domains.length > 0;
  const q = useQuery(
    `app-traffic:${app.id}:${range}:${hasDomain}`,
    () =>
      hasDomain
        ? fetchAnalytics({
            team: app.teamId,
            project: "all",
            site: "all",
            service: app.id,
            range,
          })
        : Promise.resolve(null),
    {
      refreshMs: 30_000,
    },
  );

  if (!hasDomain)
    return (
      <Panel title="Analytics">
        <div className="flex flex-col items-center gap-3 px-6 py-10 text-center">
          <AnalyticsGhost />
          <p className="max-w-sm text-[13px] text-muted-foreground">
            {hasPublicPorts(app)
              ? "Add a domain for website analytics."
              : "Analytics show up here once the service has a domain."}
          </p>
          <ButtonLink
            href={servicePath(app.id, "domains")}
            size="xs"
            variant="secondary"
          >
            Add domain
          </ButtonLink>
        </div>
      </Panel>
    );

  const data = q.status === "success" ? q.data : null;
  const series = data?.series ?? [];
  const sum = data?.summary;
  const metrics = [
    {
      label: "Requests",
      value: sum ? compact(sum.requests) : "–",
      values: series.map((p) => p.requests),
      format: compact,
    },
    {
      label: "Visitors",
      value: sum ? compact(sum.estimatedVisitors) : "–",
      values: series.map((p) => p.visitors),
      format: compact,
    },
    {
      label: "Error rate",
      value: sum ? pct(sum.errorRate, 1) : "–",
      values: series.map((p) => (p.requests ? p.errors / p.requests : 0)),
      format: (v: number) => pct(v, 1),
    },
    {
      label: "p95 response",
      value: sum ? `${Math.round(sum.latency.p95)} ms` : "–",
      values: series.map((p) => p.p95),
      format: (v: number) => `${Math.round(v)} ms`,
    },
  ];
  const m = metrics[metric];

  return (
    <Layered bare>
      <section
        aria-label="Analytics"
        className="overflow-hidden rounded-[14px] border border-card-border bg-card"
      >
        <div className="flex items-center justify-between gap-3 border-b border-border px-4 py-3">
          <div className="flex items-center gap-3">
            <h2 className="text-[14px] font-medium tracking-tight">Analytics</h2>
            <Link
              to="/$team/observability/analytics"
              params={{ team }}
              search={{
                project: app.projectId,
                site: app.domains.length === 1 ? app.domains[0].id : undefined,
              }}
              className="text-[12.5px] text-muted-foreground hover:text-foreground"
            >
              Open analytics
            </Link>
          </div>
          <Segmented
            label="Time range"
            size="xs"
            value={range}
            onChange={setRange}
            options={RANGES.map((r) => ({ value: r, label: r }))}
          />
        </div>
        <div
          role="tablist"
          aria-label="Metric"
          className="grid grid-cols-2 sm:grid-cols-4"
        >
          {metrics.map((x, i) => (
            <button
              key={x.label}
              type="button"
              role="tab"
              aria-selected={i === metric}
              onClick={() => setMetric(i)}
              className={`relative border-b border-border px-4 py-3 text-left transition-colors odd:border-r sm:border-r sm:last:border-r-0 ${i === metric ? "bg-accent/40" : "hover:bg-accent/20"}`}
            >
              <span className="block text-[12px] text-muted-foreground">
                {x.label}
              </span>
              <span className="mt-0.5 block text-[20px] font-semibold tabular-nums tracking-tight">
                {q.status === "loading" ? (
                  <Skeleton className="mt-1 h-6 w-14" />
                ) : (
                  x.value
                )}
              </span>
              {i === metric ? (
                <span className="absolute inset-x-0 bottom-0 h-0.5 bg-foreground" />
              ) : null}
            </button>
          ))}
        </div>
        <div className="px-3 pb-3 pt-4 sm:px-4">
          {q.status === "error" ? (
            <p className="py-12 text-center text-[13px] text-muted-foreground">
              {q.error}
            </p>
          ) : q.status === "loading" ? (
            <Skeleton className="h-[200px] w-full" />
          ) : (
            <TimeSeriesChart
              label={`${m.label} over the last ${range}`}
              area
              times={series.map((p) => p.at)}
              series={[
                {
                  key: m.label,
                  label: m.label,
                  color: "var(--series-1)",
                  values: m.values,
                },
              ]}
              format={m.format}
              height={200}
            />
          )}
        </div>
      </section>
    </Layered>
  );
}

/** A faint, dashed chart outline for the empty state. */
function AnalyticsGhost() {
  return (
    <svg
      width="160"
      height="48"
      viewBox="0 0 160 48"
      aria-hidden
      className="text-muted-foreground/40"
    >
      <path
        d="M0 40 C 20 38, 30 30, 45 32 S 70 18, 90 22 S 120 10, 135 14 S 155 6, 160 8"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeDasharray="4 4"
      />
      <line
        x1="0"
        y1="47"
        x2="160"
        y2="47"
        stroke="currentColor"
        strokeWidth="1"
      />
    </svg>
  );
}

function LastRun({ app }: { app: Application }) {
  const run = app.runtime.lastRun;
  if (!run) return <span className="text-muted-foreground">Not run yet</span>;
  return (
    <span>
      {run.succeeded == null
        ? "Running"
        : run.succeeded
          ? "Succeeded"
          : "Failed"}{" "}
      <span className="text-muted-foreground">{timeAgo(run.startedAt)}</span>
    </span>
  );
}

// ── Resources ────────────────────────────────────────────────────────────────

function Resources({ app, s }: { app: Application; s: PyoState }) {
  const team = currentSlug();
  const stopped = app.status === "stopped";
  const n = Math.max(1, stopped ? 0 : desiredReplicas(app));
  // Servers its pods run on, as the agent last saw them.
  const workload = s.systemWorkloads.find((w) => w.serviceId === app.id);
  const nodes = (workload?.servers ?? []).map((name) => ({
    name,
    node: s.servers.find((x) => x.name === name && x.clusterId === app.clusterId),
  }));
  return (
    <Panel title="Resources">
      <div className="space-y-5 px-4 py-4">
        <Usage
          label="CPU"
          used={app.runtime.cpuMillis}
          max={app.resources.cpuLimit * n}
          fmt={cpu}
        />
        <Usage
          label="Memory"
          used={app.runtime.memoryMi}
          max={app.resources.memoryLimit * n}
          fmt={mem}
        />
        <div className="space-y-1.5 border-t border-border pt-3 text-[12.5px]">
          <div className="flex items-baseline justify-between">
            <span className="text-muted-foreground">Network</span>
            <span className="tabular-nums">
              {app.runtime.netRxBps == null && app.runtime.netTxBps == null ? (
                "—"
              ) : (
                <>
                  {rate(app.runtime.netRxBps)} <span className="text-muted-foreground">in</span> ·{" "}
                  {rate(app.runtime.netTxBps)} <span className="text-muted-foreground">out</span>
                </>
              )}
            </span>
          </div>
          <div className="flex items-baseline justify-between">
            <span className="text-muted-foreground">Storage used</span>
            <span className="tabular-nums">
              {app.runtime.storageMi == null ? "—" : mem(app.runtime.storageMi)}
            </span>
          </div>
        </div>
        <div className="flex items-baseline justify-between border-t border-border pt-3 text-[12.5px]">
          <span className="text-muted-foreground">Restarts</span>
          <span
            className={`tabular-nums ${app.runtime.restarts > 5 ? "text-destructive" : ""}`}
          >
            {app.runtime.restarts}
          </span>
        </div>
        <div className="border-t border-border pt-3 text-[12.5px]">
          <p className="mb-2 text-muted-foreground">Running on</p>
          {nodes.length ? (
            <ul className="space-y-1.5">
              {nodes.map(({ name, node }) => (
                <li key={name}>
                  <Link
                    to="/$team/clusters/$clusterId/$serverId"
                    params={{
                      team,
                      clusterId: clusterRef(s, app.clusterId),
                      serverId: name,
                    }}
                    className="flex items-center gap-2 rounded-md hover:text-foreground"
                  >
                    <Dot
                      tone={
                        node?.status === "ready"
                          ? "success"
                          : node
                            ? "danger"
                            : "neutral"
                      }
                    />
                    <HardDrivesIcon
                      size={13}
                      className="shrink-0 text-muted-foreground"
                    />
                    <span className="truncate">{name}</span>
                    <span className="ml-auto shrink-0 text-muted-foreground">
                      {node ? (
                        node.status === "ready" ? (
                          <ServerRole node={node} focusable={false} />
                        ) : (
                          "Offline"
                        )
                      ) : null}
                    </span>
                  </Link>
                </li>
              ))}
            </ul>
          ) : (
            <p className="text-muted-foreground">
              {stopped
                ? "Nothing runs while it's stopped"
                : "Waiting for the agent to report"}
            </p>
          )}
        </div>
      </div>
    </Panel>
  );
}

function Usage({
  label,
  used,
  max,
  fmt,
}: {
  label: string;
  used: number;
  max: number;
  fmt: (v: number) => string;
}) {
  return (
    <div>
      <div className="mb-1.5 flex items-baseline justify-between text-[12.5px]">
        <span className="text-muted-foreground">{label}</span>
        <span className="tabular-nums">
          {fmt(used)}{" "}
          <span className="text-muted-foreground">/ {fmt(max)}</span>
        </span>
      </div>
      <Meter value={used} max={max} label={`${label} use`} />
    </div>
  );
}

// ── Attention ────────────────────────────────────────────────────────────────

/** Only when something is off or in motion; a healthy app needs no banner. */
function Attention({
  app,
  activeId,
  current,
  latest,
  s,
  onRollback,
}: {
  app: Application;
  activeId?: string;
  current?: Deployment;
  latest?: Deployment;
  s: PyoState;
  onRollback: (d: Deployment) => void;
}) {
  const { readyReplicas: ready, restarts } = app.runtime;
  const restarted = restarts
    ? ` Restarted ${restarts} ${restarts === 1 ? "time" : "times"}.`
    : "";
  const link = (href: string, label: string) => (
    <ButtonLink href={href} size="xs" variant="secondary">
      {label}
    </ButtonLink>
  );
  const logs = link(servicePath(app.id, "logs"), "View logs");
  const undoTo =
    current?.trigger === "rollback" && current.rollbackFrom
      ? s.deployments.find(
          (d) =>
            d.id === current.rollbackFrom &&
            d.status === "succeeded" &&
            !d.imageExpired,
        )
      : undefined;
  let note: {
    tone: NoteTone;
    icon: ReactNode;
    title: string;
    body: string;
    action?: ReactNode;
  } | null = null;

  if (activeId)
    note = {
      tone: "progress",
      icon: <CircleNotchIcon size={15} className="animate-spin" />,
      title: "Deploying…",
      body: "Rolling out. The current version serves until it's ready.",
      action: link(`/deployments/${activeId}`, "See logs"),
    };
  else if (
    latest &&
    latest.id !== current?.id &&
    latest.autoRolledBack &&
    (latest.status === "failed" || latest.status === "cancelled")
  )
    note = {
      tone: "warning",
      icon: <ArrowCounterClockwiseIcon size={15} />,
      title:
        latest.status === "failed"
          ? "Last deploy failed, so it was rolled back"
          : "Deploy cancelled and rolled back",
      body: `${latest.failure?.message ? `${latest.failure.message} ` : ""}The previous version is serving, so nothing changed for your users.`,
      action: link(`/deployments/${latest.id}`, "See what happened"),
    };
  else if (!current && app.status !== "running")
    note = {
      tone: "neutral",
      icon: <MinusCircleIcon size={15} />,
      title: "Not deployed yet",
      body: "Deploy it to start it on your cluster.",
    };
  else if (app.status === "degraded")
    note = {
      tone: "warning",
      icon: <WarningIcon size={15} />,
      title: "Partly running",
      body: `${ready} of ${desiredReplicas(app)} replicas are healthy.${restarted}`,
      action: logs,
    };
  else if (app.status === "failed")
    note = {
      tone: "danger",
      icon: <WarningCircleIcon size={15} />,
      title: "Down",
      body: `It crashed or isn't passing its health check.${restarted}`,
      action: logs,
    };
  else if (app.status === "stopped")
    note = {
      tone: "neutral",
      icon: <MinusCircleIcon size={15} />,
      title: "Stopped",
      body: "Scaled to zero. Settings and data are kept. Deploy to start it again.",
    };
  else if (current?.trigger === "rollback")
    note = {
      tone: "neutral",
      icon: <ArrowCounterClockwiseIcon size={15} />,
      title: `Rolled back to ${current.revisionLabel || "an earlier version"}`,
      body: `${current.actor} rolled back ${timeAgo(current.createdAt)}. The next deploy uses your saved settings.`,
      action: undoTo ? (
        <Button
          size="xs"
          variant="secondary"
          onClick={() => onRollback(undoTo)}
        >
          Undo rollback
        </Button>
      ) : undefined,
    };
  else if (restarts > 5)
    note = {
      tone: "warning",
      icon: <WarningIcon size={15} />,
      title: "Running, but restarting",
      body: `It has restarted ${restarts} times. The logs usually say why.`,
      action: logs,
    };

  if (!note) return null;
  return (
    <StatusNote
      tone={note.tone}
      icon={note.icon}
      title={note.title}
      action={note.action}
    >
      {note.body}
    </StatusNote>
  );
}
