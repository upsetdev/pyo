import { useState } from "react";
import { Badge, Dot, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Select } from "~/components/ui/Field";
import { plural } from "~/lib/pyo/format";
import {
  api,
  cloudflareIn,
  clusterPaths,
  usePyo,
  type PyoState,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster, ClusterTunnel } from "~/lib/pyo/types";

// A cluster's Cloudflare Tunnel: its settings in the Traffic panel, its
// health and its badge in the cluster list. The Traffic card's switch
// turns it on and off.

/** Why the cluster can't use a tunnel yet, or null when it can. */
export function tunnelUnavailable(s: PyoState, c: Cluster): string | null {
  const account = cloudflareIn(s, c.teamId);
  if (!account) return "Connect Cloudflare first.";
  if (!account.zones.some((z) => z.tunnel?.ready))
    return "Add Cloudflare Tunnel:Edit to the Cloudflare token.";
  if (c.capabilities.tunnel !== true) return "Not reported yet.";
  return null;
}

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
      return { tone: "danger", text: "Tunnel down. Its websites are offline." };
    // Pyo tried Cloudflare and failed, or no read came in time.
    case "unknown":
      return { tone: "neutral", text: "Pyo can't reach Cloudflare. Websites keep working." };
    case "removing":
      return { tone: "progress", text: "Removing the tunnel…" };
    // "starting", and no health yet.
    default:
      return { tone: "progress", text: "Starting the tunnel…" };
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
      .map((x) => `Tunnel on ${x.server}: ${x.reason}`),
    t.state !== "failed" && t.error
      ? t.error
      : !cloudflareIn(s, c.teamId)
        ? "Connect Cloudflare again to change the tunnel."
        : null,
    t.refused
      ? "Run the command under Refresh this cluster."
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

/** "Tunnel" beside a cluster while its tunnel is on, warning tone while it's degraded or down. */
export function TunnelBadge({ c, className }: { c: Cluster; className?: string }) {
  if (!clusterPaths(c).tunnel) return null;
  const bad = c.tunnel?.health === "degraded" || c.tunnel?.health === "down";
  return (
    <Badge tone={bad ? "warning" : "neutral"} className={className}>
      Tunnel
    </Badge>
  );
}

/**
 * The Traffic card's tunnel part: its settings while it's turned on or on
 * (default domain, account), its status and Rotate while it has one.
 */
export function TunnelSettings({
  c,
  editing,
  turningOn,
  onDone,
}: {
  c: Cluster;
  /** Its settings can be changed (owners and admins). */
  editing: boolean;
  /** Being turned on: Turn on and Cancel. */
  turningOn: boolean;
  onDone: () => void;
}) {
  const s = usePyo();
  const t = c.tunnel && c.tunnel.state !== "removing" ? c.tunnel : null;
  const zones = (cloudflareIn(s, c.teamId)?.zones ?? []).filter(
    (z) => z.tunnel?.ready && z.accountId,
  );
  const accounts = [...new Map(zones.map((z) => [z.accountId!, z.accountName ?? z.accountId!]))];
  const [picked, setAccount] = useState("");
  // The tunnel's own account, as picked, or the first one.
  const account = t?.accountId ?? (picked || zones[0]?.accountId || "");
  const [domain, setDomain] = useState(t?.defaultDomain ?? "");
  const [confirm, setConfirm] = useState<"rotate" | null>(null);
  const install = c.capabilities.agentInstall;
  const outdated = !!install && install.state !== "current";
  const dirty =
    turningOn ||
    domain !== (t?.defaultDomain ?? "") ||
    (!!account && !!t && account !== t.accountId);

  const save = async () => {
    const res = await api.setClusterPaths(c.id, {
      tunnel: true,
      defaultDomain: domain,
      accountId: account || undefined,
    });
    if (res?.ok) {
      toast({ title: turningOn ? "Cloudflare Tunnel on" : "Tunnel saved", description: c.name });
      onDone();
    }
  };

  return (
    <>
      {editing && !t?.removeWhenBack ? (
        <div className="space-y-4 border-t border-border p-4 pl-10">
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
          {outdated ? (
            <p className="text-[12px] text-warning">
              {install?.state === "updating"
                ? "This cluster is updating. Try again in a few minutes."
                : "Run the command under Refresh this cluster first."}
            </p>
          ) : null}
        </div>
      ) : null}
      {c.tunnel ? (
        <div className="border-t border-border px-4 py-3 pl-10">
          <TunnelStatus c={c} />
        </div>
      ) : null}
      {editing && (t || dirty) ? (
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
          {turningOn ? (
            <Button variant="ghost" size="sm" onClick={onDone}>
              Cancel
            </Button>
          ) : null}
          {dirty ? (
            <Button size="sm" disabled={outdated || !account} onClick={() => void save()}>
              {turningOn ? "Turn on" : "Save"}
            </Button>
          ) : null}
        </div>
      ) : null}
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
