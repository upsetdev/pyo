import { useState } from "react";
import { Badge, Dot, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Select, Switch } from "~/components/ui/Field";
import { plural } from "~/lib/pyo/format";
import { isTailnetDomain } from "~/lib/pyo/freeDomain";
import {
  api,
  cloudflareIn,
  cloudflareZoneFor,
  clusterTraffic,
  listedServices,
  usePyo,
  type PyoState,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster, ClusterTunnel } from "~/lib/pyo/types";

// A cluster's Cloudflare Tunnel: its settings in the Traffic panel, its
// health, its badge in the cluster list and its removal.

/** Why the cluster can't use a tunnel yet, or null when it can. */
export function tunnelUnavailable(s: PyoState, c: Cluster): string | null {
  const account = cloudflareIn(s, c.teamId);
  if (!account) return "Connect Cloudflare first.";
  if (!account.zones.some((z) => z.tunnel?.ready))
    return "Give the token Cloudflare Tunnel:Edit.";
  if (c.capabilities.tunnel !== true) return "Not reported yet.";
  return null;
}

/** "a", "a and b", "a, b and c". */
const list = (xs: string[]) =>
  xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs.at(-1)}`;

/** The tunnel's health as one line. */
function healthLine(t: ClusterTunnel): { tone: Tone; text: string } {
  const ready = t.connectors.filter((x) => x.ready).length;
  if (t.state === "failed")
    return { tone: "danger", text: `Cloudflare: ${t.error ?? "the tunnel wasn't made."}` };
  switch (t.health) {
    case "connected":
      return {
        tone: "success",
        text: ["Tunnel connected", plural(ready, "connector"), t.colos.join(", ")]
          .filter(Boolean)
          .join(" · "),
      };
    case "degraded":
      return {
        tone: "warning",
        text: `Tunnel degraded · ${ready} of ${plural(t.connectors.length, "connector")}`,
      };
    case "down":
      return { tone: "danger", text: "Tunnel down. Its websites don't answer." };
    case "starting":
      return { tone: "progress", text: "Starting the tunnel…" };
    case "removing":
      return { tone: "progress", text: "Removing the tunnel…" };
    default:
      return { tone: "neutral", text: "Pyo can't reach Cloudflare. Websites keep working." };
  }
}

/** The tunnel's status line, and the lines that say what's wrong. */
export function TunnelStatus({ c }: { c: Cluster }) {
  const s = usePyo();
  const t = c.tunnel;
  if (!t) return null;
  const line = healthLine(t);
  const notes = [
    ...t.connectors
      .filter((x) => !x.ready && x.reason)
      .map((x) => `pyo-tunnel can't start on ${x.server}: ${x.reason}`),
    t.state !== "failed" && t.error
      ? t.error
      : !cloudflareIn(s, c.teamId)
        ? "Connect Cloudflare again to change the tunnel."
        : null,
    t.refused
      ? "Run the command under Refresh this cluster in the cluster's settings."
      : null,
  ].filter((x): x is string => !!x);
  return (
    <div className="space-y-1 text-[12.5px]">
      <p className="flex items-center gap-2">
        <Dot tone={line.tone} className="h-2 w-2" />
        {line.text}
      </p>
      {notes.map((n) => (
        <p key={n} className="pl-4 text-[12px] text-destructive">
          {n}
        </p>
      ))}
    </div>
  );
}

/** "Tunnel" beside a cluster in tunnel mode, warning tone while it's degraded or down. */
export function TunnelBadge({ c }: { c: Cluster }) {
  if (clusterTraffic(c) !== "tunnel") return null;
  const bad = c.tunnel?.health === "degraded" || c.tunnel?.health === "down";
  return <Badge tone={bad ? "warning" : "neutral"}>Tunnel</Badge>;
}

/**
 * The Traffic panel's tunnel part: its settings while Cloudflare Tunnel is
 * picked, its status and Rotate while it has one.
 */
export function TunnelSettings({
  c,
  editing,
  onCancel,
}: {
  c: Cluster;
  /** Cloudflare Tunnel is picked. */
  editing: boolean;
  onCancel: () => void;
}) {
  const s = usePyo();
  const t = c.tunnel && c.tunnel.state !== "removing" ? c.tunnel : null;
  const zones = (cloudflareIn(s, c.teamId)?.zones ?? []).filter(
    (z) => z.tunnel?.ready && z.accountId,
  );
  const accounts = [...new Map(zones.map((z) => [z.accountId!, z.accountName ?? z.accountId!]))];
  const [account, setAccount] = useState(t?.accountId ?? zones[0]?.accountId ?? "");
  const [domain, setDomain] = useState(t?.defaultDomain ?? "");
  const [ports, setPorts] = useState(t?.portsPublic ?? true);
  const [confirm, setConfirm] = useState<"switch" | "rotate" | null>(null);
  const inTunnel = clusterTraffic(c) === "tunnel";
  const install = c.capabilities.agentInstall;
  const outdated = !!install && install.state !== "current";
  const services = listedServices(s).filter((a) => a.clusterId === c.id);
  const hasPorts = services.some((a) =>
    a.ports.some(
      (p) => p.protocol !== "HTTP" && (p.exposure === "loadbalancer" || p.exposure === "nodeport"),
    ),
  );
  const dirty =
    !inTunnel ||
    domain !== (t?.defaultDomain ?? "") ||
    ports !== (t?.portsPublic ?? true) ||
    (!!account && account !== t?.accountId);

  // What the switch does, for its confirm dialog.
  const domains = services
    .flatMap((a) => a.domains)
    .filter((d) => !isTailnetDomain(d.hostname));
  const zoneOf = (host: string) => {
    const z = cloudflareZoneFor(s, host, c.teamId);
    return z?.tunnel?.ready && z.accountId === account ? z.name : undefined;
  };
  const moving = [...new Set(domains.map((d) => zoneOf(d.hostname)).filter((z): z is string => !!z))];
  const outside = domains.filter((d) => !zoneOf(d.hostname)).length;
  const lines = [
    moving.length
      ? `Websites on ${list(moving)} move to the tunnel. Records Pyo didn't make wait for your OK.`
      : null,
    outside === 1
      ? "1 domain isn't in your Cloudflare account. It stops answering from the internet."
      : outside
        ? `${outside} domains aren't in your Cloudflare account. They stop answering from the internet.`
        : null,
    c.sameCluster
      ? "The public dashboard closes. Open Pyo through Tailscale or your local network."
      : null,
  ].filter((x): x is string => !!x);

  const save = async () => {
    const res = await api.setClusterTraffic(c.id, "tunnel", {
      defaultDomain: domain || null,
      accountId: account || undefined,
      portsPublic: ports,
    });
    if (res?.ok)
      toast({
        title: inTunnel ? "Tunnel saved" : "Cloudflare Tunnel",
        description: res.moving ? `${plural(res.moving, "domain")} moving` : c.name,
      });
  };

  return (
    <>
      {editing ? (
        <div className="space-y-4 border-t border-border p-4">
          {accounts.length > 1 && !t ? (
            <Field label="Cloudflare account">
              {(p) => (
                <Select
                  {...p}
                  value={account}
                  onChange={(e) => {
                    setAccount(e.target.value);
                    setDomain("");
                  }}
                >
                  {accounts.map(([id, name]) => (
                    <option key={id} value={id}>
                      {name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          ) : null}
          <Field
            label="Default domain"
            optional
            hint={`Services get names like web.${domain || "example.com"}.`}
          >
            {(p) => (
              <Select {...p} value={domain} onChange={(e) => setDomain(e.target.value)}>
                <option value="">None</option>
                {zones
                  .filter((z) => z.accountId === account)
                  .map((z) => (
                    <option key={z.id} value={z.name}>
                      {z.name}
                    </option>
                  ))}
              </Select>
            )}
          </Field>
          {hasPorts ? (
            <Switch
              checked={ports}
              onChange={setPorts}
              label="Public TCP and UDP ports"
              description="On every server except build-only servers. They don't go through the tunnel."
            />
          ) : null}
          {outdated ? (
            <p className="text-[12px] text-warning">
              Run the command under Refresh this cluster in the cluster's settings first.
            </p>
          ) : null}
        </div>
      ) : null}
      {c.tunnel ? (
        <div className="border-t border-border px-4 py-3">
          <TunnelStatus c={c} />
        </div>
      ) : null}
      {t || (editing && dirty) ? (
        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-border px-4 py-3">
          {t ? (
            <Button
              variant="secondary"
              size="sm"
              className="mr-auto"
              onClick={() => setConfirm("rotate")}
            >
              Rotate
            </Button>
          ) : null}
          {editing && !inTunnel ? (
            <Button variant="ghost" size="sm" onClick={onCancel}>
              Cancel
            </Button>
          ) : null}
          {editing && dirty ? (
            <Button
              size="sm"
              disabled={outdated || !account}
              onClick={() => (inTunnel ? void save() : setConfirm("switch"))}
            >
              {inTunnel ? "Save" : "Switch"}
            </Button>
          ) : null}
        </div>
      ) : null}
      <ConfirmDialog
        open={confirm === "switch"}
        onClose={() => setConfirm(null)}
        tone="primary"
        title={`Switch ${c.name} to Cloudflare Tunnel?`}
        description={
          lines.length ? (
            <div className="space-y-1.5">
              {lines.map((l) => (
                <p key={l}>{l}</p>
              ))}
            </div>
          ) : null
        }
        confirmLabel="Switch"
        onConfirm={() => void save()}
      />
      <ConfirmDialog
        open={confirm === "rotate"}
        onClose={() => setConfirm(null)}
        tone="primary"
        title={`Rotate the tunnel token of ${c.name}?`}
        description="The tunnel gets a new token and certificate, and reconnects."
        confirmLabel="Rotate"
        onConfirm={() =>
          void api.rotateTunnelToken(c.id).then((r) => {
            if (r?.ok) toast({ title: "Tunnel token rotated", description: c.name });
          })
        }
      />
    </>
  );
}

/** Danger zone: remove the tunnel of a cluster that's no longer in tunnel mode. */
export function RemoveTunnelRow({ c }: { c: Cluster }) {
  const [open, setOpen] = useState(false);
  const [restore, setRestore] = useState(true);
  if (!c.tunnel || clusterTraffic(c) === "tunnel") return null;
  const removing = c.tunnel.state === "removing";
  return (
    <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
      <div>
        <p className="text-[13px] font-medium">Remove tunnel</p>
        <p className="text-[12px] text-muted-foreground">
          {removing ? "Removing…" : c.tunnel.tunnelName}
        </p>
      </div>
      <Button
        variant="secondary"
        size="sm"
        className="text-destructive"
        disabled={removing}
        onClick={() => {
          setRestore(true);
          setOpen(true);
        }}
      >
        Remove
      </Button>
      <ConfirmDialog
        open={open}
        onClose={() => setOpen(false)}
        title={`Remove the tunnel of ${c.name}?`}
        description="Pyo deletes the DNS records it made for it."
        confirmLabel="Remove tunnel"
        onConfirm={() =>
          void api.removeClusterTunnel(c.id, restore).then((r) => {
            if (r?.ok) toast({ title: "Removing the tunnel", description: c.name });
          })
        }
      >
        <Switch
          checked={restore}
          onChange={setRestore}
          label="Put back the records it replaced"
        />
      </ConfirmDialog>
    </li>
  );
}
