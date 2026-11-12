import { HardDrivesIcon, type Icon } from "@phosphor-icons/react";
import { useEffect, useRef, useState, type HTMLAttributes, type ReactNode } from "react";
import { Dot } from "~/components/ui/Badge";

// The pieces the database and service topologies share, so they look the
// same: a dotted canvas that lays out left to right, or top to bottom where
// that doesn't fit; node cards; connectors with endpoint dots; dashed groups.

export type Dir = "lr" | "tb";
export type Box = { x: number; y: number; w: number; h: number };
export type EdgeStyle = "solid" | "dashed" | "dotted";
export type GraphEdge = {
  key: string;
  d: string;
  p1: [number, number];
  p2: [number, number];
  style: EdgeStyle;
  /** "bad": into something unhealthy; "warn": somewhere unexpected. */
  tone?: "normal" | "bad" | "warn" | "hot";
  /** Requests flowing along it: moving dots. */
  flow?: boolean;
  width?: number;
  dim?: boolean;
};

/** The dotted canvas: measures itself and lays out `build` for its width. */
export function GraphCanvas<G extends { width: number; height: number }>({
  build,
  children,
  className = "",
}: {
  build: (dir: Dir) => G;
  children: (g: G, dir: Dir) => ReactNode;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [room, setRoom] = useState<number | null>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(([entry]) => setRoom(entry.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const lr = build("lr");
  const dir: Dir = room !== null && room < lr.width ? "tb" : "lr";
  const g = dir === "lr" ? lr : build("tb");
  return (
    <div ref={ref} className={`overflow-x-auto ${DOTS} ${className}`} style={DOT_STYLE}>
      <div className="relative mx-auto" style={{ width: g.width, height: g.height }}>
        {children(g, dir)}
      </div>
    </div>
  );
}

/** A dot grid, like a diagram's canvas (the cards on it stay solid). */
export const DOTS = "bg-muted/40";
export const DOT_STYLE = {
  backgroundImage: "radial-gradient(hsl(var(--foreground) / 0.1) 1px, transparent 1px)",
  backgroundSize: "16px 16px",
} as const;

const STROKE = {
  solid: "hsl(var(--muted-foreground) / 0.7)",
  dashed: "hsl(var(--muted-foreground) / 0.55)",
  dotted: "hsl(var(--muted-foreground) / 0.55)",
};
const TONE: Record<NonNullable<GraphEdge["tone"]>, string | null> = {
  normal: null,
  hot: "hsl(var(--primary))",
  bad: "hsl(var(--destructive) / 0.75)",
  warn: "hsl(var(--warning) / 0.8)",
};

/** Connectors, drawn behind the cards. */
export function GraphEdges({ edges, width, height }: { edges: GraphEdge[]; width: number; height: number }) {
  return (
    <svg aria-hidden className="pointer-events-none absolute left-0 top-0 overflow-visible" width={width} height={height}>
      <style>{`
        @keyframes pyo-graph-flow { to { stroke-dashoffset: -16; } }
        .pyo-graph-flow { stroke-dasharray: 1 15; animation: pyo-graph-flow 1.6s linear infinite; }
        @media (prefers-reduced-motion: reduce) { .pyo-graph-flow { animation: none; } }
      `}</style>
      {edges.map((e) => {
        const stroke = TONE[e.tone ?? "normal"] ?? STROKE[e.style];
        const w = e.width ?? 1.25;
        return (
          <g key={e.key} className={e.dim ? "opacity-25" : ""}>
            <path
              d={e.d}
              fill="none"
              stroke={stroke}
              strokeWidth={w}
              strokeDasharray={e.style === "dashed" ? "5 4" : e.style === "dotted" ? "1.5 3.5" : undefined}
              strokeLinecap="round"
            />
            {e.flow ? (
              <path d={e.d} fill="none" strokeWidth={w + 1} strokeLinecap="round" stroke={e.tone === "bad" ? TONE.bad! : "hsl(var(--primary) / 0.7)"} className="pyo-graph-flow" />
            ) : null}
            {[e.p1, e.p2].map(([x, y], i) => (
              <circle key={i} cx={x} cy={y} r={2.75} fill="hsl(var(--card))" stroke={stroke} strokeWidth={1.25} />
            ))}
          </g>
        );
      })}
    </svg>
  );
}

/** Left to right: from a's right side to b's left, a smooth curve. */
export function curveLR(a: Box, b: Box, key: string, style: EdgeStyle = "solid"): GraphEdge {
  const x1 = a.x + a.w;
  const y1 = a.y + a.h / 2;
  const x2 = b.x;
  const y2 = b.y + b.h / 2;
  const c = (x2 - x1) / 2;
  return { key, d: `M${x1},${y1} C${x1 + c},${y1} ${x2 - c},${y2} ${x2},${y2}`, p1: [x1, y1], p2: [x2, y2], style };
}

/** Top to bottom: from a's bottom to b's top, a smooth curve. */
export function curveTB(a: Box, b: Box, key: string, style: EdgeStyle = "solid"): GraphEdge {
  const x1 = a.x + a.w / 2;
  const y1 = a.y + a.h;
  const x2 = b.x + b.w / 2;
  const y2 = b.y;
  const c = (y2 - y1) / 2;
  return { key, d: `M${x1},${y1} C${x1},${y1 + c} ${x2},${y2 - c} ${x2},${y2}`, p1: [x1, y1], p2: [x2, y2], style };
}

/** Left to right along a shared bus halfway, rounded into b (one of several). */
export function busLR(a: Box, b: Box, key: string, style: EdgeStyle = "dashed"): GraphEdge {
  const x1 = a.x + a.w;
  const y1 = a.y + a.h / 2;
  const x2 = b.x;
  const y2 = b.y + b.h / 2;
  const xm = x1 + (x2 - x1) / 2;
  const dy = y2 - y1;
  const r = Math.min(8, Math.abs(dy) / 2);
  const sy = Math.sign(dy);
  const d = sy
    ? `M${x1},${y1} H${xm - r} Q${xm},${y1} ${xm},${y1 + sy * r} V${y2 - sy * r} Q${xm},${y2} ${xm + r},${y2} H${x2}`
    : `M${x1},${y1} H${x2}`;
  return { key, d, p1: [x1, y1], p2: [x2, y2], style };
}

/** Top to bottom: down a rail at `x1` under a, then right into b's left side. */
export function railTB(a: Box, b: Box, x1: number, key: string, style: EdgeStyle = "dashed", r = 10): GraphEdge {
  const y1 = a.y + a.h;
  const x2 = b.x;
  const y2 = b.y + b.h / 2;
  return { key, d: `M${x1},${y1} L${x1},${y2 - r} Q${x1},${y2} ${x1 + r},${y2} L${x2},${y2}`, p1: [x1, y1], p2: [x2, y2], style };
}

/** A dashed frame around a group of nodes, with its label inside. */
export function GraphGroup({ box, label }: { box: Box; label: ReactNode }) {
  return (
    <div
      className="absolute rounded-[14px] border border-dashed border-border-strong bg-card/30"
      style={{ left: box.x, top: box.y, width: box.w, height: box.h }}
    >
      <p className="truncate whitespace-nowrap px-2.5 pt-2 text-[11px] text-muted-foreground">{label}</p>
    </div>
  );
}

/** A small label on the canvas (a group's, above its nodes). */
export function GraphLabel({ x, y, children }: { x: number; y: number; children: ReactNode }) {
  return (
    <p className="absolute whitespace-nowrap text-[11px] text-muted-foreground" style={{ left: x + 2, top: y }}>
      {children}
    </p>
  );
}

/**
 * A node: an icon tile, its role (small, uppercase), its name (mono, with a
 * ready dot), a detail line, the server it's on, and a footer (resources). `emphasis` is
 * the one that matters most (a primary); `small` an entry point (a pooler).
 */
export function GraphNode({
  box,
  icon: IconCmp,
  role,
  name,
  detail,
  mono = true,
  ready,
  server,
  footer,
  emphasis = false,
  small = false,
  tone,
  as = "div",
  className = "",
  children,
  ...rest
}: {
  box: Box;
  icon: Icon;
  role?: ReactNode;
  name?: ReactNode;
  /** A line under the name (an address). */
  detail?: ReactNode;
  mono?: boolean;
  ready?: boolean;
  server?: ReactNode;
  footer?: ReactNode;
  emphasis?: boolean;
  small?: boolean;
  /** "bad": offline; "warn": unexpected; "muted": takes no traffic. */
  tone?: "bad" | "warn" | "muted";
  as?: "div" | "button";
  className?: string;
  children?: ReactNode;
} & Omit<HTMLAttributes<HTMLElement>, "role">) {
  const Tag = as;
  const border = emphasis
    ? "border-primary/50 ring-[3px] ring-primary/10"
    : tone === "bad"
      ? "border-destructive/50"
      : tone === "warn"
        ? "border-dashed border-warning/50"
        : tone === "muted"
          ? "border-dashed border-border-strong shadow-none"
          : "border-card-border";
  return (
    <Tag
      {...(as === "button" ? { type: "button" as const } : {})}
      {...rest}
      className={`absolute flex flex-col rounded-xl border bg-card text-left shadow-card ${border} ${as === "button" ? "transition-colors hover:border-border-strong/80" : ""} ${className}`}
      style={{ left: box.x, top: box.y, width: box.w, height: box.h }}
    >
      <div className={`flex min-h-0 w-full flex-1 items-center gap-2.5 ${small ? "px-2.5" : "px-3"}`}>
        <span
          aria-hidden
          className={`flex shrink-0 items-center justify-center rounded-lg border ${small ? "h-7 w-7" : "h-8 w-8"} ${emphasis ? "border-primary/30 bg-primary/10 text-primary" : tone === "warn" ? "border-warning/30 bg-warning/10 text-warning" : "border-border bg-muted text-muted-foreground"}`}
        >
          <IconCmp size={small ? 13 : 15} />
        </span>
        <div className={`min-w-0 flex-1 leading-tight ${tone === "muted" ? "opacity-75" : ""}`}>
          {role !== undefined ? (
            <p className={`flex min-w-0 items-center gap-1.5 text-[10.5px] font-medium uppercase tracking-[0.06em] ${emphasis ? "text-primary" : "text-muted-foreground"}`}>
              {role}
            </p>
          ) : null}
          {name !== undefined ? (
            <p className={`${role !== undefined ? "mt-0.5 " : ""}flex min-w-0 items-center gap-1.5 ${mono ? "font-mono text-[12px]" : "text-[12.5px] font-medium"}`}>
              {ready !== undefined ? <Dot tone={ready ? "success" : "warning"} /> : null}
              <span className="truncate">{name}</span>
            </p>
          ) : null}
          {detail ? <div className="mt-0.5 min-w-0">{detail}</div> : null}
          {server ? (
            <p className="mt-0.5 flex min-w-0 items-center gap-1 text-[11.5px] text-muted-foreground">
              <HardDrivesIcon size={11} aria-hidden className="shrink-0" />
              <span className="truncate">{server}</span>
            </p>
          ) : null}
          {children}
        </div>
      </div>
      {footer ? <div className="w-full truncate border-t border-border px-3 py-1.5 text-[11px] text-muted-foreground">{footer}</div> : null}
    </Tag>
  );
}
