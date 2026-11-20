import {
  ArrowCounterClockwiseIcon,
  ArrowSquareOutIcon,
  ClockCounterClockwiseIcon,
  CloudArrowUpIcon,
  DatabaseIcon,
  DotsThreeIcon,
  LinkBreakIcon,
  LinkSimpleIcon,
  ArrowsOutSimpleIcon,
  ProhibitIcon,
  LockSimpleIcon,
  PlusIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState, type ReactNode } from "react";
import {
  ServiceStatusBadge,
  Badge,
  Status,
  type Tone,
} from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, Select } from "~/components/ui/Field";
import { Menu, type MenuItem } from "~/components/ui/Menu";
import { Callout, EmptyState } from "~/components/ui/Panel";
import {
  SearchInput,
  sortBy,
  SortTh,
  Table,
  Td,
  Th,
  Tr,
  type Sort,
} from "~/components/ui/Table";
import { DNS_LABEL, gib, timeAgo } from "~/lib/pyo/format";
import {
  api,
  clustersIn,
  deploymentsWhere,
  usePyo,
  type PyoState,
  serviceParams,
  servicePath,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  Application,
  TeamBackupRun,
  TeamBackups,
  TeamBackupTarget,
  Volume,
} from "~/lib/pyo/types";
import { ServiceIcon, EntityLink, SourceSummary, TimeAgo } from "./common";
import { EXPOSURE_LABEL } from "./editors";
import { currentSlug } from "~/lib/pyo/paths";
import {
  AttachVolumeDialog,
  DeleteVolumeDialog,
  pinnedServer,
  resizeRefusal,
  ResizeVolumeDialog,
  RestoreVolumeDialog,
} from "./VolumeDialogs";

// ── Applications ───────────────────────────────────────────────────────────

type AppSort = "name" | "status" | "deployed";

export function AppTable({
  apps,
  s,
  showProject,
}: {
  apps: Application[];
  s: PyoState;
  showProject?: boolean;
}) {
  const navigate = useNavigate();
  const [sort, setSort] = useState<Sort<AppSort>>({
    key: "deployed",
    dir: "desc",
  });
  const lastDeploy = (a: Application) =>
    deploymentsWhere(s, { serviceId: a.id })[0];
  const rows = sortBy(apps, sort, (a, k) =>
    k === "name"
      ? a.name
      : k === "status"
        ? a.status
        : (lastDeploy(a)?.createdAt ?? 0),
  );
  return (
    <Table>
      <thead>
        <tr>
          <SortTh label="Name" k="name" sort={sort} onSort={setSort} />
          <SortTh label="Status" k="status" sort={sort} onSort={setSort} />
          <Th>Source</Th>
          <Th>Cluster</Th>
          <Th>Replicas</Th>
          <SortTh
            label="Last deploy"
            k="deployed"
            sort={sort}
            onSort={setSort}
          />
        </tr>
      </thead>
      <tbody>
        {rows.map((a) => {
          const d = lastDeploy(a);
          const cluster = s.clusters.find((c) => c.id === a.clusterId);
          const project = s.projects.find((p) => p.id === a.projectId);
          return (
            <Tr
              key={a.id}
              onClick={() =>
                void navigate({
                  to: "/$team/$projectId/$serviceId",
                  params: { team: currentSlug(), ...serviceParams(a.id) },
                })
              }
            >
              <Td>
                <div className="flex items-center gap-2.5">
                  <ServiceIcon app={a} size="sm" />
                  <div className="min-w-0">
                    <EntityLink to={servicePath(a.id)}>{a.name}</EntityLink>
                    <p className="text-[11.5px] text-muted-foreground">
                      {a.kind === "service" ? "Template" : "Custom"} ·{" "}
                      {a.workloadKind}
                      {showProject && project ? ` · ${project.name}` : ""}
                    </p>
                  </div>
                </div>
              </Td>
              <Td>
                <ServiceStatusBadge status={a.status} waiting={a.runtime.waiting} />
              </Td>
              <Td className="max-w-[240px]">
                <SourceSummary source={a.source} />
              </Td>
              <Td>
                {cluster ? (
                  <Link
                    to="/$team/clusters/$clusterId"
                    params={{ team: currentSlug(), clusterId: cluster.name }}
                    className="text-foreground hover:text-primary"
                    onClick={(e) => e.stopPropagation()}
                  >
                    {cluster.name}
                  </Link>
                ) : (
                  "—"
                )}
              </Td>
              <Td className="tabular-nums">
                {a.workloadKind === "CronJob" ? (
                  <span className="font-mono text-[11.5px]">{a.schedule}</span>
                ) : a.runtime.static ? (
                  "Static"
                ) : (
                  `${a.runtime.readyReplicas}/${a.status === "stopped" ? 0 : a.replicas}`
                )}
              </Td>
              <Td className="text-muted-foreground">
                {d ? <TimeAgo at={d.createdAt} /> : "—"}
              </Td>
            </Tr>
          );
        })}
      </tbody>
    </Table>
  );
}

// ── Network endpoints ──────────────────────────────────────────────────────

/** Every port and route of the given services, from the outside in. */
export function EndpointTable({
  apps,
  s,
}: {
  apps: Application[];
  s: PyoState;
}) {
  const rows = apps.flatMap((a) => {
    const ns =
      s.projects.find((p) => p.id === a.projectId)?.namespace ?? "default";
    return a.ports.map((p) => ({
      a,
      p,
      ns,
      domains: a.domains.filter((d) => d.portName === p.name),
    }));
  });
  if (!rows.length)
    return (
      <EmptyState
        compact
        title="No ports exposed"
        description="Services without ports get no hostname."
      />
    );
  return (
    <Table>
      <thead>
        <tr>
          <Th>Service</Th>
          <Th>Internal address</Th>
          <Th>Protocol</Th>
          <Th>Exposure</Th>
          <Th>Public endpoint</Th>
        </tr>
      </thead>
      <tbody>
        {rows.map(({ a, p, ns, domains }) => {
          const cluster = s.clusters.find((c) => c.id === a.clusterId);
          const ip = cluster?.publicIp ?? undefined;
          return (
            <Tr key={p.id}>
              <Td>
                <EntityLink to={servicePath(a.id, "networking")}>
                  {a.name}
                </EntityLink>
              </Td>
              <Td>
                <span className="font-mono text-[11.5px]">
                  {a.name}.{ns}.svc:{p.containerPort}
                </span>
              </Td>
              <Td>
                <Badge mono>{p.protocol}</Badge>
              </Td>
              <Td>
                <span className="inline-flex items-center gap-1.5">
                  {p.exposure === "internal" ? (
                    <LockSimpleIcon
                      size={12}
                      className="text-muted-foreground"
                    />
                  ) : (
                    <LinkSimpleIcon size={12} className="text-warning" />
                  )}
                  {EXPOSURE_LABEL[p.exposure]}
                </span>
              </Td>
              <Td className="font-mono text-[11.5px]">
                {p.exposure !== "internal" && cluster?.publicTraffic === false ? (
                  <span className="font-sans text-muted-foreground">
                    Public traffic is off
                  </span>
                ) : p.exposure === "http" ? (
                  domains.map((d) => (
                    <div
                      key={d.id}
                    >{`${d.tls === "issued" ? "https" : "http"}://${d.hostname}`}</div>
                  ))
                ) : p.exposure === "nodeport" ? (
                  `<any server IP>:${p.publicPort ?? "auto"}`
                ) : p.exposure === "loadbalancer" ? (
                  ip ? (
                    `${ip}:${p.publicPort ?? p.containerPort}/${p.protocol === "UDP" ? "udp" : "tcp"}`
                  ) : (
                    "Waiting for an IP"
                  )
                ) : (
                  <span className="font-sans text-muted-foreground">
                    Cluster only
                  </span>
                )}
              </Td>
            </Tr>
          );
        })}
      </tbody>
    </Table>
  );
}

// ── Volumes ────────────────────────────────────────────────────────────────

// One row per volume: what it is, where it's mounted, how full it is and how
// it stands. The rest (class, server, backups) sits in a muted line, the
// actions in one menu. Columns fold into stacked rows in narrow panels.

const VOLUME_COLS =
  "@xl:grid-cols-[minmax(0,1.5fr)_minmax(0,1.1fr)_8.5rem_8rem_1.75rem]";

export function VolumeTable({
  volumes,
  s,
  showCluster = true,
  showService = true,
  showProject = false,
  onDetach,
  empty = "Created on deploy.",
}: {
  volumes: Volume[];
  s: PyoState;
  showCluster?: boolean;
  /** Its service, and "Open service" in its menu (not on the service's own page). */
  showService?: boolean;
  /** Each volume's project (outside a project's pages). */
  showProject?: boolean;
  /** "Detach" in its menu (on the service's own page). */
  onDetach?: (v: Volume) => void;
  /** Under "No volumes". */
  empty?: string;
}) {
  const [attaching, setAttaching] = useState<Volume | null>(null);
  const [removing, setRemoving] = useState<Volume | null>(null);
  const [restoring, setRestoring] = useState<Volume | null>(null);
  const [resizing, setResizing] = useState<Volume | null>(null);
  const role = s.members.find(
    (m) => m.teamId === s.currentTeamId && m.email === s.user.email,
  )?.role;
  const admin = role === "owner" || role === "admin";
  const canRun = admin || role === "developer";
  if (!volumes.length)
    return (
      <EmptyState
        compact
        icon={DatabaseIcon}
        title="No volumes"
        description={empty}
      />
    );
  // The cluster only tells volumes apart when there's more than one.
  const manyClusters =
    showCluster && new Set(volumes.map((v) => v.clusterId)).size > 1;
  return (
    <div className="@container">
      <div
        aria-hidden
        className={`hidden h-10 items-center gap-x-4 border-b border-card-border bg-muted/60 px-4 text-[11.5px] font-medium text-muted-foreground @xl:grid ${VOLUME_COLS}`}
      >
        <span>Volume</span>
        <span>{showService ? "Service" : "Folder"}</span>
        <span className="text-right">Usage</span>
        <span>Status</span>
        <span />
      </div>
      <ul className="list-rows">
        {volumes.map((v) => {
          const app = s.services.find((a) => a.id === v.serviceId);
          const cluster = s.clusters.find((c) => c.id === v.clusterId);
          const sc = cluster?.capabilities.storageClasses.find(
            (c) => c.name === v.storageClass,
          );
          const server = pinnedServer(s, v);
          const project = s.projects.find((p) => p.id === v.projectId);
          const backup = volumeBackup(s, v);
          const busy = !!(v.restore || v.moving);
          const items =
            v.deleting || busy
              ? []
              : volumeMenu({
                  v,
                  app: showService ? app : undefined,
                  backup,
                  restorable: !!s.teamBackups.find((t) => t.teamId === v.teamId)
                    ?.settings,
                  growable: !resizeRefusal(s, v),
                  admin,
                  canRun,
                  onAttach: () => setAttaching(v),
                  onDetach: onDetach ? () => onDetach(v) : undefined,
                  onRestore: () => setRestoring(v),
                  onResize: () => setResizing(v),
                  onDelete: () => setRemoving(v),
                });
          const details: ReactNode[] = [];
          if (showProject)
            details.push(project ? project.name : "Deleted project");
          if (manyClusters && cluster) details.push(cluster.name);
          details.push(
            <span
              key="class"
              title={
                sc && !sc.replicated
                  ? "Its data stays on one server"
                  : undefined
              }
            >
              <span className="font-mono text-[11.5px]">{v.storageClass}</span>
              {server
                ? ` on ${server}`
                : sc && !sc.replicated
                  ? " on one server"
                  : ""}
            </span>,
          );
          const bk = backupText(v.backup);
          if (bk) details.push(bk);
          return (
            <li
              key={v.id}
              className={`grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-2.5 px-4 py-3 ${VOLUME_COLS}`}
            >
              <div
                className="min-w-0"
                title={`${v.accessMode} · created ${timeAgo(v.createdAt)}`}
              >
                <p className="truncate font-mono text-[12.5px] text-foreground">
                  {v.name}
                </p>
                <p className="mt-0.5 text-[12px] leading-[18px] text-muted-foreground">
                  {details.map((d, i) => (
                    <span key={i}>
                      {i ? " · " : ""}
                      {d}
                    </span>
                  ))}
                </p>
              </div>
              <div className="col-start-2 row-start-1 justify-self-end @xl:col-start-5">
                {items.length ? (
                  <Menu
                    label={`Actions for ${v.name}`}
                    items={items}
                    trigger={(t) => (
                      <Button
                        {...t}
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7 text-muted-foreground"
                      >
                        <DotsThreeIcon size={16} weight="bold" />
                      </Button>
                    )}
                  />
                ) : null}
              </div>
              <div className="col-start-1 row-start-2 min-w-0 @xl:col-start-2 @xl:row-start-1">
                {!showService ? (
                  <p className="truncate font-mono text-[12px] text-foreground">
                    {v.mountPath ?? "—"}
                  </p>
                ) : app ? (
                  <>
                    <EntityLink
                      to={servicePath(app.id, "resources")}
                      className="block text-[12.5px]"
                    >
                      {app.name}
                    </EntityLink>
                    <p className="truncate font-mono text-[11.5px] text-muted-foreground">
                      {v.mountPath}
                    </p>
                  </>
                ) : (
                  <span
                    className="hidden text-muted-foreground @xl:inline"
                    title={project ? undefined : "Its project was deleted"}
                  >
                    —
                  </span>
                )}
              </div>
              <div className="col-span-2 row-start-3 @xl:col-span-1 @xl:col-start-3 @xl:row-start-1">
                <VolumeUsage v={v} />
              </div>
              <div className="col-start-2 row-start-2 justify-self-end @xl:col-start-4 @xl:row-start-1 @xl:justify-self-stretch">
                <VolumeState v={v} s={s} app={app} />
              </div>
            </li>
          );
        })}
      </ul>
      <AttachVolumeDialog
        volume={attaching}
        onClose={() => setAttaching(null)}
      />
      <DeleteVolumeDialog
        volume={removing}
        onClose={() => setRemoving(null)}
      />
      <RestoreVolumeDialog
        volume={restoring}
        onClose={() => setRestoring(null)}
      />
      <ResizeVolumeDialog
        volume={resizing}
        onClose={() => setResizing(null)}
      />
    </div>
  );
}

/** "1.2 of 10 GiB" over a slim bar, which turns only when it's nearly full. */
function VolumeUsage({ v }: { v: Volume }) {
  if (v.usedGi === undefined)
    return (
      <p className="text-[12.5px] tabular-nums text-foreground @xl:text-right">
        {gib(v.sizeGi)}
      </p>
    );
  const ratio = v.sizeGi > 0 ? Math.min(1, v.usedGi / v.sizeGi) : 0;
  const bar =
    ratio > 0.95 ? "bg-destructive" : ratio > 0.85 ? "bg-warning" : "bg-primary";
  return (
    <div className="flex items-center gap-3 @xl:block">
      <p className="order-2 shrink-0 text-[12.5px] tabular-nums text-foreground @xl:text-right">
        {+v.usedGi.toFixed(1)}
        <span className="text-muted-foreground"> of {gib(v.sizeGi)}</span>
      </p>
      <div
        className="h-1 flex-1 overflow-hidden rounded-full bg-foreground/10 @xl:mt-1.5"
        role="meter"
        aria-label={`${v.name} usage`}
        aria-valuemin={0}
        aria-valuemax={v.sizeGi}
        aria-valuenow={v.usedGi}
      >
        <div
          className={`h-full rounded-full ${bar}`}
          style={{ width: `${Math.max(ratio * 100, ratio > 0 ? 2 : 0)}%` }}
        />
      </div>
    </div>
  );
}

const VOLUME_STATUS: Record<Volume["status"], [Tone, string]> = {
  bound: ["success", "In use"],
  pending: ["progress", "Pending"],
  degraded: ["warning", "Degraded"],
  error: ["danger", "Error"],
};

/** How a volume stands, and one short line on what's under way or wrong. */
function VolumeState({
  v,
  s,
  app,
}: {
  v: Volume;
  s: PyoState;
  app?: Application;
}) {
  const failed = v.restoreError
    ? `Restore failed: ${v.restoreError}`
    : v.moveError
      ? `Move failed: ${v.moveError}`
      : v.resizeError
        ? `Resize failed: ${v.resizeError}`
        : "";
  const [tone, label]: [Tone, string] = v.deleting
    ? ["progress", "Deleting"]
    : v.restore
      ? ["progress", "Restoring"]
      : v.moving
        ? ["progress", "Moving"]
        : v.resizeTo
          ? ["progress", "Resizing"]
          : !v.serviceId && v.status !== "error" && v.status !== "degraded"
            ? ["neutral", "Not attached"]
            : VOLUME_STATUS[v.status];
  const note = v.deleting
    ? v.deleteError
    : v.restore
      ? app
        ? `${app.name} starts again once it's done.`
        : undefined
      : v.moving
        ? `To ${s.projects.find((p) => p.id === v.moving?.projectId)?.name ?? "another project"}`
        : v.resizeTo
          ? `To ${gib(v.resizeTo)}`
          : failed ||
            v.statusMessage ||
            (v.status === "pending" && v.serviceId
              ? "Created on next deploy"
              : undefined);
  return (
    <div className="text-right @xl:text-left">
      <Status tone={tone} label={label} className="text-[12.5px]" />
      {note ? (
        <p
          className={`mt-0.5 max-w-[220px] text-[11.5px] leading-4 ${failed ? "text-destructive" : "text-muted-foreground"}`}
        >
          {note}
        </p>
      ) : null}
    </div>
  );
}

/** A volume's backup target, for its menu: null when it can't be backed up. */
type VolumeBackup = {
  tb: TeamBackups;
  target: TeamBackupTarget;
  last?: TeamBackupRun;
} | null;

function volumeBackup(s: PyoState, v: Volume): VolumeBackup {
  const tb = s.teamBackups.find((t) => t.teamId === v.teamId);
  const target = tb?.settings
    ? tb.targets.find((t) => t.volumeId === v.id)
    : undefined;
  if (!tb || !target) return null;
  const runs = tb.history.filter((h) => h.volumeId === v.id);
  return { tb, target, last: runs[0] };
}

function volumeMenu({
  v,
  app,
  backup,
  restorable,
  growable,
  admin,
  canRun,
  onAttach,
  onDetach,
  onRestore,
  onResize,
  onDelete,
}: {
  v: Volume;
  app?: Application;
  backup: VolumeBackup;
  /** The team backs up somewhere to restore from. */
  restorable: boolean;
  /** Its storage class can grow it. */
  growable: boolean;
  admin: boolean;
  canRun: boolean;
  onAttach: () => void;
  onDetach?: () => void;
  onRestore: () => void;
  onResize: () => void;
  onDelete: () => void;
}): MenuItem[] {
  const groups: MenuItem[][] = [];
  if (app)
    groups.push([
      {
        label: "Open service",
        icon: <ArrowSquareOutIcon size={14} />,
        href: servicePath(app.id),
      },
    ]);
  const g: MenuItem[] = [];
  if (backup) {
    const { tb, target, last } = backup;
    const cfg = tb.settings!;
    const busy = last?.status === "running" || last?.status === "queued";
    const on = target.included;
    if (admin)
      g.push({
        label: on ? "Turn off backups" : "Turn on backups",
        icon: on ? (
          <ProhibitIcon size={14} />
        ) : (
          <ClockCounterClockwiseIcon size={14} />
        ),
        onSelect: () => {
          // In "all" mode the exceptions are left out; in "selected", put in.
          const listed = cfg.mode === "all" ? on : !on;
          void api.setTeamBackupSelection(
            tb.teamId,
            cfg.mode,
            listed
              ? [...cfg.exceptions, target.id]
              : cfg.exceptions.filter((e) => e !== target.id),
          );
        },
      });
    if (canRun)
      g.push({
        label: busy ? "Backing up…" : "Back up now",
        icon: <CloudArrowUpIcon size={14} />,
        disabled: busy,
        onSelect: () => void api.runTeamBackup(tb.teamId, target.id),
      });
  }
  if (admin && restorable)
    g.push({
      label: "Restore",
      icon: <ArrowCounterClockwiseIcon size={14} />,
      onSelect: onRestore,
    });
  if (admin && growable)
    g.push({
      label: "Resize",
      icon: <ArrowsOutSimpleIcon size={14} />,
      onSelect: onResize,
    });
  if (g.length) groups.push(g);
  if (v.serviceId && onDetach && canRun)
    groups.push([
      {
        label: "Detach",
        icon: <LinkBreakIcon size={14} />,
        onSelect: onDetach,
      },
    ]);
  if (!v.serviceId) {
    const d: MenuItem[] = [];
    if (canRun)
      d.push({
        label: "Attach",
        icon: <LinkSimpleIcon size={14} />,
        onSelect: onAttach,
      });
    if (admin)
      d.push({
        label: "Delete",
        icon: <TrashIcon size={14} />,
        danger: true,
        onSelect: onDelete,
      });
    if (d.length) groups.push(d);
  }
  return groups.flatMap((g, i) =>
    i ? [{ type: "separator" } as const, ...g] : g,
  );
}

/** A volume's backups in a few words, as the API works them out. */
function backupText(b: Volume["backup"]): ReactNode {
  if (b.status === "unsupported") return null;
  if (b.status === "disabled") return "no backups";
  if (b.status === "failed")
    return (
      <span key="backup" className="text-destructive" title={b.error ?? undefined}>
        backup failed{b.failedAt ? ` ${timeAgo(b.failedAt)}` : ""}
      </span>
    );
  if (b.running) return "backing up";
  if (b.lastAt) return `backed up ${timeAgo(b.lastAt)}`;
  return b.schedule === "manual"
    ? "backups by hand"
    : b.schedule === "weekly"
      ? "backs up weekly"
      : "backs up daily";
}

export function SearchableServiceTable({
  apps,
  s,
  showProject,
}: {
  apps: Application[];
  s: PyoState;
  showProject?: boolean;
}) {
  const [q, setQ] = useState("");
  const filtered = apps.filter((a) => !q || a.name.includes(q.toLowerCase()));
  return (
    <>
      <div className="border-b border-border px-4 py-2.5">
        <SearchInput
          value={q}
          onChange={setQ}
          placeholder="Filter by name"
          className="w-full sm:w-64"
        />
      </div>
      {filtered.length ? (
        <AppTable apps={filtered} s={s} showProject={showProject} />
      ) : (
        <EmptyState compact title="No services match" />
      )}
    </>
  );
}
