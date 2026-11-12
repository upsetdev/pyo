import { expiry } from "~/components/pyo/Certificates";
import { isFreeDomain, isPrivateHost } from "~/lib/pyo/freeDomain";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Tooltip } from "~/components/ui/Tooltip";
import { timeAgo } from "~/lib/pyo/format";
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
  if (d.tls === "failed") return { tone: "broken", text: "Certificate failed" };
  if (d.tls !== "issued") return { tone: "busy", text: "Issuing certificate…" };
  if (d.tlsMode === "custom")
    return {
      tone: "ok",
      text: "HTTPS",
      hint: `Your certificate ${certs.find((c) => c.id === d.certificateId)?.name ?? ""}`.trim(),
    };
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
