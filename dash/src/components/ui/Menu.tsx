import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { AppLink } from "./AppLink";

// Dropdown menu with keyboard support: arrows move between items, Escape
// closes and returns focus to the trigger, clicking outside closes. Like
// Select, the panel is portaled with fixed positioning so scrolling tables
// and overflow-hidden panels can't clip it.

export type MenuItem =
  | {
      type?: "item";
      label: ReactNode;
      icon?: ReactNode;
      onSelect?: () => void;
      href?: string;
      danger?: boolean;
      disabled?: boolean;
      hint?: ReactNode;
      /** Text the search box matches (defaults to a string label). */
      keywords?: string;
    }
  | { type: "separator" }
  | { type: "label"; label: ReactNode };

export function Menu({
  trigger,
  items,
  align = "end",
  width = "w-56",
  label,
  hover = false,
  children,
  search,
}: {
  trigger: (p: {
    onClick: () => void;
    "aria-expanded": boolean;
    "aria-haspopup": "menu";
    "aria-label"?: string;
  }) => ReactNode;
  items?: MenuItem[];
  align?: "start" | "end";
  width?: string;
  label?: string;
  /** Also open while the pointer is over the trigger (and the menu). */
  hover?: boolean;
  /** Custom content above the items (e.g. a user card). */
  children?: ReactNode;
  /** Show a search box (its placeholder) that filters the items. */
  search?: string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);
  const [pos, setPos] = useState<{
    left?: number;
    right?: number;
    top?: number;
    bottom?: number;
  } | null>(null);
  const [host, setHost] = useState<Element | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const place = useCallback(() => {
    const r = ref.current?.getBoundingClientRect();
    if (!r) return;
    const up =
      window.innerHeight - r.bottom < 260 &&
      r.top > window.innerHeight - r.bottom;
    setPos({
      ...(align === "end"
        ? { right: window.innerWidth - r.right }
        : { left: r.left }),
      ...(up
        ? { bottom: window.innerHeight - r.top + 6 }
        : { top: r.bottom + 6 }),
    });
  }, [align]);

  const show = () => {
    setQuery("");
    setHost(ref.current?.closest("dialog") ?? document.body);
    place();
    setOpen(true);
  };
  const toggle = () => {
    // A hover menu is already open under the pointer; a click keeps it open.
    if (hover && open) {
      hovered.current = false;
      return;
    }
    hovered.current = false;
    if (open) setOpen(false);
    else show();
  };

  // Hover: open on mouseover (it bubbles from the trigger, and through React
  // from the portaled menu); close shortly after leaving both the trigger and
  // the menu (the delay bridges the gap between them).
  const hovered = useRef(false);
  const leaveTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const enter = () => {
    clearTimeout(leaveTimer.current);
    if (!open) {
      hovered.current = true;
      show();
    }
  };
  const leave = () => {
    clearTimeout(leaveTimer.current);
    leaveTimer.current = setTimeout(() => setOpen(false), 150);
  };
  useEffect(() => () => clearTimeout(leaveTimer.current), []);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (!ref.current?.contains(t) && !menuRef.current?.contains(t))
        setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    window.addEventListener("scroll", place, true);
    window.addEventListener("resize", place);
    return () => {
      document.removeEventListener("mousedown", onDown);
      window.removeEventListener("scroll", place, true);
      window.removeEventListener("resize", place);
    };
  }, [open, place]);

  useEffect(() => {
    // Opened by hover: leave focus where it is.
    if (open && host && !hovered.current && searchRef.current)
      searchRef.current.focus();
    else if (open && host && !hovered.current)
      menuRef.current
        ?.querySelector<HTMLElement>(
          "[role=menuitem]:not([aria-disabled=true])",
        )
        ?.focus();
  }, [open, host]);

  const close = (refocus = true) => {
    setOpen(false);
    if (refocus)
      ref.current?.querySelector<HTMLElement>("[aria-haspopup]")?.focus();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    const els = Array.from(
      menuRef.current?.querySelectorAll<HTMLElement>(
        "[role=menuitem]:not([aria-disabled=true])",
      ) ?? [],
    );
    const i = els.indexOf(document.activeElement as HTMLElement);
    if (e.key === "Escape") {
      e.preventDefault();
      // Keep a surrounding dialog open.
      e.stopPropagation();
      close();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      els[(i + 1) % els.length]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      els[(i - 1 + els.length) % els.length]?.focus();
    } else if (
      e.key === "Enter" &&
      document.activeElement === searchRef.current
    ) {
      e.preventDefault();
      els[0]?.click();
    } else if (e.key === "Tab") {
      setOpen(false);
    }
  };

  // Searching: keep the items that match, and the group labels above them.
  const q = query.trim().toLowerCase();
  const matches = (it: MenuItem) =>
    it.type === "separator" || it.type === "label"
      ? false
      : (it.keywords ?? (typeof it.label === "string" ? it.label : ""))
          .toLowerCase()
          .includes(q);
  const shown =
    search && q
      ? (items ?? []).filter((it, i, all) => {
          if (it.type === "separator") return false;
          if (it.type !== "label") return matches(it);
          // A label stays if an item before the next label matches.
          for (const next of all.slice(i + 1)) {
            if (next.type === "label") return false;
            if (matches(next)) return true;
          }
          return false;
        })
      : items;

  const itemClass = (danger?: boolean) =>
    `flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-[13px] outline-none transition-colors aria-disabled:pointer-events-none aria-disabled:opacity-45 ${
      danger
        ? "text-destructive hover:bg-destructive/10 focus:bg-destructive/10"
        : "text-foreground hover:bg-accent focus:bg-accent"
    }`;

  return (
    <div
      ref={ref}
      className="relative inline-flex"
      onMouseOver={hover ? enter : undefined}
      onMouseLeave={hover ? leave : undefined}
    >
      {trigger({
        onClick: toggle,
        "aria-expanded": open,
        "aria-haspopup": "menu",
        "aria-label": label,
      })}
      {open && pos && host
        ? createPortal(
            <div
              ref={menuRef}
              role="menu"
              aria-label={label}
              onKeyDown={onKeyDown}
              onMouseEnter={
                hover ? () => clearTimeout(leaveTimer.current) : undefined
              }
              onMouseLeave={hover ? leave : undefined}
              style={pos}
              className={`fixed z-[80] ${width} ${search ? "max-h-[min(24rem,70vh)] overflow-y-auto" : ""} animate-dropdown-in rounded-xl border border-card-border bg-canvas p-1 shadow-pop`}
            >
              {children}
              {search ? (
                <div className="sticky -top-1 z-10 -mx-1 -mt-1 mb-1 border-b border-border bg-canvas px-2 pb-1 pt-1">
                  <input
                    ref={searchRef}
                    type="search"
                    value={query}
                    onChange={(e) => setQuery(e.target.value)}
                    placeholder={search}
                    aria-label={search}
                    className="h-8 w-full rounded-md bg-transparent px-1.5 text-[13px] text-foreground outline-none placeholder:text-muted-foreground"
                  />
                </div>
              ) : null}
              {search && q && !shown?.length ? (
                <div className="px-2.5 py-2 text-[13px] text-muted-foreground">
                  No matches
                </div>
              ) : null}
              {shown?.map((it, i) => {
                if (it.type === "separator")
                  return (
                    <div
                      key={i}
                      className="my-1 h-px bg-border"
                      role="separator"
                    />
                  );
                if (it.type === "label")
                  return (
                    <div
                      key={i}
                      className="px-2 pb-1 pt-1.5 text-[11px] font-medium text-muted-foreground"
                    >
                      {it.label}
                    </div>
                  );
                const content = (
                  <>
                    {it.icon ? (
                      <span className="flex w-4 shrink-0 justify-center text-muted-foreground">
                        {it.icon}
                      </span>
                    ) : null}
                    <span className="min-w-0 flex-1 truncate">{it.label}</span>
                    {it.hint ? (
                      <span className="text-[11px] text-muted-foreground">
                        {it.hint}
                      </span>
                    ) : null}
                  </>
                );
                return it.href ? (
                  <AppLink
                    key={i}
                    href={it.href}
                    role="menuitem"
                    tabIndex={-1}
                    className={itemClass(it.danger)}
                    onClick={() => close(false)}
                  >
                    {content}
                  </AppLink>
                ) : (
                  <button
                    key={i}
                    type="button"
                    role="menuitem"
                    tabIndex={-1}
                    aria-disabled={it.disabled || undefined}
                    className={itemClass(it.danger)}
                    onClick={() => {
                      // Back on the trigger first: a dialog the item opens
                      // returns focus there when it closes.
                      close();
                      it.onSelect?.();
                    }}
                  >
                    {content}
                  </button>
                );
              })}
            </div>,
            host,
          )
        : null}
    </div>
  );
}
