// TODO(tailnet-domains): backend
//
// A stand-in for tailnet domains until the API has them, so the dashboard
// can be clicked through on any cluster:
// - refresh() gives every cluster without a tailnet the tailnet below, and
//   every cluster without a free domain IP a fake public IP (documentation
//   range), so both free kinds can be picked;
// - tailnet domains and nip.io domains on the fake IP live in this
//   browser's localStorage and never reach the API: saving or creating a
//   service keeps them here and sends the service without them. The fake
//   IP is only ever in the dashboard's own state.
// Disabled states: localStorage "pyo.stub.noTailnet" = "1" (not on
// Tailscale), "pyo.stub.noPublicIp" = "1" (no public IP).
//
// To remove: delete this file and every line marked TODO(tailnet-domains).

import { isTailnetDomain } from "./freeDomain";
import type { Application, Cluster, Domain } from "./types";

const KEY = "pyo.stub.tailnetDomains";
const TAILNET = "tail1234.ts.net";
const FAKE_IP = "203.0.113.7";
/** Tailscale's certificate takes a moment. */
const ISSUE_MS = 6000;

type Saved = Record<string, Domain[]>;

function load(): Saved {
  try {
    return JSON.parse(localStorage.getItem(KEY) ?? "{}") as Saved;
  } catch {
    return {};
  }
}

function save(v: Saved) {
  try {
    localStorage.setItem(KEY, JSON.stringify(v));
  } catch {
    // Private mode: they last until the next refresh.
  }
}

const flag = (name: string) => {
  try {
    return localStorage.getItem(`pyo.stub.${name}`) === "1";
  } catch {
    return false;
  }
};

/** Kept in the browser only: a tailnet domain, or a nip.io one on the fake IP. */
export const isStubDomain = (hostname: string) =>
  isTailnetDomain(hostname) || hostname.endsWith(`.${FAKE_IP}.nip.io`);

function stubCluster(c: Cluster): Cluster {
  const tailnet = c.tailnet ?? (flag("noTailnet") ? null : TAILNET);
  if (c.freeDomainIp || flag("noPublicIp")) return { ...c, tailnet };
  return {
    ...c,
    tailnet,
    publicIp: c.publicIp ?? FAKE_IP,
    publicTraffic: true,
    freeDomainAddress: "public",
    freeDomainIp: c.publicIp ?? FAKE_IP,
  };
}

/** The snapshot with tailnets on clusters and the saved tailnet domains on services. */
export function withTailnetStub<
  T extends { clusters?: Cluster[]; services?: Application[] },
>(snap: T): T {
  if (typeof window === "undefined") return snap;
  const saved = load();
  const now = Date.now();
  return {
    ...snap,
    clusters: snap.clusters?.map(stubCluster),
    services: snap.services?.map((a) =>
      saved[a.id]?.length
        ? {
            ...a,
            domains: [
              ...a.domains.filter((d) => !isStubDomain(d.hostname)),
              ...saved[a.id].map((d) => {
                const issued =
                  !isTailnetDomain(d.hostname) || now - d.createdAt > ISSUE_MS;
                return {
                  ...d,
                  status: "active" as const,
                  tls: issued ? ("issued" as const) : ("pending" as const),
                };
              }),
            ],
          }
        : a,
    ),
  };
}

/** Keep the service's stub domains here; the service to send, without them. */
export function takeTailnetDomains(app: Application): Application {
  const saved = load();
  const mine = app.domains.filter((d) => isStubDomain(d.hostname));
  if (mine.length) saved[app.id] = mine;
  else delete saved[app.id];
  save(saved);
  return { ...app, domains: app.domains.filter((d) => !isStubDomain(d.hostname)) };
}
