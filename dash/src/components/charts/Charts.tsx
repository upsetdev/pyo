import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";

// Lightweight SVG charts. Drawn in pixel space from a measured width so text
// never stretches. Every chart has a hover layer (crosshair + tooltip for
// time series, per-row hover for bar lists) and an accessible summary.

export type Series = {
  key: string;
  label: string;
  color: string;
  /** null where there's no data: drawn at 0, and "No data" on hover. */
  values: (number | null)[];
  /** Set to fade a series in or out (stacked bars): 0 hidden, 1 shown. */
  opacity?: number;
};

/**
 * The points to draw, as [index, value] runs. A gap (no sample) is drawn at
 * 0 so the line stays continuous and the outage shows as a drop.
 */
function runs(values: (number | null)[]) {
  return [
    values.map((v, i): [number, number] => [
      i,
      v == null || !Number.isFinite(v) ? 0 : v,
    ]),
  ];
}

const known = (vs: (number | null)[]) =>
  vs.filter((v): v is number => v != null && Number.isFinite(v));

function useWidth<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) =>
      setWidth(Math.floor(e.contentRect.width)),
    );
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, width] as const;
}

/** Smallest "nice" maximum whose quarter steps are round numbers. */
function niceMax(v: number) {
  if (v <= 0) return 1;
  const raw = v / 4;
  const exp = Math.pow(10, Math.floor(Math.log10(raw)));
  const n = raw / exp;
  const step =
    (n <= 1 ? 1 : n <= 2 ? 2 : n <= 2.5 ? 2.5 : n <= 5 ? 5 : 10) * exp;
  return step * 4;
}

const tickTime = (at: number, spanMs: number) =>
  spanMs > 2 * 86_400_000
    ? new Date(at).toLocaleDateString("en-US", {
        month: "short",
        day: "numeric",
      })
    : new Date(at).toLocaleTimeString("en-GB", {
        hour: "2-digit",
        minute: "2-digit",
      });

export function Legend({
  items,
}: {
  items: { label: string; color: string; value?: ReactNode; opacity?: number }[];
}) {
  return (
    <ul className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[11.5px] text-muted-foreground">
      {items.map((i) => (
        <li
          key={i.label}
          className="inline-flex items-center gap-1.5 transition-opacity duration-200 ease-out motion-reduce:transition-none"
          style={i.opacity === undefined ? undefined : { opacity: i.opacity }}
        >
          <span
            aria-hidden
            className="h-2 w-2 rounded-[2px]"
            style={{ background: i.color }}
          />
          {i.label}
          {i.value !== undefined ? (
            <span className="tabular-nums text-foreground">{i.value}</span>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

export function TimeSeriesChart({
  times,
  series,
  height = 180,
  format = (v) => String(Math.round(v)),
  max,
  area = false,
  stacked = false,
  label,
}: {
  times: number[];
  series: Series[];
  height?: number;
  format?: (v: number) => string;
  /** Fixed y maximum (e.g. 1 for utilization). */
  max?: number;
  area?: boolean;
  /** Stacked bars instead of lines. */
  stacked?: boolean;
  label: string;
}) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);
  const pad = { top: 8, right: 8, bottom: 22, left: 44 };
  const w = Math.max(0, width - pad.left - pad.right);
  const h = height - pad.top - pad.bottom;
  const n = times.length;

  const yMax = useMemo(() => {
    if (max) return max;
    const peak = stacked
      ? Math.max(
          ...times.map((_, i) =>
            series.reduce((a, s) => a + (s.values[i] ?? 0), 0),
          ),
        )
      : Math.max(0, ...known(series.flatMap((s) => s.values)));
    return niceMax(peak * 1.1);
  }, [max, series, stacked, times]);

  const x = (i: number) =>
    stacked ? (w / n) * (i + 0.5) : n <= 1 ? w / 2 : (w / (n - 1)) * i;
  const y = (v: number) => h - (v / yMax) * h;
  const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => f * yMax);
  const xTickEvery = Math.max(
    1,
    Math.ceil(n / Math.max(2, Math.floor(w / 80))),
  );
  const span = n > 1 ? times[n - 1] - times[0] : 0;

  const onMove = (e: React.PointerEvent<SVGRectElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    const px = e.clientX - rect.left;
    const i = stacked
      ? Math.floor((px / w) * n)
      : Math.round((px / w) * (n - 1));
    setHover(Math.max(0, Math.min(n - 1, i)));
  };

  const hx = hover !== null ? x(hover) : 0;
  const tooltipLeft =
    hover !== null
      ? Math.min(Math.max(pad.left + hx - 80, 0), Math.max(0, width - 170))
      : 0;

  return (
    <div ref={ref} className="relative w-full select-none" style={{ height }}>
      {width > 0 && n > 0 ? (
        <svg
          width={width}
          height={height}
          role="img"
          aria-label={label}
          className="overflow-visible"
        >
          <g transform={`translate(${pad.left},${pad.top})`}>
            {ticks.map((t, i) => (
              <g key={i}>
                <line
                  x1={0}
                  x2={w}
                  y1={y(t)}
                  y2={y(t)}
                  stroke="hsl(var(--chart-grid))"
                  strokeWidth={1}
                />
                <text
                  x={-8}
                  y={y(t)}
                  dy="0.32em"
                  textAnchor="end"
                  className="fill-muted-foreground text-[10.5px] tabular-nums"
                >
                  {format(t)}
                </text>
              </g>
            ))}
            {times.map((t, i) => {
              // Skip ticks whose label repeats the previous one (e.g. two per day).
              if (
                i % xTickEvery !== 0 ||
                (i >= xTickEvery &&
                  tickTime(times[i - xTickEvery], span) === tickTime(t, span))
              )
                return null;
              return (
                <text
                  key={t}
                  x={x(i)}
                  y={h + 15}
                  textAnchor="middle"
                  className="fill-muted-foreground text-[10.5px]"
                >
                  {tickTime(t, span)}
                </text>
              );
            })}
            {stacked
              ? times.map((_, i) => {
                  let acc = 0;
                  const bw = Math.max(1, w / n - 2);
                  return (
                    <g
                      key={i}
                      opacity={hover === null || hover === i ? 1 : 0.55}
                    >
                      {series.map((s) => {
                        const v = s.values[i] ?? 0;
                        const y0 = y(acc);
                        acc += v;
                        const y1 = y(acc);
                        return v > 0 ? (
                          <rect
                            key={s.key}
                            x={x(i) - bw / 2}
                            y={y1}
                            width={bw}
                            height={Math.max(0, y0 - y1 - (acc > v ? 1 : 0))}
                            fill={s.color}
                            rx={1}
                            style={
                              s.opacity === undefined
                                ? undefined
                                : { opacity: s.opacity, transition: "opacity 200ms ease-out, y 220ms ease-out, height 220ms ease-out" }
                            }
                          />
                        ) : null;
                      })}
                    </g>
                  );
                })
              : series.map((s) => {
                  const parts = runs(s.values);
                  const line = (run: [number, number][]) =>
                    run
                      .map(
                        ([i, v], j) =>
                          `${j ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`,
                      )
                      .join("");
                  const d = parts.map(line).join("");
                  return (
                    <g key={s.key}>
                      {area
                        ? parts.map((run) => (
                            <path
                              key={run[0][0]}
                              d={`${line(run)}L${x(run[run.length - 1][0])},${h}L${x(run[0][0])},${h}Z`}
                              fill={s.color}
                              opacity={0.12}
                            />
                          ))
                        : null}
                      <path
                        d={d}
                        fill="none"
                        stroke={s.color}
                        strokeWidth={2}
                        strokeLinejoin="round"
                        strokeLinecap="round"
                      />
                    </g>
                  );
                })}
            {hover !== null && !stacked ? (
              <g>
                <line
                  x1={hx}
                  x2={hx}
                  y1={0}
                  y2={h}
                  stroke="hsl(var(--muted-foreground))"
                  strokeOpacity={0.5}
                  strokeDasharray="3 3"
                />
                {series.map((s) => (
                  <circle
                    key={s.key}
                    cx={hx}
                    cy={y(s.values[hover] ?? 0)}
                    r={4}
                    fill={s.color}
                    stroke="hsl(var(--card))"
                    strokeWidth={2}
                  />
                ))}
              </g>
            ) : null}
            <rect
              x={0}
              y={0}
              width={w}
              height={h}
              fill="transparent"
              onPointerMove={onMove}
              onPointerLeave={() => setHover(null)}
            />
          </g>
        </svg>
      ) : null}
      {hover !== null ? (
        <div
          className="pointer-events-none absolute top-0 z-10 w-[170px] rounded-md border border-border bg-popover px-2.5 py-2 text-[11.5px] shadow-lg"
          style={{ left: tooltipLeft }}
        >
          <p className="mb-1 text-muted-foreground">
            {new Date(times[hover]).toLocaleString("en-US", {
              month: "short",
              day: "numeric",
              hour: "2-digit",
              minute: "2-digit",
              hour12: false,
            })}
          </p>
          {series.map((s) => (
            <p key={s.key} className="flex items-center justify-between gap-3">
              <span className="inline-flex items-center gap-1.5 text-muted-foreground">
                <span
                  className="h-2 w-2 rounded-[2px]"
                  style={{ background: s.color }}
                />
                {s.label}
              </span>
              <span className="tabular-nums text-foreground">
                {s.values[hover] == null
                  ? "No data"
                  : format(s.values[hover] ?? 0)}
              </span>
            </p>
          ))}
        </div>
      ) : null}
    </div>
  );
}

/** Ranked horizontal bars; labels and values stay in text ink. */
export function BarList({
  rows,
  format = (v) => v.toLocaleString(),
  mono,
  empty = "No data",
}: {
  rows: { label: ReactNode; value: number; key: string; sub?: ReactNode }[];
  format?: (v: number) => string;
  mono?: boolean;
  empty?: string;
}) {
  const top = Math.max(1, ...rows.map((r) => r.value));
  if (!rows.length)
    return (
      <p className="px-4 py-6 text-center text-[12px] text-muted-foreground">
        {empty}
      </p>
    );
  return (
    <ul className="space-y-1 px-2 py-2">
      {rows.map((r) => (
        <li
          key={r.key}
          className="group relative flex items-center justify-between gap-3 rounded px-2 py-1.5 text-[12.5px] hover:bg-accent/40"
        >
          <span
            aria-hidden
            className="absolute inset-y-0.5 left-0 rounded-[3px] bg-primary/12 transition-colors group-hover:bg-primary/20"
            style={{ width: `${(r.value / top) * 100}%` }}
          />
          <span
            className={`relative min-w-0 truncate text-foreground ${mono ? "font-mono text-[11.5px]" : ""}`}
          >
            {r.label}
          </span>
          <span className="relative flex shrink-0 items-center gap-3 tabular-nums">
            {r.sub ? (
              <span className="text-muted-foreground">{r.sub}</span>
            ) : null}
            <span className="text-foreground">{format(r.value)}</span>
          </span>
        </li>
      ))}
    </ul>
  );
}

/** Single proportional bar with a legend (e.g. status-code mix). */
export function ProportionBar({
  parts,
  label,
}: {
  parts: { label: string; value: number; color: string }[];
  label: string;
}) {
  const total = parts.reduce((a, p) => a + p.value, 0) || 1;
  return (
    <div>
      <div
        role="img"
        aria-label={`${label}: ${parts.map((p) => `${p.label} ${((p.value / total) * 100).toFixed(1)}%`).join(", ")}`}
        className="flex h-2.5 gap-0.5 overflow-hidden rounded-full"
      >
        {parts.map((p) =>
          p.value > 0 ? (
            <div
              key={p.label}
              title={`${p.label}: ${p.value.toLocaleString()}`}
              style={{
                width: `${(p.value / total) * 100}%`,
                background: p.color,
              }}
              className="min-w-[3px] first:rounded-l-full last:rounded-r-full"
            />
          ) : null,
        )}
      </div>
      <div className="mt-2.5">
        <Legend
          items={parts.map((p) => ({
            label: p.label,
            color: p.color,
            value: `${((p.value / total) * 100).toFixed(1)}%`,
          }))}
        />
      </div>
    </div>
  );
}

/** A tiny trend line. `domain` fixes the scale (e.g. [0, 1] for a share of capacity) so small wobbles stay small. */
export function Sparkline({
  values,
  color = "var(--series-1)",
  width = 80,
  height = 24,
  label,
  domain,
  fill = false,
}: {
  values: (number | null)[];
  color?: string;
  width?: number;
  height?: number;
  label: string;
  domain?: [number, number];
  /** Stretch to the container's width (the line keeps its thickness). */
  fill?: boolean;
}) {
  const have = known(values);
  if (values.length < 2 || !have.length) return null;
  const max = domain ? domain[1] : Math.max(...have) || 1;
  const min = domain ? domain[0] : Math.min(...have);
  const span = max - min || 1;
  const d = runs(values)
    .map((run) =>
      run
        .map(
          ([i, v], j) =>
            `${j ? "L" : "M"}${((i / (values.length - 1)) * width).toFixed(1)},${(height - 2 - ((v - min) / span) * (height - 4)).toFixed(1)}`,
        )
        .join(""),
    )
    .join("");
  return (
    <svg
      width={fill ? "100%" : width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      role="img"
      aria-label={label}
      className={fill ? "block" : "shrink-0"}
    >
      <path
        d={d}
        fill="none"
        stroke={color}
        strokeWidth={1.5}
        strokeLinejoin="round"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}

export const SERIES = [
  "var(--series-1)",
  "var(--series-2)",
  "var(--series-3)",
] as const;
export const STATUS_COLORS = {
  good: "#0ca30c",
  warning: "#fab219",
  serious: "#ec835a",
  critical: "#d03b3b",
} as const;
