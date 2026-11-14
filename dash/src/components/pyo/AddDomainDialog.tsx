import { useState } from "react";
import { Choice, Choices } from "~/components/pyo/Databases";
import {
  CertificateDialog,
  covers,
  expiry,
  TLS_MODES,
} from "~/components/pyo/Certificates";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import {
  Field,
  fieldGroupClass,
  groupInputClass,
  Input,
  Select,
  Switch,
} from "~/components/ui/Field";
import { HOSTNAME } from "~/lib/pyo/format";
import {
  clusterTailnet,
  freeDomainIp,
  freeHostname,
  tailnetHostname,
} from "~/lib/pyo/freeDomain";
import { api, servicesIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, TlsMode } from "~/lib/pyo/types";

/** Your own domain (a DNS record to make), or a free one: nip.io or the tailnet. */
type Kind = "own" | "public" | "tailnet";

/** One DNS label. */
const LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/**
 * Add a domain: your own or a free one, its hostname, the service it routes to (asked when it isn't
 * given, e.g. from the team's Domains page), the port when there's a
 * choice, and how HTTPS works.
 */
export function AddDomainDialog({
  app: fixed,
  open,
  onClose,
}: {
  /** The service it's for; without one, the dialog asks which. */
  app?: Application;
  open: boolean;
  onClose: () => void;
}) {
  const s = usePyo();
  const services = servicesIn(s)
    .map((a) => ({
      a,
      label: `${s.projects.find((p) => p.id === a.projectId)?.name ?? "?"} / ${a.name}`,
      http: a.ports.some((p) => p.protocol === "HTTP"),
    }))
    .sort((x, y) => x.label.localeCompare(y.label));
  const usable = services.filter((x) => x.http);
  const [picked, setPicked] = useState("");
  // One service that can take a domain: no need to ask.
  const app =
    fixed ??
    services.find((x) => x.a.id === picked)?.a ??
    (usable.length === 1 ? usable[0].a : undefined);
  const httpPorts = app?.ports.filter((p) => p.protocol === "HTTP") ?? [];
  const [kind, setKind] = useState<Kind>("own");
  const [ownHost, setOwnHost] = useState("");
  /** The free domain's name, as edited; null: the suggested one. */
  const [label, setLabel] = useState<string | null>(null);
  const [portName, setPortName] = useState("");
  const [touched, setTouched] = useState(false);
  const [tlsMode, setTlsMode] = useState<TlsMode>("auto");
  const [redirect, setRedirect] = useState(true);
  const [certificateId, setCertificateId] = useState("");
  const [uploading, setUploading] = useState(false);
  const port = portName || httpPorts[0]?.name || "";
  const cluster = s.clusters.find((c) => c.id === app?.clusterId);
  const freeIp = app ? freeDomainIp(s, app.clusterId) : undefined;
  const tailnet = app ? clusterTailnet(s, app.clusterId) : undefined;
  const namespace =
    s.projects.find((p) => p.id === app?.projectId)?.namespace ?? "app";
  // A free kind the picked service's cluster doesn't have: back to your own.
  const k: Kind =
    (kind === "public" && !freeIp) || (kind === "tailnet" && !tailnet)
      ? "own"
      : kind;
  const suffix =
    k === "public" ? `${freeIp}.nip.io` : k === "tailnet" ? tailnet : "";
  const suggested =
    !app || k === "own"
      ? ""
      : (k === "public"
          ? freeHostname(app, namespace, freeIp!)
          : tailnetHostname(s, app, namespace, tailnet!)
        ).slice(0, -suffix!.length - 1);
  const name = label ?? suggested;
  const hostname = k === "own" ? ownHost : `${name}.${suffix}`;
  const matching = s.certificates.filter(
    (c) =>
      c.teamId === (app?.teamId ?? s.currentTeamId) &&
      (!hostname || covers(c, hostname)),
  );
  const cert = matching.find((c) => c.id === certificateId) ?? matching[0];
  const taken = s.services.some((a) =>
    a.domains.some((d) => d.hostname === hostname),
  );
  const error =
    k !== "own" && !LABEL.test(name)
      ? "Use lowercase letters, digits and hyphens."
      : !HOSTNAME.test(hostname)
        ? "Enter a valid hostname, e.g. app.example.com."
        : !app
          ? "Pick the service it routes to."
          : taken
            ? "This hostname is already routed to a service."
            : k !== "tailnet" && tlsMode === "custom" && !cert
              ? `Upload a certificate valid for ${hostname}, or pick another HTTPS option.`
              : "";
  const close = () => {
    setKind("own");
    setOwnHost("");
    setLabel(null);
    setPicked("");
    setPortName("");
    setTouched(false);
    onClose();
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      title="Add domain"
      onSubmit={() => {
        setTouched(true);
        if (error || !app) return;
        api.addDomain(
          app.id,
          k === "tailnet"
            ? // Tailscale serves its HTTPS.
              { hostname, portName: port, redirectHttps: true, tlsMode: "auto" }
            : {
                hostname,
                portName: port,
                redirectHttps: tlsMode !== "off" && redirect,
                tlsMode,
                ...(tlsMode === "custom" && cert
                  ? { certificateId: cert.id }
                  : {}),
              },
        );
        toast({
          title: fixed
            ? `Added ${hostname}`
            : `Added ${hostname} to ${app.name}`,
          description: !app.currentDeploymentId
            ? "Live after the first deploy."
            : k !== "own"
              ? undefined
              : "Create the DNS record, then check DNS.",
        });
        close();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" size="sm">
            Add domain
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {fixed ? null : (
          <Field
            label="Service"
            error={
              touched && !app ? "Pick the service it routes to." : undefined
            }
          >
            {(p) => (
              <Select
                {...p}
                value={app?.id ?? ""}
                onChange={(e) => {
                  setPicked(e.target.value);
                  setPortName("");
                }}
                heading="Routes to"
              >
                <option value="" hidden>
                  Pick a service
                </option>
                {services.map((x) => (
                  <option key={x.a.id} value={x.a.id} disabled={!x.http}>
                    {x.label}
                    {x.http ? "" : " (no HTTP port)"}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        )}
        <Choices label="Domain" cols="sm:grid-cols-3">
          <Choice
            label="Your domain"
            detail="Needs a DNS record"
            selected={k === "own"}
            onSelect={() => setKind("own")}
          />
          <Choice
            label={
              cluster?.freeDomainAddress === "tailscale"
                ? "Tailscale IP"
                : "Public"
            }
            detail={
              !app
                ? "Pick a service first"
                : freeIp
                  ? "Free on nip.io"
                  : cluster?.freeDomainAddress === "tailscale"
                    ? "No Tailscale IP"
                    : cluster?.publicTraffic === false
                      ? "Public traffic is off"
                      : "No public IP yet"
            }
            selected={k === "public"}
            disabled={!freeIp}
            onSelect={() => {
              setKind("public");
              setLabel(null);
            }}
          />
          <Choice
            label="Tailnet"
            detail={
              !app
                ? "Pick a service first"
                : tailnet
                  ? "Free on ts.net"
                  : s.tailscale
                    ? "Cluster not on your tailnet"
                    : "Set up Tailscale in Integrations"
            }
            selected={k === "tailnet"}
            disabled={!tailnet}
            onSelect={() => {
              setKind("tailnet");
              setLabel(null);
            }}
          />
        </Choices>
        <Field
          label="Hostname"
          error={touched ? error || undefined : undefined}
          hint={
            k === "public"
              ? `Points at ${freeIp}. No DNS needed.`
              : k === "tailnet"
                ? "Only devices on your tailnet can open it."
                : undefined
          }
        >
          {(p) =>
            k === "own" ? (
              <Input
                {...p}
                mono
                value={ownHost}
                onChange={(e) =>
                  setOwnHost(e.target.value.trim().toLowerCase())
                }
                placeholder="shop.example.com"
              />
            ) : (
              <div className={fieldGroupClass(!!(touched && error))}>
                <input
                  {...p}
                  value={name}
                  onChange={(e) =>
                    setLabel(e.target.value.trim().toLowerCase())
                  }
                  spellCheck={false}
                  autoComplete="off"
                  className={`${groupInputClass} flex-1 pr-0 font-mono text-[12px]`}
                />
                <span className="flex shrink-0 items-center pr-3.5 font-mono text-[12px] text-muted-foreground">
                  .{suffix}
                </span>
              </div>
            )
          }
        </Field>
        {httpPorts.length > 1 ? (
          <Field label="Route to port">
            {(p) => (
              <Select
                {...p}
                value={port}
                onChange={(e) => setPortName(e.target.value)}
              >
                {httpPorts.map((x) => (
                  <option key={x.id} value={x.name}>
                    {x.name} ({x.containerPort})
                  </option>
                ))}
              </Select>
            )}
          </Field>
        ) : null}
        {k === "tailnet" ? null : (
          <Field
            label="HTTPS"
            hint={TLS_MODES.find((m) => m.value === tlsMode)?.hint}
          >
            {(p) => (
              <Select
                {...p}
                value={tlsMode}
                onChange={(e) => setTlsMode(e.target.value as TlsMode)}
              >
                {TLS_MODES.map((m) => (
                  <option key={m.value} value={m.value}>
                    {m.label}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        )}
        {k !== "tailnet" && tlsMode === "custom" ? (
          <Field
            label={
              <>
                Certificate
                <button
                  type="button"
                  onClick={() => setUploading(true)}
                  className="ml-auto text-[12px] font-medium text-primary hover:text-primary/80"
                >
                  Upload new
                </button>
              </>
            }
            hint={
              cert
                ? `${cert.hostnames.join(", ")} · ${expiry(cert).text}`
                : hostname
                  ? `No certificate covers ${hostname} yet.`
                  : undefined
            }
          >
            {(p) => (
              <Select
                {...p}
                value={cert?.id ?? ""}
                onChange={(e) => setCertificateId(e.target.value)}
                disabled={!matching.length}
              >
                {matching.length ? (
                  matching.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))
                ) : (
                  <option value="">No matching certificate</option>
                )}
              </Select>
            )}
          </Field>
        ) : null}
        {k !== "tailnet" && tlsMode !== "off" ? (
          <Switch
            checked={redirect}
            onChange={setRedirect}
            label="Redirect HTTP to HTTPS"
          />
        ) : null}
      </div>
      <CertificateDialog
        open={uploading}
        onClose={() => setUploading(false)}
        forHost={hostname || undefined}
        onSaved={setCertificateId}
      />
    </Dialog>
  );
}
