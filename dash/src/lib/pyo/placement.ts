// Where a service runs: any server of its cluster, or only the servers
// picked and every server with a tag picked (tagged now or later). Mirrors
// the API's crates/pyo-api/src/placement.rs, which refuses the same.
import type { ClusterServer } from "./types";

export type Placement = { servers: string[]; tags: string[] };

/** A service's placement. Services saved before kept `servers` and `serverTags`. */
export function placementOf(app: {
  placement?: Partial<Placement> | null;
  servers?: string[];
  serverTags?: string[];
}): Placement {
  if (app.placement !== undefined)
    return {
      servers: app.placement?.servers ?? [],
      tags: app.placement?.tags ?? [],
    };
  return {
    servers: (app.servers ?? []).filter(Boolean),
    tags: app.serverTags ?? [],
  };
}

export const isAny = (p: Placement) => !p.servers.length && !p.tags.length;

/** This server is picked, by name or by a tag it has. */
export function picks(p: Placement, n: Pick<ClusterServer, "name" | "tags">) {
  return (
    p.servers.includes(n.name) || p.tags.some((t) => (n.tags ?? []).includes(t))
  );
}

/** It may run on this server. */
export function takes(p: Placement, n: Pick<ClusterServer, "name" | "tags">) {
  return isAny(p) || picks(p, n);
}

export const samePlacement = (a: Placement, b: Placement) =>
  [...a.servers].sort().join() === [...b.servers].sort().join() &&
  [...a.tags].sort().join() === [...b.tags].sort().join();

/** What one replica needs. */
export type Need = { gpu: boolean; cpu: number; memoryGi: number };

/**
 * Why no server picked can run it, or null when one can. Any server is
 * never refused: its volume keeps it on its server, and a GPU service goes
 * to GPU servers by itself.
 */
export function placementProblem(
  p: Placement,
  servers: ClusterServer[],
  volume: { name: string; server: string } | null,
  need: Need,
  /** What its replicas ask for now on each server, given back when they move. */
  own: Record<string, { cpu: number; memoryGi: number }> = {},
): string | null {
  if (isAny(p)) return null;
  const missing = p.servers.find((n) => !servers.some((s) => s.name === n));
  if (missing) return `${missing} isn't a server of this cluster.`;
  const picked = servers.filter((s) => takes(p, s));
  if (!picked.length) return "Pick at least one server.";
  if (volume && !picked.some((s) => s.name === volume.server))
    return `Its volume lives on ${volume.server}, so it has to run there.`;
  let usable = picked.filter((s) => !s.builder);
  if (!usable.length) return "These servers only run builds.";
  if (need.gpu) {
    usable = usable.filter((s) => s.gpuDetected);
    if (!usable.length) return "It asks for a GPU. Pick a GPU server.";
  } else {
    usable = usable.filter((s) => !s.gpuDedicated);
    if (!usable.length) return "These servers only run services that use a GPU.";
  }
  // Room: only when every one of them has said what it has.
  if (usable.some((s) => !s.allocatable || s.allocatable.cpu <= 0)) return null;
  const fits = usable.some((s) => {
    const a = s.allocatable!;
    const o = own[s.name] ?? { cpu: 0, memoryGi: 0 };
    const cpu = a.cpu - (s.requested?.cpu ?? 0) + o.cpu;
    const mem = a.memoryGi - (s.requested?.memoryGi ?? 0) + o.memoryGi;
    return need.cpu <= cpu + 0.01 && need.memoryGi <= mem + 0.01;
  });
  return fits ? null : "None of these servers has room for it.";
}
