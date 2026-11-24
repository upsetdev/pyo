import { servicesIn, type PyoState } from "./store";
import type { Application, Domain } from "./types";

// The team's hostnames, each with the domain entries that use it: the
// team's Domains page lists hostnames, not entries.

export type HostEntry = { d: Domain; a: Application };

export type Host = {
  hostname: string;
  /** Every entry for it, oldest first. */
  entries: HostEntry[];
  /** The one with its DNS record, certificate and tunnel. */
  primary: HostEntry;
};

/** The team's hostnames, in order. */
export function hostsOf(s: PyoState): Host[] {
  const by = new Map<string, HostEntry[]>();
  for (const a of servicesIn(s))
    for (const d of a.domains) {
      const key = d.hostname.toLowerCase();
      by.set(key, [...(by.get(key) ?? []), { d, a }]);
    }
  return [...by.entries()]
    .map(([hostname, entries]) => {
      const sorted = [...entries].sort((x, y) => x.d.createdAt - y.d.createdAt);
      return { hostname, entries: sorted, primary: sorted[0] };
    })
    .sort((x, y) => x.hostname.localeCompare(y.hostname));
}

/** One hostname of the team, or undefined. */
export function hostOf(s: PyoState, hostname: string): Host | undefined {
  const want = hostname.toLowerCase();
  return hostsOf(s).find((h) => h.hostname === want);
}
