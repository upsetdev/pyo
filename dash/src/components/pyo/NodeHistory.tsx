import { useState } from "react";
import { SERIES, TimeSeriesChart, type Series } from "~/components/charts/Charts";
import { Select } from "~/components/ui/Field";
import { ErrorState, Panel, Skeleton } from "~/components/ui/Panel";
import { fetchMetrics, type MetricsEntity } from "~/lib/pyo/backend";
import { pct, rate } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";

// One server's history: CPU and memory (metrics-server), disk and network
// (the kubelet), load and disk I/O (Vector's host metrics). Each tab shows
// the latest value; "—" where a figure isn't collected (an older agent, or
// not yet).

type V = number | null;
const RANGES = [
  { value: "1h", label: "Last hour" },
  { value: "24h", label: "Last 24 hours" },
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days" },
];
type Tab = "cpu" | "memory" | "disk" | "network" | "load" | "io";

const latest = (v: V[] | undefined): V => {
  for (let i = (v?.length ?? 0) - 1; i >= 0; i--) if (v![i] != null) return v![i] as number;
  return null;
};
const has = (v: V[] | undefined) => !!v?.some((x) => x != null);

export function NodeHistory({ team, clusterId, node }: { team: string; clusterId: string; node: string }) {
  const [range, setRange] = useState("24h");
  const [tab, setTab] = useState<Tab>("cpu");
  const q = useQuery(`metrics:${team}:${clusterId}:${range}`, () => fetchMetrics({ team, cluster: clusterId, range }), { refreshMs: 30_000 });
  const rangePicker = (
    <div className="w-40 sm:w-44">
      <Select value={range} onChange={(e) => setRange(e.target.value)} aria-label="Time range" heading="Time range">
        {RANGES.map((r) => (
          <option key={r.value} value={r.value}>
            {r.label}
          </option>
        ))}
      </Select>
    </div>
  );
  if (q.status === "loading")
    return (
      <Panel title="History" actions={rangePicker}>
        <Skeleton className="m-4 h-60 rounded-lg" />
      </Panel>
    );
  if (q.status === "error")
    return (
      <Panel title="History" actions={rangePicker}>
        <ErrorState title="Couldn't load history" description={q.error} onRetry={q.retry} />
      </Panel>
    );
  const e = q.data.servers.find((n) => n.id === node);
  const times = q.data.times;
  if (!e || !times.length)
    return (
      <Panel title="History" actions={rangePicker}>
        <p className="px-4 py-6 text-[13px] text-muted-foreground">Collecting: history shows up within a minute.</p>
      </Panel>
    );
  return (
    <Panel title="History" actions={rangePicker}>
      <Loaded e={e} times={times} tab={tab} setTab={setTab} />
    </Panel>
  );
}

function Loaded({ e, times, tab, setTab }: { e: MetricsEntity; times: number[]; tab: Tab; setTab: (t: Tab) => void }) {
  const ratio = (v: V[], cap: number | null | undefined) => v.map((x) => (x == null || !cap ? null : x / cap));
  // Network: the server's cards (host metrics) when there, else the kubelet's view.
  const nicHere = has(e.nicRxBps) || has(e.nicTxBps);
  const rx = nicHere ? e.nicRxBps : e.netRxBps;
  const tx = nicHere ? e.nicTxBps : e.netTxBps;
  const disk = latest(e.diskUsedGi);
  const tabs: { id: Tab; label: string; value: string; sub: string }[] = [
    { id: "cpu", label: "CPU", value: latest(e.cpu) == null ? "—" : pct((latest(e.cpu) as number) / (e.cpuCapacity || 1)), sub: `${e.cpuCapacity} cores` },
    {
      id: "memory",
      label: "Memory",
      value: latest(e.memoryMi) == null ? "—" : pct((latest(e.memoryMi) as number) / (e.memoryCapacityMi || 1)),
      sub: `${Math.round(e.memoryCapacityMi / 1024)} GB`,
    },
    {
      id: "disk",
      label: "Disk",
      value: disk == null || !e.diskCapacityGi ? "—" : pct(disk / e.diskCapacityGi),
      sub: disk == null || !e.diskCapacityGi ? "Not reported" : `${Math.round(disk)} of ${Math.round(e.diskCapacityGi)} GB`,
    },
    { id: "network", label: "Network", value: rate(latest(rx)), sub: `in · ${rate(latest(tx))} out` },
    { id: "load", label: "Load", value: latest(e.load1) == null ? "—" : (latest(e.load1) as number).toFixed(2), sub: "1-minute average" },
    { id: "io", label: "Disk I/O", value: rate(latest(e.diskReadBps)), sub: `read · ${rate(latest(e.diskWriteBps))} write` },
  ];
  const lines: Record<Tab, { series: Series[]; format: (v: number) => string; max?: number }> = {
    cpu: { series: [{ key: "cpu", label: "CPU", color: SERIES[0], values: ratio(e.cpu, e.cpuCapacity) }], format: (v) => pct(v), max: 1 },
    memory: { series: [{ key: "mem", label: "Memory", color: SERIES[0], values: ratio(e.memoryMi, e.memoryCapacityMi) }], format: (v) => pct(v), max: 1 },
    disk: { series: [{ key: "disk", label: "Disk used", color: SERIES[0], values: ratio(e.diskUsedGi ?? [], e.diskCapacityGi) }], format: (v) => pct(v), max: 1 },
    network: {
      series: [
        { key: "rx", label: "In", color: SERIES[0], values: rx ?? [] },
        { key: "tx", label: "Out", color: SERIES[1], values: tx ?? [] },
      ],
      format: (v) => rate(v),
    },
    load: { series: [{ key: "load", label: "Load (1 min)", color: SERIES[0], values: e.load1 ?? [] }], format: (v) => v.toFixed(2) },
    io: {
      series: [
        { key: "read", label: "Read", color: SERIES[0], values: e.diskReadBps ?? [] },
        { key: "write", label: "Write", color: SERIES[1], values: e.diskWriteBps ?? [] },
      ],
      format: (v) => rate(v),
    },
  };
  const cur = lines[tab];
  const empty = !cur.series.some((s) => has(s.values));
  return (
    <div>
      <div role="tablist" aria-label="Metric" className="grid grid-cols-3 border-b border-border sm:grid-cols-6">
        {tabs.map((t) => {
          const on = t.id === tab;
          return (
            <button
              key={t.id}
              type="button"
              role="tab"
              aria-selected={on}
              onClick={() => setTab(t.id)}
              className={`relative border-b border-r border-border px-4 py-3 text-left transition-colors sm:border-b-0 ${on ? "bg-accent/40" : "hover:bg-accent/20"}`}
            >
              <span className="block text-[12px] font-medium text-muted-foreground">{t.label}</span>
              <span className="mt-1 block text-[18px] font-semibold tabular-nums tracking-tight">{t.value}</span>
              <span className="block truncate text-[11.5px] tabular-nums text-muted-foreground">{t.sub}</span>
              {on ? <span className="absolute inset-x-0 bottom-0 h-0.5 bg-primary" /> : null}
            </button>
          );
        })}
      </div>
      <div className="px-3 pb-4 pt-4 sm:px-5">
        {empty ? (
          <p className="py-10 text-center text-[13px] text-muted-foreground">
            {tab === "load" || tab === "io"
              ? "Not reported yet."
              : "Not reported for this period."}
          </p>
        ) : (
          <TimeSeriesChart label={tabs.find((t) => t.id === tab)!.label} times={times} series={cur.series} format={cur.format} max={cur.max} height={220} />
        )}
      </div>
    </div>
  );
}
