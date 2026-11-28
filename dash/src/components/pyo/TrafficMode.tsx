import { useState, type ReactNode } from "react";
import { teamTailscale } from "~/components/pyo/Tailscale";
import { TunnelSettings, tunnelUnavailable } from "~/components/pyo/Tunnel";
import { AppLink } from "~/components/ui/AppLink";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { LearnMore } from "~/components/ui/LearnMore";
import { optionClass, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { plural } from "~/lib/pyo/format";
import { agentBlock } from "~/lib/pyo/gateway";
import {
  api,
  clusterPaths,
  domainPath,
  listedServices,
  roleIn,
  tunnelZoneFor,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster, PathDomain, TrafficMode, TrafficPath } from "~/lib/pyo/types";

// How a cluster routes requests. Replicas are placed the same way in every
// mode (evenly across regions, then servers); this only changes where each
// request goes.

const MODES: { value: TrafficMode; label: string; hint: string }[] = [
  { value: "local", label: "Local", hint: "Requests stay on the same server." },
  { value: "region", label: "Regional", hint: "Requests stay in the same region." },
  { value: "spread", label: "Global", hint: "Requests go to any server." },
];

/** One choice of a radio group panel: a label and a hint line. */
function RadioOption({
  name,
  checked,
  disabled,
  label,
  hint,
  onSelect,
}: {
  name: string;
  checked: boolean;
  disabled: boolean;
  label: string;
  hint: ReactNode;
  onSelect: () => void;
}) {
  return (
    <label
      className={`${optionClass({ selected: checked })} flex gap-3 px-4 py-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20 ${disabled ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
    >
      <input
        type="radio"
        name={name}
        className="mt-0.5"
        checked={checked}
        disabled={disabled}
        onChange={onSelect}
      />
      <span>
        <span className="block text-[13px] font-medium">{label}</span>
        <span className="block text-[12px] leading-5 text-muted-foreground">{hint}</span>
      </span>
    </label>
  );
}

export function ClusterTrafficMode({ cluster: c }: { cluster: Cluster }) {
  const capable = !!c.capabilities.trafficModes;
  const current = c.trafficMode ?? "region";
  // The API checks the team role (owners and admins).
  const off = (m: TrafficMode) => !capable || (m === "local" && !c.capabilities.trafficLocal);
  return (
    <Panel title="Traffic routing" description={!capable ? "Not reported yet." : undefined}>
      <div
        role="radiogroup"
        aria-label="How traffic is routed"
        className="space-y-2 p-4"
      >
        {MODES.map((m) => (
          <RadioOption
            key={m.value}
            name={`traffic-${c.id}`}
            checked={current === m.value}
            disabled={off(m.value)}
            label={m.label}
            hint={capable && off(m.value) ? "Not reported yet." : m.hint}
            onSelect={async () => {
              const res = await api.setClusterTrafficMode(c.id, m.value);
              if (res?.ok) toast({ title: m.label, description: c.name });
            }}
          />
        ))}
      </div>
    </Panel>
  );
}

const LABEL: Record<TrafficPath, string> = {
  public: "Public ports",
  tunnel: "Cloudflare Tunnel",
  tailnet: "Tailscale",
};

const ONE_PATH = "Turn on another path first. A cluster needs one.";

/** "a, b and 2 more": at most 10 names. */
function names(xs: string[]) {
  const shown = xs.slice(0, 10);
  const more = xs.length - shown.length;
  if (more > 0) return `${shown.join(", ")} and ${more} more`;
  return shown.length < 2 ? shown.join("") : `${shown.slice(0, -1).join(", ")} and ${shown.at(-1)}`;
}

/** A change waiting for its confirm. */
type Pending =
  | { kind: "public" | "tunnel" | "tailnet"; domains: PathDomain[] }
  | { kind: "proxy"; on: boolean }
  | { kind: "tailnetOn" }
  | { kind: "publicOn" }
  | { kind: "ports" };

/** One path's row: its switch, hint and what's under it. */
function PathRow({
  label,
  hint,
  on,
  disabled,
  onChange,
  children,
}: {
  label: string;
  hint: ReactNode;
  on: boolean;
  disabled: boolean;
  onChange: (on: boolean) => void;
  children?: ReactNode;
}) {
  return (
    <div className="space-y-3 border-t border-border px-4 py-3 first:border-t-0">
      <Switch checked={on} disabled={disabled} onChange={onChange} label={label} description={hint} />
      {children ? <div className="space-y-3 pl-6">{children}</div> : null}
    </div>
  );
}

/**
 * The cluster's traffic paths: Public ports, Cloudflare Tunnel and
 * Tailscale, each on or off. Each domain takes one of the paths that are on.
 */
export function ClusterTraffic({ cluster: c }: { cluster: Cluster }) {
  const s = usePyo();
  const admin = ["owner", "admin"].includes(roleIn(s) ?? "");
  const paths = clusterPaths(c);
  // Cloudflare Tunnel waits for its settings and Turn on.
  const [opening, setOpening] = useState(false);
  const [pending, setPending] = useState<Pending | null>(null);
  const [move, setMove] = useState(true);
  const [ports, setPorts] = useState(true);
  const [restore, setRestore] = useState(true);
  const unavailable = paths.tunnel ? null : tunnelUnavailable(s, c);
  const tailscale = !!teamTailscale(s)?.clusters.some((x) => x.id === c.id) || !!c.tailnet || !!c.tailscale;
  const services = listedServices(s).filter((a) => a.clusterId === c.id);
  const hasPorts = services.some((a) => a.ports.some((p) => p.exposure === "loadbalancer"));
  const domains: (PathDomain & { kept: boolean })[] = services.flatMap((a) =>
    a.domains.map((d) => ({
      // Switch back puts this record back.
      kept: !!d.dnsRecord?.previous?.length,
      serviceId: a.id,
      service: a.name,
      domainId: d.id,
      hostname: d.hostname,
      path: domainPath(d, c),
      outcome: "stops" as const,
    })),
  );
  const onPath = (p: TrafficPath) => domains.filter((d) => d.path === p);
  // The tailnet listener changes on the cluster Pyo runs on, until its
  // agent and install carry the policy on Pyo's own tailnet routes: its own
  // dashboard there drops once (traffic_paths.rs `dashboard_drops`).
  const dashboardDrops =
    !!c.sameCluster && !!agentBlock(c) && domains.some((d) => d.hostname.toLowerCase().endsWith(".ts.net"));
  // ServiceLB moves them between two Services: measured 1 to 5 seconds.
  const PORTS_PAUSE = "TCP and UDP ports pause for a few seconds. Their open connections drop.";
  const DASHBOARD_DROPS = "Open connections to Pyo over Tailscale drop once. The dashboard reconnects.";
  const movable = (host: string) => !!tunnelZoneFor(s, host, c);
  const removing = !!c.tunnel?.removeWhenBack || c.tunnel?.state === "removing";
  const switching = services.flatMap((a) => a.domains).filter((d) => d.tunnel === "switching");
  const conflicts = switching.filter((d) => d.dnsRecord?.status === "conflict");

  const send = async (args: Parameters<typeof api.setClusterPaths>[1], title: string) => {
    const r = await api.setClusterPaths(c.id, args);
    if (!r?.ok) return;
    // Out of date here: the cluster's list, from the API.
    if (r.status === "confirm") {
      const kind = args.public === false ? "public" : args.tunnel === false ? "tunnel" : "tailnet";
      setPending({ kind, domains: r.domains });
      return;
    }
    toast({ title, description: c.name });
  };

  const toggle = (p: TrafficPath, on: boolean) => {
    if (!on && Object.entries(paths).filter(([k, v]) => v && k !== p).length === 0) {
      toast({ tone: "error", title: ONE_PATH });
      return;
    }
    if (on) {
      if (p === "tunnel") return setOpening(true);
      if (p === "tailnet" && dashboardDrops) return setPending({ kind: "tailnetOn" });
      // Published TCP and UDP ports move to the websites' Service.
      if (p === "public" && hasPorts && c.publicPorts) return setPending({ kind: "publicOn" });
      return void send({ [p]: true }, `${LABEL[p]} on`);
    }
    // Off: the confirm lists what it touches.
    // Back on public ports takes a kept record or the cluster's public IP.
    const list: PathDomain[] = onPath(p).map(({ kept, ...d }) => ({
      ...d,
      outcome:
        (p === "public" && paths.tunnel && movable(d.hostname)) ||
        (p === "tunnel" && paths.public && !domainUnderDefault(d.hostname, c) && (!!c.publicIp || kept))
          ? ("moves" as const)
          : ("stops" as const),
    }));
    if (p === "public" || list.length) {
      setMove(true);
      setPorts(true);
      setRestore(true);
      setPending({ kind: p, domains: list });
      return;
    }
    void send({ [p]: false }, `${LABEL[p]} off`);
  };

  const go = () => {
    if (!pending) return;
    if (pending.kind === "proxy")
      return void send({ proxyProtocol: pending.on }, pending.on ? "PROXY protocol on" : "PROXY protocol off");
    if (pending.kind === "ports") return void send({ publicPorts: false }, "TCP and UDP ports closed");
    if (pending.kind === "tailnetOn") return void send({ tailnet: true }, "Tailscale on");
    if (pending.kind === "publicOn") return void send({ public: true }, "Public ports on");
    const args =
      pending.kind === "public"
        ? { public: false, moveToTunnel: move, ...(hasPorts ? { publicPorts: ports } : {}) }
        : pending.kind === "tunnel"
          ? { tunnel: false, restoreRecords: restore }
          : { tailnet: false };
    void send({ ...args, confirm: true }, `${LABEL[pending.kind]} off`);
  };

  const listed = pending && "domains" in pending ? pending.domains : [];
  const moving = listed.filter((d) => d.outcome === "moves");
  const stopping = listed.filter((d) => d.outcome === "stops");
  const dialog = (() => {
    if (!pending) return null;
    switch (pending.kind) {
      case "public":
        return {
          title: `Close public ports on ${c.name}?`,
          confirm: "Close public ports",
          lines: [
            moving.length === 1
              ? "1 domain is in your Cloudflare account."
              : moving.length
                ? `${moving.length} domains are in your Cloudflare account.`
                : null,
            moving.length && move ? "Open connections to them drop once as they move." : null,
            stopping.length || (!move && moving.length)
              ? `These stop working from the internet: ${names((move ? stopping : listed).map((d) => d.hostname))}.`
              : null,
            c.sameCluster ? "The public dashboard closes. Open Pyo through Tailscale or your local network." : null,
            hasPorts ? (ports ? PORTS_PAUSE : "TCP and UDP ports close.") : null,
          ],
        };
      case "publicOn":
        return {
          title: `Open public ports on ${c.name}?`,
          confirm: "Open public ports",
          lines: [PORTS_PAUSE],
        };
      case "tunnel":
        return {
          title: `Turn off Cloudflare Tunnel on ${c.name}?`,
          confirm: "Turn off",
          lines: [
            moving.length === 1
              ? "1 domain switches back to public ports."
              : moving.length
                ? `${moving.length} domains switch back to public ports.`
                : null,
            moving.length ? "Open connections to each of them drop once." : null,
            stopping.length ? `These stop working: ${names(stopping.map((d) => d.hostname))}.` : null,
            "Pyo deletes the tunnel in Cloudflare.",
          ],
        };
      case "tailnet":
        return {
          title: `Turn off Tailscale on ${c.name}?`,
          confirm: "Turn off",
          lines: [
            stopping.length ? `These stop working: ${names(stopping.map((d) => d.hostname))}.` : null,
            dashboardDrops ? DASHBOARD_DROPS : null,
            "Servers stay connected to Tailscale.",
          ],
        };
      case "tailnetOn":
        return {
          title: `Turn on Tailscale on ${c.name}?`,
          confirm: "Turn on",
          lines: [DASHBOARD_DROPS],
        };
      case "proxy":
        return {
          title: `Turn ${pending.on ? "on" : "off"} PROXY protocol on ${c.name}?`,
          confirm: "Save",
          lines: [
            `Open connections to every website on ${c.name} drop once. WebSockets and uploads in progress are cut.`,
          ],
        };
      case "ports":
        return {
          title: `Close TCP and UDP ports on ${c.name}?`,
          confirm: "Close ports",
          lines: ["Their open connections drop."],
        };
    }
  })();

  const publicHint = (() => {
    if (paths.public && c.closingPublic)
      return conflicts.length
        ? `Waiting for Replace and switch on ${names(conflicts.map((d) => d.hostname))}. Public ports close after the last domain moves.`
        : `Moving ${plural(switching.length, "domain")} to the tunnel… Public ports close after the last one.`;
    return "Ports 80 and 443 on all servers except build-only ones.";
  })();
  const tunnelHint = (() => {
    if (c.tunnel?.removeWhenBack)
      return `Moving ${plural(switching.length, "domain")} back to public ports. The tunnel is removed after.`;
    if (c.tunnel?.state === "removing") return "Removing the tunnel…";
    if (!paths.tunnel && unavailable) return unavailable;
    return "Websites through Cloudflare. No open ports needed.";
  })();
  const tailnetHint = !tailscale ? (
    "Connect Tailscale on Integrations first."
  ) : (
    <>
      Websites on ts.net names, for devices on your tailnet.
      {c.tailnet ? <span className="ml-1 font-mono">{c.tailnet}</span> : null}
    </>
  );

  return (
    <Panel title="Traffic" actions={<LearnMore page="gateway" />}>
      <PathRow
        label={LABEL.public}
        hint={publicHint}
        on={paths.public}
        disabled={!admin}
        onChange={(on) => toggle("public", on)}
      >
        <Switch
          checked={!!(c.proxyProtocol ?? c.webProxyProtocol)}
          disabled={!admin}
          onChange={(on) => setPending({ kind: "proxy", on })}
          label="PROXY protocol"
          description="Only for a load balancer that sends it on every connection."
        />
        {hasPorts ? (
          paths.public ? (
            <p className="flex flex-wrap justify-between gap-2 text-[13px]">
              <span className="font-medium">TCP and UDP ports</span>
              <span className="text-[12px] text-muted-foreground">Published with the websites.</span>
            </p>
          ) : (
            <Switch
              checked={!!c.publicPorts}
              disabled={!admin}
              onChange={(on) => (on ? void send({ publicPorts: true }, "TCP and UDP ports published") : setPending({ kind: "ports" }))}
              label="Publish TCP and UDP ports"
              description="On all servers except build-only ones."
            />
          )
        ) : null}
      </PathRow>
      <PathRow
        label={LABEL.tunnel}
        hint={tunnelHint}
        on={paths.tunnel || removing}
        disabled={!admin || removing || (!paths.tunnel && !!unavailable) || opening}
        onChange={(on) => toggle("tunnel", on)}
      />
      {opening || c.tunnel ? (
        <TunnelSettings c={c} editing={admin && (opening || paths.tunnel)} turningOn={opening} onDone={() => setOpening(false)} />
      ) : null}
      <PathRow
        label={LABEL.tailnet}
        hint={tailnetHint}
        on={paths.tailnet}
        disabled={!admin || !tailscale}
        onChange={(on) => toggle("tailnet", on)}
      >
        {tailscale ? null : (
          <AppLink href="/integrations#tailscale" className="text-[12.5px] font-medium text-primary hover:text-primary/80">
            Integrations
          </AppLink>
        )}
      </PathRow>
      {dialog ? (
        <ConfirmDialog
          open
          onClose={() => setPending(null)}
          tone={pending?.kind === "proxy" || pending?.kind === "tailnetOn" || pending?.kind === "publicOn" ? "primary" : "danger"}
          title={dialog.title}
          description={
            <div className="space-y-1.5">
              {dialog.lines.filter(Boolean).map((l) => (
                <p key={l}>{l}</p>
              ))}
            </div>
          }
          confirmLabel={dialog.confirm}
          onConfirm={go}
        >
          {pending?.kind === "public" && moving.length ? (
            <Switch
              checked={move}
              onChange={setMove}
              label={moving.length === 1 ? "Move it to the tunnel first" : "Move them to the tunnel first"}
              description="Public ports close once they're all on the tunnel."
            />
          ) : null}
          {pending?.kind === "public" && hasPorts ? (
            <Switch checked={ports} onChange={setPorts} label="Publish TCP and UDP ports" description="On all servers except build-only ones." />
          ) : null}
          {pending?.kind === "tunnel" && moving.length ? (
            <Switch checked={restore} onChange={setRestore} label="Restore the DNS records it replaced" />
          ) : null}
        </ConfirmDialog>
      ) : null}
    </Panel>
  );
}

/** One label under the tunnel's default domain: no direct record to go back to. */
function domainUnderDefault(host: string, c: Cluster) {
  const dd = c.tunnel?.defaultDomain?.toLowerCase();
  const h = host.toLowerCase();
  return !!dd && h.endsWith(`.${dd}`) && !h.slice(0, -dd.length - 1).includes(".");
}
