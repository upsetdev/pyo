import {
  ArrowsInSimpleIcon,
  BinocularsIcon,
  CopySimpleIcon,
  GlobeSimpleIcon,
  type Icon,
} from "@phosphor-icons/react";
import type { ReactNode } from "react";
import {
  ENGINE_ICON,
  SIZES,
  dbState,
  engineDetail,
  sizeSpec,
  totals,
} from "~/components/pyo/Databases";
import { Sparkline } from "~/components/charts/Charts";
import { lastOf, perSec, size, storageOf, throughput, useDatabaseAnalytics } from "~/components/pyo/DatabaseAnalytics";
import { AppLink } from "~/components/ui/AppLink";
import {
  busLR,
  curveLR,
  curveTB,
  type Dir,
  GraphCanvas,
  type GraphEdge,
  GraphEdges,
  GraphGroup,
  GraphLabel,
  GraphNode,
  railTB,
} from "~/components/pyo/canvas/Graph";
import { Badge, Status } from "~/components/ui/Badge";
import { KeyValue, Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { dateTime } from "~/lib/pyo/format";
import { databaseHref, usePyo } from "~/lib/pyo/store";
import type { Database } from "~/lib/pyo/types";
import { useDatabase } from "./DatabaseLayout";
import { ImportPanel } from "./Import";

export function DatabaseOverview() {
  const d = useDatabase();
  const s = usePyo();
  if (!d) return null;
  const st = dbState(d);
  const cluster = s.clusters.find((c) => c.id === d.clusterId);
  const sizeName = SIZES.find((x) => x.value === d.size)?.label ?? d.size;
  const ha = d.instances > 1;
  const e = d.status?.endpoints ?? {};
  const endpoints: [string, string | undefined][] = [
    ["Primary", e.primary],
    ["Pooler", e.pooler],
    ["Replicas", e.replicas],
    // KV's reads go through its endpoint (REDIS_URL): no address of their own.
    ["Read replicas", d.engine === "postgres" ? e.read : undefined],
    ["Sentinel", e.sentinel],
    ["REST", e.rest],
  ];
  return (
    <div className="grid items-start gap-6 lg:grid-cols-[minmax(0,1fr)_19rem]">
      <div className="min-w-0 space-y-6">
        <ImportPanel d={d} />
        <Panel
          title={
            <span className="flex items-center gap-2">
              Topology
              {ha ? <Badge tone="info">HA</Badge> : null}
              <SharedServerWarning d={d} />
            </span>
          }
          actions={
            <Status
              tone={st.tone}
              label={d.status?.message ? `${st.label} · ${d.status.message}` : st.label}
              className="text-[12.5px]"
            />
          }
        >
          <Topology d={d} />
        </Panel>
        <Panel title="Addresses" description="Inside the cluster, for its services.">
          {endpoints.some(([, v]) => v) ? (
            <ul className="list-rows">
              {endpoints
                .filter(([, v]) => v)
                .map(([k, v]) => (
                  <li key={k} className="flex items-center justify-between gap-4 px-4 py-2.5 text-[13px]">
                    <span className="shrink-0 text-muted-foreground">{k}</span>
                    <span className="flex min-w-0 items-center gap-1">
                      <code className="truncate font-mono text-[12px]">{v}</code>
                      <CopyButton value={v!} label={`Copy the ${k.toLowerCase()} address`} />
                    </span>
                  </li>
                ))}
            </ul>
          ) : (
            <p className="px-4 py-6 text-center text-[12.5px] text-muted-foreground">
              Shown once it's running.
            </p>
          )}
        </Panel>
      </div>
      <div className="min-w-0 space-y-6">
      <Health d={d} />
      <Panel title="Details">
        <KeyValue
          className="py-1"
          items={[
            ["Engine", engineDetail(d)],
            ["Size", `${sizeName} · ${sizeSpec(d)}`],
            ["Storage", `${d.storageGi} GiB per instance`],
            ["Total resources", totals(d).compute],
            ["Total storage", totals(d).storage],
            ["Availability", ha ? `High availability · ${d.instances - 1} replicas` : "Single server"],
            ["Ready", `${d.status?.readyInstances ?? 0} of ${d.instances}`],
            [
              "Failovers",
              <AppLink key="f" href={`${databaseHref(d.engine, d.id, "logs")}?view=events`} className="hover:text-primary">
                {d.status?.failovers ?? 0}
              </AppLink>,
            ],
            [
              "Cluster",
              cluster ? (
                <AppLink href={`/clusters/${cluster.name}`} className="hover:text-primary">
                  {cluster.name}
                </AppLink>
              ) : (
                "—"
              ),
            ],
            ["Created", dateTime(d.createdAt)],
            ...(d.restoredFrom
              ? [
                  [
                    "Restored from",
                    <>
                      <AppLink href={databaseHref(d.engine, d.restoredFrom.sourceId)} className="hover:text-primary">
                        {d.restoredFrom.sourceName}
                      </AppLink>{" "}
                      at {d.restoredFrom.targetTime ? dateTime(Date.parse(d.restoredFrom.targetTime)) : "the latest"}
                    </>,
                  ] as [ReactNode, ReactNode],
                ]
              : []),
            ...(d.import?.state === "succeeded" ? [["Imported from", <span key="i" className="break-all">{d.import.from}</span>] as [ReactNode, ReactNode]] : []),
          ]}
        />
      </Panel>
      </div>
    </div>
  );
}

/** The last day at a glance: throughput, connections against the limit, storage. */
function Health({ d }: { d: Database }) {
  const q = useDatabaseAnalytics(d.id, "24h");
  const data = q.status === "success" ? q.data : null;
  if (!data?.times.length) return null;
  const t = throughput(data);
  const tx = t.lines[0].values ?? null;
  const conn = data.connections?.primary ?? null;
  const limit = data.connections?.limit ?? null;
  const st = storageOf(data);
  const rows: { label: string; value: string; values: (number | null)[] | null; domain?: [number, number] }[] = [
    { label: t.label, value: lastOf(tx) == null ? "—" : perSec(lastOf(tx)!), values: tx },
    {
      label: data.engine === "valkey" ? "Clients" : "Connections",
      value: lastOf(conn) == null ? "—" : `${Math.round(lastOf(conn)!)}${limit ? ` of ${limit}` : ""}`,
      values: conn,
      domain: limit ? [0, limit] : undefined,
    },
    { label: st.label, value: lastOf(st.values) == null ? "—" : size(lastOf(st.values)!), values: st.values },
  ];
  return (
    <Panel
      title="Last 24 hours"
      actions={
        <AppLink href={databaseHref(d.engine, d.id, "analytics")} className="text-[12.5px] text-muted-foreground hover:text-foreground">
          Analytics
        </AppLink>
      }
    >
      <ul className="list-rows">
        {rows.map((r) => (
          <li key={r.label}>
            <AppLink href={databaseHref(d.engine, d.id, "analytics")} className="flex items-center gap-3 px-4 py-2.5 text-[13px] transition-colors hover:bg-accent/30">
              <span className="min-w-0 flex-1">
                <span className="block text-[12px] text-muted-foreground">{r.label}</span>
                <span className="block truncate tabular-nums">{r.value}</span>
              </span>
              <span className="w-24 shrink-0">
                {r.values ? <Sparkline values={r.values} fill width={96} height={24} label={`${r.label}, last 24 hours`} domain={r.domain} /> : null}
              </span>
            </AppLink>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

/** HA with two or more instances on one server: losing it takes them all. */
function SharedServerWarning({ d }: { d: Database }) {
  const members = d.members ?? [];
  if (d.instances < 2 || members.length < 2) return null;
  const per = new Map<string, number>();
  for (const m of members) if (m.server) per.set(m.server, (per.get(m.server) ?? 0) + 1);
  const most = Math.max(0, ...per.values());
  if (most < 2) return null;
  return (
    <span className="text-[12px] font-normal text-warning">
      {most} of {members.length} instances on one server. Losing it stops the database.
    </span>
  );
}

/** The server an instance runs on, linked to the server when Pyo has it. */
function OnServer({ d, server }: { d: Database; server: string }) {
  const s = usePyo();
  if (!server) return null;
  const cluster = s.clusters.find((c) => c.id === d.clusterId);
  const known = cluster && s.servers.some((n) => n.clusterId === d.clusterId && n.name === server);
  return (
    <>
      {known ? (
        <AppLink href={`/clusters/${cluster.name}/${server}`} className="text-foreground hover:text-primary">
          {server}
        </AppLink>
      ) : (
        <span className="text-foreground">{server}</span>
      )}
    </>
  );
}

type GraphNode = {
  key: string;
  kind: "service" | "primary" | "replica" | "read";
  icon: Icon;
  title: string;
  name?: string;
  ready?: boolean;
  server?: string;
  /** Read replicas: seconds behind the primary. */
  lag?: number | null;
  x: number;
  y: number;
  w: number;
  h: number;
};
type Edge = { from: GraphNode; to: GraphNode; style: "solid" | "dashed" | "dotted" };

// Node sizes: instances, and the smaller entry points (pooler, REST, sentinel).
const INST_W = 200;
const INST_H = 96;
const COMPACT_H = 64;
const SVC_W = 152;
const SVC_H = 52;
const GAP_X = 64;
const GAP_Y = 12;
const GAP_TB = 44;
const PAD = 24;
const GROUP_PAD = 10;
const GROUP_LABEL = 20;

/**
 * Where each node goes: left to right (entry points, primary, replicas) or,
 * where that doesn't fit, top to bottom with replicas on a rail.
 */
function layout(d: Database, dir: "lr" | "tb") {
  const e = d.status?.endpoints ?? {};
  const members = d.members ?? [];
  const primaryM = members.find((m) => m.role === "primary");
  const replicaM = members.filter((m) => m.role !== "primary");
  // As reported (with their servers), else as many as it should have.
  const replicaCount = members.length ? replicaM.length : Math.max(0, d.instances - 1);
  const compact = replicaCount > 4;
  const rh = compact ? COMPACT_H : INST_H;
  const svc: { key: string; icon: Icon; title: string; name?: string; style: Edge["style"] }[] = [
    ...(d.engine === "postgres" && d.pooler ? [{ key: "pooler", icon: ArrowsInSimpleIcon, title: "Pooler", name: "PgBouncer", style: "solid" as const }] : []),
    // KV's endpoint (pyo-kv: Redis, and HTTP with the REST API) last: with
    // read replicas, its reads line runs down from it.
    ...(d.engine === "valkey" && replicaCount && e.sentinel ? [{ key: "sentinel", icon: BinocularsIcon, title: "Sentinel", name: "failover", style: "dotted" as const }] : []),
    ...(d.engine === "valkey" ? [{ key: "kv", icon: GlobeSimpleIcon, title: "Endpoint", name: d.rest ? "Redis · HTTP" : "Redis", style: "solid" as const }] : []),
  ];
  const nodes: GraphNode[] = [];
  const edges: Edge[] = [];
  const colH = (n: number, h: number) => (n ? n * h + (n - 1) * GAP_Y : 0);
  let width: number;
  let height: number;
  const primary: GraphNode = {
    key: "primary",
    kind: "primary",
    icon: ENGINE_ICON[d.engine],
    title: "Primary",
    name: primaryM?.name ?? d.status?.primary,
    ready: primaryM?.ready,
    server: primaryM?.server,
    x: 0,
    y: 0,
    w: INST_W,
    h: INST_H,
  };
  const replicas: GraphNode[] = Array.from({ length: replicaCount }, (_, i) => ({
    key: replicaM[i]?.name ?? `replica-${i}`,
    kind: "replica",
    icon: CopySimpleIcon,
    title: "Replica",
    name: replicaM[i]?.name,
    ready: replicaM[i]?.ready,
    server: replicaM[i]?.server,
    x: 0,
    y: 0,
    w: INST_W,
    h: rh,
  }));
  const services: GraphNode[] = svc.map((f) => ({ ...f, kind: "service", x: 0, y: 0, w: SVC_W, h: SVC_H }));
  // Dedicated read replicas: a group of their own, behind their own pooler (Postgres).
  const rr = d.readReplicas;
  const readCount = rr ? Math.max(rr.instances, 0) : 0;
  const reads: GraphNode[] = Array.from({ length: readCount }, (_, i) => {
    const m = rr?.members[i];
    return { key: m?.name ?? `read-${i}`, kind: "read", icon: CopySimpleIcon, title: "Read replica", name: m?.name, ready: m?.ready, server: m?.server, lag: m?.lagSeconds, x: 0, y: 0, w: INST_W, h: COMPACT_H };
  });
  const readPooler: GraphNode | null =
    readCount && d.engine === "postgres" ? { key: "read-pooler", kind: "service", icon: ArrowsInSimpleIcon, title: "Read pooler", name: "PgBouncer", x: 0, y: 0, w: SVC_W, h: SVC_H } : null;
  const groupH = readCount ? GROUP_LABEL + colH(readCount, COMPACT_H) + 2 * GROUP_PAD : 0;
  let group: { x: number; y: number; w: number; h: number; text: string } | null = null;
  const extra: { key: string; d: string; p1: number[]; p2: number[]; style: Edge["style"] }[] = [];
  const notes: { x: number; y: number; text: string }[] = [];
  // The group's frame sits GROUP_PAD outside its column.
  const placeReads = (gx: number, gy: number) => {
    group = {
      x: gx - GROUP_PAD,
      y: gy,
      w: INST_W + 2 * GROUP_PAD,
      h: groupH,
      text: `${readCount} read ${readCount === 1 ? "replica" : "replicas"} · reads only`,
    };
    let y = gy + GROUP_PAD + GROUP_LABEL;
    for (const n of reads) {
      Object.assign(n, { x: gx, y });
      y += COMPACT_H + GAP_Y;
    }
  };

  if (dir === "lr") {
    const cols = [services.length ? colH(services.length, SVC_H) : 0, INST_H, colH(replicaCount, rh)];
    // 22: room for the replicas' label above them.
    const top = PAD + (replicaCount ? 22 : 0);
    height = top + Math.max(...cols) + PAD;
    let x = PAD;
    const mid = top + Math.max(...cols) / 2;
    const leftX = x;
    if (services.length || readPooler) {
      let y = mid - cols[0] / 2;
      for (const n of services) {
        Object.assign(n, { x, y });
        y += SVC_H + GAP_Y;
      }
      x += SVC_W + GAP_X;
    }
    Object.assign(primary, { x, y: mid - INST_H / 2 });
    x += INST_W + GAP_X;
    const rightX = x;
    if (replicaCount || readCount) {
      let y = mid - cols[2] / 2;
      for (const n of replicas) {
        Object.assign(n, { x, y });
        y += rh + GAP_Y;
      }
      x += INST_W;
    } else x -= GAP_X;
    width = x + PAD;
    if (readCount) {
      // Below the rest: the group on the right, its pooler on the left.
      const gy = height - PAD + 28;
      placeReads(rightX, gy);
      const gMid = gy + groupH / 2;
      const px = primary.x + primary.w / 2;
      const ty = gy + 16;
      extra.push({ key: "to-reads", d: `M${px},${primary.y + primary.h} V${ty - 8} Q${px},${ty} ${px + 8},${ty} H${rightX - GROUP_PAD}`, p1: [px, primary.y + primary.h], p2: [rightX - GROUP_PAD, ty], style: "dashed" });
      if (readPooler) {
        Object.assign(readPooler, { x: leftX, y: gMid - SVC_H / 2 });
        extra.push({ key: "read-pooler", d: `M${leftX + SVC_W},${gMid} H${rightX - GROUP_PAD}`, p1: [leftX + SVC_W, gMid], p2: [rightX - GROUP_PAD, gMid], style: "solid" });
      }
      // KV: the endpoint sends reads to the read replicas.
      const rest = services.find((n) => n.key === "kv");
      if (rest) {
        const rx = rest.x + rest.w / 2;
        extra.push({ key: "rest-reads", d: `M${rx},${rest.y + rest.h} V${gMid - 8} Q${rx},${gMid} ${rx + 8},${gMid} H${rightX - GROUP_PAD}`, p1: [rx, rest.y + rest.h], p2: [rightX - GROUP_PAD, gMid], style: "dotted" });
        notes.push({ x: rx + 12, y: gMid - 18, text: "reads" });
      }
      width += GROUP_PAD;
      height = gy + groupH + PAD;
    }
  } else {
    const svcRow = services.length ? services.length * SVC_W + (services.length - 1) * GAP_Y : 0;
    const rail = 28;
    width = Math.max(svcRow, INST_W + (replicaCount || readCount ? rail : 0)) + PAD * 2;
    let y = PAD;
    if (services.length) {
      let x = (width - svcRow) / 2;
      for (const n of services) {
        Object.assign(n, { x, y });
        x += SVC_W + GAP_Y;
      }
      y += SVC_H + GAP_TB;
    }
    const left = (width - INST_W - (replicaCount || readCount ? rail : 0)) / 2;
    Object.assign(primary, { x: left, y });
    y += INST_H + GAP_TB - (replicaCount ? 4 : 0);
    if (replicaCount) {
      y += 22;
      for (const n of replicas) {
        Object.assign(n, { x: left + rail, y });
        y += rh + GAP_Y;
      }
      y -= GAP_Y;
    } else y -= GAP_TB;
    if (readCount) {
      // Below the rest, on the replicas' rail; its pooler in front.
      const gx = left + rail;
      y += GAP_TB - 4;
      if (readPooler) {
        Object.assign(readPooler, { x: gx + (INST_W - SVC_W) / 2, y });
        y += SVC_H + 28;
        const cx = gx + INST_W / 2;
        extra.push({ key: "read-pooler", d: `M${cx},${y - 28} V${y}`, p1: [cx, y - 28], p2: [cx, y], style: "solid" });
      }
      placeReads(gx, y);
      const x1 = primary.x + 14;
      const ty = y + 16;
      extra.push({ key: "to-reads", d: `M${x1},${primary.y + primary.h} V${ty - 4} Q${x1},${ty} ${x1 + 4},${ty} H${gx - GROUP_PAD}`, p1: [x1, primary.y + primary.h], p2: [gx - GROUP_PAD, ty], style: "dashed" });
      // KV: the endpoint's reads, down the right side into the group.
      const rest = services.find((n) => n.key === "kv");
      if (rest) {
        const ry = rest.y + rest.h / 2;
        const right = gx + INST_W + GROUP_PAD;
        const xr = Math.max(rest.x + rest.w, right) + 14;
        const ty2 = y + 16;
        extra.push({ key: "rest-reads", d: `M${rest.x + rest.w},${ry} H${xr - 8} Q${xr},${ry} ${xr},${ry + 8} V${ty2 - 8} Q${xr},${ty2} ${xr - 8},${ty2} H${right}`, p1: [rest.x + rest.w, ry], p2: [right, ty2], style: "dotted" });
        notes.push({ x: xr - 40, y: ty2 - 18, text: "reads" });
        width = Math.max(width, xr + 12);
      }
      y += groupH;
      width = Math.max(width, gx + INST_W + GROUP_PAD + PAD);
    }
    height = y + PAD;
  }
  nodes.push(...services, primary, ...replicas, ...(readPooler ? [readPooler] : []), ...reads);
  services.forEach((n, i) => edges.push({ from: n, to: primary, style: svc[i].style }));
  for (const r of replicas) edges.push({ from: primary, to: r, style: "dashed" });
  const label =
    replicaCount > 0
      ? {
          text: `${replicaCount} ${replicaCount === 1 ? "replica" : "replicas"} · ${d.engine === "postgres" ? "streaming" : "replicating"}`,
          x: replicas[0].x,
          y: replicas[0].y - 20,
        }
      : null;
  return { nodes, edges, width, height, compact, label, group: group as { x: number; y: number; w: number; h: number; text: string } | null, extra, notes };
}

/** A connector as the shared graph draws it. */
function toEdge(e: Edge, dir: Dir): GraphEdge {
  const key = `${e.from.key}-${e.to.key}`;
  if (e.from.kind === "service") return dir === "lr" ? curveLR(e.from, e.to, key, e.style) : curveTB(e.from, e.to, key, e.style);
  return dir === "lr" ? busLR(e.from, e.to, key, e.style) : railTB(e.from, e.to, e.from.x + 14, key, e.style);
}

/** What runs: the entry points (pooler, REST, sentinel), the primary, its replicas. */
function Topology({ d }: { d: Database }) {
  const spec = `${sizeSpec(d)} · ${d.storageGi} GiB`;
  return (
    <GraphCanvas build={(dir) => layout(d, dir)}>
      {(g, dir) => (
        <>
          {g.group ? <GraphGroup box={g.group} label={g.group.text} /> : null}
          <GraphEdges
            width={g.width}
            height={g.height}
            edges={[...g.edges.map((e) => toEdge(e, dir)), ...g.extra.map((e) => ({ ...e, p1: e.p1 as [number, number], p2: e.p2 as [number, number] }))]}
          />
          {g.label ? <GraphLabel x={g.label.x} y={g.label.y}>{g.label.text}</GraphLabel> : null}
          {g.notes.map((n) => (
            <GraphLabel key={n.text} x={n.x} y={n.y}>
              {n.text}
            </GraphLabel>
          ))}
          {g.nodes.map((n) => (
            <GraphNode
              key={n.key}
              box={n}
              icon={n.icon}
              role={n.title}
              name={n.kind === "service" ? n.name : (n.name ?? "starting…")}
              mono={n.kind !== "service"}
              ready={n.kind === "service" ? undefined : n.ready}
              small={n.kind === "service"}
              emphasis={n.kind === "primary"}
              server={
                n.server ? (
                  <>
                    <OnServer d={d} server={n.server} />
                    {n.lag != null ? ` · ${n.lag < 1 ? "<1" : Math.round(n.lag)}s behind` : null}
                  </>
                ) : undefined
              }
              footer={n.kind === "service" || n.kind === "read" || (n.kind === "replica" && g.compact) ? undefined : spec}
            />
          ))}
        </>
      )}
    </GraphCanvas>
  );
}
