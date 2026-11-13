// Whether a service's replicas fit on its servers. Pyo sets no maximum:
// replicas that don't fit wait until there's room. Mirrors the API's
// crates/pyo-api/src/capacity.rs, which says the same on the change.
import type { ClusterServer, Resources } from "./types";

/** Kubernetes counts replicas in 32 bits. */
export const MAX_REPLICAS = 2147483647;

type Room = { cpu: number; memoryGi: number };

/**
 * Free CPU (cores) and memory (GiB) on the servers a service can run on:
 * what they can hand out less what pods there ask for. Null when none of
 * the cluster's servers has said.
 */
export function freeRoom(
  servers: ClusterServer[],
  clusterId: string,
  placement: { servers?: string[]; serverTags?: string[]; gpu?: number },
): Room | null {
  const named = (placement.servers ?? []).filter(Boolean);
  const tags = placement.serverTags ?? [];
  const gpu = (placement.gpu ?? 0) > 0;
  let seen = false;
  const free = { cpu: 0, memoryGi: 0 };
  for (const n of servers) {
    if (n.clusterId !== clusterId || n.removal) continue;
    const alloc = n.allocatable;
    if (!alloc || alloc.cpu <= 0) continue;
    seen = true;
    const usable =
      n.status === "ready" &&
      n.schedulable &&
      !n.maintenance &&
      !n.builder &&
      (gpu || !n.gpuDedicated) &&
      (named.length === 0 || named.includes(n.name)) &&
      tags.every((t) => (n.tags ?? []).includes(t));
    if (!usable) continue;
    free.cpu += Math.max(0, alloc.cpu - (n.requested?.cpu ?? 0));
    free.memoryGi += Math.max(0, alloc.memoryGi - (n.requested?.memoryGi ?? 0));
  }
  return seen ? free : null;
}

/** "2.5", "30": one decimal under 10, whole numbers above. */
function amount(x: number) {
  if (x < 10) return String(Math.round(x * 10) / 10);
  return String(Math.round(x));
}

/**
 * The note for `replicas` asking `cpuRequest` millicores and `memoryRequest`
 * MiB each, when they won't fit: `running` already hold their share, and
 * `free` is the rest. Null when they fit.
 */
export function roomWarning(
  ask: "run" | "upTo",
  replicas: number,
  resources: Pick<Resources, "cpuRequest" | "memoryRequest">,
  running: number,
  free: Room | null,
): string | null {
  if (!free || replicas < 1) return null;
  const eachCpu = Math.max(0, resources.cpuRequest) / 1000;
  const eachMem = Math.max(0, resources.memoryRequest) / 1024;
  const own = Math.min(Math.max(0, running), replicas);
  const needCpu = eachCpu * replicas;
  const needMem = eachMem * replicas;
  const roomCpu = free.cpu + eachCpu * own;
  const roomMem = free.memoryGi + eachMem * own;
  const shortCpu = needCpu > roomCpu + 0.01;
  const shortMem = needMem > roomMem + 0.01;
  if (!shortCpu && !shortMem) return null;
  const [need, room] =
    shortCpu && shortMem
      ? [
          `${amount(needCpu)} CPU and ${amount(needMem)} GB of memory`,
          `${amount(roomCpu)} CPU and ${amount(roomMem)} GB`,
        ]
      : shortCpu
        ? [`${amount(needCpu)} CPU`, amount(roomCpu)]
        : [`${amount(needMem)} GB of memory`, `${amount(roomMem)} GB`];
  if (ask === "upTo")
    return `At ${replicas} replicas this service needs about ${need}. Your servers have ${room} free, so some would wait until there's room.`;
  if (replicas === 1)
    return `This replica needs about ${need}. Your servers have ${room} free, so it will wait until there's room.`;
  return `These ${replicas} replicas need about ${need}. Your servers have ${room} free, so some will wait until there's room.`;
}

/** The note for a service with `replicas` (and, when changing, these resources). */
export function serviceRoomWarning(
  servers: ClusterServer[],
  app: {
    clusterId: string;
    servers?: string[];
    serverTags?: string[];
    resources: Resources;
    workloadKind?: string;
    runtime: { readyReplicas: number };
  },
  replicas: number,
  ask: "run" | "upTo" = "run",
  resources: Resources = app.resources,
): string | null {
  if (app.workloadKind === "CronJob") return null;
  const free = freeRoom(servers, app.clusterId, {
    servers: app.servers,
    serverTags: app.serverTags,
    gpu: resources.gpu,
  });
  return roomWarning(ask, replicas, resources, app.runtime.readyReplicas, free);
}

/** How many replicas fit: the running ones plus what `free` holds. Null when they ask for nothing. */
export function fitting(
  resources: Pick<Resources, "cpuRequest" | "memoryRequest">,
  running: number,
  free: Room,
): number | null {
  const eachCpu = Math.max(0, resources.cpuRequest) / 1000;
  const eachMem = Math.max(0, resources.memoryRequest) / 1024;
  const per = (f: number, each: number) =>
    each > 0 ? Math.floor(f / each + 1e-6) : null;
  const a = per(free.cpu, eachCpu);
  const b = per(free.memoryGi, eachMem);
  if (a === null && b === null) return null;
  return Math.max(0, running) + Math.min(a ?? Infinity, b ?? Infinity);
}

/** Fewer than this never asks. */
export const CONFIRM_FROM = 20;

/** A change worth asking about first: to at least 20 replicas, and over 10 times what it had or twice what fits. */
export function needsConfirm(replicas: number, before: number, fits: number | null) {
  return (
    replicas >= CONFIRM_FROM &&
    replicas > before &&
    (replicas > 10 * Math.max(1, before) || (fits !== null && replicas > 2 * fits))
  );
}

/**
 * The question to ask before a very large change (the API holds it back
 * without `confirm`), or null when it needs none.
 */
export function bigChange(
  servers: ClusterServer[],
  app: {
    clusterId: string;
    servers?: string[];
    serverTags?: string[];
    resources: Resources;
    workloadKind?: string;
    runtime: { readyReplicas: number };
  },
  replicas: number,
  before: number,
  ask: "run" | "upTo" = "run",
  resources: Resources = app.resources,
): { title: string; body: string } | null {
  if (app.workloadKind === "CronJob") return null;
  const free = freeRoom(servers, app.clusterId, {
    servers: app.servers,
    serverTags: app.serverTags,
    gpu: resources.gpu,
  });
  const running = app.runtime.readyReplicas;
  const fits = free ? fitting(resources, running, free) : null;
  if (!needsConfirm(replicas, before, fits)) return null;
  const warning = roomWarning(ask, replicas, resources, running, free);
  return {
    title:
      ask === "upTo"
        ? `Autoscale up to ${replicas} replicas?`
        : `Scale to ${replicas} replicas?`,
    body:
      warning ??
      (before > 0 ? `That's more than 10 times the ${before} it has now.` : ""),
  };
}
