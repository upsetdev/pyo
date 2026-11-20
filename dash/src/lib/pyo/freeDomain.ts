import type { PyoState } from "./store";
import type { Application, ClusterServer } from "./types";

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

/**
 * "blog.tail1234.ts.net". Names are flat on a tailnet, so a name another
 * service has gets the project's namespace, like nip.io names: "blog-shop".
 */
export function tailnetHostname(
  s: PyoState,
  app: Pick<Application, "name"> & { id?: string },
  namespace: string,
  tailnet: string,
) {
  const taken = (h: string) =>
    s.services.some(
      (a) => a.id !== app.id && a.domains.some((d) => d.hostname === h),
    );
  const label = (x: string) => x.slice(0, 63).replace(/-+$/, "");
  const short = `${label(app.name)}.${tailnet}`;
  if (!taken(short)) return short;
  const long = `${label(`${app.name}-${namespace}`)}.${tailnet}`;
  if (!taken(long)) return long;
  let i = 2;
  while (taken(`${label(`${app.name}-${namespace}-${i}`)}.${tailnet}`)) i++;
  return `${label(`${app.name}-${namespace}-${i}`)}.${tailnet}`;
}

/**
 * Where a first service's free domain points (onboarding): the cluster's
 * public IP, or its Tailscale IP behind NAT (the API picks it, never a
 * private one). None without one. The dialogs use the public IP.
 */
export function freeDomainIp(s: PyoState, clusterId: string) {
  return s.clusters.find((c) => c.id === clusterId)?.freeDomainIp ?? undefined;
}

/** Why there's no free domain (on the public IP) on the cluster, in a few words. */
export function noFreeDomain(s: PyoState, clusterId: string) {
  const c = s.clusters.find((x) => x.id === clusterId);
  return c?.publicTraffic === false
    ? "Public traffic is off, so no free domain."
    : "No public IP yet, so no free domain.";
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
  // "app.100.101.67.2.nip.io", or the dashed "app.100-101-67-2.nip.io".
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
