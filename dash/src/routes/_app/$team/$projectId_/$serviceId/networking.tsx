import { InfoIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { serviceSettingsNav, SettingsFrame } from "~/components/pyo/SettingsFrame";
import { useState } from "react";
import { PortEditor, publicPortError } from "~/components/pyo/editors";
import { Button } from "~/components/ui/Button";
import { Callout, KeyValue, Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import type { DraftPort } from "~/lib/pyo/draft";
import { DNS_LABEL } from "~/lib/pyo/format";
import { api, usePyo, serviceIdOf } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application } from "~/lib/pyo/types";

export const Route = createFileRoute(
  "/_app/$team/$projectId_/$serviceId/networking",
)({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const app = usePyo().services.find((a) => a.id === serviceId);
    return app ? (
      <SettingsFrame items={serviceSettingsNav(app.id)}>
        <Networking key={app.id} app={app} />
      </SettingsFrame>
    ) : null;
  },
});

function Networking({ app }: { app: Application }) {
  const s = usePyo();
  const [rows, setRows] = useState<DraftPort[]>(() =>
    app.ports.map((p) => ({ ...p })),
  );
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  const ns =
    s.projects.find((p) => p.id === app.projectId)?.namespace ?? "default";
  const host = `${app.name}.${ns}.svc.cluster.local`;
  const dirty = JSON.stringify(rows) !== JSON.stringify(app.ports);

  // Validation, plus conflicts with public ports other services already hold on this cluster.
  const errors: Record<string, string> = {};
  const conflicts: Record<string, string> = {};
  const seen = new Set<string>();
  for (const p of rows) {
    const key = `${p.containerPort}/${p.protocol === "UDP" ? "UDP" : "TCP"}`;
    if (!DNS_LABEL.test(p.name))
      errors[`port.${p.id}`] = "Port name must be a lowercase DNS label.";
    else if (!(p.containerPort > 0 && p.containerPort < 65536))
      errors[`port.${p.id}`] = "Port must be 1–65535.";
    else if (seen.has(key))
      errors[`port.${p.id}`] = `Port ${key} is defined twice.`;
    else if (
      p.exposure === "http" &&
      !app.domains.some((d) => d.portName === p.name)
    )
      errors[`port.${p.id}`] = "Add a domain on the Domains tab to route HTTP.";
    else if (
      p.exposure === "nodeport" &&
      p.publicPort !== undefined &&
      !(p.publicPort >= 30000 && p.publicPort <= 32767)
    )
      errors[`port.${p.id}`] = "Ports on every server must be 30000–32767.";
    else if (
      p.exposure === "loadbalancer" &&
      cluster?.capabilities.loadBalancer === "none"
    )
      errors[`port.${p.id}`] =
        `${cluster.name} can't give services a public address.`;
    else if (publicPortError(p))
      errors[`port.${p.id}`] = publicPortError(p)!;
    seen.add(key);
    if (p.publicPort) {
      const other = s.services.find(
        (a) =>
          a.id !== app.id &&
          a.clusterId === app.clusterId &&
          a.ports.some(
            (x) =>
              x.publicPort === p.publicPort &&
              x.exposure === p.exposure &&
              (x.protocol === "UDP") === (p.protocol === "UDP"),
          ),
      );
      if (other)
        conflicts[p.id] =
          `Port ${p.publicPort} is already used by ${other.name} on ${cluster?.name}.`;
    }
  }
  const invalid =
    Object.keys(errors).length > 0 || Object.keys(conflicts).length > 0;
  const off = cluster?.publicTraffic === false;
  const exposed = off ? [] : rows.filter((p) => p.exposure !== "internal");

  return (
    <div className="space-y-4">
      <Panel title="Internal address">
        <KeyValue
          items={[
            [
              "Internal hostname",
              <span className="inline-flex items-center gap-1">
                <span className="font-mono text-[11.5px]">{host}</span>
                <CopyButton value={host} label="Copy hostname" />
              </span>,
            ],
            [
              "Short name",
              <span className="font-mono text-[11.5px]">
                {app.name}.{ns}
              </span>,
            ],
            [
              "Visibility",
              exposed.length ? (
                <span className="text-warning">
                  Public ({exposed.length} port{exposed.length > 1 ? "s" : ""})
                </span>
              ) : (
                "Private to the cluster"
              ),
            ],
            [
              "Public IPs",
              off
                ? "Public traffic is off for this cluster."
                : cluster?.capabilities.loadBalancer === "none"
                ? "Not available"
                : `${cluster?.capabilities.loadBalancer} · ${cluster?.capabilities.loadBalancerIPs.join(", ") || "no IPs"}`,
            ],
          ]}
        />
      </Panel>
      <Panel
        title="Ports"

        actions={
          <>
            <Button
              variant="ghost"
              size="xs"
              disabled={!dirty}
              onClick={() => setRows(app.ports.map((p) => ({ ...p })))}
            >
              Discard
            </Button>
            <Button
              size="xs"
              disabled={!dirty || invalid}
              onClick={() => {
                api.setPorts(app.id, rows);
                toast({
                  title: "Ports saved",
                  description: "Redeploy to apply.",
                });
              }}
            >
              Save changes
            </Button>
          </>
        }
      >
        <div className="p-4">
          <PortEditor
            rows={rows}
            onChange={setRows}
            errors={errors}
            cluster={cluster}
            conflicts={conflicts}
          />
        </div>
      </Panel>
      <Callout tone="neutral" icon={InfoIcon}>
        Domains carry HTTP only. For TCP and UDP, use Public IP.
      </Callout>
    </div>
  );
}
