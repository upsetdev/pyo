import { useState } from "react";
import { BrandMark, CLOUDFLARE_LOGO } from "~/components/pyo/common";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Switch } from "~/components/ui/Field";
import { api, cloudflareZoneFor, clusterTraffic, roleIn, usePyo } from "~/lib/pyo/store";
import type { Application, Cluster, Domain, PreviousRecord } from "~/lib/pyo/types";

// A domain's side of the cluster's Cloudflare Tunnel on the service's
// Domains: switching to it and back, and what's in the way.

const isAdmin = (role?: string) => role === "owner" || role === "admin";

/** The domain is through the tunnel, or on its way there or back. */
export const onTunnel = (d: Domain) =>
  !!d.tunnel ||
  d.dnsRecord?.via === "tunnel" ||
  d.dnsRecord?.status === "switching" ||
  d.dnsRecord?.status === "reverting";

/**
 * The tunnel's default domain when it covers the name (one label under
 * it). Such a name always goes through the tunnel, so there's no Switch back.
 */
export const defaultDomainOver = (c: Cluster | undefined, hostname: string) => {
  const dd = c?.tunnel?.defaultDomain?.toLowerCase();
  const host = hostname.toLowerCase();
  if (!dd || !host.endsWith(`.${dd}`)) return undefined;
  return host.slice(0, -dd.length - 1).includes(".") ? undefined : dd;
};

/** "1f2e….cfargotunnel.com" for the tunnel's long id. */
const shortTarget = (content: string) => {
  const m = /^([^.]+)\.cfargotunnel\.com\.?$/.exec(content);
  return m ? `${m[1].slice(0, 4)}….cfargotunnel.com` : content;
};

const recordText = (p: PreviousRecord) => `${p.type} ${p.name} ${p.content}`;

const lineClass =
  "mt-1.5 inline-flex flex-wrap items-center gap-1.5 text-[12px] text-muted-foreground";

/** Through the tunnel, switching, in the way, or failed. */
export function TunnelRecordLine({ app, d }: { app: Application; d: Domain }) {
  const s = usePyo();
  const r = d.dnsRecord;
  const admin = isAdmin(roleIn(s));
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const run = async (opts: { on: boolean; replace?: boolean; restore?: boolean }) => {
    setBusy(true);
    await api.setDomainTunnel(app.id, d.id, opts);
    setBusy(false);
  };
  const back = r?.status === "reverting";
  const wildcard = defaultDomainOver(cluster, d.hostname);

  if (r?.status === "conflict")
    return (
      <div className="mt-3 rounded-lg border border-warning/40 bg-warning/5 px-3 py-2.5 text-[12px]">
        <p className="mb-1.5">Cloudflare has a record for {d.hostname}:</p>
        {r.existing.map((x) => (
          <div key={x.id} className="flex flex-wrap gap-x-4 font-mono text-[11.5px]">
            <span>{x.type}</span>
            <span>{x.name}</span>
            <span>{x.content}</span>
            {x.proxied ? <span>proxied</span> : null}
          </div>
        ))}
        <p className="mt-1.5 text-muted-foreground">Pyo keeps a copy. Switch back puts it back.</p>
        <div className="mt-2 flex items-center gap-2">
          {admin ? (
            <Button
              size="xs"
              variant="secondary"
              disabled={busy}
              onClick={() => void run({ on: true, replace: true })}
            >
              Replace and switch
            </Button>
          ) : (
            <span className="text-muted-foreground">An owner or admin can replace it.</span>
          )}
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => void run({ on: false })}>
            Cancel
          </Button>
        </div>
      </div>
    );

  if (r?.status === "failed" || (r?.error && (r.status === "switching" || back)))
    return (
      <p className="mt-1.5 text-[12px] text-destructive">
        Cloudflare: {r.error ?? "the record wasn't changed."}
        <button
          type="button"
          disabled={busy}
          onClick={() => void run({ on: !back, restore: back && !!r.previous?.length })}
          className="ml-1.5 font-medium text-primary hover:text-primary/80"
        >
          Try again
        </button>
      </p>
    );

  // No record of its own: the default domain's wildcard has it.
  if (!r && d.tunnel === "on" && wildcard)
    return (
      <p className={lineClass}>
        <BrandMark icon={CLOUDFLARE_LOGO} size={12} />
        Via Tunnel ·
        <span className="font-mono">*.{wildcard}</span>
      </p>
    );

  if (!r || r.status === "switching" || back)
    return (
      <p className={lineClass}>
        <BrandMark icon={CLOUDFLARE_LOGO} size={12} />
        {back ? "Switching back…" : "Switching to the tunnel…"}
      </p>
    );

  const kept = r.previous ?? [];
  const ip = cluster && clusterTraffic(cluster) === "public" ? cluster.publicIp : null;
  return (
    <>
      <p className={lineClass}>
        <BrandMark icon={CLOUDFLARE_LOGO} size={12} />
        Via Tunnel ·
        <span className="font-mono" title={r.content}>
          {r.type} {shortTarget(r.content)}
        </span>
        {r.error ? <span className="text-destructive">· {r.error}</span> : null}
        {wildcard && !kept.length ? null : kept.length && !admin ? (
          <span>· An owner or admin can switch it back.</span>
        ) : (
          <Button
            size="xs"
            variant="ghost"
            className="-my-1"
            disabled={busy}
            onClick={() => setConfirming(true)}
          >
            Switch back
          </Button>
        )}
      </p>
      <ConfirmDialog
        open={confirming}
        onClose={() => setConfirming(false)}
        tone="primary"
        title={
          kept.length
            ? `Put back ${kept[0].type} ${kept[0].content} for ${d.hostname}?`
            : ip
              ? `Point ${d.hostname} at ${ip}?`
              : `Remove Pyo's record for ${d.hostname}?`
        }
        description={null}
        confirmLabel="Switch back"
        onConfirm={() => void run({ on: false, restore: kept.length > 0 })}
      />
    </>
  );
}

/** "Switch to Tunnel" for a direct domain in a tunnel-ready zone. */
export function SwitchToTunnel({ app, d }: { app: Application; d: Domain }) {
  const s = usePyo();
  const [busy, setBusy] = useState(false);
  if (roleIn(s) === "viewer") return null;
  return (
    <Button
      size="xs"
      variant="secondary"
      className="mt-2"
      disabled={busy}
      onClick={async () => {
        setBusy(true);
        await api.setDomainTunnel(app.id, d.id, { on: true });
        setBusy(false);
      }}
    >
      <BrandMark icon={CLOUDFLARE_LOGO} size={12} />
      Switch to Tunnel
    </Button>
  );
}

/** In tunnel mode, a domain no tunnel zone covers doesn't answer from the internet. */
export function OutsideTunnel({ c, d }: { c: Cluster; d: Domain }) {
  const s = usePyo();
  const zone = cloudflareZoneFor(s, d.hostname, c.teamId);
  return (
    <p className="mt-1.5 text-[12px] text-warning">
      {zone && c.tunnel && zone.accountId !== c.tunnel.accountId
        ? "Another Cloudflare account. It doesn't answer from the internet."
        : "Not in your Cloudflare account. It doesn't answer from the internet."}
    </p>
  );
}

/** The remove dialog's choice to put back the record a tunnel domain replaced. */
export function RestoreRecordSwitch({
  d,
  checked,
  onChange,
}: {
  d: Domain | null;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  const kept = d?.dnsRecord?.previous ?? [];
  if (!kept.length) return null;
  return (
    <Switch
      checked={checked}
      onChange={onChange}
      label="Put back the old record"
      description={kept.map(recordText).join(", ")}
    />
  );
}
