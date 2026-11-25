import {
  CircleDashedIcon,
  LockSimpleIcon,
  WarningCircleIcon,
  XCircleIcon,
  type Icon,
} from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { siTailscale } from "simple-icons";
import { expiry, tlsLine } from "~/components/pyo/Certificates";
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
 * `hint`: more detail, for a tooltip. `short`: fewer words, for a list.
 */
export function domainState(
  d: Domain,
  certs: Certificate[],
  notDeployed = false,
): { tone: DomainTone; text: string; hint?: string; short?: string } {
  if (notDeployed)
    return { tone: "busy", text: "Live after first deploy", short: "Not deployed" };
  if (d.status !== "active") {
    // No DNS to wait for: Pyo sets it up on the cluster's tailnet.
    if (isTailnetDomain(d.hostname))
      return d.status === "error"
        ? { tone: "broken", text: "Tailscale failed", hint: d.error }
        : { tone: "busy", text: "Setting up on Tailscale…" };
    // Cloudflare's address, not the cluster's: Pyo says when it's there.
    if (d.tunnel) return { tone: "busy", text: "Moving to the tunnel…" };
    if (isFreeDomain(d.hostname))
      return { tone: "busy", text: "Checking DNS…" };
    return {
      tone: d.status === "error" ? "broken" : "attention",
      text: `Waiting for DNS${d.dnsCheckedAt ? ` · checked ${timeAgo(d.dnsCheckedAt)}` : ""}`,
      short: "Waiting for DNS",
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
        short: c.notAfter <= Date.now() ? "Certificate expired" : "Certificate expiring",
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

const PROBLEM: Record<Exclude<DomainTone, "ok">, { icon: Icon; className: string }> = {
  busy: { icon: CircleDashedIcon, className: "text-muted-foreground" },
  attention: { icon: WarningCircleIcon, className: "text-warning" },
  broken: { icon: XCircleIcon, className: "text-destructive" },
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
  const badge = <Badge tone={BADGE[st.tone]}>{st.text}</Badge>;
  return st.hint ? <Tooltip content={st.hint}>{badge}</Tooltip> : badge;
}

/** The width of the slot DomainLock keeps before a hostname. */
const SLOT = "inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center";

/**
 * For before a hostname in a list: a green lock when HTTPS works, an icon
 * for a problem, an empty slot for a working domain without HTTPS. The slot
 * keeps its width so hostnames line up. DomainNote says the problem in words.
 */
export function DomainLock({
  d,
  certs,
  notDeployed,
}: {
  d: Domain;
  certs: Certificate[];
  notDeployed?: boolean;
}) {
  const st = domainState(d, certs, notDeployed);
  if (st.tone === "ok")
    return st.text === "HTTPS" ? (
      <Tooltip content={tlsLine(d, certs)}>
        <span role="img" aria-label="HTTPS" className={`${SLOT} text-success`}>
          <LockSimpleIcon size={14} weight="bold" />
        </span>
      </Tooltip>
    ) : (
      <span aria-hidden className={SLOT} />
    );
  const p = PROBLEM[st.tone];
  const hint = st.hint ?? (st.short ? st.text : undefined);
  const icon = (
    <span role="img" aria-label={st.text} className={`${SLOT} ${p.className}`}>
      <p.icon size={14} weight="bold" />
    </span>
  );
  return hint ? <Tooltip content={hint}>{icon}</Tooltip> : icon;
}

/** For after a hostname in a list: a problem in a few words, in its icon's colour. */
export function DomainNote({
  d,
  certs,
  notDeployed,
}: {
  d: Domain;
  certs: Certificate[];
  notDeployed?: boolean;
}) {
  const st = domainState(d, certs, notDeployed);
  if (st.tone === "ok") return null;
  return (
    <span
      className={`shrink-0 whitespace-nowrap text-[12.5px] ${PROBLEM[st.tone].className}`}
    >
      {st.short ?? st.text}
    </span>
  );
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

/** A logo in a box the size of the lock. */
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
 * What a domain goes through: the Tailscale logo on the tailnet, the
 * Cloudflare logo through the cluster's tunnel or a record Cloudflare
 * proxies. Nothing for the rest.
 */
export function DomainMark({
  d,
  clusterId,
}: {
  d: Domain;
  clusterId: string;
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
  return null;
}
