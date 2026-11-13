import { PlusIcon } from "@phosphor-icons/react";
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
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Stepper } from "~/components/ui/Field";
import { Meter, Panel } from "~/components/ui/Panel";
import { bigChange, MAX_REPLICAS, serviceRoomWarning } from "~/lib/pyo/capacity";
import { cpu, mem } from "~/lib/pyo/format";
import { api, desiredReplicas, usePyo, serviceIdOf } from "~/lib/pyo/store";
import { getTemplate } from "~/lib/pyo/templates";
import { toast } from "~/lib/pyo/toast";
import type { Application, HealthCheck } from "~/lib/pyo/types";

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

function Resources({ app }: { app: Application }) {
  const s = usePyo();
  const [resources, setResources] = useState(app.resources);
  const [replicas, setReplicas] = useState(app.replicas);
  const t =
    app.source.type === "template"
      ? getTemplate(app.source.templateId)
      : undefined;
  const dirty =
    JSON.stringify({ resources, replicas }) !==
    JSON.stringify({ resources: app.resources, replicas: app.replicas });
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
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  // Only a change is checked: a server that's down shouldn't block saving.
  const gpuErr =
    (resources.gpu ?? 0) !== (app.resources.gpu ?? 0)
      ? gpuError(resources.gpu ?? 0, cluster)
      : undefined;
  if (gpuErr) errors.gpu = gpuErr;
  const n = Math.max(1, app.status === "stopped" ? 0 : desiredReplicas(app));
  // Saved either way: replicas that don't fit wait for room.
  const roomNote = app.autoscale
    ? null
    : serviceRoomWarning(s.servers, app, replicas, "run", resources);
  // A very large change asks first (the API holds it back otherwise).
  const big = app.autoscale
    ? null
    : bigChange(s.servers, app, replicas, app.replicas, "run", resources);
  const [asking, setAsking] = useState(false);
  const save = (confirm: boolean) => {
    api.updateService(
      app.id,
      { resources, replicas },
      { requiresRedeploy: true, label: "resources", confirm },
    );
    toast({
      title: "Resources saved",
      description: "Redeploy to apply.",
    });
  };

  return (
    <div className="space-y-4">
      <Panel title="Observed usage">
        <div className="grid gap-4 p-4 sm:grid-cols-2">
          <div>
            <p className="mb-1.5 text-[12px] text-muted-foreground">
              CPU{" "}
              <span className="text-foreground">
                {cpu(app.runtime.cpuMillis)}
              </span>{" "}
              of {cpu(app.resources.cpuLimit * n)} limit
            </p>
            <Meter
              value={app.runtime.cpuMillis}
              max={app.resources.cpuLimit * n}
              label="CPU usage vs limit"
            />
          </div>
          <div>
            <p className="mb-1.5 text-[12px] text-muted-foreground">
              Memory{" "}
              <span className="text-foreground">
                {mem(app.runtime.memoryMi)}
              </span>{" "}
              of {mem(app.resources.memoryLimit * n)} limit
            </p>
            <Meter
              value={app.runtime.memoryMi}
              max={app.resources.memoryLimit * n}
              label="Memory usage vs limit"
            />
          </div>
        </div>
      </Panel>
      <Panel
        title="Configuration"

        actions={
          <Button
            size="xs"
            disabled={!dirty || Object.keys(errors).length > 0}
            onClick={() => (big ? setAsking(true) : save(false))}
          >
            Save changes
          </Button>
        }
      >
        <div className="space-y-6 p-4">
          <ResourceFields
            value={resources}
            onChange={setResources}
            errors={errors}
            cluster={cluster}
            autoscaled={!!app.autoscale}
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
              save(true);
            }}
          />
        </div>
      </Panel>
      {app.workloadKind === "CronJob" ? null : <HealthCheck app={app} />}
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

const NO_HEALTH_CHECK: HealthCheck = {
  type: "none",
  path: "/",
  port: 0,
  command: "",
  initialDelaySeconds: 0,
  periodSeconds: 10,
};

/** When a replica counts as ready: its own card, saved on its own. */
function HealthCheck({ app }: { app: Application }) {
  // Services made through the API or MCP may have none.
  const saved = app.healthCheck ?? NO_HEALTH_CHECK;
  const [health, setHealth] = useState(saved);
  const dirty = JSON.stringify(health) !== JSON.stringify(saved);
  return (
    <Panel
      title="Health check"
      actions={
        <Button
          size="xs"
          disabled={!dirty}
          onClick={() => {
            api.updateService(
              app.id,
              { healthCheck: health },
              { requiresRedeploy: true, label: "health check" },
            );
            toast({ title: "Health check saved", description: "Redeploy to apply." });
          }}
        >
          Save changes
        </Button>
      }
    >
      <div className="space-y-3 p-4">
        {health.auto === "static" ? (
          <p className="text-[12px] text-muted-foreground">
            Automatic (static site)
          </p>
        ) : null}
        <HealthCheckFields label="Type" value={health} onChange={setHealth} />
      </div>
    </Panel>
  );
}
