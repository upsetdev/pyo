/** Names that are pages under /clusters, so no cluster can have them. */
export const RESERVED_CLUSTER_NAMES = [
  "new",
  "workloads",
  "networking",
  "storage",
];

// A region: where a server (or a cluster, by default) is. Detected as
// "City, CC" (or just "CC", CC an ISO 3166 country code) from GeoIP, or a
// name of its own ("US East") that groups servers. Replicas spread across
// regions, traffic stays in one, and services can be pinned to one.

export function parseLocation(location: string): {
  city: string;
  country: string;
} {
  const m = /^(?:(.*?),\s*)?([A-Za-z]{2})$/.exec(location.trim());
  return m
    ? { city: (m[1] ?? "").trim(), country: m[2].toUpperCase() }
    : { city: location.trim(), country: "" };
}

export function formatLocation(city: string, country: string) {
  return city.trim() ? `${city.trim()}, ${country}` : country;
}

const names =
  typeof Intl !== "undefined" && "DisplayNames" in Intl
    ? new Intl.DisplayNames(["en"], { type: "region" })
    : null;

export function countryName(code: string) {
  try {
    return names?.of(code.toUpperCase()) ?? code;
  } catch {
    return code;
  }
}

/** "Yogyakarta, Indonesia", or just the country. */
export function locationLabel(location: string) {
  const { city, country } = parseLocation(location);
  const name = country ? countryName(country) : "";
  // "Singapore, SG" is just Singapore.
  if (city && name && city.toLowerCase() === name.toLowerCase()) return name;
  return city && name ? `${city}, ${name}` : name || city || "Region unknown";
}

/** A server's region: its own, or its cluster's. */
export function regionOf(
  n: { location?: string },
  cluster?: { location: string },
) {
  return n.location || cluster?.location || "";
}

/**
 * The flag for a region: its country code, or for a name of its own, the
 * country its servers are all in (none when they differ).
 */
export function regionCountry(
  region: string,
  nodes: { location?: string; publicCountry?: string; clusterId?: string }[] = [],
  clusters: { id: string; location: string }[] = [],
) {
  const cc = parseLocation(region).country;
  if (cc) return cc;
  const countries = new Set(
    nodes
      .filter(
        (n) =>
          (n.location ||
            clusters.find((c) => c.id === n.clusterId)?.location ||
            "") === region,
      )
      .map((n) => (n.publicCountry ?? "").toUpperCase()),
  );
  const [only] = [...countries];
  return countries.size === 1 && only && only.length === 2 ? only : "";
}

/**
 * The places a cluster's servers are in, with how many servers each; the
 * cluster's own location when it has no servers yet.
 */
export function placesOf(
  cluster: { id: string; location: string },
  nodes: { clusterId: string; location?: string }[],
) {
  const counts = new Map<string, number>();
  for (const n of nodes)
    if (n.clusterId === cluster.id)
      counts.set(
        n.location || cluster.location,
        (counts.get(n.location || cluster.location) ?? 0) + 1,
      );
  const places = [...counts].map(([location, servers]) => ({
    location,
    servers,
  }));
  return places.length
    ? places.sort(
        (a, b) => b.servers - a.servers || a.location.localeCompare(b.location),
      )
    : [{ location: cluster.location, servers: 0 }];
}
