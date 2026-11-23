import { GlobeIcon, LockSimpleIcon } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { siTailscale } from "simple-icons";
import { expiry } from "~/components/pyo/Certificates";
import { BrandMark, CLOUDFLARE_LOGO } from "~/components/pyo/common";
import { throughTunnel, tunnelNote } from "~/components/pyo/TunnelRecord";
import {
  freeKind,
  isFreeDomain,
  isPrivateHost,
  isTailnetDomain,
} from "~/lib/pyo/freeDomain";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Tooltip } from "~/components/ui/Tooltip";
import { timeAgo } from "~/lib/pyo/format";
import { roleIn, usePyo } from "~/lib/pyo/store";
import type { Certificate, Domain } from "~/lib/pyo/types";

// One status for a domain instead of separate DNS and certificate badges:
// a short label saying what matters right now.

export type DomainTone = "ok" | "busy" | "attention" | "broken";

const selfSigned = (d: Domain) =>
  d.tlsMode === "selfsigned" ||
  ((d.tlsMode ?? "auto") === "auto" && isPrivateHost(d.hostname));

/**
 * Domain changes go live without a redeploy; `notDeployed`: the service has
 * never been deployed, so its domains go live with its first deploy.
 * `hint`: more detail, for a tooltip.
 */
export function domainState(
  d: Domain,
  certs: Certificate[],
  notDeployed = false,
): { tone: DomainTone; text: string; hint?: string } {
  if (notDeployed) return { tone: "busy", text: "Live after first deploy" };
  if (d.status !== "active") {
    // No DNS to wait for: Pyo sets it up on the cluster's tailnet.
    if (isTailnetDomain(d.hostname))
      return d.status === "error"
        ? { tone: "broken", text: "Tailscale failed", hint: d.error }
        : { tone: "busy", text: "Setting up on Tailscale…" };
    if (isFreeDomain(d.hostname))
      return { tone: "busy", text: "Checking DNS…" };
    return {
      tone: d.status === "error" ? "broken" : "attention",
      text: `Waiting for DNS${d.dnsCheckedAt ? ` · checked ${timeAgo(d.dnsCheckedAt)}` : ""}`,
    };
  }
  if (d.tlsMode === "off") return { tone: "ok", text: "HTTP only" };
  if (d.tlsMode === "custom") {
    const c = certs.find((x) => x.id === d.certificateId);
    if (!c) return { tone: "broken", text: "Certificate removed" };
    const e = expiry(c);
    if (e.tone !== "text-muted-foreground")
      return {
        tone: e.tone === "text-destructive" ? "broken" : "attention",
        text: "HTTPS",
        hint: `Your certificate ${c.name}. ${e.text}.`,
      };
  }
  if (d.tls === "failed")
    return { tone: "broken", text: "Certificate failed", hint: d.tlsError };
  if (d.tls !== "issued") return { tone: "busy", text: "Issuing certificate…" };
  if (d.tlsMode === "custom")
    return {
      tone: "ok",
      text: "HTTPS",
      hint: `Your certificate ${certs.find((c) => c.id === d.certificateId)?.name ?? ""}`.trim(),
    };
  if (isTailnetDomain(d.hostname))
    return { tone: "ok", text: "HTTPS", hint: "Tailscale certificate" };
  if (selfSigned(d))
    return { tone: "ok", text: "HTTPS", hint: "Self-signed certificate" };
  return { tone: "ok", text: "HTTPS", hint: "Let's Encrypt certificate" };
}

const BADGE: Record<DomainTone, Tone> = {
  ok: "success",
  busy: "neutral",
  attention: "warning",
  broken: "danger",
};

/** The status as one label, in the app's badge style (details in a tooltip). */
export function DomainStatus({
  d,
  certs,
  notDeployed,
}: {
  d: Domain;
  certs: Certificate[];
  notDeployed?: boolean;
}) {
  const st = domainState(d, certs, notDeployed);
  const label = <Badge tone={BADGE[st.tone]}>{st.text}</Badge>;
  return st.hint ? <Tooltip content={st.hint}>{label}</Tooltip> : label;
}

const KINDS = {
  public: { label: "Free", tone: "neutral", hint: "Free nip.io domain. Anyone can open it." },
  private: { label: "Private", tone: "neutral", hint: "Free nip.io domain on a private IP." },
} as const;

/** Which nip.io domain it is, as a badge. None for the team's own, or a tailnet one: DomainMark has its logo. */
export function DomainKind({ hostname }: { hostname: string }) {
  const k = freeKind(hostname);
  if (!k || k === "tailnet") return null;
  return (
    <Tooltip content={KINDS[k].hint}>
      <Badge tone={KINDS[k].tone}>{KINDS[k].label}</Badge>
    </Tooltip>
  );
}

/** A logo in the lock's box, so hostnames stay in a column. */
function Logo({
  label,
  hint,
  icon,
}: {
  label: string;
  hint: ReactNode;
  icon: Parameters<typeof BrandMark>[0]["icon"];
}) {
  return (
    <Tooltip content={hint}>
      <span
        role="img"
        aria-label={label}
        className="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-foreground"
      >
        <BrandMark icon={icon} size={13} />
      </span>
    </Tooltip>
  );
}

/**
 * What a domain goes through, before its name: the Tailscale logo on the
 * tailnet, the Cloudflare logo through the cluster's tunnel or a record
 * Cloudflare proxies, else the lock (the globe without HTTPS). `logoOnly`:
 * nothing for the rest.
 */
export function DomainMark({
  d,
  clusterId,
  logoOnly,
}: {
  d: Domain;
  clusterId: string;
  logoOnly?: boolean;
}) {
  const s = usePyo();
  if (isTailnetDomain(d.hostname))
    return (
      <Logo
        label="Tailscale"
        icon={siTailscale}
        hint="Tailscale. Only devices on your tailnet can open it."
      />
    );
  if (throughTunnel(d)) {
    const role = roleIn(s);
    const note = tunnelNote(
      d,
      s.clusters.find((c) => c.id === clusterId),
      role === "owner" || role === "admin",
    );
    return (
      <Logo
        label="Cloudflare Tunnel"
        icon={CLOUDFLARE_LOGO}
        hint={
          note ? (
            <>
              Cloudflare Tunnel · <span className="font-mono">{note.name}</span>
              {note.more ? <span className="block">{note.more}</span> : null}
            </>
          ) : (
            "Cloudflare Tunnel"
          )
        }
      />
    );
  }
  const r = d.dnsRecord;
  if (r?.proxied && (r.status === "created" || r.status === "exists"))
    return <Logo label="Cloudflare" icon={CLOUDFLARE_LOGO} hint="Cloudflare · Proxied" />;
  if (logoOnly) return null;
  return d.tls === "issued" ? (
    <LockSimpleIcon size={14} className="text-success" aria-label="HTTPS" />
  ) : (
    <GlobeIcon size={14} className="text-muted-foreground" aria-label="HTTP only" />
  );
}
