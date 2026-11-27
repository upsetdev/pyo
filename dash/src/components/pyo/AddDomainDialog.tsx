import { useEffect, useRef, useState } from "react";
import { Choice, Choices } from "~/components/pyo/Databases";
import {
  CertificateDialog,
  covers,
  expiry,
  TLS_MODES,
} from "~/components/pyo/Certificates";
import {
  CloudflareFields,
  useCloudflareFor,
  useTunnelChoice,
  type CloudflareChoice,
} from "~/components/pyo/Cloudflare";
import { parseTarget } from "~/components/pyo/DomainRules";
import { SetUpTailscaleLink } from "~/components/pyo/Tailscale";
import { AppLink } from "~/components/ui/AppLink";
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
import { isPath, type HostEntry } from "~/lib/pyo/hosts";
import {
  clusterTailnet,
  freeDomainNoun,
  freeSuffix,
  hostnameUsed,
  unusedHostname,
} from "~/lib/pyo/freeDomain";
import { api, clusterPaths, servicesIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, TlsMode } from "~/lib/pyo/types";

/**
 * Your own domain (a DNS record to make), a free one on nip.io at the
 * cluster's public IP, or a private one on the tailnet (Tailscale).
 */
type Kind = "own" | "public" | "tailnet";

/** One DNS label. */
const LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/**
 * Add a domain: your own, a free one (a tunnel one in tunnel mode) or a Tailscale one, its hostname, the service it routes to (asked when it isn't
 * given, e.g. from the team's Domains page), the port when there's a
 * choice, and how HTTPS works.
 *
 * With a hostname of the team, it adds a path of it. With a route, it
 * edits that route's path, strip prefix, port and, for a path, its service.
 */
export function AddDomainDialog({
  app: fixed,
  open,
  onClose,
  hostname: lockedHost,
  editing,
}: {
  /** The service it's for; without one, the dialog asks which. */
  app?: Application;
  open: boolean;
  onClose: () => void;
  /** Add a path to this hostname: it's fixed, and the cursor starts on Path. */
  hostname?: string;
  /** Edit this route instead. Its hostname is fixed. */
  editing?: HostEntry | null;
}) {
  const s = usePyo();
  const fixedHost = editing?.d.hostname ?? lockedHost;
  const locked = !!fixedHost;
  // A hostname's paths are on one cluster.
  const hostCluster = locked
    ? s.services.find((a) =>
        a.domains.some(
          (d) => d.hostname.toLowerCase() === fixedHost.toLowerCase(),
        ),
      )?.clusterId
    : undefined;
  const services = servicesIn(s)
    .map((a) => ({
      a,
      label: `${s.projects.find((p) => p.id === a.projectId)?.name ?? "?"} / ${a.name}`,
      http: a.ports.some((p) => p.protocol === "HTTP"),
      elsewhere: !!hostCluster && a.clusterId !== hostCluster,
    }))
    .sort((x, y) => x.label.localeCompare(y.label));
  const usable = services.filter((x) => x.http && !x.elsewhere);
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
  /** Only redirect it (www to the apex): where to, else "". */
  const [redirectOnly, setRedirectOnly] = useState(false);
  const [redirectTo, setRedirectTo] = useState("");
  /** Only this path of the hostname, as typed. */
  const [pathInput, setPathInput] = useState("");
  const [strip, setStrip] = useState(false);
  const [certificateId, setCertificateId] = useState("");
  const [uploading, setUploading] = useState(false);
  const [cf, setCf] = useState<CloudflareChoice>({
    create: true,
    proxied: false,
  });
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState("");
  /** The route the fields were filled from. */
  const [loaded, setLoaded] = useState<string | null>(null);
  if (open && editing && loaded !== editing.d.id) {
    setLoaded(editing.d.id);
    setPicked(editing.a.id);
    setPortName(editing.d.portName);
    setPathInput(editing.d.path && editing.d.path !== "/" ? editing.d.path : "");
    setStrip(!!editing.d.stripPrefix);
    setTouched(false);
    setSaveError("");
  }
  const pathRef = useRef<HTMLInputElement>(null);
  // Adding a path: the hostname is known, so the cursor goes to Path.
  useEffect(() => {
    if (open && lockedHost && !editing) pathRef.current?.focus();
  }, [open, lockedHost, editing]);
  const port = portName || httpPorts[0]?.name || "";
  const cluster = s.clusters.find((c) => c.id === app?.clusterId);
  // Free domains go on the public IP, whatever the cluster's free domains
  // use by default: traffic goes straight to its servers. In tunnel mode,
  // under the tunnel's default domain.
  const tunnelMode = cluster?.traffic === "tunnel";
  // The cluster's Tailnet switch is off: no new tailnet domains.
  const tailnetOff = !!cluster && !clusterPaths(cluster).tailnet;
  // Before a service is picked: by the clusters it could be on.
  const freeNoun = freeDomainNoun(
    cluster
      ? [cluster]
      : [...new Set(usable.map((x) => x.a.clusterId))].map((id) =>
          s.clusters.find((c) => c.id === id),
        ),
  );
  const freeIp = cluster?.publicIp ?? undefined;
  const freeEnd = app ? freeSuffix(s, app.clusterId) : undefined;
  const tailnet = app ? clusterTailnet(s, app.clusterId) : undefined;
  const namespace =
    s.projects.find((p) => p.id === app?.projectId)?.namespace ?? "app";
  // Picked, but the service's cluster has none: a note says why.
  const missing =
    !locked &&
    !!app &&
    ((kind === "public" && !freeEnd) || (kind === "tailnet" && !tailnet));
  // No service yet: only your own domain, the free ones are named after it.
  const k: Kind = app && !locked ? kind : "own";
  const suffix =
    k === "public" ? freeEnd : k === "tailnet" ? tailnet : "";
  const suggested =
    !app || k === "own" || missing
      ? ""
      : // The first name nothing uses yet ("web-2" when "web" is).
        unusedHostname(
          s,
          k === "public" ? `${app.name}-${namespace}` : app.name,
          suffix!,
        ).slice(0, -suffix!.length - 1);
  const name = label ?? suggested;
  const hostname = locked
    ? fixedHost
    : k === "own"
      ? ownHost
      : `${name}.${suffix}`;
  const inCloudflare = useCloudflareFor(
    k === "own" && !locked ? hostname : "",
    app?.clusterId,
  );
  // Through the cluster's Cloudflare Tunnel instead of a direct record.
  const viaTunnel = useTunnelChoice(
    k === "own" && !locked && HOSTNAME.test(hostname) ? hostname : "",
    app?.clusterId,
    cf,
  ).on;
  const matching = s.certificates.filter(
    (c) =>
      c.teamId === (app?.teamId ?? s.currentTeamId) &&
      (!hostname || covers(c, hostname)),
  );
  const cert = matching.find((c) => c.id === certificateId) ?? matching[0];
  const path = (() => {
    const p = pathInput.trim().replace(/\/+$/, "");
    return p === "" ? "/" : p;
  })();
  const pathOk = path === "/" || /^\/[A-Za-z0-9\-._~!$&'()+,;=:@%/]*$/.test(path) && !path.includes("//");
  const routes = !(k === "own" && redirectOnly);
  const taken = locked
    ? // This exact path, on another route.
      s.services.some((a) =>
        a.domains.some(
          (d) =>
            d.id !== editing?.d.id &&
            d.hostname.toLowerCase() === hostname.toLowerCase() &&
            (d.path || "/") === path,
        ),
      )
    : hostnameUsed(s, hostname, routes ? path : undefined);
  // Another of the team's services has the hostname: this is a path of it,
  // with that one's DNS record and certificate.
  const held = locked || (path !== "/" && hostnameUsed(s, hostname));
  const error = missing
    ? "Pick another option."
    : k !== "own" && !LABEL.test(name)
      ? "Use lowercase letters, digits and hyphens."
      : !HOSTNAME.test(hostname)
        ? "Enter a valid hostname, e.g. app.example.com."
        : !app
          ? "Pick the service it routes to."
          : routes && !pathOk
            ? "Enter a path like /api."
            : taken
            ? `${path === "/" || !routes ? hostname : hostname + path} is already used.`
            : k === "own" && redirectOnly && !parseTarget(redirectTo).toHost
              ? "Enter where it redirects to, e.g. example.com."
            : k !== "tailnet" && !held && tlsMode === "custom" && !cert
              ? `Upload a certificate valid for ${hostname}, or pick another HTTPS option.`
              : "";
  const close = () => {
    setKind("own");
    setOwnHost("");
    setLabel(null);
    setPicked("");
    setPortName("");
    setTouched(false);
    setCf({ create: true, proxied: false });
    setRedirectOnly(false);
    setRedirectTo("");
    setPathInput("");
    setStrip(false);
    setLoaded(null);
    setSaving(false);
    setSaveError("");
    onClose();
  };
  /** Save the edited route, or move it to the picked service. */
  const saveEdit = async (e: HostEntry, to: Application) => {
    const route = {
      path,
      stripPrefix: path !== "/" && strip,
      portName: port,
    };
    const was = e.d.path || "/";
    if (
      to.id === e.a.id &&
      was === route.path &&
      !!e.d.stripPrefix === route.stripPrefix &&
      e.d.portName === route.portName
    )
      return close();
    const shown = path === "/" ? hostname : hostname + path;
    setSaving(true);
    setSaveError("");
    if (to.id === e.a.id) {
      const why = await api.saveRoute(to.id, e.d.id, route);
      setSaving(false);
      if (why) return setSaveError(why);
      toast({ title: `Saved ${shown}` });
      return close();
    }
    const r = await api.moveRoute(e.a.id, e.d.id, to.id, route);
    setSaving(false);
    if (r && !r.partial) return setSaveError(r.error);
    toast(
      r
        ? {
            tone: "error",
            title: "The move didn't finish",
            description: r.error,
            timeout: null,
          }
        : { title: `Moved ${shown} to ${to.name}` },
    );
    close();
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      title={editing ? "Edit route" : lockedHost ? "Add path" : "Add domain"}
      onSubmit={() => {
        setTouched(true);
        if (error || !app || saving) return;
        if (editing) return void saveEdit(editing, app);
        const only = k === "own" && redirectOnly;
        const redirects = only
          ? [
              {
                path: "/",
                ...parseTarget(redirectTo),
                status: 301 as const,
                keepPath: true,
              },
            ]
          : undefined;
        void api.addDomain(
          app.id,
          k === "tailnet"
            ? // Tailscale serves its HTTPS.
              {
                hostname,
                portName: port,
                ...(path !== "/"
                  ? { path, ...(strip ? { stripPrefix: true } : {}) }
                  : {}),
                redirectHttps: true,
                tlsMode: "auto",
              }
            : {
                hostname,
                portName: only ? "" : port,
                redirects,
                ...(routes && path !== "/"
                  ? { path, ...(strip ? { stripPrefix: true } : {}) }
                  : {}),
                redirectHttps: tlsMode !== "off" && redirect,
                tlsMode,
                ...(tlsMode === "custom" && cert
                  ? { certificateId: cert.id }
                  : {}),
              },
          k !== "own" || held
            ? undefined
            : viaTunnel
              ? { tunnel: true }
              : inCloudflare && cf.create
                ? { proxied: cf.proxied }
                : undefined,
        );
        const added = routes && path !== "/" ? hostname + path : hostname;
        toast({
          title: fixed
            ? `Added ${added}`
            : `Added ${added} to ${app.name}`,
          description: !app.currentDeploymentId
            ? "Live after the first deploy."
            : k !== "own" || held || viaTunnel || (inCloudflare && cf.create)
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
          <Button type="submit" size="sm" disabled={missing || saving}>
            {editing ? "Save" : lockedHost ? "Add path" : "Add domain"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {/* The route that holds its hostname stays on its service. */}
        {fixed || (editing && !isPath(editing.d)) ? null : (
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
                  <option
                    key={x.a.id}
                    value={x.a.id}
                    disabled={!x.http || x.elsewhere}
                  >
                    {x.label}
                    {!x.http
                      ? " (no HTTP port)"
                      : x.elsewhere
                        ? " (another cluster)"
                        : ""}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        )}
        {locked ? null : (
          <Choices label="Domain" cols="sm:grid-cols-3">
            <Choice
              label="Your domain"
              detail="Needs a DNS record"
              selected={k === "own"}
              onSelect={() => setKind("own")}
            />
            <Choice
              label={freeNoun}
              detail={
                !app
                  ? "Pick a service first"
                  : tunnelMode
                    ? "Ready instantly"
                    : "Public, on nip.io"
              }
              selected={k === "public"}
              disabled={!app}
              onSelect={() => {
                setKind("public");
                setLabel(null);
              }}
            />
            <Choice
              label="Tailscale"
              detail={
                !app
                  ? "Pick a service first"
                  : tailnetOff
                    ? `Tailnet is off on ${cluster?.name}.`
                    : "Private, on ts.net"
              }
              selected={k === "tailnet"}
              disabled={!app || tailnetOff}
              onSelect={() => {
                setKind("tailnet");
                setLabel(null);
              }}
            />
          </Choices>
        )}
        {missing && cluster ? (
          <p className="rounded-lg border border-border bg-muted/40 px-3.5 py-3 text-[12.5px] text-muted-foreground">
            {k === "tailnet" ? (
              cluster.tailscale ||
              (s.tailscale && s.tailscale.clusterId === cluster.id) ? (
                `${cluster.name} is still joining your tailnet.`
              ) : (
                <>
                  Tailscale isn't set up on {cluster.name}.{" "}
                  <SetUpTailscaleLink clusterId={cluster.id} />
                </>
              )
            ) : tunnelMode ? (
              <>
                Pick a default domain first.{" "}
                <AppLink
                  href={`/clusters/${cluster.id}/settings`}
                  className="font-medium text-primary hover:text-primary/80"
                >
                  Cluster settings
                </AppLink>
              </>
            ) : cluster.publicTraffic === false ? (
              <>
                Public traffic is off on {cluster.name}.{" "}
                <AppLink
                  href={`/clusters/${cluster.id}/settings`}
                  className="font-medium text-primary hover:text-primary/80"
                >
                  Cluster settings
                </AppLink>
              </>
            ) : (
              `No public IP found on ${cluster.name} yet.`
            )}
          </p>
        ) : null}
        {missing ? null : (
          <>
            <Field
              label="Hostname"
              error={touched ? error || undefined : undefined}
              hint={
                k === "public"
                  ? tunnelMode
                    ? "Through Cloudflare Tunnel. No DNS needed."
                    : cluster?.behindNat
                    ? `Points at ${freeIp}. Forward ports 80 and 443 to the server.`
                    : `Points at ${freeIp}. No DNS needed.`
                  : k === "tailnet"
                    ? "Only devices on your tailnet can open it."
                    : undefined
              }
            >
              {(p) =>
                locked ? (
                  <Input {...p} mono value={hostname} disabled />
                ) : k === "own" ? (
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
            {routes ? (
              <Field
                label="Path"
                optional
                hint={
                  held && !editing && k !== "tailnet"
                    ? `${hostname} keeps its DNS record and certificate.`
                    : "Only this path. Other services can serve the rest."
                }
              >
                {(p) => (
                  <Input
                    {...p}
                    ref={pathRef}
                    mono
                    value={pathInput}
                    onChange={(e) => setPathInput(e.target.value.trim())}
                    placeholder="/"
                  />
                )}
              </Field>
            ) : null}
            {routes && path !== "/" ? (
              <Switch
                checked={strip}
                onChange={setStrip}
                label="Strip prefix"
                description={`Send ${path}/x as /x.`}
              />
            ) : null}
            {k === "own" && !held ? (
              <CloudflareFields
                hostname={HOSTNAME.test(hostname) ? hostname : ""}
                clusterId={app?.clusterId}
                value={cf}
                onChange={setCf}
                tunnel
              />
            ) : null}
            {k === "own" && !locked ? (
              <Switch
                checked={redirectOnly}
                onChange={setRedirectOnly}
                label="Only redirect"
                description="Like www to your main domain."
              />
            ) : null}
            {k === "own" && redirectOnly ? (
              <Field
                label="Redirect to"
                hint="The path and query are kept."
              >
                {(p) => (
                  <Input
                    {...p}
                    mono
                    value={redirectTo}
                    onChange={(e) => setRedirectTo(e.target.value)}
                    placeholder="example.com"
                  />
                )}
              </Field>
            ) : null}
            {httpPorts.length > 1 && !(k === "own" && redirectOnly) ? (
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
            {k === "tailnet" || held ? null : (
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
            {k !== "tailnet" && !held && tlsMode === "custom" ? (
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
            {k !== "tailnet" && !held && tlsMode !== "off" ? (
              <Switch
                checked={redirect}
                onChange={setRedirect}
                label="Redirect HTTP to HTTPS"
              />
            ) : null}
            {saveError ? (
              <p className="text-[12px] text-destructive">{saveError}</p>
            ) : null}
          </>
        )}
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
