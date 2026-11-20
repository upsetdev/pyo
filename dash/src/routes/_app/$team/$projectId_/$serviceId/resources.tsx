import { createFileRoute } from "@tanstack/react-router";
import { serviceSettingsNav, SettingsFrame } from "~/components/pyo/SettingsFrame";
import { useState } from "react";
import {
  gpuError,
  HealthCheckFields,
  ResourceFields,
} from "~/components/pyo/editors";
import { RoomNote } from "~/components/pyo/RoomNote";
import { VolumeTable } from "~/components/pyo/tables";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Stepper } from "~/components/ui/Field";
import { optionClass } from "~/components/ui/fieldStyles";
import { Panel } from "~/components/ui/Panel";
import { Tags } from "~/components/ui/TagInput";
import { bigChange, MAX_REPLICAS, serviceRoomWarning } from "~/lib/pyo/capacity";
import {
  isAny,
  placementOf,
  picks,
  placementProblem,
  samePlacement,
  type Placement,
} from "~/lib/pyo/placement";
import { api, desiredReplicas, usePyo, serviceIdOf } from "~/lib/pyo/store";
import { getTemplate } from "~/lib/pyo/templates";
import { toast } from "~/lib/pyo/toast";
import type {
  Application,
  ClusterServer,
  HealthCheck as HealthCheckSpec,
} from "~/lib/pyo/types";

export const Route = createFileRoute(
  "/_app/$team/$projectId_/$serviceId/resources",
)({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const app = usePyo().services.find((a) => a.id === serviceId);
    return app ? (
      <SettingsFrame items={serviceSettingsNav(app.id)}>
        <Resources key={app.id} app={app} />
      </SettingsFrame>
    ) : null;
  },
});

const NO_HEALTH_CHECK: HealthCheckSpec = {
  type: "none",
  path: "/",
  port: 0,
  command: "",
  initialDelaySeconds: 0,
  periodSeconds: 10,
};

const ANY: Placement = { servers: [], tags: [] };

function Resources({ app }: { app: Application }) {
  const s = usePyo();
  const [resources, setResources] = useState(app.resources);
  const [replicas, setReplicas] = useState(app.replicas);
  // Services made through the API or MCP may have no health check.
  const savedHealth = app.healthCheck ?? NO_HEALTH_CHECK;
  const [health, setHealth] = useState(savedHealth);
  const savedPlacement = placementOf(app);
  const [only, setOnly] = useState(!isAny(savedPlacement));
  const [picked, setPicked] = useState<Placement>(savedPlacement);
  const t =
    app.source.type === "template"
      ? getTemplate(app.source.templateId)
      : undefined;
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  const servers = s.servers.filter(
    (n) => n.clusterId === app.clusterId && !n.removal,
  );
  // A volume that keeps its data on one server's disk pins it there.
  const pinned = s.volumes.find(
    (v) => v.serviceId === app.id && v.server && !v.deleting,
  );
  const volume = pinned?.server
    ? { name: pinned.name, server: pinned.server }
    : null;
  // Only these: its volume's server is always one of them.
  const placement: Placement = !only
    ? ANY
    : volume && !servers.some((n) => n.name === volume.server && picks(picked, n))
      ? { ...picked, servers: [volume.server, ...picked.servers] }
      : picked;

  const sizeDirty =
    JSON.stringify({ resources, replicas }) !==
    JSON.stringify({ resources: app.resources, replicas: app.replicas });
  const healthDirty = JSON.stringify(health) !== JSON.stringify(savedHealth);
  const placementDirty = !samePlacement(placement, savedPlacement);
  const dirty = sizeDirty || healthDirty || placementDirty;

  const errors: Record<string, string> = {};
  if (resources.cpuLimit < resources.cpuRequest)
    errors.cpuLimit = "Limit must be at least the request.";
  if (resources.memoryLimit < resources.memoryRequest)
    errors.memoryLimit = "Limit must be at least the request.";
  if (!(Number.isInteger(replicas) && replicas >= 0 && replicas <= MAX_REPLICAS))
    errors.replicas = "0 or more.";
  // A template no longer in the catalog: its workload says if it was stateful.
  const single = t
    ? t.stateful
    : app.source.type === "template" && app.workloadKind === "StatefulSet";
  if (single && replicas > 1)
    errors.replicas = `${t?.name ?? "This service"} runs as a single replica.`;
  // Only a change is checked: a server that's down shouldn't block saving.
  const gpuErr =
    (resources.gpu ?? 0) !== (app.resources.gpu ?? 0)
      ? gpuError(resources.gpu ?? 0, cluster)
      : undefined;
  if (gpuErr) errors.gpu = gpuErr;
  const gpu = (resources.gpu ?? 0) > 0;
  // What its replicas hold now, given back when they move.
  const own: Record<string, { cpu: number; memoryGi: number }> = {};
  for (const r of app.runtime.replicas ?? []) {
    const o = (own[r.server] ??= { cpu: 0, memoryGi: 0 });
    o.cpu += app.resources.cpuRequest / 1000;
    o.memoryGi += app.resources.memoryRequest / 1024;
  }
  // As the API checks it: room counts only when where it runs changes.
  const placementError =
    only && isAny(placement)
      ? "Pick at least one server."
      : placementDirty || (gpu && only)
        ? placementProblem(
            placement,
            servers,
            volume,
            {
              gpu,
              cpu: placementDirty ? resources.cpuRequest / 1000 : 0,
              memoryGi: placementDirty ? resources.memoryRequest / 1024 : 0,
            },
            own,
          )
        : null;
  if (placementError) errors.placement = placementError;

  const n = Math.max(1, app.status === "stopped" ? 0 : desiredReplicas(app));
  const running = app.status !== "stopped" && app.runtime.readyReplicas > 0;
  // Saved either way: replicas that don't fit wait for room.
  const roomNote = app.autoscale
    ? null
    : serviceRoomWarning(s.servers, app, replicas, "run", resources, placement);
  // A very large change asks first (the API holds it back otherwise).
  const big = app.autoscale
    ? null
    : bigChange(s.servers, app, replicas, app.replicas, "run", resources, placement);
  const [asking, setAsking] = useState(false);
  const save = async (confirm: boolean) => {
    const redeploy = sizeDirty || healthDirty;
    const res = await api.updateService(
      app.id,
      {
        resources,
        replicas,
        healthCheck: health,
        // Null is any server. The older fields go, so they can't come back.
        placement: isAny(placement) ? null : placement,
        servers: undefined,
        serverTags: undefined,
      },
      { requiresRedeploy: redeploy, label: "resources", confirm },
    );
    if (!res) return;
    const moving = placementDirty && !!app.currentDeploymentId;
    toast({
      title: "Saved",
      description:
        moving && redeploy
          ? "Moving it now. Redeploy to apply the rest."
          : moving
            ? "Moving it now."
            : redeploy
              ? "Redeploy to apply."
              : undefined,
    });
  };

  return (
    <div className="space-y-4">
      <Panel title="Servers">
        <ServersPicker
          only={only}
          onOnly={setOnly}
          picked={placement}
          onPick={setPicked}
          servers={servers}
          volume={volume}
          gpu={gpu}
          error={errors.placement}
        />
      </Panel>
      <Panel title="Configuration">
        <div className="space-y-6 p-4">
          <ResourceFields
            value={resources}
            onChange={setResources}
            errors={errors}
            cluster={cluster}
            autoscaled={!!app.autoscale}
            using={
              running
                ? {
                    cpuMillis: app.runtime.cpuMillis / n,
                    memoryMi: app.runtime.memoryMi / n,
                  }
                : undefined
            }
          />
          <Field
            label={
              app.autoscale
                ? `Autoscaling between ${app.autoscale.min} and ${app.autoscale.max} replicas`
                : "Replicas"
            }
            error={errors.replicas}
            className="max-w-[200px]"
          >
            {(p) => (
              <Stepper
                {...p}
                min={0}
                max={MAX_REPLICAS}
                disabled={!!app.autoscale}
                value={app.autoscale ? desiredReplicas(app) : replicas}
                onChange={setReplicas}
                className="w-[140px]"
              />
            )}
          </Field>
          <RoomNote text={roomNote} />
          <ConfirmDialog
            open={asking && !!big}
            onClose={() => setAsking(false)}
            tone="primary"
            title={big?.title ?? ""}
            description={big?.body ?? ""}
            confirmLabel="Scale"
            onConfirm={() => {
              setAsking(false);
              void save(true);
            }}
          />
        </div>
      </Panel>
      {app.workloadKind === "CronJob" ? null : (
        <Panel title="Health check">
          <div className="space-y-3 p-4">
            {health.auto === "static" ? (
              <p className="text-[12px] text-muted-foreground">
                Automatic (static site)
              </p>
            ) : null}
            <HealthCheckFields label="Type" value={health} onChange={setHealth} />
          </div>
        </Panel>
      )}
      {/* One Save for the whole tab, in view while there's something to save. */}
      <div
        className={`flex justify-end ${dirty ? "sticky bottom-[calc(env(safe-area-inset-bottom,0px)+0.75rem)] z-10" : ""}`}
      >
        <Button
          disabled={!dirty || Object.keys(errors).length > 0}
          onClick={() => (big ? setAsking(true) : void save(false))}
          className={dirty ? "shadow-lg" : ""}
        >
          Save changes
        </Button>
      </div>
      <Panel title="Persistent volumes">
        <VolumeTable
          s={s}
          volumes={s.volumes.filter((v) => v.serviceId === app.id)}
          showCluster={false}
          showService={false}
        />
      </Panel>
    </div>
  );
}

/** "2 CPU · 4 GB" */
function size(n: ClusterServer) {
  return `${+n.capacity.cpu.toFixed(1)} CPU · ${Math.round(n.capacity.memoryGi)} GB`;
}

/**
 * Where it runs: any server, or only the servers and tags picked. A tag
 * means every server with it, tagged now or later.
 */
function ServersPicker({
  only,
  onOnly,
  picked,
  onPick,
  servers,
  volume,
  gpu,
  error,
}: {
  only: boolean;
  onOnly: (only: boolean) => void;
  /** What's picked, its volume's server included. */
  picked: Placement;
  onPick: (p: Placement) => void;
  servers: ClusterServer[];
  volume: { name: string; server: string } | null;
  gpu: boolean;
  error?: string;
}) {
  const tags = [...new Set(servers.flatMap((n) => n.tags ?? []))].sort();
  // A GPU service only runs on GPU servers, wherever it's put.
  const fits = (n: ClusterServer) => !gpu || !!n.gpuDetected;
  const toggle = <T,>(list: T[], x: T) =>
    list.includes(x) ? list.filter((y) => y !== x) : [...list, x];
  const row = (on: boolean, disabled: boolean) =>
    `${optionClass({ selected: on })} flex items-center gap-3 px-3.5 py-2.5 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20 ${disabled ? "cursor-not-allowed" : "cursor-pointer"}`;
  return (
    <div className="space-y-3 p-4">
      <div role="radiogroup" aria-label="Where it runs" className="space-y-2">
        <label className={row(!only, false)}>
          <input
            type="radio"
            name="placement"
            checked={!only}
            onChange={() => onOnly(false)}
          />
          <span className="text-[13px] font-medium">
            Any server{" "}
            <span className="font-normal text-muted-foreground">
              (recommended)
            </span>
          </span>
        </label>
        <label className={row(only, false)}>
          <input
            type="radio"
            name="placement"
            checked={only}
            onChange={() => onOnly(true)}
          />
          <span className="text-[13px] font-medium">Only these</span>
        </label>
      </div>
      {only ? (
        <div
          role="group"
          aria-label="Servers to run on"
          className="space-y-2 sm:pl-7"
        >
          {tags.map((tag) => {
            const tagged = servers.filter((n) => (n.tags ?? []).includes(tag));
            const disabled = !tagged.some(fits);
            const on = picked.tags.includes(tag);
            return (
              <label key={`tag:${tag}`} className={row(on, disabled)}>
                <input
                  type="checkbox"
                  checked={on}
                  disabled={disabled}
                  onChange={() =>
                    onPick({ ...picked, tags: toggle(picked.tags, tag) })
                  }
                />
                <span className={`text-[13px] ${disabled ? "text-muted-foreground" : ""}`}>
                  All servers tagged{" "}
                  <span className="font-mono text-[12.5px]">{tag}</span>{" "}
                  <span className="text-muted-foreground">({tagged.length})</span>
                </span>
              </label>
            );
          })}
          {servers.map((n) => {
            const isVolumes = volume?.server === n.name;
            const viaTag = picks({ servers: [], tags: picked.tags }, n);
            const on = isVolumes || viaTag || picked.servers.includes(n.name);
            const disabled = isVolumes || viaTag || !fits(n);
            return (
              <label key={n.id} className={row(on, disabled)}>
                <input
                  type="checkbox"
                  checked={on}
                  disabled={disabled}
                  onChange={() =>
                    onPick({ ...picked, servers: toggle(picked.servers, n.name) })
                  }
                />
                <span
                  className={`flex min-w-0 flex-1 flex-wrap items-center gap-x-2 gap-y-1 ${!on && disabled ? "text-muted-foreground" : ""}`}
                >
                  <span className="truncate text-[13px] font-medium">{n.name}</span>
                  <span className="text-[12px] text-muted-foreground">{size(n)}</span>
                  {n.gpuDetected && !n.tags?.includes("gpu") ? (
                    <Badge>GPU</Badge>
                  ) : null}
                  <Tags tags={n.tags} />
                </span>
              </label>
            );
          })}
        </div>
      ) : null}
      {volume ? (
        <p className="text-[12px] text-muted-foreground">
          Its volume lives on {volume.server}, so it runs there.
        </p>
      ) : gpu && !only ? (
        <p className="text-[12px] text-muted-foreground">
          It asks for a GPU, so it runs on GPU servers.
        </p>
      ) : null}
      {error ? (
        <p role="alert" className="text-[12px] text-destructive">
          {error}
        </p>
      ) : null}
    </div>
  );
}
