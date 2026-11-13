import { ObservabilityOff, useObservabilityOn } from "~/components/pyo/ObservabilityOff";
import { useState } from "react";
import { TimeSeriesChart } from "~/components/charts/Charts";
import { Segmented } from "~/components/ui/Field";
import { Flag } from "~/components/ui/Flag";
import { Layered } from "~/components/ui/Layered";
import { Skeleton } from "~/components/ui/Panel";
import { fetchConnections } from "~/lib/pyo/backend";
import { compact, duration } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import type { Application } from "~/lib/pyo/types";

// Connections to a service's public TCP and UDP ports. The gateway
// logs each one when it closes (a UDP exchange after a minute without
// packets), so open connections show up later. Ports that bypass the gateway
// aren't counted.

const RANGES = ["24h", "7d", "30d"] as const;

const bytes = (n: number) =>
  n >= 1e9
    ? `${+(n / 1e9).toFixed(1)} GB`
    : n >= 1e6
      ? `${+(n / 1e6).toFixed(1)} MB`
      : n >= 1e3
        ? `${Math.round(n / 1e3)} kB`
        : `${Math.round(n)} B`;

export function hasPublicPorts(app: Application) {
  return app.ports.some((p) => p.exposure === "loadbalancer");
}

export function ConnectionsCard({ app }: { app: Application }) {
  const [range, setRange] = useState<(typeof RANGES)[number]>("24h");
  const [metric, setMetric] = useState(0);
  const pub = app.ports.filter((p) => p.exposure === "loadbalancer");
  const proxied = pub.filter((p) => p.proxy !== false);
  const on = useObservabilityOn();
  const q = useQuery(
    `app-connections:${app.id}:${range}:${proxied.length}:${on}`,
    () =>
      proxied.length && on
        ? fetchConnections(app.id, range)
        : Promise.resolve(null),
    { refreshMs: 30_000 },
  );
  const data = q.status === "success" ? q.data : null;
  const series = data?.series ?? [];
  const total = (k: "connections" | "bytesIn" | "bytesOut") =>
    (data?.ports ?? []).reduce((a, p) => a + p[k], 0);
  const metrics = [
    {
      label: "Connections",
      value: data ? compact(total("connections")) : "–",
      values: series.map((p) => p.connections),
      format: compact,
    },
    {
      label: "Clients",
      value: data
        ? compact((data.ports ?? []).reduce((a, p) => a + p.clients, 0))
        : "–",
      values: series.map((p) => p.clients),
      format: compact,
    },
    {
      label: "Data",
      value: data ? bytes(total("bytesIn") + total("bytesOut")) : "–",
      values: series.map((p) => p.bytes),
      format: bytes,
    },
  ];
  const m = metrics[metric];
  const stats = (port: number, protocol: string) =>
    data?.ports.find(
      (x) =>
        x.port === port &&
        x.protocol === (protocol === "UDP" ? "UDP" : "TCP"),
    );

  return (
    <Layered bare>
      <section
        aria-label="Connections"
        className="overflow-hidden rounded-[14px] border border-card-border bg-card"
      >
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-border px-4 py-3">
          <div className="min-w-0">
            <h2 className="text-[14px] font-medium tracking-tight">
              Connections
            </h2>
            <p className="text-[12px] text-muted-foreground">
              Counted when a connection closes.
            </p>
          </div>
          {proxied.length ? (
            <Segmented
              label="Time range"
              size="xs"
              value={range}
              onChange={setRange}
              options={RANGES.map((r) => ({ value: r, label: r }))}
            />
          ) : null}
        </div>
        {proxied.length ? (
          <>
            <div
              role="tablist"
              aria-label="Metric"
              className="grid grid-cols-3"
            >
              {metrics.map((x, i) => (
                <button
                  key={x.label}
                  type="button"
                  role="tab"
                  aria-selected={i === metric}
                  onClick={() => setMetric(i)}
                  className={`relative border-b border-r border-border px-4 py-3 text-left transition-colors last:border-r-0 ${i === metric ? "bg-accent/40" : "hover:bg-accent/20"}`}
                >
                  <span className="block text-[12px] text-muted-foreground">
                    {x.label}
                  </span>
                  <span className="mt-0.5 block text-[20px] font-semibold tabular-nums tracking-tight">
                    {q.status === "loading" ? (
                      <Skeleton className="mt-1 h-6 w-14" />
                    ) : (
                      x.value
                    )}
                  </span>
                  {i === metric ? (
                    <span className="absolute inset-x-0 bottom-0 h-0.5 bg-foreground" />
                  ) : null}
                </button>
              ))}
            </div>
            <div className="px-3 pb-3 pt-4 sm:px-4">
              {q.status === "error" ? (
                <p className="py-10 text-center text-[13px] text-muted-foreground">
                  {q.error}
                </p>
              ) : q.status === "loading" ? (
                <Skeleton className="h-[160px] w-full" />
              ) : (
                <TimeSeriesChart
                  label={`${m.label} over the last ${range}`}
                  area
                  times={series.map((p) => p.at)}
                  series={[
                    {
                      key: m.label,
                      label: m.label,
                      color: "var(--series-1)",
                      values: m.values,
                    },
                  ]}
                  format={m.format}
                  height={160}
                />
              )}
            </div>
          </>
        ) : null}
        <ul className="list-rows border-t border-border">
          {pub.map((p) => {
            const port = p.publicPort ?? p.containerPort;
            const proto = p.protocol === "UDP" ? "UDP" : "TCP";
            const st = p.proxy === false ? undefined : stats(port, proto);
            return (
              <li
                key={p.id}
                className="flex flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2.5 text-[12.5px]"
              >
                <span className="w-24 shrink-0 font-mono tabular-nums">
                  {port}
                  <span className="text-muted-foreground">/{proto}</span>
                </span>
                {p.proxy === false ? (
                  <span className="text-muted-foreground">
                    No analytics outside the gateway
                  </span>
                ) : st ? (
                  <span className="flex min-w-0 flex-wrap gap-x-4 gap-y-1 text-muted-foreground tabular-nums">
                    <span>
                      <span className="text-foreground">
                        {compact(st.connections)}
                      </span>{" "}
                      connections
                    </span>
                    <span>
                      <span className="text-foreground">
                        {compact(st.clients)}
                      </span>{" "}
                      clients
                    </span>
                    <span>
                      {bytes(st.bytesIn)} in · {bytes(st.bytesOut)} out
                    </span>
                    {proto === "TCP" ? (
                      <span>median {duration(st.durationP50Ms)}</span>
                    ) : null}
                    {st.blocked ? (
                      <span>{compact(st.blocked)} blocked</span>
                    ) : null}
                    {st.limited ? (
                      <span>{compact(st.limited)} over the limit</span>
                    ) : null}
                    {st.unreachable ? (
                      <span className="text-warning">
                        {compact(st.unreachable)} unreachable
                      </span>
                    ) : null}
                  </span>
                ) : (
                  <span className="text-muted-foreground">
                    {q.status === "loading" ? "…" : "No connections yet"}
                  </span>
                )}
              </li>
            );
          })}
        </ul>
        {data?.countries.length ? (
          <div className="flex flex-wrap gap-x-4 gap-y-1.5 border-t border-border px-4 py-3 text-[12.5px]">
            {data.countries.slice(0, 8).map((c) => (
              <span key={c.code} className="inline-flex items-center gap-1.5">
                <Flag country={c.code === "??" ? "" : c.code} size={14} />
                {c.name}
                <span className="tabular-nums text-muted-foreground">
                  {compact(c.clients)}
                </span>
              </span>
            ))}
          </div>
        ) : null}
      </section>
    </Layered>
  );
}
