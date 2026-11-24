import type { PyoState } from "./store";
import type { Application, Cluster, ClusterServer } from "./types";

// Free domains through nip.io: "<anything>.<ip>.nip.io" resolves to <ip>
// (e.g. "app.203.0.113.7.nip.io"), so a hostname works with no DNS record.

// Tailscale domains ("tailnet" in code): "<name>.<tailnet>.ts.net" through
// Tailscale Services, with Tailscale's own certificate. Only devices on the
// tailnet can open them.

/** A tailnet domain (ts.net). */
export const isTailnetDomain = (hostname: string) => /\.ts\.net$/.test(hostname);

/** Free: no DNS record to make (nip.io or the tailnet). */
export const isFreeDomain = (hostname: string) =>
  /\.nip\.io$/.test(hostname) || isTailnetDomain(hostname);

/** The kind of free domain, or null for the team's own. */
export function freeKind(hostname: string): "public" | "private" | "tailnet" | null {
  if (isTailnetDomain(hostname)) return "tailnet";
  if (!/\.nip\.io$/.test(hostname)) return null;
  return isPrivateHost(hostname) ? "private" : "public";
}

/** The tailnet a cluster's tailnet domains are on ("tail1234.ts.net"); none without Tailscale. */
export function clusterTailnet(s: PyoState, clusterId: string) {
  return s.clusters.find((c) => c.id === clusterId)?.tailnet ?? undefined;
}

/** Its host, from a URL or a hostname ("https://pyo.tail1234.ts.net/" → "pyo.tail1234.ts.net"). */
const hostOf = (x: string) => x.replace(/^[a-z]+:\/\//i, "").split(/[/:]/)[0].toLowerCase();

/** A service has the hostname already, or Pyo's own dashboard or registry on the tailnet. */
export function hostnameUsed(s: PyoState, hostname: string) {
  const h = hostname.toLowerCase();
  return (
    s.services.some((a) => a.domains.some((d) => d.hostname.toLowerCase() === h)) ||
    [s.tailscale?.dashboard, s.tailscale?.registry].some((x) => !!x && hostOf(x) === h)
  );
}

/**
 * "<label>.<suffix>", else "<label>-2.<suffix>", "<label>-3.<suffix>" and
 * on: the first one nothing uses. Each label at most 63 characters.
 */
export function unusedHostname(s: PyoState, label: string, suffix: string) {
  const cut = (x: string, n: number) => x.slice(0, n).replace(/-+$/, "");
  let host = `${cut(label, 63)}.${suffix}`;
  for (let i = 2; hostnameUsed(s, host); i++)
    host = `${cut(label, 62 - String(i).length)}-${i}.${suffix}`;
  return host;
}

/** "blog.tail1234.ts.net", or "blog-2.tail1234.ts.net" when that's used. Names are flat on a tailnet. */
export function tailnetHostname(
  s: PyoState,
  app: Pick<Application, "name">,
  tailnet: string,
) {
  return unusedHostname(s, app.name, tailnet);
}

/**
 * Where a first service's free domain points (onboarding): the cluster's
 * public IP, or its Tailscale IP behind NAT (the API picks it, never a
 * private one). None without one. The dialogs use the public IP.
 */
export function freeDomainIp(s: PyoState, clusterId: string) {
  return s.clusters.find((c) => c.id === clusterId)?.freeDomainIp ?? undefined;
}

/**
 * What the name Pyo makes for a service is called on these clusters: a
 * "Tunnel domain" when all of them take web traffic through Cloudflare
 * Tunnel (it sits under the team's own domain), else a "Free domain".
 */
export function freeDomainNoun(clusters: (Pick<Cluster, "traffic"> | undefined)[]) {
  return clusters.length && clusters.every((c) => c?.traffic === "tunnel")
    ? "Tunnel domain"
    : "Free domain";
}

/** Why there's no free domain (on the public IP) on the cluster, in a few words. */
export function noFreeDomain(s: PyoState, clusterId: string) {
  const c = s.clusters.find((x) => x.id === clusterId);
  if (c?.traffic === "tunnel") return "No default domain yet, so no tunnel domain.";
  return c?.publicTraffic === false
    ? "Public traffic is off, so no free domain."
    : "No public IP yet, so no free domain.";
}

/**
 * What a cluster's free domains end in: "203.0.113.7.nip.io" on its public
 * IP, or in tunnel mode its tunnel's default domain ("example.com"). None
 * without either.
 */
export function freeSuffix(s: PyoState, clusterId: string | undefined) {
  const c = s.clusters.find((x) => x.id === clusterId);
  if (!c) return undefined;
  if (c.traffic === "tunnel") return c.freeDomainSuffix ?? undefined;
  return c.publicIp ? `${c.publicIp}.nip.io` : undefined;
}

/** "hello-storefront.example.com": one label of at most 63 characters, then the suffix. */
export function freeHostnameIn(
  app: Pick<Application, "name">,
  namespace: string,
  suffix: string,
) {
  return `${`${app.name}-${namespace}`.slice(0, 63).replace(/-+$/, "")}.${suffix}`;
}

/** "hello-storefront.203.0.113.7.nip.io": one label of at most 63 characters, then the address. */
export function freeHostname(
  app: Pick<Application, "name">,
  namespace: string,
  ip: string,
) {
  return `${`${app.name}-${namespace}`.slice(0, 63).replace(/-+$/, "")}.${ip}.nip.io`;
}

/** Only a tailnet, LAN or this machine reaches it, so Let's Encrypt can't (mirrors the API's rule). */
export function isPrivateHost(host: string) {
  const h = host.toLowerCase();
  if (
    h === "localhost" ||
    [".localhost", ".local", ".internal", ".lan", ".home.arpa", ".test"].some(
      (s) => h.endsWith(s),
    )
  )
    return true;
  // "app.100.64.0.2.nip.io", or the dashed "app.100-64-0-2.nip.io".
  const m = /\.(\d+)[.-](\d+)[.-]\d+[.-]\d+\.(nip|sslip)\.io$/.exec(h);
  if (!m) return false;
  const [a, b] = [Number(m[1]), Number(m[2])];
  return (
    a === 10 ||
    a === 127 ||
    (a === 192 && b === 168) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 169 && b === 254) ||
    a === 0 ||
    (a === 100 && b >= 64 && b <= 127)
  );
}

/** Where a cluster's first service opens: on the web, on the tailnet, or only on the local network. */
export function reachOf(s: PyoState, clusterId: string): "web" | "tailnet" | "network" | "cluster" {
  const ip = freeDomainIp(s, clusterId);
  if (ip) return isPrivateHost(`x.${ip}.nip.io`) ? "tailnet" : "web";
  return s.clusters.find((c) => c.id === clusterId)?.publicTraffic === false ? "cluster" : "network";
}

/** A server of the cluster to open ports on: a ready one first. */
function lanServer(servers: ClusterServer[], clusterId: string) {
  const own = servers.filter((x) => x.clusterId === clusterId && x.internalIP);
  return own.find((x) => x.status === "ready") ?? own[0];
}

/**
 * "http://192.168.1.5:31234": where a service with no domain opens on the
 * local network, through its HTTP port on every server.
 */
export function lanUrl(
  s: PyoState,
  app: Pick<Application, "clusterId" | "ports">,
) {
  if (s.clusters.find((c) => c.id === app.clusterId)?.publicTraffic === false) return undefined;
  const port = app.ports.find(
    (p) => p.exposure === "nodeport" && p.protocol === "HTTP" && p.publicPort,
  );
  const server = lanServer(s.servers, app.clusterId);
  if (!port || !server) return undefined;
  const host = server.internalIP.includes(":") ? `[${server.internalIP}]` : server.internalIP;
  return `http://${host}:${port.publicPort}`;
}
