import { AppLink } from "~/components/ui/AppLink";
import {
  ArrowRightIcon,
  CheckCircleIcon,
  WarningCircleIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { Dot, DeploymentStatusBadge } from "~/components/ui/Badge";
import { EmptyState, Meter, Panel } from "~/components/ui/Panel";
import { duration, pct } from "~/lib/pyo/format";
import { isActive } from "~/lib/pyo/simulation";
import {
  clusterRef,
  getState,
  serverRef,
  type PyoState,
  serverHref,
  projectPath,
  servicePath,
} from "~/lib/pyo/store";
import type { ActivityEvent, Deployment } from "~/lib/pyo/types";
import { SourceSummary, TimeAgo } from "./common";
import { currentSlug } from "~/lib/pyo/paths";

// Panels reused across overview, project, service and cluster pages.

export function DeploymentRows({
  deployments,
  s,
  showService = true,
  empty,
}: {
  deployments: Deployment[];
  s: PyoState;
  showService?: boolean;
  empty?: ReactNode;
}) {
  if (!deployments.length)
    return <>{empty ?? <EmptyState compact title="No deployments yet" />}</>;
  return (
    <ul className="list-rows">
      {deployments.map((d) => {
        const app = s.services.find((a) => a.id === d.serviceId);
        const elapsed = (d.finishedAt ?? Date.now()) - d.createdAt;
        return (
          <li key={d.id}>
            <Link
              to="/$team/deployments/$deploymentId"
              params={{ team: currentSlug(), deploymentId: d.id }}
              className="flex items-center gap-3 px-4 py-2.5 transition-colors hover:bg-accent/40"
            >
              <div className="w-[150px] shrink-0">
                <DeploymentStatusBadge status={d.status} />
              </div>
              <div className="min-w-0 flex-1">
                {showService ? (
                  <p className="truncate text-[13px] font-medium text-foreground">
                    {app?.name ?? "Deleted service"}
                  </p>
                ) : null}
                <div className="flex min-w-0 items-center gap-2 text-[12px] text-muted-foreground">
                  <SourceSummary
                    source={d.source}
                    revision={d.source.type === "git" ? d.revision : undefined}
                  />
                </div>
              </div>
              <div className="hidden shrink-0 text-right text-[12px] text-muted-foreground sm:block">
                <TimeAgo at={d.createdAt} />
                <p className="tabular-nums">
                  {isActive(d.status) ? "running" : duration(elapsed)}
                </p>
              </div>
            </Link>
          </li>
        );
      })}
    </ul>
  );
}

const ACT_TONE = {
  success: "success",
  failure: "danger",
  warning: "warning",
  info: "neutral",
} as const;

/** Where an activity event leads, and what the link to it says. */
export function activityLink(e: ActivityEvent): {
  href: string;
  label?: string;
} {
  if (e.target.kind === "deployment")
    return { href: `/deployments/${e.target.id}`, label: "Open deployment" };
  if (e.target.kind === "server")
    return { href: serverHref(getState(), e.target.id), label: "Open server" };
  if (e.target.kind === "volume")
    return { href: "/clusters/storage", label: "Open storage" };
  if (e.serviceId)
    return { href: servicePath(e.serviceId), label: "Open service" };
  if (e.clusterId)
    return {
      href: `/clusters/${clusterRef(getState(), e.clusterId)}`,
      label: "Open cluster",
    };
  if (e.projectId)
    return { href: projectPath(e.projectId), label: "Open project" };
  return { href: "/activity" };
}

export const activityHref = (e: ActivityEvent) => activityLink(e).href;

export function ActivityRows({
  events,
  onSelect,
  emptyTitle = "No activity yet",
}: {
  events: ActivityEvent[];
  onSelect?: (e: ActivityEvent) => void;
  emptyTitle?: string;
}) {
  if (!events.length) return <EmptyState compact title={emptyTitle} />;
  return (
    <ul className="list-rows">
      {events.map((e) => {
        const body = (
          <>
            <Dot tone={ACT_TONE[e.status]} className="mt-[7px]" />
            <div className="min-w-0 flex-1">
              <p className="truncate text-[12.5px] text-foreground">
                {e.title}
              </p>
              <p className="truncate text-[11.5px] text-muted-foreground">
                {e.actor.name}
                {e.details.revision ? ` · ${e.details.revision}` : ""}
                {e.details.reason ? ` · ${e.details.reason}` : ""}
              </p>
            </div>
            <TimeAgo
              at={e.at}
              className="shrink-0 text-[11.5px] text-muted-foreground"
            />
          </>
        );
        return (
          <li key={e.id}>
            {onSelect ? (
              <button
                type="button"
                onClick={() => onSelect(e)}
                className="flex w-full items-start gap-2.5 px-4 py-2.5 text-left transition-colors hover:bg-accent/40"
              >
                {body}
              </button>
            ) : (
              <AppLink
                href={activityHref(e)}
                className="flex items-start gap-2.5 px-4 py-2.5 transition-colors hover:bg-accent/40"
              >
                {body}
              </AppLink>
            )}
          </li>
        );
      })}
    </ul>
  );
}

/**
 * Current CPU and memory from metrics-server, as fractions of capacity or
 * limits. History isn't stored yet, so there's no chart over time.
 */
export function UtilizationPanel({
  cpuNow,
  memNow,
  title = "Resource utilization",
  description,
  unavailable,
}: {
  seed?: string;
  cpuNow: number;
  memNow: number;
  title?: string;
  description?: ReactNode;
  unavailable?: string;
}) {
  return (
    <Panel title={title} description={description}>
      {unavailable ? (
        <p className="px-4 py-6 text-[12.5px] text-muted-foreground">
          {unavailable}
        </p>
      ) : (
        <div className="space-y-4 px-4 py-4">
          {(
            [
              ["CPU", cpuNow],
              ["Memory", memNow],
            ] as const
          ).map(([label, v]) => (
            <div key={label}>
              <div className="mb-1.5 flex items-center justify-between text-[12.5px]">
                <span className="text-muted-foreground">{label}</span>
                <span className="tabular-nums">{pct(v)}</span>
              </div>
              <Meter
                value={v}
                max={1}
                label={`${label} utilization`}
                showLabel={false}
              />
            </div>
          ))}
          <p className="text-[11.5px] text-muted-foreground">Right now.</p>
        </div>
      )}
    </Panel>
  );
}

export type Issue = {
  id: string;
  severity: "critical" | "warning";
  title: string;
  detail: string;
  href: string;
};

export function IssueList({ issues }: { issues: Issue[] }) {
  if (!issues.length)
    return (
      <div className="flex items-center gap-2.5 px-4 py-4 text-[12.5px] text-muted-foreground">
        <CheckCircleIcon size={16} weight="fill" className="text-success" />
        Nothing needs attention.
      </div>
    );
  return (
    <ul className="list-rows">
      {issues.map((i) => (
        <li key={i.id}>
          <AppLink
            href={i.href}
            className="group flex items-start gap-2.5 px-4 py-2.5 transition-colors hover:bg-accent/40"
          >
            {i.severity === "critical" ? (
              <WarningCircleIcon
                size={15}
                weight="fill"
                className="mt-0.5 shrink-0 text-destructive"
                aria-label="Critical"
              />
            ) : (
              <WarningIcon
                size={15}
                weight="fill"
                className="mt-0.5 shrink-0 text-warning"
                aria-label="Warning"
              />
            )}
            <div className="min-w-0 flex-1">
              <p className="text-[12.5px] font-medium text-foreground">
                {i.title}
              </p>
              <p className="text-[12px] text-muted-foreground">{i.detail}</p>
            </div>
            <ArrowRightIcon
              size={13}
              className="mt-1 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100"
            />
          </AppLink>
        </li>
      ))}
    </ul>
  );
}

/** Everything that needs attention in a team (optionally one project). */
export function collectIssues(
  s: PyoState,
  wsId: string,
  projectId?: string,
): Issue[] {
  const out: Issue[] = [];
  const apps = s.services.filter(
    (a) => a.teamId === wsId && (!projectId || a.projectId === projectId),
  );
  for (const a of apps) {
    if (a.status === "failed")
      out.push({
        id: `service-${a.id}`,
        severity: "critical",
        title: `${a.name} is down`,
        detail: `0/${a.replicas} ready · ${a.runtime.restarts} restarts`,
        href: servicePath(a.id),
      });
    if (a.status === "degraded")
      out.push({
        id: `service-${a.id}`,
        severity: "warning",
        title: (a.runtime.waiting ?? 0) > 0
          ? `${a.name} is waiting for room`
          : `${a.name} is degraded`,
        detail: (a.runtime.waiting ?? 0) > 0
          ? `${a.runtime.readyReplicas}/${a.replicas} ready, ${a.runtime.waiting} waiting for room`
          : `${a.runtime.readyReplicas}/${a.replicas} ready`,
        href: servicePath(a.id),
      });
    for (const d of a.domains)
      if (d.status === "error")
        out.push({
          id: `dom-${d.id}`,
          severity: "warning",
          title: `${d.hostname} DNS error`,
          detail: d.error ?? "Verification failed",
          href: servicePath(a.id, "domains"),
        });
  }
  const dayAgo = Date.now() - 86_400_000;
  for (const d of s.deployments) {
    if (
      d.teamId !== wsId ||
      (projectId && d.projectId !== projectId) ||
      d.status !== "failed" ||
      (d.finishedAt ?? 0) < dayAgo
    )
      continue;
    const a = s.services.find((x) => x.id === d.serviceId);
    const newer = s.deployments.some(
      (x) =>
        x.serviceId === d.serviceId &&
        x.createdAt > d.createdAt &&
        x.status === "succeeded",
    );
    if (!newer)
      out.push({
        id: `dep-${d.id}`,
        severity: "critical",
        title: `${a?.name ?? "Service"} deploy failed`,
        detail: d.failure?.message ?? "See logs",
        href: `/deployments/${d.id}`,
      });
  }
  if (!projectId) {
    const clusterIds = new Set(
      s.clusters.filter((c) => c.teamId === wsId).map((c) => c.id),
    );
    for (const c of s.clusters.filter(
      (x) => clusterIds.has(x.id) && x.status === "unreachable",
    ))
      out.push({
        id: `cl-${c.id}`,
        severity: "critical",
        title: `${c.name} is unreachable`,
        detail: "Agent not reporting",
        href: `/clusters/${c.name}`,
      });
    for (const n of s.servers.filter(
      (x) => clusterIds.has(x.clusterId) && x.status !== "ready",
    )) {
      const c = s.clusters.find((x) => x.id === n.clusterId)!;
      out.push({
        id: `node-${n.id}`,
        severity: "critical",
        title: `${n.name} is offline`,
        detail: `Server in ${c.name} is not responding`,
        href: serverHref(s, n.id),
      });
    }
    for (const v of s.volumes.filter(
      (x) =>
        x.teamId === wsId && (x.status === "degraded" || x.status === "error"),
    ))
      out.push({
        id: `vol-${v.id}`,
        severity: v.status === "error" ? "critical" : "warning",
        title: `${v.name} volume ${v.status}`,
        detail: v.statusMessage ?? "",
        href: "/clusters/storage",
      });
    for (const v of s.volumes.filter(
      (x) => x.teamId === wsId && x.backup.status === "failed",
    ))
      out.push({
        id: `bak-${v.id}`,
        severity: "warning",
        title: `${v.name} backup failed`,
        detail: "Last backup didn't finish",
        href: "/clusters/storage",
      });
  }
  return out.sort((a, b) =>
    a.severity === b.severity ? 0 : a.severity === "critical" ? -1 : 1,
  );
}
