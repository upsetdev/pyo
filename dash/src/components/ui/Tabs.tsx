import { CaretLeftIcon, CaretRightIcon } from "@phosphor-icons/react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import { AppLink } from "./AppLink";

// Tabs in two looks, after Cloudflare's Kumo: "segmented" (a recessed track
// with a raised pill that slides to the active tab) and "underline" (a line
// under the list with a primary bar that slides). Items are buttons, or links
// for routed tabs. The indicator is one element, measured from the active tab.

export type TabItem<T extends string = string> = {
  value: T;
  label: ReactNode;
  /** A routed tab: rendered as a link (team-relative, like AppLink). */
  href?: string;
  /** Match only the link's own path (passed to the router). */
  exact?: boolean;
  count?: number;
  disabled?: boolean;
  /** For icon-only tabs. */
  ariaLabel?: string;
  title?: string;
};

type Variant = "segmented" | "underline";
type Size = "md" | "base" | "sm";

const LIST: Record<Variant, Record<Size, string>> = {
  segmented: {
    // A primary choice. On small screens it takes the full width as a grid of
    // equal segments (see WIDE), so its height is a minimum.
    md: "min-h-9 rounded-[18px] p-0.5 max-sm:grid max-sm:w-full",
    base: "h-9 rounded-lg px-0.5",
    sm: "h-[26px] rounded-md px-0.5",
  },
  underline: {
    md: "gap-4 border-b border-divider",
    base: "gap-4 border-b border-divider",
    sm: "gap-3 border-b border-divider",
  },
};

const TAB: Record<Variant, Record<Size, string>> = {
  segmented: {
    md: "h-8 min-w-[5.5rem] grow basis-auto justify-center rounded-md px-4 text-[13.5px] font-medium max-sm:min-w-0 max-sm:px-3",
    base: "my-0.5 rounded-md px-2.5 text-[13px]",
    sm: "my-0.5 rounded-sm px-2 text-[12px]",
  },
  underline: {
    md: "my-1 rounded px-2 py-2 text-[13px]",
    base: "my-1 rounded px-2 py-2 text-[13px]",
    sm: "my-1 rounded px-1.5 py-1.5 text-[12px]",
  },
};

// "md" on small screens: up to three options side by side, more stacked, so
// no label is clipped.
const WIDE: Record<number, string> = { 1: "max-sm:grid-cols-1", 2: "max-sm:grid-cols-2", 3: "max-sm:grid-cols-3" };

export function Tabs<T extends string>({
  items,
  value,
  onChange,
  label,
  variant = "segmented",
  size = "base",
  semantics = "tabs",
  pill = false,
  from,
  className = "",
}: {
  items: TabItem<T>[];
  value: T | undefined;
  onChange?: (v: T) => void;
  label: string;
  variant?: Variant;
  size?: Size;
  /** "radio" for a value picker (radiogroup/radio), else tablist/tab. */
  semantics?: "tabs" | "radio";
  /** Segmented as a value picker: a pill track, pill segments. */
  pill?: boolean;
  /** The tab that was active before this mounted (routed tabs): the
   * indicator slides from it instead of appearing in place. */
  from?: T;
  className?: string;
}) {
  const list = useRef<HTMLDivElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const [box, setBox] = useState<{ left: number; top: number; width: number; height: number } | null>(null);
  // No slide on first paint: the indicator appears where it belongs.
  const [animate, setAnimate] = useState(false);
  const [edges, setEdges] = useState({ left: false, right: false });

  const slideFrom = useRef(from !== undefined && from !== value ? from : undefined);
  const measure = useCallback((at?: string) => {
    const el = list.current?.querySelector<HTMLElement>(
      at === undefined ? '[data-active="true"]' : `[data-value="${CSS.escape(at)}"]`,
    );
    if (!el) return setBox(null);
    setBox({ left: el.offsetLeft, top: el.offsetTop, width: el.offsetWidth, height: el.offsetHeight });
  }, []);
  const clip = useCallback(() => {
    const s = scroller.current;
    if (!s) return;
    setEdges({ left: s.scrollLeft > 1, right: s.scrollLeft + s.clientWidth < s.scrollWidth - 1 });
  }, []);

  useLayoutEffect(() => {
    measure(slideFrom.current);
    clip();
  }, [measure, clip, value, items.length]);
  useEffect(() => {
    let g = 0;
    const f = requestAnimationFrame(() => {
      setAnimate(true);
      // Then on to the active tab, now that the indicator transitions.
      if (slideFrom.current !== undefined)
        g = requestAnimationFrame(() => {
          slideFrom.current = undefined;
          measure();
        });
    });
    const ro = new ResizeObserver(() => {
      measure();
      clip();
    });
    if (list.current) ro.observe(list.current);
    if (scroller.current) ro.observe(scroller.current);
    return () => {
      cancelAnimationFrame(f);
      cancelAnimationFrame(g);
      ro.disconnect();
    };
  }, [measure, clip]);
  // Keep the active tab in view.
  useEffect(() => {
    list.current?.querySelector<HTMLElement>('[data-active="true"]')?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [value]);

  const tabs = () => [...(list.current?.querySelectorAll<HTMLElement>("[data-tab]:not([aria-disabled=true])") ?? [])];
  const onKeyDown = (e: KeyboardEvent) => {
    const all = tabs();
    const i = all.indexOf(document.activeElement as HTMLElement);
    if (i < 0) return;
    const next =
      e.key === "ArrowRight" ? all[(i + 1) % all.length]
      : e.key === "ArrowLeft" ? all[(i - 1 + all.length) % all.length]
      : e.key === "Home" ? all[0]
      : e.key === "End" ? all[all.length - 1]
      : null;
    if (!next) return;
    e.preventDefault();
    next.focus();
    // A value picker follows focus, as radio groups do.
    if (semantics === "radio" && !next.dataset.href) next.click();
  };

  const vars = box
    ? ({
        "--active-tab-left": `${box.left}px`,
        "--active-tab-top": `${box.top}px`,
        "--active-tab-width": `${box.width}px`,
        "--active-tab-height": `${box.height}px`,
      } as CSSProperties)
    : undefined;
  const seg = variant === "segmented";
  const big = seg && size === "md";
  // Tailwind's translate-x-* sets the CSS `translate` property, not
  // `transform`, so that's what has to transition (Kumo: transition-all,
  // 200ms, the standard ease).
  const slide = animate
    ? "transition-[translate,width,top,height] duration-200 ease-[cubic-bezier(0.4,0,0.2,1)] motion-reduce:transition-none"
    : "";
  const listRole = semantics === "radio" ? "radiogroup" : "tablist";
  const tabRole = semantics === "radio" ? "radio" : "tab";

  return (
    <div className={`relative min-w-0 ${seg ? "inline-flex max-w-full" : "flex"} ${big ? "max-sm:w-full" : ""} ${className}`}>
      {edges.left ? (
        <ScrollButton side="left" onClick={() => scroller.current?.scrollBy({ left: -160, behavior: "smooth" })} />
      ) : null}
      <div
        ref={scroller}
        onScroll={clip}
        className={`scrollbar-none min-w-0 overflow-x-auto ${big ? "max-sm:flex-1" : ""} ${seg ? `${big ? "rounded-[18px]" : pill ? "rounded-full" : "rounded-lg"} bg-foreground/[0.05]` : "flex-1"}`}
        style={
          edges.left || edges.right
            ? {
                maskImage: `linear-gradient(to right, ${edges.left ? "transparent, black 24px" : "black"}, ${edges.right ? "black calc(100% - 24px), transparent" : "black"})`,
              }
            : undefined
        }
      >
        <div
          ref={list}
          role={listRole}
          aria-label={label}
          onKeyDown={onKeyDown}
          style={vars}
          className={`relative flex min-w-full items-stretch ${big ? (WIDE[items.length] ?? "max-sm:grid-cols-1") : "w-max"} ${LIST[variant][size]} ${seg && pill && !big ? "!rounded-full" : ""}`}
        >
          {box ? (
            seg ? (
              <span
                aria-hidden
                className={`absolute left-0 top-(--active-tab-top) z-[1] h-(--active-tab-height) w-(--active-tab-width) translate-x-(--active-tab-left) ${pill ? "rounded-full bg-card shadow-sm ring-1 ring-card-border" : `bg-card shadow-sm ring-1 ring-card-border ${size === "sm" ? "rounded-sm" : "rounded-md"}`} ${slide}`}
              />
            ) : (
              <span
                aria-hidden
                className={`absolute -bottom-px left-0 z-[1] h-0.5 w-(--active-tab-width) translate-x-(--active-tab-left) rounded-full bg-primary ${slide}`}
              />
            )
          ) : null}
          {items.map((t) => {
            const active = t.value === value;
            const cls = `relative z-[2] flex shrink-0 items-center gap-1.5 whitespace-nowrap outline-none transition-colors duration-200 ease-out focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-primary ${TAB[variant][size]} ${seg && pill ? `!rounded-full ${big ? "" : "px-3"}` : ""} ${seg ? "" : "font-medium"} ${
              active ? "text-foreground" : "text-muted-foreground hover:text-foreground"
            } ${t.disabled ? "pointer-events-none opacity-40" : ""}`;
            const body = (
              <>
                {t.label}
                {t.count !== undefined ? (
                  <span className="rounded bg-muted px-1 text-[11px] tabular-nums text-muted-foreground">{t.count}</span>
                ) : null}
              </>
            );
            const common = {
              "data-tab": "",
              "data-value": t.value,
              "data-active": active ? "true" : undefined,
              role: tabRole,
              "aria-selected": semantics === "tabs" ? active : undefined,
              "aria-checked": semantics === "radio" ? active : undefined,
              "aria-disabled": t.disabled || undefined,
              "aria-label": t.ariaLabel,
              title: t.title,
              tabIndex: active ? 0 : -1,
              className: cls,
            } as const;
            return t.href ? (
              <AppLink
                key={t.value}
                href={t.href}
                {...common}
                data-href={t.href}
                aria-current={active ? "page" : undefined}
                activeOptions={t.exact ? { exact: true } : undefined}
              >
                {body}
              </AppLink>
            ) : (
              <button
                key={t.value}
                type="button"
                {...common}
                disabled={t.disabled}
                onClick={() => onChange?.(t.value)}
              >
                {body}
              </button>
            );
          })}
        </div>
      </div>
      {edges.right ? (
        <ScrollButton side="right" onClick={() => scroller.current?.scrollBy({ left: 160, behavior: "smooth" })} />
      ) : null}
    </div>
  );
}

function ScrollButton({ side, onClick }: { side: "left" | "right"; onClick: () => void }) {
  return (
    <button
      type="button"
      tabIndex={-1}
      aria-hidden
      onClick={onClick}
      className={`absolute inset-y-0 z-[3] my-auto flex h-6 w-6 items-center justify-center rounded-full border border-border-strong bg-card text-muted-foreground shadow-sm hover:text-foreground ${side === "left" ? "left-0" : "right-0"}`}
    >
      {side === "left" ? <CaretLeftIcon size={11} /> : <CaretRightIcon size={11} />}
    </button>
  );
}
