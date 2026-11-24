import { useState, type ReactNode } from "react";
import { Layered } from "~/components/ui/Layered";
import { Status } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Input } from "~/components/ui/Field";
import { EmptyState, Panel } from "~/components/ui/Panel";
import { CopyButton, Table, Td, Th, Tr } from "~/components/ui/Table";
import { DomainStatus } from "~/components/pyo/DomainState";
import {
  api,
  listedServices,
  roleIn,
  usePyo,
  servicePath,
} from "~/lib/pyo/store";
import { dateTime, portsTakenText } from "~/lib/pyo/format";
import { toast } from "~/lib/pyo/toast";
import type { Cluster } from "~/lib/pyo/types";
import { CodeText, EntityLink } from "./common";
import { EndpointTable } from "./tables";

/** Exposure setup of one cluster: what it can do, its HTTP routes and its Services. */
export function ClusterNetworking({
  cluster: c,
  titled,
}: {
  cluster: Cluster;
  titled?: boolean;
}) {
  const s = usePyo();
  const apps = listedServices(s).filter((a) => a.clusterId === c.id);
  const routes = apps.flatMap((a) => a.domains.map((d) => ({ a, d })));
  const lb = c.capabilities.loadBalancer;
  return (
    <div className="space-y-4">
      {titled ? (
        <h2 className="pt-2 text-[15px] font-semibold">{c.name}</h2>
      ) : null}
      <Layered bare>
        <div className="grid gap-1.5 sm:grid-cols-2 lg:grid-cols-4">
          {(
            [
              [
                "Gateway",
                c.capabilities.gatewayPortsTaken?.length ? (
                  <span className="text-warning">
                    {c.capabilities.gatewayPortsTaken.map((t) => (
                      <span key={t.server} className="block">
                        {portsTakenText(t)}
                      </span>
                    ))}
                  </span>
                ) : c.capabilities.gatewayError ? (
                  <span className="text-destructive">
                    <CodeText text={c.capabilities.gatewayError} />
                  </span>
                ) : c.capabilities.ingressController === "envoy" ? (
                  "Running"
                ) : (
                  "Not running yet"
                ),
              ],
              [
                "Public IP",
                c.publicTraffic === false
                  ? "Public traffic is off"
                  : lb === "none"
                  ? "None"
                  : `${c.publicIp ?? "None found"} · ${lb === "servicelb" ? "k3s ServiceLB" : "MetalLB"}`,
              ],
              [
                "TLS",
                c.capabilities.certManager
                  ? "cert-manager issues certificates"
                  : "No cert-manager, so HTTP only",
              ],
              ["CIDRs", `containers ${c.podCidr} · services ${c.serviceCidr}`],
            ] as [string, ReactNode][]
          ).map(([k, v]) => (
            <div
              key={k}
              className="rounded-[14px] border border-card-border bg-card px-4 py-3"
            >
              <p className="text-[11px] font-medium text-muted-foreground">
                {k}
              </p>
              <p className="mt-0.5 break-words text-[12.5px]">{v}</p>
            </div>
          ))}
        </div>
      </Layered>
      <ClusterPreviewDomain cluster={c} />
      <Panel title="HTTP routes">
        {routes.length ? (
          <Table>
            <thead>
              <tr>
                <Th>Host</Th>
                <Th>Backend</Th>
                <Th>Status</Th>
              </tr>
            </thead>
            <tbody>
              {routes.map(({ a, d }) => (
                <Tr key={d.id}>
                  <Td className="font-mono text-[12px]">{d.hostname}</Td>
                  <Td>
                    <EntityLink to={servicePath(a.id, "domains")}>
                      {a.name}
                    </EntityLink>
                    <span className="text-muted-foreground">:{d.portName}</span>
                  </Td>
                  <Td>
                    <DomainStatus
                      d={d}
                      certs={s.certificates}
                      notDeployed={!a.currentDeploymentId}
                    />
                  </Td>
                </Tr>
              ))}
            </tbody>
          </Table>
        ) : (
          <EmptyState compact title="No HTTP routes" />
        )}
      </Panel>
      <Panel title="Ports">
        <EndpointTable apps={apps} s={s} />
      </Panel>
    </div>
  );
}

/** A hostname with at least two labels: "preview.example.com". */
const HOSTNAME =
  /^(?=.{1,253}$)([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z][a-z0-9-]{0,61}[a-z0-9]$/;

/** Where the cluster's previews get their URLs: a wildcard domain, else nip.io. */
export function ClusterPreviewDomain({ cluster: c }: { cluster: Cluster }) {
  const s = usePyo();
  const admin = ["owner", "admin"].includes(roleIn(s, c.teamId) ?? "");
  const [domain, setDomain] = useState("");
  const [busy, setBusy] = useState(false);
  const [removing, setRemoving] = useState(false);
  const pd = c.previewDomain;
  const typed = domain.trim().toLowerCase().replace(/\.$/, "");
  const valid = HOSTNAME.test(typed);
  const cert = pd?.certificate;
  return (
    <Panel title="Preview domain">
      {pd ? (
        <div className="space-y-3 p-4">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
            <span className="font-mono text-[13px]">*.{pd.domain}</span>
            {pd.dns === "cloudflare" ? (
              <Status
                tone={
                  cert?.state === "issued"
                    ? "success"
                    : cert?.state === "failed"
                      ? "danger"
                      : "progress"
                }
                label={
                  cert?.state === "issued"
                    ? cert.notAfter
                      ? `Certificate until ${dateTime(cert.notAfter)}`
                      : "Certificate issued"
                    : cert?.state === "failed"
                      ? "Certificate failed"
                      : "Getting certificate"
                }
              />
            ) : null}
            <Button
              variant="ghost"
              size="xs"
              className="ml-auto text-destructive"
              disabled={!admin}
              onClick={() => setRemoving(true)}
            >
              Remove
            </Button>
          </div>
          {cert?.state === "failed" && cert.error ? (
            <p className="text-[12px] text-destructive">{cert.error}</p>
          ) : null}
          {pd.error ? (
            <p className="text-[12px] text-destructive">{pd.error}</p>
          ) : null}
          {pd.dns === "manual" ? (
            <div className="rounded-lg border border-border bg-muted/40 px-3 py-2.5 text-[12px]">
              <p className="mb-1.5 text-muted-foreground">
                Create this DNS record at your DNS provider:
              </p>
              <div className="flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-[11.5px]">
                <span>{pd.record.type}</span>
                <span className="inline-flex items-center gap-1">
                  {pd.record.name}
                  <CopyButton value={pd.record.name} label="Copy record name" />
                </span>
                <span className="inline-flex items-center gap-1">
                  {pd.record.content ?? "No public IP found yet"}
                  {pd.record.content ? (
                    <CopyButton
                      value={pd.record.content}
                      label="Copy record value"
                    />
                  ) : null}
                </span>
              </div>
            </div>
          ) : null}
        </div>
      ) : (
        <form
          className="space-y-1.5 p-4"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!valid || busy) return;
            setBusy(true);
            const res = await api.setClusterPreviewDomain(c.id, typed);
            setBusy(false);
            if (res) {
              setDomain("");
              toast({ title: "Preview domain saved", description: c.name });
            }
          }}
        >
          <div className="flex flex-wrap items-center gap-2">
            <Input
              mono
              value={domain}
              onChange={(e) => setDomain(e.target.value)}
              placeholder="preview.example.com"
              aria-label="Preview domain"
              disabled={!admin}
              className="min-w-0 flex-1"
            />
            <Button
              type="submit"
              size="sm"
              variant="secondary"
              disabled={!admin || !valid || busy}
            >
              {busy ? "Saving…" : "Save"}
            </Button>
          </div>
          <p className="text-[12px] text-muted-foreground">
            {domain.trim() && !valid
              ? "Use a hostname like preview.example.com."
              : "Without one, previews use nip.io."}
          </p>
        </form>
      )}
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title={`Remove ${pd?.domain}?`}
        description="Previews move to nip.io URLs."
        confirmLabel="Remove"
        onConfirm={async () => {
          if (await api.setClusterPreviewDomain(c.id, null))
            toast({ title: "Preview domain removed", description: c.name });
        }}
      />
    </Panel>
  );
}
