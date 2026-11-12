import { ChartLineIcon } from "@phosphor-icons/react";
import { useState, type ReactNode } from "react";
import {
  Legend,
  SERIES,
  Sparkline,
  TimeSeriesChart,
  type Series,
} from "~/components/charts/Charts";
import { Layered } from "~/components/ui/Layered";
import { EmptyState, ErrorState, Panel, Skeleton } from "~/components/ui/Panel";
import {
  fetchDatabaseAnalytics,
  type DatabaseAnalytics as Data,
  type DbMemberSeries,
} from "~/lib/pyo/backend";
import { compact, cpu, mem, pct } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import type { Database } from "~/lib/pyo/types";

// A database's health over time, from the figures its cluster's agent reads
// off each member every 30 seconds: throughput, connections, cache hits,
// storage, replication lag and each member's CPU, memory and disk.

type V = number | null;
type Ser = V[] | null | undefined;

const has = (v: Ser): v is V[] => !!v?.some((x) => x != null);
export const lastOf = (v: Ser) => {
  for (let i = (v?.length ?? 0) - 1; i >= 0; i--) if (v![i] != null) return v![i] as number;
  return null;
};
const firstOf = (v: Ser) => v?.find((x) => x != null) ?? null;
const worst = (v: Ser) => (has(v) ? Math.max(...v.filter((x): x is number => x != null)) : null);

/** "1.2k/s". */
export const perSec = (v: number) => `${v > 0 && v < 10 ? +v.toFixed(1) : compact(v)}/s`;
/** Bytes, binary units. */
export function size(b: number) {
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  while (Math.abs(b) >= 1024 && i < u.length - 1) {
    b /= 1024;
    i++;
  }
  return `${i ? +b.toFixed(Math.abs(b) < 10 ? 1 : 0) : Math.round(b)} ${u[i]}`;
}
export const lag = (s: number) =>
  s < 1 ? "<1s" : s < 120 ? `${Math.round(s)}s` : `${Math.floor(s / 60)}m ${Math.round(s % 60)}s`;

/** Sum of two series, null only where both are. */
const plus = (a: Ser, b: Ser): V[] | null => {
  if (!has(a) && !has(b)) return null;
  const n = Math.max(a?.length ?? 0, b?.length ?? 0);
  return Array.from({ length: n }, (_, i) =>
    a?.[i] == null && b?.[i] == null ? null : (a?.[i] ?? 0) + (b?.[i] ?? 0),
  );
};

/** What the engine calls its throughput, and the series behind it. */
export function throughput(d: Data): { label: string; lines: { key: string; label: string; values: Ser }[]; sub?: string } {
  if (d.engine === "valkey") return { label: "Commands", lines: [{ key: "commands", label: "Commands", values: d.commands }] };
  if (d.engine === "mysql") return { label: "Queries", lines: [{ key: "queries", label: "Queries", values: d.queries }] };
  // Postgres counts transactions, not statements; the pooler counts its queries.
  return {
    label: "Transactions",
    lines: [{ key: "tx", label: "Transactions", values: d.transactions }],
    sub: has(d.poolerQueries) ? `${perSec(lastOf(d.poolerQueries) ?? 0)} queries via pooler` : undefined,
  };
}

/** Database size: Postgres's own; MySQL's primary volume; KV's memory. */
export function storageOf(d: Data): { label: string; values: V[] | null; bytes: boolean } {
  if (d.engine === "valkey") return { label: "Memory", values: d.storage?.memoryUsedBytes ?? null, bytes: true };
  if (has(d.storage?.sizeBytes)) return { label: "Size", values: d.storage!.sizeBytes, bytes: true };
  const primary = d.members.find((m) => m.component === "instance" && m.role === "primary");
  return { label: "Disk", values: primary?.diskUsedGi?.map((v) => (v == null ? null : v * 1024 ** 3)) ?? null, bytes: true };
}

/** Each PgBouncer's server connections per pool (PgBouncer's default_pool_size). */
const POOL_SIZE = 20;

const RANGES = ["1h", "24h", "7d", "30d"] as const;
export type Range = (typeof RANGES)[number];
export const RANGE_LABEL: Record<Range, string> = {
  "1h": "Last hour",
  "24h": "Last 24 hours",
  "7d": "Last 7 days",
  "30d": "Last 30 days",
};

export function useDatabaseAnalytics(id: string, range: Range) {
  return useQuery(`db-analytics:${id}:${range}`, () => fetchDatabaseAnalytics(id, range), { refreshMs: 30_000 });
}

type Tab = "throughput" | "connections" | "efficiency" | "storage";

export function DatabaseAnalytics({ d, range }: { d: Database; range: Range }) {
  const q = useDatabaseAnalytics(d.id, range);
  if (q.status === "loading")
    return (
      <div className="space-y-4" aria-busy aria-label="Loading analytics">
        <Skeleton className="h-[380px] rounded-xl" />
        <Skeleton className="h-48 rounded-xl" />
      </div>
    );
  if (q.status === "error")
    return (
      <Panel>
        <ErrorState title="Couldn't load analytics" description={q.error} onRetry={q.retry} />
      </Panel>
    );
  if (!q.data.times.length)
    return (
      <Panel>
        <EmptyState icon={ChartLineIcon} title="Collecting metrics" description="Shown once its servers report." />
      </Panel>
    );
  return <Loaded d={d} data={q.data} range={range} />;
}

function Loaded({ d, data, range }: { d: Database; data: Data; range: Range }) {
  const [tab, setTab] = useState<Tab>("throughput");
  const { times } = data;
  const t = throughput(data);
  const c = data.connections;
  const kv = data.engine === "valkey";
  const st = storageOf(data);
  const limit = c?.limit ?? null;
  const hit = kv ? data.keyHitRatio : data.cacheHitRatio;
  const growth = has(st.values) ? (lastOf(st.values) ?? 0) - (firstOf(st.values) ?? 0) : null;
  const maxmem = data.storage?.maxmemoryBytes ?? null;

  const tabs: { id: Tab; label: string; value: string; sub: string }[] = [
    {
      id: "throughput",
      label: t.label,
      value: has(t.lines[0].values) ? perSec(lastOf(t.lines[0].values) ?? 0) : "—",
      sub: t.sub ?? `peak ${has(t.lines[0].values) ? perSec(worst(t.lines[0].values) ?? 0) : "—"}`,
    },
    {
      id: "connections",
      label: kv ? "Clients" : "Connections",
      value: has(c?.primary) ? String(Math.round(lastOf(c!.primary) ?? 0)) : "—",
      sub: [
        limit ? `of ${limit} on the primary` : null,
        has(c?.poolerWaiting) ? `${Math.round(lastOf(c!.poolerWaiting) ?? 0)} waiting` : null,
      ]
        .filter(Boolean)
        .join(" · ") || "on the primary",
    },
    {
      id: "efficiency",
      label: kv ? "Key hits" : data.engine === "mysql" ? "Buffer pool hits" : "Cache hits",
      value: has(hit) ? pct(lastOf(hit) ?? 0, 1) : "—",
      sub: kv && has(data.evictions) ? `${perSec(lastOf(data.evictions) ?? 0)} evicted` : "of reads from memory",
    },
    {
      id: "storage",
      label: st.label,
      value: has(st.values) ? size(lastOf(st.values) ?? 0) : "—",
      sub:
        kv
          ? [maxmem ? `of ${size(maxmem)}` : null, lastOf(data.storage?.keys) != null ? `${compact(lastOf(data.storage?.keys) ?? 0)} keys` : null].filter(Boolean).join(" · ") || "in use"
          : growth != null
            ? `${growth >= 0 ? "+" : "−"}${size(Math.abs(growth))} in ${range}`
            : "Not reported yet",
    },
  ];

  const color = (i: number) => SERIES[i];
  const line = (key: string, label: string, values: Ser, i: number): Series | null =>
    has(values) ? { key, label, color: color(i), values } : null;
  let lines: Series[] = [];
  let format = (v: number) => perSec(v);
  let max: number | undefined;
  if (tab === "throughput") lines = t.lines.map((l, i) => line(l.key, l.label, l.values, i)).filter((l): l is Series => !!l);
  if (tab === "connections") {
    lines = [line("primary", "Primary", c?.primary, 0), line("replicas", "Replicas", c?.replicas, 1)].filter((l): l is Series => !!l);
    format = (v) => String(Math.round(v));
  }
  if (tab === "efficiency") {
    lines = [line("hit", tabs[2].label, hit, 0)].filter((l): l is Series => !!l);
    // Whole percents on the axis, a decimal on hover.
    format = (v) => pct(v, Number.isInteger(Math.round(v * 1e6) / 1e4) ? 0 : 1);
    max = 1;
  }
  if (tab === "storage") {
    lines = [line("size", st.label, st.values, 0)].filter((l): l is Series => !!l);
    format = (v) => size(v);
  }

  const lagging = data.members.filter((m) => has(m.lagSeconds));
  const routing = data.routing;
  const pooler = has(c?.poolerActive) || has(c?.poolerWaiting);
  const poolers = data.members.filter((m) => m.component === "pooler" || m.component === "read-pooler").length;

  return (
    <div className="space-y-6">
      <Layered bare>
        <section aria-label="Database health" className="overflow-hidden rounded-[14px] border border-card-border bg-card">
          <div role="tablist" aria-label="Metric" className="grid grid-cols-2 border-b border-border sm:grid-cols-4">
            {tabs.map((x) => {
              const on = x.id === tab;
              return (
                <button
                  key={x.id}
                  type="button"
                  role="tab"
                  aria-selected={on}
                  onClick={() => setTab(x.id)}
                  className={`relative border-b border-r border-border px-4 py-4 text-left transition-colors last:border-r-0 sm:border-b-0 sm:px-6 sm:py-5 ${on ? "bg-accent/40" : "hover:bg-accent/20"}`}
                >
                  <span className="block text-[12.5px] font-medium text-muted-foreground">{x.label}</span>
                  <span className="mt-1.5 block text-[22px] font-semibold tabular-nums tracking-tight sm:text-[26px]">{x.value}</span>
                  <span className="block truncate text-[12px] tabular-nums text-muted-foreground">{x.sub}</span>
                  {on ? <span className="absolute inset-x-0 bottom-0 h-0.5 bg-foreground" /> : null}
                </button>
              );
            })}
          </div>
          <div className="space-y-3 px-3 pb-4 pt-5 sm:px-5">
            {lines.length ? (
              <TimeSeriesChart label={tabs.find((x) => x.id === tab)!.label} times={times} series={lines} max={max} format={format} height={240} />
            ) : (
              <p className="flex h-[240px] items-center justify-center text-[12.5px] text-muted-foreground">Not reported yet</p>
            )}
            {lines.length > 1 ? (
              <div className="pl-11">
                <Legend items={lines.map((l) => ({ label: l.label, color: l.color }))} />
              </div>
            ) : null}
          </div>
        </section>
      </Layered>

      {lagging.length ? (
        <Panel title="Replication lag">
          <ul className="list-rows">
            {lagging.map((m) => (
              <li key={m.name} className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 gap-y-2 px-4 py-3 md:grid-cols-[minmax(0,1fr)_minmax(0,2fr)_88px]">
                <MemberName m={m} />
                <span className="col-span-2 flex min-w-0 items-center gap-3 text-[12.5px] md:col-span-1">
                  <span className="w-12 shrink-0 text-right tabular-nums">{lag(lastOf(m.lagSeconds) ?? 0)}</span>
                  <Sparkline values={m.lagSeconds!} fill width={240} height={22} label={`${m.name} lag`} />
                </span>
                <span className="hidden text-right text-[12.5px] tabular-nums text-muted-foreground md:block">
                  worst {lag(worst(m.lagSeconds) ?? 0)}
                </span>
              </li>
            ))}
          </ul>
        </Panel>
      ) : null}

      {kv && (has(routing?.replica) || has(routing?.primary)) ? (
        <Panel title="Reads">
          <ChartBody>
            <TimeSeriesChart
              label="Reads by where they were served"
              times={times}
              stacked
              series={[line("replica", "Replicas", routing?.replica, 0), line("primary", "Primary", routing?.primary, 1)].filter((l): l is Series => !!l)}
              format={perSec}
              height={160}
            />
            <Legend
              items={[
                { label: "Replicas", color: color(0), value: has(routing?.replica) ? perSec(lastOf(routing!.replica) ?? 0) : "—" },
                { label: "Primary", color: color(1), value: has(routing?.primary) ? perSec(lastOf(routing!.primary) ?? 0) : "—" },
              ]}
            />
            {(worst(routing?.fallbacks) ?? 0) > 0 ? (
              <p className="pl-11 text-[12px] text-muted-foreground">
                Up to {perSec(worst(routing!.fallbacks) ?? 0)} fell back to the primary when no replica was ready.
              </p>
            ) : null}
          </ChartBody>
        </Panel>
      ) : null}

      {kv && has(data.evictions) ? (
        <Panel title="Evictions">
          <ChartBody>
            <TimeSeriesChart label="Keys evicted per second" times={times} series={[line("ev", "Evicted", data.evictions, 0)!]} format={perSec} height={140} />
          </ChartBody>
        </Panel>
      ) : null}

      {pooler ? (
        <Panel title="Pooler">
          <ChartBody>
            <TimeSeriesChart
              label="Pooler clients"
              times={times}
              series={[line("active", "Active", c?.poolerActive, 0), line("waiting", "Waiting", c?.poolerWaiting, 1)].filter((l): l is Series => !!l)}
              format={(v) => String(Math.round(v))}
              height={140}
            />
            <Legend
              items={[
                { label: "Active", color: color(0), value: has(c?.poolerActive) ? Math.round(lastOf(c!.poolerActive) ?? 0) : "—" },
                { label: "Waiting", color: color(1), value: has(c?.poolerWaiting) ? Math.round(lastOf(c!.poolerWaiting) ?? 0) : "—" },
              ]}
            />
            {has(c?.poolerServersActive) || has(c?.poolerMaxWaitSeconds) ? (
              <p className="pl-11 text-[12px] tabular-nums text-muted-foreground">
                {[
                  has(c?.poolerServersActive) ? `${Math.round(lastOf(c!.poolerServersActive) ?? 0)} of ${POOL_SIZE * poolers} server connections in use` : null,
                  has(c?.poolerMaxWaitSeconds) ? `longest wait ${lag(lastOf(c!.poolerMaxWaitSeconds) ?? 0)}` : null,
                ]
                  .filter(Boolean)
                  .join(" · ")}
              </p>
            ) : null}
          </ChartBody>
        </Panel>
      ) : null}

      <Members d={d} data={data} range={range} />
    </div>
  );
}

function ChartBody({ children }: { children: ReactNode }) {
  return <div className="space-y-3 px-3 pb-4 pt-4 sm:px-5 [&>ul]:pl-11">{children}</div>;
}

const PART: Record<string, string> = {
  read: "Read replica",
  pooler: "Pooler",
  "read-pooler": "Read pooler",
  proxy: "Endpoint",
  sentinel: "Sentinel",
};
const partOf = (m: DbMemberSeries) => (m.component === "instance" ? (m.role === "primary" ? "Primary" : "Replica") : (PART[m.component] ?? m.component));

function MemberName({ m }: { m: DbMemberSeries }) {
  return (
    <span className="min-w-0">
      <span className="block truncate font-mono text-[12.5px]">{m.name}</span>
      <span className="block truncate text-[12px] text-muted-foreground">
        {partOf(m)}
        {m.server ? ` · ${m.server}` : ""}
      </span>
    </span>
  );
}

/** Each member's CPU, memory and disk: now, the trend, and the disk's growth. */
function Members({ d, data, range }: { d: Database; data: Data; range: Range }) {
  const grid = "md:grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)_minmax(0,1fr)_minmax(0,1.2fr)]";
  return (
    <Panel title="Members">
      <div className={`hidden gap-x-6 border-b border-border px-4 py-2 text-[12px] text-muted-foreground md:grid ${grid}`}>
        <span>Member</span>
        <span>CPU</span>
        <span>Memory</span>
        <span>Disk</span>
      </div>
      <ul className="list-rows">
        {data.members.map((m) => {
          const data = m.component === "instance" || m.component === "read";
          const cpuMax = data && d.cpuMillis ? d.cpuMillis / 1000 : undefined;
          const memMax = data && d.memoryMi ? d.memoryMi : undefined;
          const disk = lastOf(m.diskUsedGi);
          const grew = disk != null ? disk - (firstOf(m.diskUsedGi) ?? disk) : null;
          return (
            <li key={m.name} className={`grid grid-cols-[minmax(0,1fr)] items-center gap-x-6 gap-y-2 px-4 py-3 ${grid}`}>
              <MemberName m={m} />
              <Cell label="CPU" values={m.cpu} max={cpuMax} text={(v) => cpu(v * 1000)} maxText={(v) => String(+v.toFixed(2))} />
              <Cell label="Memory" values={m.memoryMi} max={memMax} text={mem} />
              <Cell
                label="Disk"
                values={m.diskUsedGi}
                max={m.diskCapacityGi ?? undefined}
                text={(v) => `${+v.toFixed(v < 10 ? 2 : 1)} GiB`}
                extra={grew != null && Math.abs(grew) >= 0.01 ? `${grew > 0 ? "+" : "−"}${+Math.abs(grew).toFixed(2)} GiB in ${range}` : undefined}
              />
            </li>
          );
        })}
      </ul>
    </Panel>
  );
}

/** "0.12 vCPU ~~~ of 0.5": the latest value, its trend, and the limit when there is one. */
function Cell({ label, values, max, text, maxText, extra }: { label: string; values: Ser; max?: number; text: (v: number) => string; maxText?: (v: number) => string; extra?: string }) {
  if (!has(values))
    return (
      <span className="flex items-center gap-3 text-[12.5px] text-muted-foreground">
        <span className="w-16 shrink-0 md:hidden">{label}</span>—
      </span>
    );
  const now = lastOf(values) ?? 0;
  const share = max ? now / max : null;
  return (
    <span className="flex min-w-0 items-center gap-3 text-[12.5px]">
      <span className="w-16 shrink-0 text-muted-foreground md:hidden">{label}</span>
      <span className={`shrink-0 tabular-nums ${share != null && share > 0.9 ? "text-destructive" : share != null && share > 0.8 ? "text-warning" : ""}`}>{text(now)}</span>
      <Sparkline values={values} width={56} height={20} label={`${label} trend`} domain={max ? [0, max] : undefined} />
      <span className="truncate tabular-nums text-muted-foreground">{extra ?? (max ? `of ${(maxText ?? text)(max)}` : "")}</span>
    </span>
  );
}
