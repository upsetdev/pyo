import { ListSection } from "~/components/ui/Layered";
import {
  CheckCircleIcon,
  DotsThreeIcon,
  HardDrivesIcon,
  PauseCircleIcon,
  PlayCircleIcon,
  MapPinIcon,
  TrashIcon,
  WrenchIcon,
} from "@phosphor-icons/react";
import { Dialog } from "~/components/ui/Dialog";
import { Flag } from "~/components/ui/Flag";
import {
  locationLabel,
  parseLocation,
  regionCountry,
  regionOf,
} from "~/lib/pyo/location";
import { RemoveServerDialog } from "./RemoveServer";
import { Link, useNavigate } from "@tanstack/react-router";
import { Fragment, useState, type ReactNode } from "react";
import { Dot, type Tone, WorkloadStatusBadge } from "~/components/ui/Badge";
import { Tags } from "~/components/ui/TagInput";
import { Tooltip } from "~/components/ui/Tooltip";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Select } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Meter, PageHeader, Panel } from "~/components/ui/Panel";
import {
  RouteTabs,
  SearchInput,
  Table,
  Td,
  Th,
  Tr,
} from "~/components/ui/Table";
import { cpu, gpuProblem, gpuReason, gpuText, mem } from "~/lib/pyo/format";
import {
  api,
  type PyoState,
  clusterRef,
  getState,
  serverHref,
  serviceParams,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { ClusterServer, Workload } from "~/lib/pyo/types";
import { EntityLink } from "./common";
import { currentSlug } from "~/lib/pyo/paths";

export function InfraHeader({
  actions,
  description,
}: {
  actions?: ReactNode;
  description?: ReactNode;
}) {
  return (
    <div className="space-y-4">
      <PageHeader
        title="Clusters"
        description={description}
        actions={actions}
      />
      <RouteTabs
        label="Infrastructure sections"
        tabs={[
          { label: "Clusters", to: "/clusters", exact: true },
          { label: "Workloads", to: "/clusters/workloads" },
          { label: "Networking", to: "/clusters/networking" },
          { label: "Volumes", to: "/clusters/storage" },
        ]}
      />
    </div>
  );
}

/** "Main" (runs the cluster itself), "Builder" (build-only), or nothing. */
export const serverRole = (n: ClusterServer) =>
  n.roles.includes("control-plane") ? "Main" : n.builder ? "Builder" : null;

/** A server's role label, with what "Main" means on hover. */
export function ServerRole({
  node,
  focusable,
}: {
  node: ClusterServer;
  /** False inside a link. */
  focusable?: boolean;
}) {
  const role = serverRole(node);
  return role === "Main" ? (
    <Tooltip content="Runs the cluster itself" focusable={focusable}>
      Main
    </Tooltip>
  ) : (
    role
  );
}

/** Scheduling (cordon) and maintenance (drain) actions with confirmation; shared by lists and the server page. */
export function useNodeActions() {
  const [pending, setPending] = useState<{
    node: ClusterServer;
    action: "cordon" | "uncordon" | "maintenance" | "resume";
  } | null>(null);
  const [reason, setReason] = useState("");
  const [removing, setRemoving] = useState<ClusterServer | null>(null);
  const [regionOf, setRegionOf] = useState<ClusterServer | null>(null);
  const items = (n: ClusterServer) => [
    ...(n.removal ? [] : base(n)),
    {
      label: "Change region",
      icon: <MapPinIcon size={14} />,
      onSelect: () => setRegionOf(n),
    },
    { type: "separator" as const },
    {
      label: "Remove server",
      icon: <TrashIcon size={14} />,
      danger: true,
      disabled: !!n.removal,
      onSelect: () => setRemoving(n),
    },
  ];
  const base = (n: ClusterServer) => [
    n.schedulable
      ? {
          label: "Pause scheduling",
          icon: <PauseCircleIcon size={14} />,
          onSelect: () => setPending({ node: n, action: "cordon" as const }),
          disabled: n.status !== "ready",
        }
      : {
          label: "Resume scheduling",
          icon: <PlayCircleIcon size={14} />,
          onSelect: () => setPending({ node: n, action: "uncordon" as const }),
          disabled: n.maintenance,
        },
    n.maintenance
      ? {
          label: "End maintenance",
          icon: <CheckCircleIcon size={14} />,
          onSelect: () => setPending({ node: n, action: "resume" as const }),
        }
      : {
          label: "Enter maintenance",
          icon: <WrenchIcon size={14} />,
          onSelect: () =>
            setPending({ node: n, action: "maintenance" as const }),
        },
  ];
  const p = pending;
  const copy = p
    ? {
        cordon: {
          title: `Pause scheduling on ${p.node.name}?`,
          body: "No new workloads start here. Running ones keep running.",
          label: "Pause scheduling",
        },
        uncordon: {
          title: `Resume scheduling on ${p.node.name}?`,
          body: "New workloads can start on this server again.",
          label: "Resume scheduling",
        },
        maintenance: {
          title: `Put ${p.node.name} in maintenance?`,
          body: "No new workloads start here. Running ones aren't moved.",
          label: "Enter maintenance",
        },
        resume: {
          title: `End maintenance on ${p.node.name}?`,
          body: "The server accepts workloads again.",
          label: "End maintenance",
        },
      }[p.action]
    : null;
  const dialog = (
    <ConfirmDialog
      open={!!p}
      onClose={() => {
        setPending(null);
        setReason("");
      }}
      tone={p?.action === "maintenance" ? "danger" : "primary"}
      title={copy?.title ?? ""}
      description={copy?.body}
      confirmLabel={copy?.label ?? ""}
      onConfirm={() => {
        if (!p) return;
        if (p.action === "cordon" || p.action === "uncordon")
          api.setServerSchedulable(p.node.id, p.action === "uncordon");
        else
          api.setServerMaintenance(p.node.id, p.action === "maintenance", reason);
        toast({ title: `${copy?.label}: ${p.node.name}` });
        setReason("");
      }}
    >
      {p?.action === "maintenance" ? (
        <Field label="Reason" optional>
          {(fp) => (
            <Input
              {...fp}
              value={reason}
              onChange={(e) => setReason(e.target.value)}
              placeholder="Kernel upgrade"
            />
          )}
        </Field>
      ) : null}
    </ConfirmDialog>
  );
  const dialogs = (
    <>
      {dialog}
      <RemoveServerDialog node={removing} onClose={() => setRemoving(null)} />
      {regionOf ? (
        <RegionDialog node={regionOf} onClose={() => setRegionOf(null)} />
      ) : null}
    </>
  );
  return { items, base, remove: setRemoving, editRegion: setRegionOf, dialog: dialogs };
}

/**
 * A server's region: detected from its IP ("Singapore, SG"), or a name of
 * its own that groups servers ("US East"). Other regions are offered.
 */
export function RegionDialog({
  node,
  onClose,
}: {
  node: ClusterServer;
  onClose: () => void;
}) {
  const s = usePyo();
  const cluster = s.clusters.find((c) => c.id === node.clusterId);
  const [value, setValue] = useState(regionOf(node, cluster));
  const known = [
    ...new Set(
      s.servers
        .map((n) => regionOf(n, s.clusters.find((c) => c.id === n.clusterId)))
        .concat(s.clusters.map((c) => c.location))
        .filter(Boolean),
    ),
  ].sort();
  const save = async (region: string) => {
    const res = await api.setServerLocation(node.id, region);
    if (res) {
      toast({
        title: region.trim()
          ? `${node.name} is in ${locationLabel(region.trim())}`
          : `Detecting ${node.name}'s region`,
        description: "Replicas move at their next deploy.",
      });
      onClose();
    }
  };
  return (
    <Dialog
      open
      onClose={onClose}
      title={`Region of ${node.name}`}
      description={
        node.locationSource === "manual"
          ? "Set by hand."
          : "Detected from the server's IP."
      }
      onSubmit={() => void save(value)}
      footer={
        <>
          {node.locationSource === "manual" ? (
            <Button
              variant="ghost"
              size="sm"
              className="mr-auto"
              onClick={() => void save("")}
            >
              Detect automatically
            </Button>
          ) : null}
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!value.trim()}>
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-2 px-4 py-3">
        <Field
          label="Region"
          hint='A name like "US East", or a place like "Singapore, SG".'
        >
          {(p) => (
            <Input
              {...p}
              maxLength={80}
              value={value}
              onChange={(e) => setValue(e.target.value)}
              placeholder="US East"
            />
          )}
        </Field>
        {known.length ? (
          <div className="flex flex-wrap gap-1.5">
            {known.map((r) => {
              const cc = regionCountry(r, s.servers, s.clusters);
              return (
                <button
                  key={r}
                  type="button"
                  onClick={() => setValue(r)}
                  className={`inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-[12px] hover:text-foreground ${r === value ? "border-foreground/30 text-foreground" : "border-border text-muted-foreground"}`}
                >
                  {cc ? <Flag country={cc} size={12} /> : null}
                  {locationLabel(r)}
                </button>
              );
            })}
          </div>
        ) : null}
      </div>
    </Dialog>
  );
}

/** A region with its flag, for tables and lists. */
export function RegionName({ region }: { region: string }) {
  const s = usePyo();
  const cc = regionCountry(region, s.servers, s.clusters);
  return (
    <span className="inline-flex items-center gap-1.5">
      {cc ? <Flag country={cc} size={13} /> : null}
      {locationLabel(region)}
    </span>
  );
}

/** Online servers just get a green dot; anything else is spelled out. */
export function serverState(n: ClusterServer): { tone: Tone; label?: string } {
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
  // A GPU is there but can't be used yet (no driver or toolkit).
  const gpu = gpuProblem(n);
  if (gpu) return { tone: "warning", label: gpu };
  return { tone: "success" };
}

/**
 * A cluster's servers, one line each: state, name, region, CPU and memory.
 * The rest (addresses, versions, disk, network, pods) is on the server's page.
 */
export function ServerTable({
  nodes,
  s,
}: {
  nodes: ClusterServer[];
  s: PyoState;
}) {
  const navigate = useNavigate();
  const { items, dialog } = useNodeActions();
  if (!nodes.length)
    return (
      <EmptyState
        icon={HardDrivesIcon}
        title="No servers yet"
        description="Add one to start deploying."
      />
    );
  // A GPU column when any of these servers has an NVIDIA GPU.
  const gpus = nodes.some((n) => n.gpuStatus);
  const builds = (n: ClusterServer) =>
    s.builders.some(
      (b) =>
        b.kind === "cluster" &&
        b.clusterId === n.clusterId &&
        b.server === n.name,
    );

  return (
    <>
      <Table fluid>
        <thead>
          <tr>
            <Th>Server</Th>
            <Th className="hidden sm:table-cell">Region</Th>
            <Th className="hidden w-36 sm:table-cell">CPU</Th>
            <Th className="hidden w-36 sm:table-cell">Memory</Th>
            {gpus ? <Th className="hidden w-36 lg:table-cell">GPU</Th> : null}
            <Th className="w-10">
              <span className="sr-only">Actions</span>
            </Th>
          </tr>
        </thead>
        <tbody>
          {nodes.map((n) => {
            const st = serverState(n);
            const off = n.status !== "ready";
            const region = regionOf(
              n,
              s.clusters.find((c) => c.id === n.clusterId),
            );
            const reason = st.label?.startsWith("GPU") ? gpuReason(n) : null;
            const notes: ReactNode[] = [
              serverRole(n) ? <ServerRole node={n} /> : null,
              builds(n) && !n.builder ? "Builder" : null,
              `${n.capacity.cpu} CPU · ${n.capacity.memoryGi} GB`,
              n.gpuDedicated ? "GPU only" : null,
            ].filter(Boolean);
            return (
              <Tr
                key={n.id}
                onClick={() =>
                  void navigate({
                    to: "/$team/clusters/$clusterId/$serverId",
                    params: {
                      team: currentSlug(),
                      clusterId: clusterRef(getState(), n.clusterId),
                      serverId: n.name,
                    },
                  })
                }
              >
                <Td className="w-full max-w-0">
                  <div className="flex min-w-0 items-center gap-2.5">
                    <Dot tone={st.tone} className="h-2 w-2" />
                    <EntityLink to={serverHref(getState(), n.id)}>
                      {n.name}
                    </EntityLink>
                    {st.label ? (
                      <span
                        className={`shrink-0 text-[12px] ${st.tone === "danger" ? "text-destructive" : "text-warning"}`}
                      >
                        {reason ? (
                          <Tooltip content={reason}>{st.label}</Tooltip>
                        ) : (
                          st.label
                        )}
                      </span>
                    ) : null}
                    <Tags tags={n.tags} className="shrink-0" />
                  </div>
                  <p className="truncate pl-[18px] text-[12px] text-muted-foreground">
                    <span className="sm:hidden">{locationLabel(region)} · </span>
                    {notes.map((x, i) => (
                      <Fragment key={i}>
                        {i ? " · " : null}
                        {x}
                      </Fragment>
                    ))}
                    {n.gpus ? (
                      <span className="lg:hidden">
                        {` · ${n.gpusUsed ?? 0} of ${n.gpus} GPUs in use`}
                      </span>
                    ) : null}
                  </p>
                </Td>
                <Td className="hidden sm:table-cell">
                  <RegionCell region={region} />
                </Td>
                <Td className="hidden sm:table-cell">
                  {off ? (
                    <span className="text-muted-foreground">—</span>
                  ) : (
                    <Meter
                      value={n.usage.cpu}
                      max={n.capacity.cpu}
                      label={`${n.name} CPU`}
                    />
                  )}
                </Td>
                <Td className="hidden sm:table-cell">
                  {off ? (
                    <span className="text-muted-foreground">—</span>
                  ) : (
                    <Meter
                      value={n.usage.memoryGi}
                      max={n.capacity.memoryGi}
                      label={`${n.name} memory`}
                    />
                  )}
                </Td>
                {gpus ? (
                  <Td className="hidden whitespace-nowrap text-[12px] tabular-nums text-muted-foreground lg:table-cell">
                    {n.gpus ? (
                      <Tooltip content={gpuText(n)}>
                        {n.gpusUsed ?? 0} of {n.gpus} in use
                      </Tooltip>
                    ) : n.gpuStatus ? (
                      (gpuProblem(n)?.replace(/^GPU: /, "") ?? "Starting")
                    ) : (
                      "—"
                    )}
                  </Td>
                ) : null}
                <Td>
                  <div onClick={(e) => e.stopPropagation()}>
                    <Menu
                      label={`Actions for ${n.name}`}
                      items={items(n)}
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
                  </div>
                </Td>
              </Tr>
            );
          })}
        </tbody>
      </Table>
      {dialog}
    </>
  );
}

/** A server's region in a table: flag and city; the full name on hover. */
function RegionCell({ region }: { region: string }) {
  const s = usePyo();
  const cc = regionCountry(region, s.servers, s.clusters);
  const full = locationLabel(region);
  const { city } = parseLocation(region);
  return (
    <Tooltip content={full}>
      <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
        {cc ? <Flag country={cc} size={13} /> : null}
        {city || full}
      </span>
    </Tooltip>
  );
}

export function WorkloadTable({
  workloads,
  s,
  showCluster = true,
}: {
  workloads: Workload[];
  s: PyoState;
  showCluster?: boolean;
}) {
  const [q, setQ] = useState("");
  const [kind, setKind] = useState("all");
  const [scope, setScope] = useState<"all" | "pyo" | "system">("all");
  const rows = workloads
    .filter(
      (w) =>
        (!q ||
          w.name.includes(q.toLowerCase()) ||
          w.namespace.includes(q.toLowerCase())) &&
        (kind === "all" || w.kind === kind) &&
        (scope === "all" || (scope === "system" ? w.system : !w.system)),
    )
    .sort(
      (a, b) =>
        Number(!!a.system) - Number(!!b.system) ||
        a.namespace.localeCompare(b.namespace) ||
        a.name.localeCompare(b.name),
    );
  return (
    <ListSection
      toolbar={
        <>
          <SearchInput
            value={q}
            onChange={setQ}
            placeholder="Search by name or project…"
            className="min-w-0 flex-1 basis-full sm:basis-0"
          />
          <div className="w-36 sm:w-44">
            <Select
              value={kind}
              onChange={(e) => setKind(e.target.value)}
              aria-label="Kind"
              heading="Kind"
            >
              <option value="all">All kinds</option>
              {["Deployment", "StatefulSet", "DaemonSet", "Job", "CronJob"].map(
                (k) => (
                  <option key={k}>{k}</option>
                ),
              )}
            </Select>
          </div>
          <div className="w-36 sm:w-44">
            <Select
              value={scope}
              onChange={(e) => setScope(e.target.value as typeof scope)}
              aria-label="Managed by"
              heading="Show"
            >
              <option value="all">All workloads</option>
              <option value="pyo">Managed by Pyo</option>
              <option value="system">System workloads</option>
            </Select>
          </div>
        </>
      }
    >
      <Panel>
        {rows.length ? (
          <Table>
            <thead>
              <tr>
                <Th>Workload</Th>
                <Th>Project</Th>
                <Th>Kind</Th>
                <Th>Status</Th>
                <Th>Ready</Th>
                <Th>CPU</Th>
                <Th>Memory</Th>
                {showCluster ? <Th>Cluster</Th> : null}
                <Th>Pyo service</Th>
              </tr>
            </thead>
            <tbody>
              {rows.map((w) => {
                const project = s.projects.find((p) => p.id === w.projectId);
                return (
                  <Tr key={w.id}>
                    <Td className="font-mono text-[12px]">{w.name}</Td>
                    <Td className="font-mono text-[11.5px] text-muted-foreground">
                      {w.namespace}
                    </Td>
                    <Td>{w.kind}</Td>
                    <Td>
                      <WorkloadStatusBadge status={w.status} />
                    </Td>
                    <Td className="tabular-nums">
                      {w.kind === "CronJob"
                        ? "—"
                        : `${w.replicas.ready}/${w.replicas.desired}`}
                    </Td>
                    <Td className="tabular-nums text-muted-foreground">
                      {cpu(w.cpuMillis)}
                    </Td>
                    <Td className="tabular-nums text-muted-foreground">
                      {mem(w.memoryMi)}
                    </Td>
                    {showCluster ? (
                      <Td>
                        {s.clusters.find((c) => c.id === w.clusterId)?.name}
                      </Td>
                    ) : null}
                    <Td>
                      {w.serviceId ? (
                        <span>
                          <Link
                            to="/$team/$projectId/$serviceId"
                            params={{
                              team: currentSlug(),
                              ...serviceParams(w.serviceId),
                            }}
                            className="font-medium hover:text-primary"
                          >
                            {w.name}
                          </Link>
                          {project ? (
                            <span className="text-muted-foreground">
                              {" "}
                              · {project.name}
                            </span>
                          ) : null}
                        </span>
                      ) : (
                        <span className="text-muted-foreground">
                          {w.system ? "System" : "Unmanaged"}
                        </span>
                      )}
                    </Td>
                  </Tr>
                );
              })}
            </tbody>
          </Table>
        ) : (
          <EmptyState compact title="No workloads match" />
        )}
      </Panel>
    </ListSection>
  );
}
