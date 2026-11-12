import {
  ArrowsOutSimpleIcon,
  CaretRightIcon,
  CubeIcon,
  GlobeSimpleIcon,
  HardDrivesIcon,
} from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useMemo, useState, type ReactNode } from "react";
import { Dialog } from "~/components/ui/Dialog";
import { Flag } from "~/components/ui/Flag";
import { Panel } from "~/components/ui/Panel";
import { fetchMeta, fetchTraffic, type ServiceTraffic } from "~/lib/pyo/backend";
import { compact } from "~/lib/pyo/format";
import { locationLabel, regionCountry } from "~/lib/pyo/location";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import { usePyo } from "~/lib/pyo/store";
import type { Application } from "~/lib/pyo/types";
import { AppLink } from "~/components/ui/AppLink";
import { midIp, privateIp, tailnetIp } from "./Addresses";
import { copyText } from "~/lib/pyo/clipboard";
import { toast } from "~/lib/pyo/toast";
import { FIT_PAD, Viewport } from "./canvas/Viewport";
import { curveLR, type Dir, GraphCanvas, type GraphEdge, GraphEdges, GraphGroup, GraphNode, railTB } from "./canvas/Graph";

// A service's Traffic section: how requests reach it, and whether anything's
// wrong, in one card.
//
//   Header   "Traffic", the cluster's routing mode, and a one-line health
//            summary ("All good", "2 things worth a look").
//   Flow     entry server → replicas. The usual case (one region, at most
//            one entry server) is a compact row with the region's numbers;
//            when requests branch (several entry servers or regions) it's
//            the full canvas, which can open bigger. Domains live on their
//            own tab. With public traffic off, the entry is the server on
//            the tailnet (its Tailscale address), else the main server (its
//            private address).

// The canvas: an 8px rhythm. Inline, it's as tall as its graph (so it opens
// at or near 100%), up to MAX_HEIGHT; past that it zooms out to fit.
const MAX_HEIGHT = 720;
const COL_LABEL_H = 24;
const TOP = COL_LABEL_H + 16;
const BOX_X = 0;
const BOX_PAD = 16;
const SERVER_X = BOX_X + BOX_PAD;
const SERVER_W = 248;
const SERVER_H = 84;
const PILLS_X = SERVER_X + SERVER_W + 48;
const PILLS_W = 216;
const BOX_W = PILLS_X + PILLS_W + BOX_PAD - BOX_X;
const BOX_HEAD = 96;
const ROW_GAP = 8;
const REGION_GAP = 24;
const GROUP_H = 36;
/** Pills in a group's one row; more collapse into "+N". */
const MAX_PILLS = 4;

type Region = ServiceTraffic["regions"][number];
type Server = Region["servers"][number];
type Box = { x: number; y: number; w: number; h: number };
type Edge = { from: string; to: string; width: number; tone: "normal" | "bad"; flow: boolean };

const HOW: Record<"server" | "region" | "anywhere" | "any", string> = {
  server: "Its own replicas",
  region: "Its region's replicas",
  anywhere: "Other regions",
  any: "Any replica",
};

export function TrafficCanvas({ app }: { app: Application }) {
  const q = useQuery(`traffic:${app.id}`, () => fetchTraffic(app.id), {
    refreshMs: 15_000,
  });
  const t = q.status === "success" ? q.data : null;
  // Deployed, or already running (replicas reported) even without a record here.
  const deployed = !!app.currentDeploymentId || !!t?.regions.some((r) => r.replicas.length);
  const issues = t ? issuesOf(t) : null;
  const tail = useInsideEntry(t);
  const [big, setBig] = useState(false);
  const simple = t ? isSimple(t, tail) : true;
  return (
    <Panel
      title={
        <span className="inline-flex flex-wrap items-center gap-2">
          Traffic
          {t ? (
            <Link
              to="/$team/clusters/$clusterId/settings"
              params={{ team: currentSlug(), clusterId: t.cluster }}
              className="rounded-full bg-muted px-2 py-0.5 text-[11px] font-medium text-muted-foreground transition-colors hover:text-foreground"
              title="How the cluster routes requests"
            >
              {t.mode === "spread" ? "Global" : t.mode === "local" ? "Local" : "Regional"}
            </Link>
          ) : null}
        </span>
      }
      actions={
        !deployed ? (
          <Health tone="muted">Not deployed yet</Health>
        ) : issues ? (
          <Health tone={issues.down ? "bad" : issues.count ? "warn" : "ok"}>{issues.label}</Health>
        ) : null
      }
    >
      {q.status === "loading" ? (
        <Skeleton />
      ) : q.status === "error" ? (
        <p className="px-4 py-6 text-[13px] text-muted-foreground">Couldn't load traffic: {q.error}</p>
      ) : (
        <div>
          {!deployed ? (
            <p className="px-4 py-6 text-[13px] text-muted-foreground">
              Not deployed yet.
            </p>
          ) : simple ? (
            <CompactFlow t={q.data} tail={tail} address={internalAddress(app)} />
          ) : (
            <div className="relative p-3">
              <button
                type="button"
                onClick={() => setBig(true)}
                className="absolute right-5 top-5 z-10 inline-flex h-7 items-center gap-1.5 rounded-[8px] border border-border-strong bg-card px-2 text-[12px] font-medium text-muted-foreground shadow-sm hover:text-foreground"
              >
                <ArrowsOutSimpleIcon size={12} /> Expand
              </button>
              <Canvas t={q.data} tail={tail} />
            </div>
          )}
        </div>
      )}
      {t && !simple ? (
        <Dialog open={big} onClose={() => setBig(false)} title={`Traffic of ${app.name}`} width="max-w-[min(1200px,calc(100vw-2rem))]">
          <Canvas t={t} tail={tail} height={Math.max(480, Math.round((typeof window === "undefined" ? 800 : window.innerHeight) * 0.7))} />
        </Dialog>
      ) : null}
    </Panel>
  );
}

/**
 * The address other services in the project reach it at: its name and its
 * first port's Service port ("api:8080"), as the cluster's DNS resolves it.
 */
export function internalAddress(app: Pick<Application, "name" | "ports">) {
  const p = app.ports[0];
  if (!p) return app.name;
  const port = p.exposure === "loadbalancer" ? (p.publicPort ?? p.containerPort) : p.containerPort;
  return `${app.name}:${port}`;
}

/** The one-line health summary. */
function issuesOf(t: ServiceTraffic) {
  const reps = t.regions.flatMap((r) => r.replicas);
  const down = reps.length > 0 && !reps.some((x) => x.ready);
  const notReady = reps.filter((x) => !x.ready).length;
  const warned = new Set(t.warnings).size;
  // Replicas that aren't ready count as one thing, after the API's warnings.
  const count = warned + (notReady ? 1 : 0);
  const label = down
    ? "No healthy replica"
    : warned
      ? `${count} thing${count === 1 ? "" : "s"} worth a look`
      : notReady
        ? `${notReady} replica${notReady === 1 ? "" : "s"} not ready`
        : "All good";
  return { down, count, label };
}

/** Where requests enter with no public traffic: a server's Tailscale addresses (and MagicDNS name), else its private ones. */
type Inside = { v4?: string; v6?: string; name?: string; tailnet: boolean };

/**
 * With no server taking public traffic: the servers requests enter through,
 * with their addresses. Those on the tailnet (on Pyo's own cluster, Pyo's
 * own server: its `pyo-tailnet` Service), the main server's first; else the
 * cluster's main server with its private address.
 */
function useInsideEntry(t: ServiceTraffic | null): Map<string, Inside> {
  const s = usePyo();
  const meta = useQuery("meta", fetchMeta);
  const publicUrl = meta.status === "success" ? meta.data.publicUrl : null;
  return useMemo(() => {
    const out = new Map<string, Inside>();
    const all = t?.regions.flatMap((r) => r.servers) ?? [];
    if (!t || !all.length || all.some((x) => x.publicTraffic)) return out;
    const cluster = s.clusters.find((c) => c.id === t.cluster || c.name === t.cluster);
    const nodeOf = (sv: Server) => s.servers.find((x) => (!cluster || x.clusterId === cluster.id) && x.name === sv.name);
    const ipsOf = (sv: Server) => {
      const n = nodeOf(sv);
      return [...new Set([n?.internalIP, ...sv.ips, n?.externalIP, ...(n?.addresses ?? [])].filter((ip): ip is string => !!ip))];
    };
    const main = (sv: Server) => !!nodeOf(sv)?.roles.includes("control-plane");
    // Build-only servers take no requests.
    const eligible = all.filter((sv) => !nodeOf(sv)?.builder);
    const pick = (list: Server[]) => (list.some(main) ? list.filter(main) : list);
    const family = (ips: string[]) => ({ v4: ips.find((ip) => !ip.includes(":")), v6: ips.find((ip) => ip.includes(":")) });

    // On the tailnet: Pyo's own server, on Pyo's own cluster.
    const onTailnet = eligible.filter((sv) => ipsOf(sv).some(tailnetIp) && (!cluster?.sameCluster || main(sv)));
    if (onTailnet.length) {
      // Pyo's own tailnet name, from its address on the tailnet.
      const host = /^https?:\/\/([^/:]+)/.exec(publicUrl ?? "")?.[1] ?? "";
      const name = cluster?.sameCluster && host.endsWith(".ts.net") ? host : undefined;
      for (const sv of pick(onTailnet)) out.set(sv.name, { ...family(ipsOf(sv).filter(tailnetIp)), name, tailnet: true });
      return out;
    }
    // Else the main server, with its private address (else any it has).
    const list = pick(eligible.length ? eligible : all);
    for (const sv of list.length > 1 && !list.some(main) ? list.slice(0, 1) : list) {
      const ips = ipsOf(sv);
      out.set(sv.name, { ...family(ips.some(privateIp) ? ips.filter(privateIp) : ips), tailnet: false });
    }
    return out;
  }, [t, s.clusters, s.servers, publicUrl]);
}

/** A server that takes requests: public traffic, or (with none) the tailnet's or its network's. */
const isEntry = (sv: Server, tail: Map<string, Inside>) => sv.publicTraffic || tail.has(sv.name);

/** One region, at most one entry server. */
function isSimple(t: ServiceTraffic, tail: Map<string, Inside>) {
  const servers = t.regions.flatMap((r) => r.servers);
  return t.regions.length <= 1 && servers.filter((s) => isEntry(s, tail)).length <= 1;
}

function Health({ tone, children }: { tone: "ok" | "warn" | "bad" | "muted"; children: ReactNode }) {
  const dot = { ok: "bg-success", warn: "bg-warning", bad: "bg-destructive", muted: "bg-muted-foreground/50" }[tone];
  return (
    <span className="inline-flex items-center gap-1.5 text-[12.5px] font-medium">
      <span aria-hidden className={`h-2 w-2 rounded-full ${dot}`} />
      {children}
    </span>
  );
}

function Status({ tone, children }: { tone: "ok" | "warn" | "bad" | "muted"; children: ReactNode }) {
  const dot = { ok: "bg-success", warn: "bg-warning", bad: "bg-destructive", muted: "bg-muted-foreground/50" }[tone];
  const text = { ok: "text-muted-foreground", warn: "text-warning", bad: "text-destructive", muted: "text-muted-foreground" }[tone];
  return (
    <span className={`inline-flex shrink-0 items-center gap-1.5 text-[11.5px] font-medium ${text}`}>
      <span aria-hidden className={`h-1.5 w-1.5 rounded-full ${dot}`} />
      {children}
    </span>
  );
}

/**
 * An entry server's addresses, each on a line of its own (click to copy):
 * its public IPv4 and IPv6, or with no public traffic its Tailscale addresses
 * and MagicDNS name (else its private ones).
 */
function addressesOf(sv: Server, tail: Map<string, Inside>): [string, string][] {
  const ts = tail.get(sv.name);
  const rows: [string, string | null | undefined][] = ts
    ? [["IPv4", ts.v4], ["IPv6", ts.v6], ["Name", ts.name]]
    : [["IPv4", sv.ipv4], ["IPv6", sv.ipv6]];
  return rows.filter((x): x is [string, string] => !!x[1]);
}
const lineCount = (rows: unknown[]) => Math.max(1, rows.length);

function AddressLines({ rows, inside }: { rows: [string, string][]; inside?: boolean }) {
  if (!rows.length) return <span className="mt-0.5 block text-[10.5px] text-muted-foreground/80">{inside ? "No address" : "No public address"}</span>;
  return (
    <>
      {rows.map(([label, ip]) => (
        <span key={label} className="mt-0.5 flex min-w-0 items-center gap-1.5 text-[10.5px]">
          <span className="w-7 shrink-0 text-muted-foreground/70">{label}</span>
          <button
            type="button"
            title={`${ip} (click to copy)`}
            aria-label={`Copy ${ip}`}
            onClick={(e) => {
              e.stopPropagation();
              copyText(ip)
                .then(() => toast({ title: `Copied ${ip}` }))
                .catch(() => toast({ tone: "error", title: "Clipboard unavailable" }));
            }}
            className="min-w-0 truncate rounded font-mono text-muted-foreground hover:text-foreground focus-visible:outline focus-visible:outline-1 focus-visible:outline-primary"
          >
            {midIp(ip, 24)}
          </button>
        </span>
      ))}
    </>
  );
}

/** Marks an entry server that takes requests from inside: on the tailnet, or its private network. */
function InsideMark({ at }: { at: Inside | undefined }) {
  if (!at) return null;
  return (
    <span
      className="shrink-0 rounded-full bg-secondary px-1.5 py-px text-[9.5px] font-medium normal-case tracking-normal text-muted-foreground"
      title={at.tailnet ? "Takes requests from the tailnet" : "Takes requests on its private network"}
    >
      {at.tailnet ? "Tailnet" : "Private"}
    </span>
  );
}

/** A region as a flag; its name on hover and focus. */
function RegionFlag({ cc, region }: { cc: string; region: string }) {
  if (!region) return null;
  const label = locationLabel(region);
  return (
    <span role="img" tabIndex={0} aria-label={`Region: ${label}`} title={label} className="ml-auto inline-flex shrink-0 rounded-sm focus-visible:outline focus-visible:outline-1 focus-visible:outline-primary">
      {cc ? <Flag country={cc} size={12} /> : <GlobeSimpleIcon size={12} aria-hidden />}
    </span>
  );
}

/** The usual case: entry server → replicas, then the numbers. */
const F_SERVER_W = 272;
const F_REP_W = 200;
const F_H = 64;
const F_REP_H = 60;
const F_GAP_X = 64;
const F_GAP_Y = 10;
const F_PAD = 24;
const F_GROUP_PAD = 10;
const F_GROUP_LABEL = 22;
/** Replica cards shown; more are "+N". */
const F_MAX_REPS = 6;

/** `entry`: a server takes requests (else it starts at the replicas). */
function flowLayout(t: ServiceTraffic, dir: Dir, entry: boolean, ipLines = 1) {
  const r = t.regions[0];
  const reps = r?.replicas ?? [];
  const shown = reps.slice(0, reps.length > F_MAX_REPS ? F_MAX_REPS - 1 : F_MAX_REPS);
  const more = reps.length - shown.length;
  const rows = shown.length + (more ? 1 : 0);
  const repsH = rows ? rows * F_REP_H + (rows - 1) * F_GAP_Y : 24;
  const groupH = F_GROUP_LABEL + repsH + 2 * F_GROUP_PAD;
  const groupW = F_REP_W + 2 * F_GROUP_PAD;
  // Taller with an address line per family.
  const server: Box = { x: 0, y: 0, w: F_SERVER_W, h: F_H + 16 * (ipLines - 1) };
  let group: Box;
  let width: number;
  let height: number;
  const repBoxes: Box[] = [];
  if (dir === "lr") {
    height = Math.max(entry ? server.h : 0, groupH) + 2 * F_PAD;
    const mid = height / 2;
    Object.assign(server, { x: F_PAD, y: mid - server.h / 2 });
    group = { x: entry ? server.x + F_SERVER_W + F_GAP_X : F_PAD, y: mid - groupH / 2, w: groupW, h: groupH };
    width = group.x + groupW + F_PAD;
  } else {
    const rail = 28;
    width = (entry ? Math.max(F_SERVER_W, groupW + rail) : groupW) + 2 * F_PAD;
    Object.assign(server, { x: F_PAD, y: F_PAD });
    group = entry
      ? { x: F_PAD + rail, y: server.y + server.h + 36, w: groupW, h: groupH }
      : { x: F_PAD, y: F_PAD, w: groupW, h: groupH };
    height = group.y + groupH + F_PAD;
  }
  let y = group.y + F_GROUP_PAD + F_GROUP_LABEL;
  for (let i = 0; i < rows; i++) {
    repBoxes.push({ x: group.x + F_GROUP_PAD, y, w: F_REP_W, h: F_REP_H });
    y += F_REP_H + F_GAP_Y;
  }
  return { width, height, server, group, repBoxes, shown, more };
}

function CompactFlow({ t, tail, address }: { t: ServiceTraffic; tail: Map<string, Inside>; address: string }) {
  const s = usePyo();
  const r = t.regions[0];
  const server = r?.servers.find((x) => isEntry(x, tail));
  const rows = server ? addressesOf(server, tail) : [];
  const cc = r ? regionCountry(r.name, s.servers, s.clusters) : "";
  const reps = r?.replicas ?? [];
  const healthy = reps.filter((x) => x.ready).length;
  const live = (r?.stats?.requests ?? 0) > 0;
  return (
    <div>
      <GraphCanvas build={(dir) => flowLayout(t, dir, !!server, lineCount(rows))}>
        {(g, dir) => {
          const edges: GraphEdge[] = [
            ...(server && reps.length
              ? [
                  {
                    ...(dir === "lr" ? curveLR(g.server, g.group, "srv-reps") : railTB(g.server, g.group, g.server.x + 14, "srv-reps", "solid", 4)),
                    flow: live,
                    tone: healthy ? ("normal" as const) : ("bad" as const),
                  },
                ]
              : []),
          ];
          const moreBox = g.repBoxes[g.shown.length];
          return (
            <>
              <GraphGroup
                box={g.group}
                label={
                  <span className="inline-flex items-center gap-1.5">
                    {reps.length} replica{reps.length === 1 ? "" : "s"}
                    <Status tone={!reps.length ? "muted" : healthy === reps.length ? "ok" : healthy ? "warn" : "bad"}>
                      {!reps.length ? "None running" : `${healthy} healthy`}
                    </Status>
                  </span>
                }
              />
              <GraphEdges edges={edges} width={g.width} height={g.height} />
              {server ? (
                <GraphNode
                  box={g.server}
                  icon={HardDrivesIcon}
                  emphasis={server.ready}
                  tone={server.ready ? undefined : "bad"}
                  role={
                    <>
                      Entry server
                      <InsideMark at={tail.get(server.name)} />
                      <RegionFlag cc={cc} region={r?.name ?? ""} />
                    </>
                  }
                  name={
                    <>
                      <AppLink href={`/clusters/${t.cluster}/${server.name}`} className="hover:text-primary" title={`Open ${server.name}`}>
                        {server.name}
                      </AppLink>
                      {!server.ready ? <span className="ml-1.5 font-sans text-[11px] text-destructive">Offline</span> : null}
                    </>
                  }
                  ready={server.ready}
                >
                  <AddressLines rows={rows} inside={tail.has(server.name)} />
                </GraphNode>
              ) : null}
              {g.shown.map((x, i) => (
                <GraphNode
                  key={x.name}
                  box={g.repBoxes[i]}
                  icon={CubeIcon}
                  small
                  name={<span title={x.name}>{x.name}</span>}
                  detail={<span className="block truncate font-mono text-[11px] text-muted-foreground" title={address}>{address}</span>}
                  ready={x.ready}
                  server={x.server || undefined}
                />
              ))}
              {g.more && moreBox ? (
                <div
                  className="absolute flex items-center rounded-xl border border-dashed border-border-strong px-3 text-[12px] text-muted-foreground"
                  style={{ left: moreBox.x, top: moreBox.y, width: moreBox.w, height: moreBox.h }}
                  title={reps.slice(g.shown.length).map((x) => x.name).join(", ")}
                >
                  +{g.more} more
                </div>
              ) : null}
            </>
          );
        }}
      </GraphCanvas>
      {r ? (
        <div className="border-t border-divider px-4 py-3">
          <Numbers t={t} r={r} />
        </div>
      ) : null}
    </div>
  );
}

/** A region's numbers as small aligned stats, with the 24h sparkline. */
function Numbers({ t, r }: { t: ServiceTraffic; r: Region }) {
  const st = r.stats;
  if (!t.statsAvailable)
    return (
      <p className="text-[12px] text-muted-foreground">
        Request numbers need observability storage.{" "}
        <Link to="/settings/observability" className="font-medium text-foreground hover:text-primary">
          Set it up
        </Link>
      </p>
    );
  return (
    <div className="flex flex-wrap items-end gap-x-8 gap-y-2">
      <Stat label={`Requests · ${t.statsHours}h`} value={st ? compact(st.requests) : "0"} />
      <Stat label="Errors" value={st && st.requests ? `${((st.errors / st.requests) * 100).toFixed(1)}%` : "0%"} bad={!!st && st.requests > 0 && st.errors / st.requests > 0.05} />
      <Stat label="p95" value={st && st.requests ? `${Math.round(st.p95Ms)} ms` : "—"} />
      {r.series && r.series.some((x) => x > 0) ? <Sparkline values={r.series} /> : null}
    </div>
  );
}

function Skeleton() {
  return (
    <div aria-busy aria-label="Loading traffic">
      <div className="flex gap-3 px-4 py-4">
        <div className="h-14 flex-1 animate-pulse rounded-[10px] bg-muted" />
        <div className="h-14 flex-1 animate-pulse rounded-[10px] bg-muted" />
        <div className="h-14 flex-1 animate-pulse rounded-[10px] bg-muted" />
      </div>
    </div>
  );
}

function layout(t: ServiceTraffic, tail: Map<string, Inside>) {
  const boxes = new Map<string, Box>();
  const regions: (Box & { r: Region })[] = [];
  let y = TOP;

  for (const r of t.regions) {
    const top = y;
    let sy = top + BOX_HEAD;
    for (const s of r.servers) {
      const h = isEntry(s, tail) ? SERVER_H + 16 * (lineCount(addressesOf(s, tail)) - 1) : 52;
      boxes.set(`srv:${s.name}`, { x: SERVER_X, y: sy, w: SERVER_W, h });
      // Its replicas, as one group beside it.
      boxes.set(`pills:${s.name}`, { x: PILLS_X, y: sy + (h - GROUP_H) / 2, w: PILLS_W, h: GROUP_H });
      sy += h + ROW_GAP;
    }
    // Replicas on a server that isn't listed: one muted group of their own.
    if (r.replicas.some((x) => !r.servers.some((s) => s.name === x.server))) {
      boxes.set(`pills:?${r.name}`, { x: PILLS_X, y: sy, w: PILLS_W, h: GROUP_H });
      sy += GROUP_H + ROW_GAP;
    }
    const h = Math.max(sy - top - ROW_GAP + BOX_PAD, BOX_HEAD + 32);
    regions.push({ r, x: BOX_X, y: top, w: BOX_W, h });
    y = top + h + REGION_GAP;
  }
  const bottom = Math.max(y - REGION_GAP, TOP);

  // Lines: each server to the replicas it sends requests to (by the server
  // they're on).
  const all = t.regions.flatMap((r) => r.servers);
  const total = all.reduce((n, s) => n + (s.stats?.requests ?? 0), 0);
  const share = (s: Server | undefined) => (total && s?.stats ? s.stats.requests / total : null);
  const width = (sh: number | null) => (sh === null ? 1.5 : 1 + 3 * sh);
  const replicas = t.regions.flatMap((r) => r.replicas);
  const edges: Edge[] = [];
  // The tailnet's entry sends to any replica.
  const sendsTo = (s: Server) => s.sendsTo ?? (tail.has(s.name) ? { how: "any" as const, replicas: replicas.map((x) => x.name) } : null);
  for (const s of all) {
    const to = sendsTo(s);
    if (!to) continue;
    // One line per group it sends to.
    const groupOf = (n: string) => {
      const rep = replicas.find((x) => x.name === n);
      if (!rep) return null;
      return all.some((x) => x.name === rep.server) ? `pills:${rep.server}` : `pills:?${t.regions.find((r) => r.replicas.includes(rep))?.name ?? ""}`;
    };
    const on = [...new Set(to.replicas.map(groupOf).filter((x): x is string => !!x))];
    for (const target of on) {
      const members = replicas.filter((x) => groupOf(x.name) === target);
      const healthy = members.some((x) => x.ready);
      edges.push({
        from: `srv:${s.name}`,
        to: target,
        width: width(share(s)) / Math.max(1, on.length / 2),
        tone: healthy ? "normal" : "bad",
        flow: (s.stats?.requests ?? 0) > 0,
      });
    }
  }
  return { boxes, regions, edges, sendsTo, width: BOX_X + BOX_W, height: bottom + 8, total };
}

function lit(id: string, edges: Edge[]) {
  const down = new Set([id]);
  const up = new Set([id]);
  for (const [set, fwd] of [
    [down, true],
    [up, false],
  ] as const) {
    let grew = true;
    while (grew) {
      grew = false;
      for (const e of edges) {
        const [a, b] = fwd ? [e.from, e.to] : [e.to, e.from];
        if (set.has(a) && !set.has(b)) {
          set.add(b);
          grew = true;
        }
      }
    }
  }
  return new Set([...down, ...up]);
}

function Canvas({ t, tail, height }: { t: ServiceTraffic; tail: Map<string, Inside>; height?: number }) {
  const s = usePyo();
  const L = useMemo(() => layout(t, tail), [t, tail]);
  // The graph's own height plus the viewport's padding, capped.
  const h = height ?? Math.min(MAX_HEIGHT, Math.ceil(L.height) + 2 * FIT_PAD + 8);
  const [focus, setFocus] = useState<string | null>(null);
  const [pinned, setPinned] = useState<string | null>(null);
  const [card, setCard] = useState<{ x: number; y: number; body: ReactNode } | null>(null);
  const active = pinned ?? focus;
  const on = useMemo(() => (active ? lit(active, L.edges) : null), [active, L.edges]);
  const dim = (id: string) => (on && !on.has(id) ? "opacity-40" : "");
  const hover = (id: string, b?: Box, body?: ReactNode) => ({
    onPointerEnter: () => {
      setFocus(id);
      if (b && body) setCard({ x: b.x, y: b.y + b.h + 6, body });
    },
    onPointerLeave: () => {
      setFocus(null);
      setCard(null);
    },
  });

  return (
    <Viewport
      label="Traffic canvas"
      height={h}
      width={L.width}
      contentHeight={L.height}
      onBackground={() => setPinned(null)}
      overlay={<Legend />}
    >
      {/* Column labels. */}
      {[
        [t.regions.some((r) => r.servers.some((x) => isEntry(x, tail))) ? "Entry servers" : "Servers", SERVER_X],
        ["Replicas", PILLS_X],
      ].map(([label, x]) => (
        <p key={label} className="absolute text-[11px] font-medium uppercase tracking-[0.06em] text-muted-foreground/70" style={{ left: x as number, top: 0 }}>
          {label}
        </p>
      ))}

      {L.regions.map(({ r, ...b }) => (
        <RegionBox key={`region:${r.name}`} r={r} b={b} t={t} cc={regionCountry(r.name, s.servers, s.clusters)} />
      ))}

      <GraphEdges
        width={L.width}
        height={L.height}
        edges={L.edges.flatMap((e) => {
          const a = L.boxes.get(e.from);
          const b = L.boxes.get(e.to);
          if (!a || !b) return [];
          const hot = on ? on.has(e.from) && on.has(e.to) : false;
          return [
            {
              ...curveLR(a, b, `${e.from}>${e.to}`),
              width: e.width,
              tone: e.tone === "bad" ? ("bad" as const) : hot ? ("hot" as const) : ("normal" as const),
              flow: e.flow,
              dim: !!on && !hot,
            },
          ];
        })}
      />



      {t.regions.flatMap((r) =>
        r.servers.map((sv) => {
          const b = L.boxes.get(`srv:${sv.name}`)!;
          const id = `srv:${sv.name}`;
          const regionTotal = r.servers.reduce((x, y) => x + (y.stats?.requests ?? 0), 0);
          const sh = sv.stats && regionTotal ? sv.stats.requests / regionTotal : null;
          const entry = isEntry(sv, tail);
          const to = L.sendsTo(sv);
          return (
            <GraphNode
              key={id}
              box={b}
              icon={HardDrivesIcon}
              small
              tone={!sv.ready ? "bad" : entry ? undefined : "muted"}
              role={
                <>
                  {entry ? "Entry server" : "Server"}
                  <InsideMark at={tail.get(sv.name)} />
                  {!sv.ready ? <Badge tone="bad">Offline</Badge> : null}
                  {sh !== null && entry ? <span className="ml-auto shrink-0 normal-case tracking-normal tabular-nums">{Math.round(sh * 100)}%</span> : null}
                </>
              }
              name={
                <AppLink href={`/clusters/${t.cluster}/${sv.name}`} className="hover:text-primary" title={`Open ${sv.name}`}>
                  {sv.name}
                </AppLink>
              }
              ready={sv.ready}
              data-node={id}
              {...hover(id)}
              className={dim(id)}
            >
              {/* Address and routing on lines of their own, so neither is cut. */}
              {entry ? (
                <>
                  <AddressLines rows={addressesOf(sv, tail)} inside={tail.has(sv.name)} />
                  <span className="flex items-center gap-2">
                    <span className="truncate text-[10.5px] text-muted-foreground">{to ? (to.how === "anywhere" && t.mode === "local" ? HOW.any : HOW[to.how]) : "Not routing yet"}</span>
                    {sh !== null ? (
                      <span className="ml-auto h-1 w-10 shrink-0 overflow-hidden rounded-full bg-foreground/10">
                        <span className="block h-full rounded-full bg-primary/60" style={{ width: `${Math.max(4, sh * 100)}%` }} />
                      </span>
                    ) : null}
                  </span>
                </>
              ) : (
                <span className="block text-[10.5px] text-muted-foreground">No public traffic</span>
              )}
            </GraphNode>
          );
        }),
      )}

      {t.regions.flatMap((r) => {
        const groups: { id: string; label: string; muted: boolean; reps: Region["replicas"] }[] = r.servers.map((sv) => ({
          id: `pills:${sv.name}`,
          label: sv.name,
          muted: !isEntry(sv, tail),
          reps: r.replicas.filter((x) => x.server === sv.name),
        }));
        const orphans = r.replicas.filter((x) => !r.servers.some((sv) => sv.name === x.server));
        if (orphans.length) groups.push({ id: `pills:?${r.name}`, label: "Other servers", muted: true, reps: orphans });
        return groups.map((g) => {
          const b = L.boxes.get(g.id)!;
          const shown = g.reps.slice(0, g.reps.length > MAX_PILLS ? MAX_PILLS - 1 : MAX_PILLS);
          return (
            <div
              key={g.id}
              data-node={g.id}
              {...hover(g.id)}
              className={`absolute flex items-center gap-1.5 overflow-hidden rounded-lg border px-1.5 ${g.muted ? "border-dashed border-frame-border bg-transparent" : "border-frame-border bg-card/70 dark:bg-muted"} ${dim(g.id)}`}
              style={{ left: b.x, top: b.y, width: b.w, height: b.h }}
              title={g.muted ? `On ${g.label}` : undefined}
            >
              {g.reps.length ? (
                <>
                  {shown.map((x) => {
                    const short = x.name.split("-").pop() ?? x.name;
                    return (
                      <span
                        key={x.name}
                        tabIndex={0}
                        onPointerEnter={() =>
                          setCard({
                            x: b.x,
                            y: b.y + b.h + 4,
                            body: (
                              <span className="block space-y-0.5">
                                <span className="block font-mono text-[11.5px]">{x.name}</span>
                                <span className="flex items-center gap-1.5 text-muted-foreground">
                                  <Dot tone={x.ready ? "ok" : "warn"} /> {x.ready ? "Healthy" : "Starting or not ready"} · on {x.server || "an unknown server"}
                                </span>
                              </span>
                            ),
                          })
                        }
                        onPointerLeave={() => setCard(null)}
                        className={`inline-flex h-6 shrink-0 items-center gap-1.5 rounded-full border border-card-border bg-card px-2 font-mono text-[11px] ${g.muted ? "opacity-70" : "shadow-sm"}`}
                        aria-label={`${x.name}: ${x.ready ? "healthy" : "not ready"}`}
                      >
                        <Dot tone={x.ready ? "ok" : "warn"} />
                        {short}
                      </span>
                    );
                  })}
                  {g.reps.length > shown.length ? (
                    <span className="inline-flex h-6 shrink-0 items-center rounded-full bg-secondary px-2 text-[11px] text-muted-foreground" title={g.reps.slice(shown.length).map((x) => x.name).join(", ")}>
                      +{g.reps.length - shown.length}
                    </span>
                  ) : null}
                  {g.muted ? <span className="ml-auto truncate pr-1 text-[10.5px] text-muted-foreground/70">{g.label === "Other servers" ? g.label : `on ${g.label}`}</span> : null}
                </>
              ) : (
                <span className="px-1 text-[11px] text-muted-foreground/70">No replica here</span>
              )}
            </div>
          );
        });
      })}

      {card ? (
        <div className="pointer-events-none absolute z-20 max-w-[280px] rounded-lg border border-border-strong bg-card px-3 py-2 text-[12px] shadow-lg" style={{ left: card.x, top: card.y }}>
          {card.body}
        </div>
      ) : null}
    </Viewport>
  );
}

/** A region: flag and name, health, and its numbers with a 24h sparkline. */
function RegionBox({ r, b, t, cc }: { r: Region; b: Box; t: ServiceTraffic; cc: string }) {
  const healthy = r.replicas.filter((x) => x.ready).length;
  const entries = r.servers.filter((s) => s.publicTraffic && s.ready).length;
  const none = entries > 0 && r.replicas.length > 0 && !healthy;
  const st = r.stats;
  return (
    <section
      aria-label={r.name ? locationLabel(r.name) : t.cluster}
      className="absolute rounded-[14px] border border-dashed border-border-strong bg-card/30"
      style={{ left: b.x, top: b.y, width: b.w, height: b.h }}
    >
      <div className="flex items-center gap-2 px-4 pt-3">
        {cc ? <Flag country={cc} size={14} /> : null}
        <p className="truncate text-[13px] font-semibold">{r.name ? locationLabel(r.name) : t.cluster}</p>
        <Badge tone={!r.replicas.length ? "muted" : healthy === r.replicas.length ? "ok" : healthy ? "warn" : "bad"}>
          {r.replicas.length ? `${healthy}/${r.replicas.length} healthy` : "No replicas"}
        </Badge>
        {none ? <Badge tone="bad">Requests go elsewhere</Badge> : null}
      </div>
      <div className="flex items-end gap-6 px-4 pt-3">
        {!t.statsAvailable ? (
          <p className="text-[11px] text-muted-foreground">Request stats need observability storage</p>
        ) : (
          <>
            <Stat label="Requests" value={st ? compact(st.requests) : "0"} />
            <Stat label="Errors" value={st && st.requests ? `${((st.errors / st.requests) * 100).toFixed(1)}%` : "0%"} bad={!!st && st.requests > 0 && st.errors / st.requests > 0.05} />
            <Stat label="p95" value={st && st.requests ? `${Math.round(st.p95Ms)} ms` : "—"} />
            {r.series && r.series.some((x) => x > 0) ? (
              <span className="flex flex-col">
                <span className="text-[10px] uppercase tracking-[0.06em] text-muted-foreground/80">24h</span>
                <Sparkline values={r.series} />
              </span>
            ) : null}
          </>
        )}
      </div>
    </section>
  );
}

function Stat({ label, value, bad }: { label: string; value: string; bad?: boolean }) {
  return (
    <span className="flex flex-col">
      <span className="text-[10px] uppercase tracking-[0.06em] text-muted-foreground/80">{label}</span>
      <span className={`text-[12.5px] font-medium tabular-nums ${bad ? "text-destructive" : ""}`}>{value}</span>
    </span>
  );
}

function Sparkline({ values }: { values: number[] }) {
  const w = 96;
  const h = 22;
  const max = Math.max(1, ...values);
  const pts = values.map((v, i) => `${(i / (values.length - 1)) * w},${h - (v / max) * (h - 2) - 1}`).join(" ");
  return (
    <svg width={w} height={h} className="text-primary/70" aria-label="Requests over the last 24 hours" role="img">
      <polyline points={pts} fill="none" stroke="currentColor" strokeWidth={1.25} strokeLinejoin="round" />
    </svg>
  );
}

function Badge({ tone, children }: { tone: "ok" | "warn" | "bad" | "muted"; children: ReactNode }) {
  const cls = {
    ok: "bg-success/12 text-success",
    warn: "bg-warning/15 text-warning",
    bad: "bg-destructive/12 text-destructive",
    muted: "bg-secondary text-muted-foreground",
  }[tone];
  return <span className={`inline-flex w-fit shrink-0 items-center rounded-full px-1.5 py-px text-[10.5px] font-medium ${cls}`}>{children}</span>;
}

function Dot({ tone }: { tone: "ok" | "warn" | "bad" }) {
  return <span aria-hidden className={`h-1.5 w-1.5 shrink-0 rounded-full ${tone === "ok" ? "bg-success" : tone === "warn" ? "bg-warning" : "bg-destructive"}`} />;
}

function Legend() {
  return (
    <div data-overlay className="pointer-events-none absolute bottom-2 left-2 flex items-center gap-3 rounded-md border border-border-strong bg-card/90 px-2.5 py-1 text-[10.5px] text-muted-foreground">
      <span className="flex items-center gap-1.5">
        <svg width="18" height="6" aria-hidden><line x1="0" y1="3" x2="18" y2="3" className="stroke-foreground/40" strokeWidth="2" /></svg>
        Requests
      </span>
      <span className="flex items-center gap-1.5">
        <svg width="18" height="6" aria-hidden><line x1="0" y1="3" x2="18" y2="3" className="stroke-destructive/70" strokeWidth="2" /></svg>
        Unhealthy
      </span>
    </div>
  );
}
