import {
  ArrowBendDownRightIcon,
  ArrowRightIcon,
  ArrowsClockwiseIcon,
  DotsThreeIcon,
  GlobeIcon,
  LockSimpleIcon,
  MagnifyingGlassIcon,
  PlusIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { copyIdItem } from "~/components/pyo/copyId";
import { Menu } from "~/components/ui/Menu";
import { isFreeDomain, isTailnetDomain } from "~/lib/pyo/freeDomain";
import { createFileRoute } from "@tanstack/react-router";
import {
  CertificateDialog,
  expiry,
  PickCertificateDialog,
  TLS_MODES,
  tlsLine,
} from "~/components/pyo/Certificates";
import { TlsBadge } from "~/components/ui/Badge";
import { Tooltip } from "~/components/ui/Tooltip";
import { DomainKind, DomainMark, DomainStatus } from "~/components/pyo/DomainState";
import { AppLink } from "~/components/ui/AppLink";
import { AddDomainDialog } from "~/components/pyo/AddDomainDialog";
import {
  DeleteRecordSwitch,
  DnsRecordLine,
} from "~/components/pyo/Cloudflare";
import { onTunnel, RestoreRecordSwitch } from "~/components/pyo/TunnelRecord";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Callout, EmptyState, Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { api, clusterTraffic, usePyo, serviceIdOf } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, Certificate, Domain } from "~/lib/pyo/types";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/domains")({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const app = usePyo().services.find((a) => a.id === serviceId);
    return app ? (
      <Domains app={app} />
    ) : null;
  },
});

function Domains({ app }: { app: Application }) {
  const s = usePyo();
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<Domain | null>(null);
  const [deleteRecord, setDeleteRecord] = useState(true);
  const [restoreRecord, setRestoreRecord] = useState(false);
  const [picking, setPicking] = useState<Domain | null>(null);
  const [replacing, setReplacing] = useState<Certificate | null>(null);
  const certOf = (d: Domain) =>
    d.tlsMode === "custom"
      ? s.certificates.find((c) => c.id === d.certificateId)
      : undefined;
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  const target = cluster?.publicIp ?? undefined;
  // In tunnel mode no direct record helps: the tunnel line says what to do.
  const tunnelMode = !!cluster && clusterTraffic(cluster) === "tunnel";
  const httpPorts = app.ports.filter((p) => p.protocol === "HTTP");

  return (
    <div className="space-y-4">
      <Panel
        title="Domains"

        actions={
          <>
            <AppLink
              href="/certificates"
              className="mr-3 inline-flex items-center gap-1.5 text-[12.5px] text-muted-foreground transition-colors hover:text-foreground"
            >
              <LockSimpleIcon size={14} aria-hidden />
              Certificates
            </AppLink>
            <Button
              size="xs"
              variant="secondary"
              onClick={() => setAdding(true)}
              disabled={!httpPorts.length}
            >
              <PlusIcon size={12} weight="bold" /> Add domain
            </Button>
          </>
        }
      >
        {!httpPorts.length ? (
          <EmptyState
            compact
            icon={GlobeIcon}
            title="No HTTP port"
            description="Domains need an HTTP port."
          />
        ) : app.domains.length ? (
          <ul className="list-rows">
            {app.domains.map((d) => {
              const cert = certOf(d);
              // Tailscale serves its HTTPS: no DNS, certificate or redirect to set.
              const tailnet = isTailnetDomain(d.hostname);
              return (
                <li key={d.id} className="px-4 py-3.5">
                  <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
                    <DomainMark d={d} clusterId={app.clusterId} />
                    <span className="inline-flex min-w-0 items-center gap-1.5">
                      <span className="truncate font-mono text-[13px]">
                        {d.hostname}
                      </span>
                    </span>
                    <DomainKind hostname={d.hostname} />
                    <Tooltip content={tlsLine(d, s.certificates)}>
                      <TlsBadge status={d.tls} />
                    </Tooltip>
                    <span className="ml-2 inline-flex items-center gap-1.5 text-[12px] text-muted-foreground">
                      <ArrowRightIcon size={12} aria-label="routes to" />
                      <span className="font-mono">{d.portName}</span>
                    </span>
                    {/* Only while it isn't live yet; a live domain needs no status. */}
                    {!app.currentDeploymentId || d.status !== "active" ? (
                      <DomainStatus
                        d={d}
                        certs={s.certificates}
                        notDeployed={!app.currentDeploymentId}
                      />
                    ) : null}
                    <span className="ml-auto flex gap-1">
                      <Menu
                        label={`Settings of ${d.hostname}`}
                        items={tailnet ? [
                          copyIdItem("Domain", d.id),
                          { type: "separator" as const },
                          {
                            label: "Remove domain",
                            icon: <TrashIcon size={14} />,
                            danger: true,
                            onSelect: () => setRemoving(d),
                          },
                        ] : [
                          // Pyo also checks domains waiting for DNS on its own; this checks now.
                          {
                            label: "Check DNS",
                            icon: <MagnifyingGlassIcon size={14} />,
                            disabled: d.status === "verifying",
                            onSelect: () => void api.verifyDomain(app.id, d.id),
                          },
                          { type: "separator" as const },
                          { type: "label", label: "HTTPS" },
                          ...TLS_MODES.map((m) => ({
                            label: m.label,
                            icon: <m.icon size={14} />,
                            hint:
                              (d.tlsMode ?? "auto") === m.value
                                ? "✓"
                                : undefined,
                            onSelect: () => {
                              if (m.value === "custom") return setPicking(d);
                              if ((d.tlsMode ?? "auto") === m.value) return;
                              api.updateDomain(app.id, d.id, {
                                tlsMode: m.value,
                              });
                              toast({
                                title: `HTTPS for ${d.hostname}: ${m.label}`,
                              });
                            },
                          })),
                          ...(certOf(d)
                            ? [
                                {
                                  label: "Replace certificate",
                                  icon: <ArrowsClockwiseIcon size={14} />,
                                  onSelect: () => setReplacing(certOf(d)!),
                                },
                              ]
                            : []),
                          { type: "separator" as const },
                          {
                            label: "Redirect HTTP to HTTPS",
                            icon: <ArrowBendDownRightIcon size={14} />,
                            hint: d.redirectHttps !== false ? "✓" : undefined,
                            disabled: d.tlsMode === "off",
                            onSelect: () => {
                              api.updateDomain(app.id, d.id, {
                                redirectHttps: d.redirectHttps === false,
                              });
                              toast({
                                title:
                                  d.redirectHttps === false
                                    ? "Redirecting to HTTPS"
                                    : "Not redirecting to HTTPS",
                              });
                            },
                          },
                          copyIdItem("Domain", d.id),
                          { type: "separator" as const },
                          {
                            label: "Remove domain",
                            icon: <TrashIcon size={14} />,
                            danger: true,
                            onSelect: () => {
                              setDeleteRecord(true);
                              setRestoreRecord(false);
                              setRemoving(d);
                            },
                          },
                        ]}
                        trigger={(t) => (
                          <Button
                            {...t}
                            variant="ghost"
                            size="icon"
                            aria-label={`Settings of ${d.hostname}`}
                          >
                            <DotsThreeIcon size={16} weight="bold" />
                          </Button>
                        )}
                      />
                    </span>
                  </div>
                  {/* Inline only when something needs doing. */}
                  {d.tlsMode === "custom" && !cert ? (
                    <p className="mt-1.5 text-[12px] text-destructive">
                      Its certificate was removed.
                      <button
                        type="button"
                        onClick={() => setPicking(d)}
                        className="ml-1.5 font-medium text-primary hover:text-primary/80"
                      >
                        Pick another
                      </button>
                    </p>
                  ) : cert && expiry(cert).tone !== "text-muted-foreground" ? (
                    <p className={`mt-1.5 text-[12px] ${expiry(cert).tone}`}>
                      Certificate {cert.name}: {expiry(cert).text.toLowerCase()}
                      .
                      <button
                        type="button"
                        onClick={() => setReplacing(cert)}
                        className="ml-1.5 font-medium text-primary hover:text-primary/80"
                      >
                        Replace
                      </button>
                    </p>
                  ) : null}
                  {d.tlsMode !== "off" && d.tls === "failed" && d.tlsError ? (
                    <p className="mt-1.5 text-[12px] text-destructive">
                      {d.tlsError}
                    </p>
                  ) : null}
                  <DnsRecordLine app={app} d={d} />
                  {d.status !== "active" &&
                  !isFreeDomain(d.hostname) &&
                  !tunnelMode &&
                  !onTunnel(d) &&
                  !(
                    d.dnsRecord &&
                    ["created", "exists"].includes(d.dnsRecord.status)
                  ) ? (
                    <div className="mt-3 rounded-lg border border-border bg-muted/40 px-3 py-2.5 text-[12px]">
                      <p className="mb-1.5 text-muted-foreground">
                        Create this DNS record at your DNS provider:
                      </p>
                      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-[11.5px]">
                        <span>{target ? "A" : "CNAME"}</span>
                        <span>{d.hostname}</span>
                        <span className="inline-flex items-center gap-1">
                          {target ?? "public address of the cluster"}
                          {target ? (
                            <CopyButton
                              value={target}
                              label="Copy record value"
                            />
                          ) : null}
                        </span>
                      </div>
                      <p className="mt-1.5 text-muted-foreground">
                        Checked automatically.
                      </p>
                    </div>
                  ) : null}
                </li>
              );
            })}
          </ul>
        ) : (
          <EmptyState
            compact
            icon={GlobeIcon}
            title="No domains"
            description="Reachable inside the cluster only."
            action={
              <Button size="sm" onClick={() => setAdding(true)}>
                Add domain
              </Button>
            }
          />
        )}
      </Panel>
      {app.domains.length && cluster && !cluster.capabilities.certManager ? (
        <Callout tone="info">
          Setting up HTTPS on {cluster.name}. Certificates come with the next
          deploy.
        </Callout>
      ) : null}
      <AddDomainDialog
        app={app}
        open={adding}
        onClose={() => setAdding(false)}
      />
      <PickCertificateDialog
        app={app}
        domain={picking}
        onClose={() => setPicking(null)}
      />
      <CertificateDialog
        open={!!replacing}
        onClose={() => setReplacing(null)}
        replacing={replacing ?? undefined}
      />
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing?.hostname}?`}
        description="Traffic to this hostname stops."
        confirmLabel="Remove domain"
        onConfirm={async () => {
          if (!removing) return;
          // Putting back the old record replaces Pyo's own as well.
          const restore =
            !!removing.dnsRecord?.previous?.length && restoreRecord;
          const res = await api.removeDomain(app.id, removing.id, {
            deleteDnsRecord:
              !restore && !!removing.dnsRecord?.mine && deleteRecord,
            restoreRecord: restore,
          });
          if (res) toast({ title: `Removed ${removing.hostname}` });
        }}
      >
        <RestoreRecordSwitch
          d={removing}
          checked={restoreRecord}
          onChange={setRestoreRecord}
        />
        {restoreRecord && removing?.dnsRecord?.previous?.length ? null : (
          <DeleteRecordSwitch
            d={removing}
            checked={deleteRecord}
            onChange={setDeleteRecord}
          />
        )}
      </ConfirmDialog>
    </div>
  );
}
