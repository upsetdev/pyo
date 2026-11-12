import { NodeHistory } from "~/components/pyo/NodeHistory";
import { Layered } from "~/components/ui/Layered";
import {
  CheckCircleIcon,
  DotsThreeIcon,
  HardDrivesIcon,
  MapPinIcon,
  QuestionIcon,
  TrashIcon,
  TreeStructureIcon,
  WarningCircleIcon,
} from "@phosphor-icons/react";
import { TagsBeside } from "~/components/ui/TagInput";
import { Switch } from "~/components/ui/Field";
import { RemovalNote } from "~/components/pyo/RemoveServer";
import { createFileRoute, Link } from "@tanstack/react-router";
import { PlaceFlags } from "~/components/ui/Flag";
import { Menu } from "~/components/ui/Menu";
import { CopyButton } from "~/components/ui/Table";
import { useState, type ReactNode } from "react";
import { IconTile } from "~/components/pyo/common";
import {
  RegionName,
  ServerRole,
  serverRole,
  useNodeActions,
} from "~/components/pyo/infra";
import { regionOf } from "~/lib/pyo/location";
import { ActivityRows } from "~/components/pyo/widgets";
import { Dot, type Tone } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import {
  Callout,
  EmptyState,
  KeyValue,
  Page,
  PageHeader,
  Panel,
} from "~/components/ui/Panel";
import { dateTime, gpuReason, gpuText } from "~/lib/pyo/format";
import {
  api,
  activityIn,
  clusterByRef,
  serverByRef,
  serversOf,
  usePyo,
  workloadsOf,
  type PyoState,
  serviceParams,
} from "~/lib/pyo/store";
import type { Application, ClusterServer, Workload } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute(
  "/_app/$team/clusters/$clusterId_/$serverId",
)({
  head: () => ({ meta: seo({ title: "Server" }) }),
  component: ServerPage,
});

function serverState(n: ClusterServer): { tone: Tone; label: string } {
  if (n.removal)
    return n.removal.state === "failed"
      ? { tone: "danger", label: "Removal failed" }
      : {
          tone: "warning",
          label: n.removal.state === "drained" ? "Drained" : "Removing",
        };
  if (n.status !== "ready")
    return {
      tone: "danger",
      label: n.status === "not_ready" ? "Offline" : "Unknown",
    };
  if (n.maintenance) return { tone: "warning", label: "Maintenance" };
  if (!n.schedulable) return { tone: "warning", label: "Scheduling paused" };
  return { tone: "success", label: "Online" };
}

/** Workloads with a pod on this server, from the agent's last report. */
function workloadsOn(s: PyoState, n: ClusterServer, teamId: string): Workload[] {
  return workloadsOf(s, n.clusterId, teamId).filter((w) =>
    w.servers?.includes(n.name),
  );
}

/** Kubernetes node conditions in plain words. */
const CHECKS: Record<string, { label: string; healthy: "True" | "False" }> = {
  Ready: { label: "Reporting to the cluster", healthy: "True" },
  MemoryPressure: { label: "Memory", healthy: "False" },
  DiskPressure: { label: "Disk", healthy: "False" },
  PIDPressure: { label: "Processes", healthy: "False" },
};

function ServerPage() {
  const { clusterId, serverId } = Route.useParams();
  const s = usePyo();
  const { base, remove, editRegion, dialog } = useNodeActions();
  const inCluster = clusterByRef(s, clusterId);
  const n =
    s.servers.find(
      (x) =>
        x.clusterId === inCluster?.id && (x.name === serverId || x.id === serverId),
    ) ?? serverByRef(s, serverId);
  const c = n && s.clusters.find((x) => x.id === n.clusterId);
  if (!n || !c)
    return (
      <Page>
        <EmptyState
          icon={HardDrivesIcon}
          title="Server not found"
          description="It may have been removed from its cluster."
          action={
            <ButtonLink href="/clusters" size="sm" variant="secondary">
              All servers
            </ButtonLink>
          }
        />
      </Page>
    );

  const [schedule, maintenance] = base(n);
  const st = serverState(n);
  const off = n.status !== "ready";
  const running = workloadsOn(s, n, c.teamId);
  const events = activityIn(s, c.teamId).filter(
    (e) =>
      e.target.id === n.id ||
      (e.target.kind === "server" && e.target.label === n.name),
  );

  return (
    <Page>
      <PageHeader
        icon={<IconTile icon={HardDrivesIcon} size="lg" />}
        title={
          <span className="flex items-center gap-2.5">
            <span className="truncate">{n.name}</span>
            <span className="inline-flex shrink-0 items-center gap-1.5 text-[13px] font-normal text-muted-foreground">
              <Dot tone={st.tone} className="h-2 w-2" />
              {st.label}
            </span>
            <span className="flex flex-wrap items-center gap-1 text-[13px] font-normal">
              <TagsBeside
                name={n.name}
                tags={n.tags}
                suggestions={[...new Set(s.servers.flatMap((x) => x.tags ?? []))]}
                hint="Used to place services, e.g. gpu."
                onSave={(tags) => api.setServerTags(n.id, tags)}
              />
            </span>
          </span>
        }
        meta={
          <>
            {serverRole(n) ? (
              <span>
                <ServerRole node={n} />
              </span>
            ) : null}
            <Link
              to="/$team/clusters/$clusterId"
              params={{ team: currentSlug(), clusterId: c.name }}
              className="inline-flex items-center gap-1.5 hover:text-foreground"
            >
              <TreeStructureIcon size={13} />
              {c.name}
            </Link>
            <span className="inline-flex items-center gap-1 font-mono">
              {n.internalIP}
              <CopyButton value={n.internalIP} label="Copy IP address" />
            </span>
            <span>k3s {n.kubeletVersion.replace(/\+k3s\d+$/, "")}</span>
            <PlaceFlags
              places={[n.location ?? c.location]}
              size={22}
              detail={
                n.locationSource === "geoip"
                  ? "detected from its IP address"
                  : n.locationSource === "manual"
                    ? "set by hand"
                    : "detecting…"
              }
            />
          </>
        }
        actions={
          <>
            {!n.maintenance && !n.removal ? (
              <Button
                variant="secondary"
                size="sm"
                disabled={schedule.disabled}
                onClick={schedule.onSelect}
              >
                {schedule.label}
              </Button>
            ) : null}
            {!n.removal ? (
              <Button
                variant={n.maintenance ? "primary" : "secondary"}
                size="sm"
                onClick={maintenance.onSelect}
              >
                {maintenance.label}
              </Button>
            ) : null}
            <Menu
              label={`More for ${n.name}`}
              items={[
                {
                  label: "Change region",
                  icon: <MapPinIcon size={14} />,
                  onSelect: () => editRegion(n),
                },
                { type: "separator" },
                {
                  label: "Remove server",
                  icon: <TrashIcon size={14} />,
                  danger: true,
                  disabled: !!n.removal,
                  onSelect: () => remove(n),
                },
              ]}
              trigger={(t) => (
                <Button
                  {...t}
                  variant="ghost"
                  size="icon"
                  aria-label={`More for ${n.name}`}
                >
                  <DotsThreeIcon size={16} weight="bold" />
                </Button>
              )}
            />
          </>
        }
      />

      {n.removal ? (
        <RemovalNote n={n} />
      ) : off ? (
        <Callout
          tone="danger"
          icon={WarningCircleIcon}
          title={`${n.name} isn't responding`}
        >
          Not reporting. Workloads moved elsewhere.
        </Callout>
      ) : n.maintenance ? (
        <Callout tone="warning" title="In maintenance">
          Nothing new starts here.
        </Callout>
      ) : !n.schedulable ? (
        <Callout tone="warning" title="Scheduling paused">
          Nothing new starts here.
        </Callout>
      ) : null}

      <Layered bare>
        <div className="grid gap-px overflow-hidden rounded-[14px] border border-card-border bg-border sm:grid-cols-2 lg:grid-cols-4">
          <Usage
            label="CPU"
            off={off}
            used={n.usage.cpu}
            total={n.capacity.cpu}
            unit="cores"
          />
          <Usage
            label="Memory"
            off={off}
            used={n.usage.memoryGi}
            total={n.capacity.memoryGi}
            unit="GB"
          />
          {n.usage.rootFs ? (
            <Usage
              label="Disk"
              off={off}
              used={n.usage.rootFs.usedGi}
              total={Math.round(n.usage.rootFs.capacityGi)}
              unit="GB"
              warnAt={0.85}
              badAt={0.95}
            />
          ) : (
            <div className="bg-card px-5 py-4">
              <p className="text-[12.5px] text-muted-foreground">Disk</p>
              <p className="mt-1.5 text-[24px] font-semibold tabular-nums tracking-tight">—</p>
              <p className="mt-2 text-[12px] text-muted-foreground">Not reported yet</p>
            </div>
          )}
          <div className="bg-card px-5 py-4">
            <p className="text-[12.5px] text-muted-foreground">Workloads</p>
            <p className="mt-1.5 text-[24px] font-semibold tabular-nums tracking-tight">
              {off ? "—" : running.length}
            </p>
            <p className="mt-2 text-[12px] text-muted-foreground">
              {off
                ? "None while offline"
                : `${n.podCount} of ${n.capacity.pods} containers`}
            </p>
          </div>
        </div>
      </Layered>

      <NodeHistory team={c.teamId} clusterId={n.clusterId} node={n.name} />

      <div className="grid items-start gap-4 lg:grid-cols-[minmax(0,1.4fr)_minmax(0,1fr)]">
        <Panel title="Running on this server">
          {running.length ? (
            <ul className="list-rows">
              {running.map((w) => {
                const project = s.projects.find((p) => p.id === w.projectId);
                const body = (
                  <>
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-[13px] font-medium">
                        {w.name}
                      </span>
                      <span className="block truncate text-[12px] text-muted-foreground">
                        {w.serviceId ? (project?.name ?? "Service") : "System"} ·{" "}
                        {w.kind}
                      </span>
                    </span>
                    <span className="shrink-0 font-mono text-[11.5px] text-muted-foreground">
                      {w.namespace}
                    </span>
                  </>
                );
                return (
                  <li key={w.id}>
                    {w.serviceId ? (
                      <Link
                        to="/$team/$projectId/$serviceId"
                        params={{
                          team: currentSlug(),
                          ...serviceParams(w.serviceId),
                        }}
                        className="flex items-center gap-3 px-4 py-2.5 transition-colors hover:bg-accent/40"
                      >
                        {body}
                      </Link>
                    ) : (
                      <div className="flex items-center gap-3 px-4 py-2.5">
                        {body}
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          ) : (
            <EmptyState
              compact
              title={off ? "Nothing is running here" : "No workloads yet"}
              description={
                off ? "Its workloads moved to other servers." : undefined
              }
            />
          )}
        </Panel>

        <div className="space-y-4">
          <Panel title="Health checks">
            <ul className="list-rows">
              {n.conditions.map((cd) => {
                const meta = CHECKS[cd.type] ?? {
                  label: cd.type,
                  healthy: "False" as const,
                };
                const ok = cd.status === meta.healthy;
                const unknown = cd.status === "Unknown";
                return (
                  <li
                    key={cd.type}
                    className="flex items-start gap-3 px-4 py-2.5"
                    title={`${cd.type}=${cd.status}`}
                  >
                    {unknown ? (
                      <QuestionIcon
                        size={16}
                        className="mt-0.5 shrink-0 text-warning"
                        aria-label="Unknown"
                      />
                    ) : ok ? (
                      <CheckCircleIcon
                        size={16}
                        weight="fill"
                        className="mt-0.5 shrink-0 text-success"
                        aria-label="OK"
                      />
                    ) : (
                      <WarningCircleIcon
                        size={16}
                        weight="fill"
                        className="mt-0.5 shrink-0 text-destructive"
                        aria-label="Problem"
                      />
                    )}
                    <span className="min-w-0">
                      <span className="block text-[13px]">{meta.label}</span>
                      {!ok || unknown ? (
                        <span className="block text-[12px] text-muted-foreground">
                          {off ? "No data while offline" : cd.message}
                        </span>
                      ) : null}
                    </span>
                  </li>
                );
              })}
            </ul>
          </Panel>
          <Panel title="Details">
            <KeyValue
              items={[
                ...(serverRole(n)
                  ? [["Role", <ServerRole node={n} />] as [ReactNode, ReactNode]]
                  : []),
                [
                  "Region",
                  <span className="inline-flex items-center gap-2">
                    <RegionName region={regionOf(n, c)} />
                    <button
                      type="button"
                      onClick={() => editRegion(n)}
                      className="text-[12px] font-medium text-primary hover:text-primary/80"
                    >
                      Change
                    </button>
                  </span>,
                ],
                ["Internal IP", <Mono>{n.internalIP}</Mono>],
                [
                  "Public IP",
                  n.externalIP ? (
                    <Mono>{n.externalIP}</Mono>
                  ) : (
                    <span className="text-muted-foreground">None</span>
                  ),
                ],
                ["Size", `${n.capacity.cpu} CPU · ${n.capacity.memoryGi} GB`],
                ...(gpuText(n)
                  ? [["GPU", gpuText(n)] as [string, string]]
                  : []),
                ["OS", `${n.osImage} · ${n.arch}`],
                [
                  "Runtime",
                  <Mono>
                    {n.containerRuntime.replace("containerd://", "containerd ")}
                  </Mono>,
                ],
                ["Joined", dateTime(n.createdAt)],
              ]}
            />
          </Panel>
          {n.gpuStatus ? (
            <GpuPanel
              n={n}
              services={running
                .map((w) => s.services.find((a) => a.id === w.serviceId))
                .filter(
                  (a): a is NonNullable<typeof a> =>
                    !!a && (a.resources.gpu ?? 0) > 0,
                )}
            />
          ) : null}
        </div>
      </div>

      {events.length ? (
        <Panel title="Recent events">
          <ActivityRows events={events.slice(0, 6)} />
        </Panel>
      ) : null}
      {dialog}
    </Page>
  );
}

function Usage({
  label,
  used,
  total,
  unit,
  off,
  warnAt = 0.75,
  badAt = 0.9,
}: {
  label: string;
  used: number;
  total: number;
  unit: string;
  off: boolean;
  warnAt?: number;
  badAt?: number;
}) {
  const r = total ? Math.min(1, used / total) : 0;
  const color =
    r > badAt ? "bg-destructive" : r > warnAt ? "bg-warning" : "bg-primary";
  return (
    <div className="bg-card px-5 py-4">
      <p className="text-[12.5px] text-muted-foreground">{label}</p>
      <p className="mt-1.5 text-[24px] font-semibold tabular-nums tracking-tight">
        {off ? "—" : `${Math.round(r * 100)}%`}
      </p>
      <div className="mt-2 flex items-center gap-3">
        <div
          className="h-1.5 flex-1 overflow-hidden rounded-full bg-foreground/10"
          role="meter"
          aria-label={`${label} usage`}
          aria-valuemin={0}
          aria-valuemax={total}
          aria-valuenow={off ? 0 : used}
        >
          {off ? null : (
            <div
              className={`h-full rounded-full ${color}`}
              style={{ width: `${Math.max(r * 100, 2)}%` }}
            />
          )}
        </div>
        <span className="shrink-0 text-[12px] tabular-nums text-muted-foreground">
          {off ? `${total} ${unit}` : `${+used.toFixed(1)} of ${total} ${unit}`}
        </span>
      </div>
    </div>
  );
}

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono text-[11.5px]">{children}</span>;
}

/** A GPU server: its GPUs, who uses them, what's missing, and whether
 * regular services run on it too. */
function GpuPanel({ n, services }: { n: ClusterServer; services: Application[] }) {
  const total = n.gpus ?? 0;
  const used = n.gpusUsed ?? 0;
  const regular = n.regularServices ?? !n.gpuDedicated;
  const reason = gpuReason(n);
  const usable = n.gpuStatus === "ready";
  return (
    <Panel title="GPU">
      <div className="space-y-3 px-4 py-3 text-[13px]">
        <KeyValue
          items={[
            ["Model", n.gpuModel ?? "NVIDIA GPU"],
            [
              "GPUs",
              total
                ? `${total} · ${used} in use · ${Math.max(0, total - used)} free`
                : "Not counted yet",
            ],
            ...(n.gpuMemoryMib
              ? [
                  [
                    "Memory",
                    `${Math.round(n.gpuMemoryMib / 1024)} GB each`,
                  ] as [string, string],
                ]
              : []),
            [
              "Driver and toolkit",
              n.gpuStatus === "no-driver"
                ? "Driver missing"
                : n.gpuStatus === "no-toolkit"
                  ? "Container Toolkit missing"
                  : n.gpuStatus === "no-runtime"
                    ? "Installed, but k3s needs a restart"
                    : "Installed",
            ],
            [
              "Used by",
              services.length
                ? services.map((a) => a.name).join(", ")
                : "No service",
            ],
          ]}
        />
        {reason ? (
          <Callout
            tone={usable || n.gpuStatus === "starting" ? "info" : "warning"}
            title={
              n.gpuStatus === "starting"
                ? "Almost ready"
                : "This GPU can't be used yet"
            }
          >
            {reason}
          </Callout>
        ) : null}
        {n.gpuDetected ? (
        <Switch
          checked={regular}
          onChange={(on) => void api.setServerRegularServices(n.id, on)}
          label="Run regular services on this server"
          description={
            regular
              ? "Any service can run here."
              : "Only GPU services start here."
          }
        />
        ) : null}
      </div>
    </Panel>
  );
}
