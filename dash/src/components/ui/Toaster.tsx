import {
  CheckIcon,
  CircleNotchIcon,
  ExclamationMarkIcon,
  InfoIcon,
  WarningIcon,
  XIcon,
  type Icon,
} from "@phosphor-icons/react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
} from "react";
import {
  dismissToast,
  holdToasts,
  useToasts,
  type Toast,
  type ToastTone,
} from "~/lib/pyo/toast";
import { Button, ButtonLink } from "./Button";

// Toasts, bottom right. At rest the newest sits in front and up to two older
// ones show as edges behind it; hovering or focusing the stack spreads them
// out into a column. Escape closes the focused toast; so does a flick down.

/** Shown at once; older toasts wait out of sight. */
const DEPTH = 3;
/** How far each card behind peeks out, and how much smaller it is. */
const PEEK = 7;
const SHRINK = 0.045;
/** Space between spread-out cards. */
const SPACING = 8;

// Tone mark: a small tinted disc with a glyph, the card stays neutral.
const marks: Record<ToastTone, { icon: Icon; disc: string }> = {
  success: { icon: CheckIcon, disc: "bg-success/15 text-success" },
  error: { icon: ExclamationMarkIcon, disc: "bg-destructive/15 text-destructive" },
  warning: { icon: WarningIcon, disc: "bg-warning/15 text-warning" },
  info: { icon: InfoIcon, disc: "bg-info/15 text-info" },
  loading: { icon: CircleNotchIcon, disc: "bg-primary/12 text-primary" },
};

function spoken(t: Toast | undefined) {
  return t ? [t.title, t.description].filter(Boolean).join(". ") : "";
}

export function Toaster() {
  const toasts = useToasts();
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [sizes, setSizes] = useState<Record<number, number>>({});
  const stack = useRef<HTMLOListElement>(null);

  const shown = toasts.filter((t) => !t.leaving);
  const spread = (hovered || focused) && shown.length > 0;

  useEffect(() => holdToasts("hover", hovered), [hovered]);
  useEffect(() => holdToasts("focus", focused), [focused]);
  useEffect(() => {
    const sync = () => holdToasts("hidden", document.hidden);
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => {
      document.removeEventListener("visibilitychange", sync);
      holdToasts("hidden", false);
    };
  }, []);
  // The stack empties under the pointer (closed with a click): reset.
  useEffect(() => {
    if (!shown.length) setHovered(false);
  }, [shown.length]);

  const size = (id: number) => sizes[id] ?? 0;
  const frontSize = size(shown[0]?.id);
  // Bottom edge to each card's bottom when spread out.
  const lift: Record<number, number> = {};
  let acc = 0;
  for (const t of shown.slice(0, DEPTH)) {
    lift[t.id] = acc;
    acc += size(t.id) + SPACING;
  }

  const report = useCallback(
    (id: number, h: number) =>
      setSizes((s) => (s[id] === h ? s : { ...s, [id]: h })),
    [],
  );

  const closeAndMoveOn = (id: number) => {
    const next = shown.find((t) => t.id !== id);
    dismissToast(id);
    stack.current
      ?.querySelector<HTMLElement>(
        `[data-toast="${next?.id}"] :is(a, button)`,
      )
      ?.focus();
  };

  return (
    <section
      aria-label="Notifications"
      className="pointer-events-none fixed bottom-4 right-4 z-[60] w-[min(360px,calc(100vw-2rem))]"
    >
      <ol
        ref={stack}
        className="relative"
        style={{ height: spread ? Math.max(acc - SPACING, 0) : frontSize }}
        onMouseEnter={() => setHovered(true)}
        onMouseLeave={() => setHovered(false)}
        onFocus={() => setFocused(true)}
        onBlur={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget)) setFocused(false);
        }}
      >
        {toasts.map((t) => {
          const depth = shown.indexOf(t);
          return (
            <ToastCard
              key={t.id}
              toast={t}
              depth={depth}
              spread={spread}
              lift={lift[t.id] ?? 0}
              frontSize={frontSize}
              onSize={report}
              onEscape={closeAndMoveOn}
            />
          );
        })}
      </ol>
      <div role="status" aria-live="polite" aria-atomic="true" className="sr-only">
        {shown[0]?.tone === "error" ? "" : spoken(shown[0])}
      </div>
      <div role="alert" aria-live="assertive" aria-atomic="true" className="sr-only">
        {spoken(shown.find((t) => t.tone === "error"))}
      </div>
    </section>
  );
}

function ToastCard({
  toast: t,
  depth,
  spread,
  lift,
  frontSize,
  onSize,
  onEscape,
}: {
  toast: Toast;
  /** 0 in front, -1 while leaving. */
  depth: number;
  spread: boolean;
  lift: number;
  frontSize: number;
  onSize: (id: number, height: number) => void;
  onEscape: (id: number) => void;
}) {
  const item = useRef<HTMLLIElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const [arriving, setArriving] = useState(true);
  const [height, setHeight] = useState<number>();
  const settled = useRef({ y: 0, scale: 1, z: 0 });
  const grab = useRef<{ y0: number; t0: number; dy: number } | null>(null);

  // Commit the arriving pose, force a style flush, then let it move.
  useLayoutEffect(() => {
    item.current?.getBoundingClientRect();
    setArriving(false);
  }, []);

  useLayoutEffect(() => {
    const el = body.current;
    if (!el) return;
    const measure = () => {
      setHeight(el.offsetHeight);
      onSize(t.id, el.offsetHeight);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [t.id, onSize]);

  const front = depth === 0;
  const buried = depth >= DEPTH;
  const behind = !front && !spread && !t.leaving;
  const level = Math.min(Math.max(depth, 0), DEPTH - 1);

  let y = spread ? -lift : -level * PEEK;
  let scale = spread ? 1 : 1 - level * SHRINK;
  // A leaving front card fades over the one taking its place; a leaving
  // card elsewhere fades under its neighbours as they close the gap.
  let z = DEPTH + 1 - level;
  if (t.leaving) ({ y, scale, z } = settled.current);
  else settled.current = { y, scale, z: front && !spread ? DEPTH + 2 : 0 };
  let opacity = buried ? 0 : 1;
  if (arriving) {
    y += 12;
    scale *= 0.96;
    opacity = 0;
  } else if (t.leaving) {
    y += 10;
    scale *= 0.97;
    opacity = 0;
  }

  const motion = t.leaving
    ? "[transition:transform_180ms_cubic-bezier(0.4,0,1,1),opacity_160ms_ease-in]"
    : "[transition:transform_320ms_cubic-bezier(0.2,0.8,0.2,1),opacity_220ms_ease-out]";

  // Drag down to throw it away; upward barely gives.
  const setDrag = (dy: number) =>
    item.current?.style.setProperty("--drag", `${dy}px`);
  const onPointerDown = (e: ReactPointerEvent<HTMLLIElement>) => {
    if (e.button !== 0 || behind || t.leaving) return;
    if ((e.target as Element).closest("a, button")) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    grab.current = { y0: e.clientY, t0: performance.now(), dy: 0 };
    item.current?.setAttribute("data-dragging", "");
  };
  const onPointerMove = (e: ReactPointerEvent) => {
    const g = grab.current;
    if (!g) return;
    const raw = e.clientY - g.y0;
    g.dy = raw >= 0 ? raw : -Math.sqrt(-raw) * 1.5;
    setDrag(g.dy);
  };
  const onPointerEnd = () => {
    const g = grab.current;
    if (!g) return;
    grab.current = null;
    item.current?.removeAttribute("data-dragging");
    const speed = g.dy / Math.max(performance.now() - g.t0, 1);
    if (g.dy > 56 || (g.dy > 8 && speed > 0.45)) dismissToast(t.id);
    else setDrag(0);
  };

  const { icon: Mark, disc } = marks[t.tone];

  return (
    <li
      ref={item}
      data-toast={t.id}
      aria-hidden={t.leaving || buried || undefined}
      inert={t.leaving || buried}
      onKeyDown={(e) => {
        if (e.key !== "Escape") return;
        e.stopPropagation();
        onEscape(t.id);
      }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerEnd}
      onPointerCancel={onPointerEnd}
      style={
        {
          "--y": `${y}px`,
          "--scale": scale,
          zIndex: z,
          opacity,
        } as CSSProperties
      }
      className={`absolute inset-x-0 bottom-0 origin-bottom touch-pan-x select-none [transform:translate3d(0,calc(var(--y)+var(--drag,0px)),0)_scale(var(--scale))] data-[dragging]:transition-none! motion-reduce:transform-none! motion-reduce:[transition:opacity_120ms_linear]! ${motion} ${
        t.leaving || buried ? "pointer-events-none" : "pointer-events-auto"
      } after:absolute after:inset-x-0 after:bottom-full after:h-2`}
    >
      <div
        className="overflow-hidden rounded-[14px] border border-border-strong bg-popover text-foreground shadow-pop transition-[height] duration-300 ease-[cubic-bezier(0.2,0.8,0.2,1)]"
        // Cards behind are cut to the front card's height: only an edge shows.
        style={{
          height:
            height === undefined
              ? undefined
              : (behind && frontSize ? frontSize : height) + 2,
        }}
      >
        <div
          ref={body}
          className={`group/toast flex items-start gap-3 py-3 pl-3 pr-2.5 transition-opacity duration-200 ${
            behind ? "opacity-0" : ""
          }`}
        >
          <span
            aria-hidden="true"
            className={`flex size-6 shrink-0 items-center justify-center rounded-full -my-0.5 ${disc}`}
          >
            <Mark
              size={13}
              weight="bold"
              className={
                t.tone === "loading"
                  ? "animate-spin motion-reduce:animate-none"
                  : ""
              }
            />
          </span>
          <div className="min-w-0 flex-1">
            <p className="text-[13px] font-medium leading-5 text-foreground">
              {t.title}
            </p>
            {t.description ? (
              <p
                title={t.description}
                className="mt-0.5 line-clamp-2 break-words text-[12px] leading-[18px] text-muted-foreground"
              >
                {t.description}
              </p>
            ) : null}
          </div>
          {t.action ? <ToastAction toast={t} /> : null}
          <button
            type="button"
            aria-label="Dismiss"
            onClick={() => dismissToast(t.id)}
            className="-my-0.5 flex size-6 shrink-0 items-center justify-center rounded-full text-muted-foreground opacity-0 transition-[opacity,background-color,color] duration-150 hover:bg-foreground/[0.07] hover:text-foreground focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring group-hover/toast:opacity-100 group-focus-within/toast:opacity-100 [@media(hover:none)]:opacity-100"
          >
            <XIcon size={12} weight="bold" />
          </button>
        </div>
      </div>
    </li>
  );
}

function ToastAction({ toast: t }: { toast: Toast }) {
  const { label, href, onClick } = t.action!;
  const go = () => {
    onClick?.();
    dismissToast(t.id);
  };
  return href ? (
    <ButtonLink href={href} variant="secondary" size="xs" className="-my-1" onClick={go}>
      {label}
    </ButtonLink>
  ) : (
    <Button variant="secondary" size="xs" className="-my-1" onClick={go}>
      {label}
    </Button>
  );
}
