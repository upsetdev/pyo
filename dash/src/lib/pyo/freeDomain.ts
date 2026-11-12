import type { PyoState } from "./store";
import type { Application } from "./types";

// Free domains through nip.io: "<anything>.<ip>.nip.io" resolves to <ip>, so
// a hostname works with no DNS record to create.

export const isFreeDomain = (hostname: string) => /\.nip\.io$/.test(hostname);

/** The cluster's ingress address (its load balancer IP), if it has one. */
export function ingressIp(s: PyoState, clusterId: string) {
  return s.clusters
    .find((c) => c.id === clusterId)
    ?.capabilities.loadBalancerIPs.find((ip) =>
      /^\d+\.\d+\.\d+\.\d+$/.test(ip),
    );
}

/** "hello-storefront.100-95-104-0.nip.io" */
export function freeHostname(
  app: Pick<Application, "name">,
  namespace: string,
  ip: string,
) {
  return `${`${app.name}-${namespace}`.slice(0, 63).replace(/-+$/, "")}.${ip.replaceAll(".", "-")}.nip.io`;
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
  const m = /\.(\d+)-(\d+)-\d+-\d+\.(nip|sslip)\.io$/.exec(h);
  if (!m) return false;
  const [a, b] = [Number(m[1]), Number(m[2])];
  return (
    a === 10 ||
    a === 127 ||
    (a === 192 && b === 168) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 100 && b >= 64 && b <= 127)
  );
}
