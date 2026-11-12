import {
  ArrowCounterClockwiseIcon,
  ArrowSquareOutIcon,
  ClockCounterClockwiseIcon,
  CloudArrowUpIcon,
  DotsThreeIcon,
  LinkSimpleIcon,
  ArrowsOutSimpleIcon,
  ProhibitIcon,
  LockSimpleIcon,
  PlusIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import {
  ServiceStatusBadge,
  Badge,
  Status,
  VolumeStatusBadge,
} from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, Select } from "~/components/ui/Field";
import { Menu, type MenuItem } from "~/components/ui/Menu";
import { Callout, EmptyState, Meter } from "~/components/ui/Panel";
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
                <ServiceStatusBadge status={a.status} />
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
          const ip = cluster?.capabilities.loadBalancerIPs[0];
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

export function VolumeTable({
  volumes,
  s,
  showCluster = true,
  showService = true,
  showProject = false,
}: {
  volumes: Volume[];
  s: PyoState;
  showCluster?: boolean;
  /** "Open service" in its menu (not on the service's own page). */
  showService?: boolean;
  /** Each volume's project (outside a project's pages). */
  showProject?: boolean;
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
        title="No volumes"
        description="Created on deploy."
      />
    );
  return (
    <>
      <Table>
        <thead>
          <tr>
            <Th>Volume</Th>
            <Th>Status</Th>
            <Th>Attached to</Th>
            {showCluster ? <Th>Cluster</Th> : null}
            <Th>Class</Th>
            <Th className="w-40">Usage</Th>
            <Th>Backup</Th>
            <Th className="w-10">
              <span className="sr-only">Actions</span>
            </Th>
          </tr>
        </thead>
        <tbody>
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
                    onRestore: () => setRestoring(v),
                    onResize: () => setResizing(v),
                    onDelete: () => setRemoving(v),
                  });
            const note = v.deleting
              ? v.deleteError
              : v.restoreError
                ? `Restore failed: ${v.restoreError}`
                : v.moveError
                  ? `Move failed: ${v.moveError}`
                  : v.resizeError
                    ? `Resize failed: ${v.resizeError}`
                    : v.statusMessage;
            return (
              <Tr key={v.id}>
                <Td>
                  <p className="font-mono text-[12px] text-foreground">
                    {v.name}
                  </p>
                  <p className="text-[11.5px] text-muted-foreground">
                    {v.accessMode} · {gib(v.sizeGi)}
                    {server ? ` · on ${server}` : ""}
                  </p>
                  {showProject ? (
                    <p className="text-[11.5px] text-muted-foreground">
                      {project ? project.name : "Deleted project"}
                    </p>
                  ) : null}
                </Td>
                <Td>
                  {v.deleting ? (
                    <Badge tone="warning">Deleting</Badge>
                  ) : v.restore ? (
                    <Badge tone="warning">Restoring…</Badge>
                  ) : v.moving ? (
                    <Badge tone="warning">Moving…</Badge>
                  ) : v.resizeTo ? (
                    <Badge tone="info">Resizing…</Badge>
                  ) : !v.serviceId &&
                    v.status !== "error" &&
                    v.status !== "degraded" ? (
                    <Status tone="neutral" label="Not attached" />
                  ) : (
                    <VolumeStatusBadge status={v.status} />
                  )}
                  {v.restore && app ? (
                    <p className="mt-1 max-w-[220px] text-[11px] leading-4 text-muted-foreground">
                      {app.name} starts again once it's done.
                    </p>
                  ) : v.moving ? (
                    <p className="mt-1 max-w-[220px] text-[11px] leading-4 text-muted-foreground">
                      To{" "}
                      {s.projects.find((p) => p.id === v.moving?.projectId)
                        ?.name ?? "another project"}
                    </p>
                  ) : v.resizeTo ? (
                    <p className="mt-1 max-w-[220px] text-[11px] leading-4 text-muted-foreground">
                      To {gib(v.resizeTo)}
                    </p>
                  ) : note ? (
                    <p
                      className={`mt-1 max-w-[220px] text-[11px] leading-4 ${v.restoreError || v.moveError || v.resizeError ? "text-destructive" : "text-muted-foreground"}`}
                    >
                      {note}
                    </p>
                  ) : null}
                </Td>
                <Td>
                  {app ? (
                    <>
                      <EntityLink to={servicePath(app.id, "resources")}>
                        {app.name}
                      </EntityLink>
                      <p className="font-mono text-[11px] text-muted-foreground">
                        {v.mountPath}
                      </p>
                    </>
                  ) : (
                    <span
                      className="text-muted-foreground"
                      title={project ? undefined : "Its project was deleted"}
                    >
                      —
                    </span>
                  )}
                </Td>
                {showCluster ? <Td>{cluster?.name ?? "—"}</Td> : null}
                <Td>
                  <span className="font-mono text-[11.5px]">
                    {v.storageClass}
                  </span>
                  {sc && !sc.replicated ? (
                    <p className="text-[11px] text-warning">One server only</p>
                  ) : null}
                </Td>
                <Td>
                  {v.usedGi !== undefined ? (
                    <>
                      <Meter
                        value={v.usedGi}
                        max={v.sizeGi}
                        label={`${v.name} usage`}
                      />
                      <p className="text-[11px] text-muted-foreground">
                        {gib(v.usedGi)} of {gib(v.sizeGi)}
                      </p>
                    </>
                  ) : (
                    <span className="text-[11.5px] text-muted-foreground">
                      Not reported
                    </span>
                  )}
                </Td>
                <Td>
                  <BackupCell b={v.backup} />
                </Td>
                <Td>
                  {items.length ? (
                    <Menu
                      label={`Actions for ${v.name}`}
                      items={items}
                      trigger={(t) => (
                        <Button
                          {...t}
                          variant="ghost"
                          size="icon"
                          className="h-7 w-7"
                        >
                          <DotsThreeIcon size={16} weight="bold" />
                        </Button>
                      )}
                    />
                  ) : null}
                </Td>
              </Tr>
            );
          })}
        </tbody>
      </Table>
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
    </>
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

/** A volume's backups, as the API works them out. */
function BackupCell({ b }: { b: Volume["backup"] }) {
  if (b.status === "unsupported")
    return <span className="text-[11.5px] text-muted-foreground">—</span>;
  if (b.status === "disabled")
    return <span className="text-[11.5px] text-muted-foreground">Off</span>;
  if (b.status === "failed")
    return (
      <span className="text-[12px]" title={b.error ?? undefined}>
        <Badge tone="danger">Failed</Badge>{" "}
        <span className="text-muted-foreground">
          {b.failedAt ? timeAgo(b.failedAt) : ""}
        </span>
      </span>
    );
  return (
    <span className="text-[12px]">
      <Badge tone="success">On</Badge>{" "}
      <span className="text-muted-foreground">
        {b.running
          ? "backing up"
          : b.lastAt
            ? `last ${timeAgo(b.lastAt)}`
            : b.schedule === "manual"
              ? "by hand"
              : b.schedule === "weekly"
                ? "weekly"
                : "daily"}
      </span>
    </span>
  );
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
