import type { ClusterServer } from "~/lib/pyo/types";

// A server's addresses: which are public, which are Tailscale's, and how to
// shorten a long one.

/** 100.64.0.0/10 and fd7a:115c:a1e0::/48: the ranges Tailscale gives out. */
export function tailnetIp(ip: string | undefined) {
  if (/^fd7a:115c:a1e0:/i.test(ip ?? "")) return true;
  const m = /^100\.(\d+)\.\d+\.\d+$/.exec(ip ?? "");
  return !!m && Number(m[1]) >= 64 && Number(m[1]) <= 127;
}

/** Not reachable from the internet: private, tailnet, loopback, link-local. */
export function privateIp(ip: string) {
  return (
    ip === "::1" ||
    ip === "::" ||
    /^(0\.|10\.|127\.|192\.168\.|169\.254\.)/.test(ip) ||
    /^172\.(1[6-9]|2\d|3[01])\./.test(ip) ||
    tailnetIp(ip) ||
    /^(fc|fd|fe80)/i.test(ip)
  );
}

/** A server's public address of each family: what the agent found, then what the cluster lists. */
export function publicIpsOf(n: ClusterServer): { v4?: string; v6?: string } {
  const all = [n.publicIpv4, n.publicIpv6, n.externalIP, ...(n.addresses ?? []), n.publicIp, n.internalIP].filter(
    (ip): ip is string => !!ip && !privateIp(ip),
  );
  return { v4: all.find((ip) => !ip.includes(":")), v6: all.find((ip) => ip.includes(":")) };
}

/** A long address shortened in the middle: "2001:db8:728…bd1e:f69e". */
export const midIp = (ip: string, max = 26) => (ip.length <= max ? ip : `${ip.slice(0, max - 11)}…${ip.slice(-10)}`);
