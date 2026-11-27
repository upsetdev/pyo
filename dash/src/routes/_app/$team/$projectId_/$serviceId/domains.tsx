import {
  ArrowBendDownRightIcon,
  ArrowCounterClockwiseIcon,
  ArrowRightIcon,
  ArrowsClockwiseIcon,
  ArrowUUpRightIcon,
  DotsThreeIcon,
  ListBulletsIcon,
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
} from "~/components/pyo/Certificates";
import { Tooltip } from "~/components/ui/Tooltip";
import { DomainKind, DomainLock, DomainMark, DomainNote } from "~/components/pyo/DomainState";
import { AppLink } from "~/components/ui/AppLink";
import { AddDomainDialog } from "~/components/pyo/AddDomainDialog";
import { hostOf, isPath, sharedWith, shownPath } from "~/lib/pyo/hosts";
import {
  HeadersDialog,
  onlyRedirects,
  redirectTarget,
  RedirectsDialog,
  RulesLines,
} from "~/components/pyo/DomainRules";
import {
  DeleteRecordSwitch,
  DnsRecordLine,
} from "~/components/pyo/Cloudflare";
import {
  canSwitchBack,
  onTunnel,
  RestoreRecordSwitch,
  SwitchBackDialog,
} from "~/components/pyo/TunnelRecord";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Callout, EmptyState, Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { api, clusterTraffic, roleIn, usePyo, serviceIdOf } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, Certificate, Domain, TrafficPath } from "~/lib/pyo/types";

/** Why a domain isn't served: its path is off on its cluster. */
const NOT_SERVED: Record<TrafficPath, string> = {
  public: "Public ports are",
  tunnel: "Cloudflare Tunnel is",
  tailnet: "Tailnet is",
};

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
  const [redirecting, setRedirecting] = useState<Domain | null>(null);
  const [heading, setHeading] = useState<Domain | null>(null);
  const [switchingBack, setSwitchingBack] = useState<Domain | null>(null);
  const role = roleIn(s);
  const admin = role === "owner" || role === "admin";
  // Its rules in Pyo Gateway, on every domain's menu.
  const rules = (d: Domain) => [
    {
      label: "Redirects",
      icon: <ArrowUUpRightIcon size={14} />,
      onSelect: () => setRedirecting(d),
    },
    {
      label: "Headers",
      icon: <ListBulletsIcon size={14} />,
      onSelect: () => setHeading(d),
    },
  ];
  const certOf = (d: Domain) =>
    d.tlsMode === "custom"
      ? s.certificates.find((c) => c.id === d.certificateId)
      : undefined;
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  const target = cluster?.publicIp ?? undefined;
  // In tunnel mode no direct record helps: the tunnel line says what to do.
  const tunnelMode = !!cluster && clusterTraffic(cluster) === "tunnel";
  const httpPorts = app.ports.filter((p) => p.protocol === "HTTP");
  // The hostname's only entry: its DNS record can go with it. Otherwise
  // another path takes the hostname over, with its record.
  const last =
    !removing || (hostOf(s, removing.hostname)?.entries.length ?? 1) <= 1;
  const keepsRecord =
    !last &&
    (!!removing?.dnsRecord?.mine || !!removing?.dnsRecord?.previous?.length);

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
                    <span className="inline-flex min-w-0 items-center gap-2">
                      <DomainLock
                        d={d}
                        certs={s.certificates}
                        notDeployed={!app.currentDeploymentId}
                      />
                      <span className="truncate font-mono text-[13px]">
                        {d.hostname}
                        {d.path && d.path !== "/" ? (
                          <span className="text-muted-foreground">{d.path}</span>
                        ) : null}
                      </span>
                      <DomainNote
                        d={d}
                        certs={s.certificates}
                        notDeployed={!app.currentDeploymentId}
                      />
                      <DomainMark d={d} clusterId={app.clusterId} />
                    </span>
                    {sharedWith(s, app, d).length ? (
                      <Tooltip
                        content={`Also on ${sharedWith(s, app, d)
                          .map((x) => x.name)
                          .join(", ")}`}
                      >
                        <span className="rounded border border-border px-1.5 text-[11px] text-muted-foreground">
                          Shared
                        </span>
                      </Tooltip>
                    ) : null}
                    <DomainKind hostname={d.hostname} />
                    {d.served === false && cluster ? (
                      <span className="text-[12px] text-warning">
                        Not served. {NOT_SERVED[d.trafficPath ?? "public"]} off on {cluster.name}.
                      </span>
                    ) : null}
                    <span className="ml-2 inline-flex items-center gap-1.5 text-[12px] text-muted-foreground">
                      {onlyRedirects(d) ? (
                        <>
                          <ArrowUUpRightIcon size={12} aria-label="redirects to" />
                          <span className="font-mono">
                            {redirectTarget(
                              d.redirects!.find((r) => r.path === "/") ??
                                d.redirects![0],
                            )}
                          </span>
                        </>
                      ) : (
                        <>
                          <ArrowRightIcon size={12} aria-label="routes to" />
                          <span className="font-mono">{d.portName}</span>
                          {d.stripPrefix ? <span>without {d.path}</span> : null}
                        </>
                      )}
                    </span>
                    <span className="ml-auto flex gap-1">
                      <Menu
                        label={`Settings of ${d.hostname}`}
                        items={tailnet || isPath(d) ? [
                          // A path follows the entry that holds its
                          // hostname: only its own header rules here.
                          ...(isPath(d) ? rules(d).slice(1) : rules(d)),
                          { type: "separator" as const },
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
                          ...(canSwitchBack(d, cluster, admin)
                            ? [
                                {
                                  label: "Switch back from Tunnel",
                                  icon: <ArrowCounterClockwiseIcon size={14} />,
                                  onSelect: () => setSwitchingBack(d),
                                },
                              ]
                            : []),
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
                          { type: "separator" as const },
                          ...rules(d),
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
                  <RulesLines d={d} />
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
                  {isPath(d) ? null : <DnsRecordLine app={app} d={d} noSwitchBack />}
                  {!isPath(d) &&
                  d.status !== "active" &&
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
      <RedirectsDialog
        app={app}
        domain={redirecting}
        onClose={() => setRedirecting(null)}
      />
      <HeadersDialog
        app={app}
        domain={heading}
        onClose={() => setHeading(null)}
      />
      <PickCertificateDialog
        app={app}
        domain={picking}
        onClose={() => setPicking(null)}
      />
      <SwitchBackDialog
        app={app}
        d={switchingBack}
        open={!!switchingBack}
        onClose={() => setSwitchingBack(null)}
      />
      <CertificateDialog
        open={!!replacing}
        onClose={() => setReplacing(null)}
        replacing={replacing ?? undefined}
      />
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing ? shownPath(removing) : ""}?`}
        description={
          last
            ? "Traffic to this hostname stops."
            : `It stops going to ${app.name}.${keepsRecord ? " Other paths still use this hostname, so its DNS record stays." : ""}`
        }
        confirmLabel="Remove domain"
        onConfirm={async () => {
          if (!removing) return;
          // Putting back the old record replaces Pyo's own as well.
          const restore =
            last && !!removing.dnsRecord?.previous?.length && restoreRecord;
          const res = await api.removeDomain(app.id, removing.id, {
            deleteDnsRecord:
              last && !restore && !!removing.dnsRecord?.mine && deleteRecord,
            restoreRecord: restore,
          });
          if (res) toast({ title: `Removed ${shownPath(removing)}` });
        }}
      >
        {last ? (
          <>
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
          </>
        ) : null}
      </ConfirmDialog>
    </div>
  );
}
