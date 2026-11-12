import { ArrowsInIcon, MinusIcon, PlusIcon } from "@phosphor-icons/react";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

/** The canvas's zoom, for dragging nodes (screen pixels to canvas units). */
const ScaleContext = createContext(1);
export const useCanvasScale = () => useContext(ScaleContext);

// The canvas both the Traffic view and the deploy canvas draw on: a dotted
// grid, content that pans (drag the background) and zooms (buttons, or
// ctrl+wheel and trackpad pinch; a plain wheel still scrolls the page), and
// fits itself on load and on resize.

export const FIT_PAD = 24;
const PAD = FIT_PAD;

export function Viewport({
  label,
  height,
  width: contentW,
  contentHeight: contentH,
  onBackground,
  maxFit = 1,
  reserveRight = 0,
  x0 = 0,
  y0 = 0,
  fitKey,
  onContextMenu,
  overlay,
  className = "",
  children,
}: {
  label: string;
  /** Of the frame, in pixels; "100%" fills its parent. */
  height: number | string;
  width: number;
  contentHeight: number;
  /** A press on the background (not a node). */
  onBackground?: () => void;
  /** The most it zooms in when fitting. */
  maxFit?: number;
  /** Pixels on the right covered by a panel: fit into the rest. */
  reserveRight?: number;
  /** Where the content's box starts, when nodes can sit anywhere. */
  x0?: number;
  y0?: number;
  /** Fits again whenever this changes (by default: when the content's size does). */
  fitKey?: string | number;
  /** A right-click on the background, at a point of the window. */
  onContextMenu?: (clientX: number, clientY: number) => void;
  /** Fixed on top of the canvas (toolbars, bars). */
  overlay?: ReactNode;
  className?: string;
  children: ReactNode;
}) {
  const frame = useRef<HTMLDivElement>(null);
  const [view, setView] = useState({ x: PAD, y: PAD, k: 1 });
  const drag = useRef<{ x: number; y: number; vx: number; vy: number } | null>(null);

  // The latest box, so a fit asked for later uses it.
  const box = useRef({ contentW, contentH, x0, y0, maxFit, reserveRight });
  box.current = { contentW, contentH, x0, y0, maxFit, reserveRight };
  const fit = useCallback(() => {
    const el = frame.current;
    if (!el) return;
    const b = box.current;
    const w = Math.max(240, el.clientWidth - b.reserveRight);
    const h = el.clientHeight;
    const k = Math.min(b.maxFit, (w - PAD * 2) / b.contentW, (h - PAD * 2) / b.contentH);
    setView({ k, x: (w - b.contentW * k) / 2 - b.x0 * k, y: Math.max(PAD, (h - b.contentH * k) / 2) - b.y0 * k });
  }, []);
  useLayoutEffect(fit, [fit, fitKey ?? `${contentW}x${contentH}`, maxFit, reserveRight]);
  useEffect(() => {
    const el = frame.current;
    if (!el) return;
    const ro = new ResizeObserver(fit);
    ro.observe(el);
    return () => ro.disconnect();
  }, [fit]);

  const zoomAt = (f: number, cx: number, cy: number) =>
    setView((v) => {
      const k = Math.min(2, Math.max(0.3, v.k * f));
      const r = k / v.k;
      return { k, x: cx - (cx - v.x) * r, y: cy - (cy - v.y) * r };
    });
  useEffect(() => {
    const el = frame.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey) return;
      e.preventDefault();
      const r = el.getBoundingClientRect();
      zoomAt(Math.exp(-e.deltaY / 300), e.clientX - r.left, e.clientY - r.top);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);
  const zoomBy = (f: number) => {
    const el = frame.current;
    if (el) zoomAt(f, el.clientWidth / 2, el.clientHeight / 2);
  };

  return (
    <div
      ref={frame}
      className={`relative touch-none select-none overflow-hidden rounded-lg border border-frame-border bg-muted/30 ${className}`}
      style={{ height }}
      onPointerDown={(e) => {
        if ((e.target as HTMLElement).closest("[data-node],button,[data-overlay]")) return;
        drag.current = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y };
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        onBackground?.();
      }}
      onPointerMove={(e) => {
        const d = drag.current;
        if (d) setView((v) => ({ ...v, x: d.vx + e.clientX - d.x, y: d.vy + e.clientY - d.y }));
      }}
      onPointerUp={() => {
        drag.current = null;
      }}
      onContextMenu={(e) => {
        if (!onContextMenu || (e.target as HTMLElement).closest("[data-node],button,[data-overlay]")) return;
        e.preventDefault();
        onContextMenu(e.clientX, e.clientY);
      }}
      role="group"
      aria-label={label}
    >
      {/* Dotted grid: quiet, follows the theme. */}
      <div
        className="pointer-events-none absolute inset-0 text-foreground/[0.07]"
        style={{
          backgroundImage: "radial-gradient(currentColor 1px, transparent 1px)",
          backgroundSize: `${16 * view.k}px ${16 * view.k}px`,
          backgroundPosition: `${view.x}px ${view.y}px`,
        }}
      />
      <div
        className="absolute left-0 top-0 origin-top-left"
        style={{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})`, width: contentW, height: contentH }}
      >
        <ScaleContext.Provider value={view.k}>{children}</ScaleContext.Provider>
      </div>
      {overlay}
      <div data-overlay className="absolute bottom-2 right-2 flex overflow-hidden rounded-md border border-border-strong bg-card shadow-sm">
        <CanvasButton label="Zoom in" onClick={() => zoomBy(1.2)}>
          <PlusIcon size={13} />
        </CanvasButton>
        <CanvasButton label="Zoom out" onClick={() => zoomBy(1 / 1.2)}>
          <MinusIcon size={13} />
        </CanvasButton>
        <CanvasButton label="Fit" onClick={fit}>
          <ArrowsInIcon size={13} />
        </CanvasButton>
      </div>
    </div>
  );
}

/** A node's frame: the soft card every canvas node shares. */
export const NODE =
  "absolute rounded-lg border border-card-border bg-card px-2.5 text-left text-[12px] shadow-card transition-[opacity,box-shadow,border-color] duration-150";

export function Health({ ok, label }: { ok: boolean | null; label?: string }) {
  return (
    <span
      className={`h-1.5 w-1.5 shrink-0 rounded-full ${ok === null ? "bg-muted-foreground/50" : ok ? "bg-success" : "bg-destructive"}`}
      role="img"
      aria-label={label ?? (ok ? "Healthy" : "Not healthy")}
    />
  );
}

export function NodeRow({ icon, children }: { icon: ReactNode; children: ReactNode }) {
  return (
    <span className="flex min-w-0 items-center gap-1.5">
      <span className="shrink-0 text-muted-foreground">{icon}</span>
      {children}
    </span>
  );
}

function CanvasButton({ label, onClick, children }: { label: string; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      className="flex h-7 w-7 items-center justify-center text-muted-foreground hover:bg-accent hover:text-foreground"
    >
      {children}
    </button>
  );
}
