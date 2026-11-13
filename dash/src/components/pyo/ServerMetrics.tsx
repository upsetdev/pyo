import { ObservabilityOff, useObservabilityOn } from "~/components/pyo/ObservabilityOff";
import { Layered } from "~/components/ui/Layered";
import { CpuIcon } from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import {
  Legend,
  SERIES,
  Sparkline,
  TimeSeriesChart,
} from "~/components/charts/Charts";
import { Dot } from "~/components/ui/Badge";
import { EmptyState, ErrorState, Panel, Skeleton } from "~/components/ui/Panel";
import {
  fetchMetrics,
  type MetricsEntity,
  type MetricsResponse,
} from "~/lib/pyo/backend";
import { cpu, mem, pct, rate } from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import { usePyo, clusterRef, serviceParams } from "~/lib/pyo/store";

// Server metrics: CPU and memory of every server and service over time, from the
// samples Pyo stores once a minute from each cluster's agent.

type Metric = "cpu" | "memory" | "disk" | "network" | "pods";

/** A sample, or null where none was taken (a gap in the chart). */
type V = number | null;
/** The latest sample there is. */
const last = (v: V[]) => {
  for (let i = v.length - 1; i >= 0; i--)
    if (v[i] != null) return v[i] as number;
  return 0;
};
function ratio(used: number, cap: number): number;
function ratio(used: V, cap: number): V;
function ratio(used: V, cap: number): V {
  return used == null ? null : cap > 0 ? used / cap : 0;
}
const any = (v: V[] | undefined) => !!v?.some((x) => x != null);
/** A server's disk use as a fraction of its root disk. */
const diskOf = (n: MetricsEntity): V[] =>
  (n.diskUsedGi ?? n.cpu.map(() => null)).map((v) =>
    v == null || !n.diskCapacityGi ? null : v / n.diskCapacityGi,
  );
/** Bytes per second in and out: the server's network cards (host metrics)
 *  when collected, else what the kubelet counts. */
const netOf = (n: MetricsEntity, dir: "rx" | "tx"): V[] => {
  const nic = dir === "rx" ? n.nicRxBps : n.nicTxBps;
  const kube = dir === "rx" ? n.netRxBps : n.netTxBps;
  return (any(nic) ? nic : kube) ?? n.cpu.map(() => null);
};

/** `inCluster`: shown on one cluster's page, so rows don't repeat its name. */
export function ServerMetrics(props: Parameters<typeof Metrics>[0]) {
  return useObservabilityOn() ? (
    <Metrics {...props} />
  ) : (
    <Panel>
      <ObservabilityOff />
    </Panel>
  );
}

function Metrics({
  team,
  cluster,
  range,
  inCluster = false,
}: {
  team: string;
  cluster: string;
  range: string;
  inCluster?: boolean;
}) {
  const q = useQuery(
    `metrics:${team}:${cluster}:${range}`,
    () => fetchMetrics({ team, cluster, range }),
    { refreshMs: 30_000 },
  );
  if (q.status === "loading")
    return (
      <div className="space-y-4" aria-busy aria-label="Loading metrics">
        <Skeleton className="h-[380px] rounded-xl" />
        <Skeleton className="h-60 rounded-xl" />
      </div>
    );
  if (q.status === "error")
    return (
      <Panel>
        <ErrorState
          title="Couldn't load metrics"
          description={q.error}
          onRetry={q.retry}
        />
      </Panel>
    );
  if (!q.data.times.length)
    return (
      <Panel>
        <EmptyState
          icon={CpuIcon}
          title="Collecting metrics"
          description="Shown once a cluster connects."
        />
      </Panel>
    );
  return <Loaded data={q.data} inCluster={inCluster} />;
}

function Loaded({
  data,
  inCluster,
}: {
  data: MetricsResponse;
  inCluster: boolean;
}) {
  const s = usePyo();
  const [metric, setMetric] = useState<Metric>("cpu");
  const { times, servers: nodes, services: apps, ingress = [] } = data;
  // A total over servers, with a gap only where none of them reported.
  const sum = (pick: (n: MetricsEntity) => V[]): V[] =>
    times.map((_, i) => {
      const vs = nodes
        .map((n) => pick(n)[i])
        .filter((v): v is number => v != null);
      return vs.length ? vs.reduce((a, v) => a + v, 0) : null;
    });
  const cpuCap = nodes.reduce((a, n) => a + n.cpuCapacity, 0);
  const memCap = nodes.reduce((a, n) => a + n.memoryCapacityMi, 0);
  const cpuUsed = sum((n) => n.cpu);
  const memUsed = sum((n) => n.memoryMi);
  const pods = nodes.reduce((a, n) => a + n.pods, 0);
  const diskNodes = nodes.filter((n) => any(n.diskUsedGi) && n.diskCapacityGi);
  const diskUsed = diskNodes.reduce((a, n) => a + last(n.diskUsedGi ?? []), 0);
  const diskCap = diskNodes.reduce((a, n) => a + (n.diskCapacityGi ?? 0), 0);
  const netNodes = nodes.filter((n) => any(netOf(n, "rx")) || any(netOf(n, "tx")));
  const rxNow = netNodes.reduce((a, n) => a + last(netOf(n, "rx")), 0);
  const txNow = netNodes.reduce((a, n) => a + last(netOf(n, "tx")), 0);
  const traffic = (n: MetricsEntity): V[] => {
    const rx = netOf(n, "rx");
    const tx = netOf(n, "tx");
    return rx.map((v, i) => (v == null && tx[i] == null ? null : (v ?? 0) + (tx[i] ?? 0)));
  };

  const tabs: { id: Metric; label: string; value: string; sub: string }[] = [
    {
      id: "cpu",
      label: "CPU",
      value: pct(ratio(last(cpuUsed), cpuCap)),
      sub: `${cpu(last(cpuUsed) * 1000)} of ${cpu(cpuCap * 1000)}`,
    },
    {
      id: "memory",
      label: "Memory",
      value: pct(ratio(last(memUsed), memCap)),
      sub: `${mem(last(memUsed))} of ${mem(memCap)}`,
    },
    {
      id: "disk",
      label: "Disk",
      value: diskNodes.length ? pct(diskUsed / diskCap) : "—",
      sub: diskNodes.length
        ? `${Math.round(diskUsed)} of ${Math.round(diskCap)} GB`
        : "Not reported yet",
    },
    {
      id: "network",
      label: "Network",
      value: netNodes.length ? rate(rxNow + txNow) : "—",
      sub: netNodes.length ? `${rate(rxNow)} in · ${rate(txNow)} out` : "Not reported yet",
    },
    {
      id: "pods",
      label: "Containers",
      value: String(pods),
      sub: `on ${nodes.length} ${nodes.length === 1 ? "server" : "servers"}`,
    },
  ];

  // One line per server while they fit the palette; past that, the total.
  const perServer = nodes.length <= SERIES.length;
  const lines = perServer
    ? nodes.map((n, i) => ({
        key: n.id,
        label: n.name,
        color: SERIES[i],
        values:
          metric === "cpu"
            ? n.cpu.map((v) => ratio(v, n.cpuCapacity))
            : metric === "memory"
              ? n.memoryMi.map((v) => ratio(v, n.memoryCapacityMi))
              : metric === "disk"
                ? diskOf(n)
                : metric === "network"
                  ? traffic(n)
                  : n.cpu.map((v) => (v == null ? null : n.pods)),
      }))
    : [
        {
          key: "all",
          label: "All servers",
          color: SERIES[0],
          values:
            metric === "cpu"
              ? cpuUsed.map((v) => ratio(v, cpuCap))
              : metric === "memory"
                ? memUsed.map((v) => ratio(v, memCap))
                : metric === "disk"
                  ? sum((n) => n.diskUsedGi ?? []).map((v) => (diskCap ? ratio(v, diskCap) : null))
                  : metric === "network"
                    ? sum(traffic)
                    : cpuUsed.map((v) => (v == null ? null : pods)),
        },
      ];
  const asPct = metric !== "pods" && metric !== "network";
  // Scale to the data, so a quiet 3% isn't a flat line along the bottom.
  const peak = Math.max(
    0,
    ...lines.flatMap((l) => l.values).filter((v): v is number => v != null),
  );
  const top = asPct
    ? ([0.04, 0.08, 0.2, 0.4, 0.6, 0.8].find((x) => peak <= x * 0.9) ?? 1)
    : undefined;
  const topServices = [...apps].sort((a, b) => last(b.memoryMi) - last(a.memoryMi));

  return (
    <>
      <Layered bare>
        <section
          aria-label="Server usage"
          className="overflow-hidden rounded-[14px] border border-card-border bg-card"
        >
          <div
            role="tablist"
            aria-label="Metric"
            className="grid grid-cols-3 border-b border-border sm:grid-cols-5"
          >
            {tabs.map((t) => {
              const on = t.id === metric;
              return (
                <button
                  key={t.id}
                  type="button"
                  role="tab"
                  aria-selected={on}
                  onClick={() => setMetric(t.id)}
                  className={`relative border-b border-r border-border px-4 py-4 text-left transition-colors last:border-r-0 sm:border-b-0 sm:px-6 sm:py-5 ${on ? "bg-accent/40" : "hover:bg-accent/20"}`}
                >
                  <span className="block text-[12.5px] font-medium text-muted-foreground">
                    {t.label}
                  </span>
                  <span className="mt-1.5 block text-[22px] font-semibold tabular-nums tracking-tight sm:text-[26px]">
                    {t.value}
                  </span>
                  <span className="block truncate text-[12px] tabular-nums text-muted-foreground">
                    {t.sub}
                  </span>
                  {on ? (
                    <span className="absolute inset-x-0 bottom-0 h-0.5 bg-foreground" />
                  ) : null}
                </button>
              );
            })}
          </div>
          <div className="space-y-3 px-3 pb-4 pt-5 sm:px-5">
            <TimeSeriesChart
              label={`${tabs.find((t) => t.id === metric)!.label} by server`}
              times={times}
              series={lines}
              max={top}
              format={
                asPct
                  ? (v) => pct(v)
                  : metric === "network"
                    ? (v) => rate(v)
                    : (v) => String(Math.round(v))
              }
              height={240}
            />
            {lines.length > 1 ? (
              <div className="pl-11">
                <Legend
                  items={lines.map((l) => ({ label: l.label, color: l.color }))}
                />
              </div>
            ) : null}
          </div>
        </section>
      </Layered>

      <Panel title="Servers">
        <div className="hidden grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_88px] gap-x-6 border-b border-border px-4 py-2 text-[12px] text-muted-foreground md:grid">
          <span>Server</span>
          <span>CPU</span>
          <span>Memory</span>
          <span>Disk</span>
          <span className="text-right">Containers</span>
        </div>
        <ul className="list-rows">
          {nodes.map((n) => {
            const node = s.servers.find(
              (x) => x.name === n.id && x.clusterId === n.clusterId,
            );
            return (
              <li key={n.id}>
                <Link
                  to="/$team/clusters/$clusterId/$serverId"
                  params={{
                    team: currentSlug(),
                    clusterId: clusterRef(s, n.clusterId),
                    serverId: n.id,
                  }}
                  className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 gap-y-2 px-4 py-3 transition-colors hover:bg-accent/30 md:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_88px]"
                >
                  <span className="flex min-w-0 items-center gap-2 text-[13px]">
                    <Dot
                      tone={
                        node?.status === "ready"
                          ? "success"
                          : node
                            ? "danger"
                            : "neutral"
                      }
                    />
                    <span className="truncate font-medium">{n.name}</span>
                    {inCluster ? null : (
                      <span className="truncate text-muted-foreground">
                        {s.clusters.find((c) => c.id === n.clusterId)?.name}
                      </span>
                    )}
                  </span>
                  <Usage
                    label="CPU"
                    values={n.cpu.map((v) => ratio(v, n.cpuCapacity))}
                    detail={`${cpu(last(n.cpu) * 1000)} / ${cpu(n.cpuCapacity * 1000)}`}
                  />
                  <Usage
                    label="Memory"
                    values={n.memoryMi.map((v) => ratio(v, n.memoryCapacityMi))}
                    detail={`${mem(last(n.memoryMi))} / ${mem(n.memoryCapacityMi)}`}
                  />
                  <Usage
                    label="Disk"
                    values={diskOf(n)}
                    warnAt={0.85}
                    detail={
                      any(n.diskUsedGi) && n.diskCapacityGi
                        ? `${Math.round(last(n.diskUsedGi ?? []))} / ${Math.round(n.diskCapacityGi)} GB`
                        : "Not reported"
                    }
                  />
                  <span className="hidden text-right text-[12.5px] tabular-nums text-muted-foreground md:block">
                    {n.pods}
                  </span>
                </Link>
              </li>
            );
          })}
        </ul>
      </Panel>

      {ingress.length ? (
        <Panel title="Gateway">
          <div className="hidden grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_88px] gap-x-6 border-b border-border px-4 py-2 text-[12px] text-muted-foreground md:grid">
            <span>Cluster</span>
            <span>CPU</span>
            <span>Memory</span>
            <span className="text-right">Containers</span>
          </div>
          <ul className="list-rows">
            {ingress.map((t) => (
              <li
                key={t.id}
                className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 gap-y-2 px-4 py-3 md:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_88px]"
              >
                <span className="truncate text-[13px] font-medium">
                  {s.clusters.find((c) => c.id === t.clusterId)?.name ??
                    t.name}
                </span>
                <Usage
                  label="CPU"
                  values={t.cpu.map((v) => ratio(v, t.cpuCapacity))}
                  detail={`${cpu(last(t.cpu) * 1000)} / ${cpu(t.cpuCapacity * 1000)}`}
                />
                <Usage
                  label="Memory"
                  values={t.memoryMi.map((v) => ratio(v, t.memoryCapacityMi))}
                  detail={`${mem(last(t.memoryMi))} / ${mem(t.memoryCapacityMi)}`}
                />
                <span className="hidden text-right text-[12.5px] tabular-nums text-muted-foreground md:block">
                  {t.pods}
                </span>
              </li>
            ))}
          </ul>
          <p className="border-t border-border px-4 py-2.5 text-[12px] text-muted-foreground">
            Against the cluster's total capacity.
          </p>
        </Panel>
      ) : null}

      <Panel title="Services">
        {topServices.length ? (
          <>
            <div className="hidden grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_88px] gap-x-6 border-b border-border px-4 py-2 text-[12px] text-muted-foreground md:grid">
              <span>Service</span>
              <span>CPU</span>
              <span>Memory</span>
              <span className="text-right">Restarts</span>
            </div>
            <ul className="list-rows">
              {topServices.map((a) => (
                <li key={a.id}>
                  <Link
                    to="/$team/$projectId/$serviceId"
                    params={{ team: currentSlug(), ...serviceParams(a.id) }}
                    className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 gap-y-2 px-4 py-3 transition-colors hover:bg-accent/30 md:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,1.3fr)_88px]"
                  >
                    <span className="flex min-w-0 items-baseline gap-2 text-[13px]">
                      <span className="truncate font-medium">{a.name}</span>
                      <span className="truncate text-[12px] text-muted-foreground">
                        {
                          s.projects.find(
                            (p) =>
                              p.id ===
                              s.services.find((x) => x.id === a.id)?.projectId,
                          )?.name
                        }
                      </span>
                    </span>
                    <Usage
                      label="CPU"
                      values={a.cpu.map((v) => ratio(v, a.cpuCapacity))}
                      detail={`${cpu(last(a.cpu) * 1000)} / ${cpu(a.cpuCapacity * 1000)}`}
                    />
                    <Usage
                      label="Memory"
                      values={a.memoryMi.map((v) =>
                        ratio(v, a.memoryCapacityMi),
                      )}
                      detail={`${mem(last(a.memoryMi))} / ${mem(a.memoryCapacityMi)}`}
                    />
                    <span
                      className={`hidden text-right text-[12.5px] tabular-nums md:block ${a.restarts > 5 ? "text-destructive" : "text-muted-foreground"}`}
                    >
                      {a.restarts}
                    </span>
                  </Link>
                </li>
              ))}
            </ul>
          </>
        ) : (
          <p className="px-4 py-6 text-[13px] text-muted-foreground">
            No services running in this range.
          </p>
        )}
      </Panel>
    </>
  );
}

/** "12% ~~~ 120m / 1 vCPU": the latest share of capacity with its trend. */
function Usage({
  label,
  values,
  detail,
  warnAt,
}: {
  label: string;
  values: V[];
  detail: string;
  /** Past this, the figure turns amber (red past 90%). */
  warnAt?: number;
}) {
  const now = last(values);
  const none = !any(values);
  return (
    <span className="col-span-2 flex min-w-0 items-center gap-3 text-[12.5px] md:col-span-1">
      <span className="w-14 shrink-0 text-muted-foreground md:hidden">
        {label}
      </span>
      <span
        className={`w-9 shrink-0 text-right tabular-nums ${none ? "text-muted-foreground" : now > 0.9 ? "text-destructive" : warnAt != null && now > warnAt ? "text-warning" : ""}`}
      >
        {none ? "—" : pct(now)}
      </span>
      <Sparkline
        values={values}
        width={64}
        height={20}
        label={`${label} trend`}
        domain={[0, 1]}
      />
      <span className="truncate tabular-nums text-muted-foreground">
        {detail}
      </span>
    </span>
  );
}
