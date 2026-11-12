import { fieldState } from "./fieldStyles";
import { CaretUpDownIcon, CheckIcon } from "@phosphor-icons/react";
import {
  Children,
  isValidElement,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";

// A field-style trigger that opens a short menu: a small
// heading, each option, and a check on the current one. It keeps the
// <select> API: pass <option> children, read e.target.value in onChange.
//
// The menu renders in a portal with fixed positioning so panels with
// overflow-hidden can't clip it. Inside a modal <dialog> it portals into the
// dialog, because the dialog sits in the top layer above document.body.

/** `hidden` options (placeholders) label an empty trigger but stay out of the menu. */
type Opt = {
  value: string;
  label: ReactNode;
  text: string;
  disabled: boolean;
  hidden: boolean;
};

/** The words in an option, through any icons or wrappers (for type-to-jump). */
function textOf(node: ReactNode): string {
  return Children.toArray(node)
    .map((n) =>
      typeof n === "string" || typeof n === "number"
        ? String(n)
        : isValidElement<{ children?: ReactNode }>(n)
          ? textOf(n.props.children)
          : "",
    )
    .join("")
    .trim();
}

function readOptions(children: ReactNode): Opt[] {
  const out: Opt[] = [];
  Children.forEach(children, (child) => {
    if (!isValidElement(child)) return;
    const el = child as ReactElement<{
      value?: string | number;
      children?: ReactNode;
      disabled?: boolean;
      hidden?: boolean;
    }>;
    if (el.type === "option") {
      const text = textOf(el.props.children);
      out.push({
        value: String(el.props.value ?? text),
        label: el.props.children,
        text,
        disabled: !!el.props.disabled,
        hidden: !!el.props.hidden,
      });
    } else if (el.props.children) {
      out.push(...readOptions(el.props.children));
    }
  });
  return out;
}

export type SelectProps = {
  value?: string | number;
  defaultValue?: string | number;
  onChange?: (e: { target: { value: string } }) => void;
  children: ReactNode;
  disabled?: boolean;
  className?: string;
  id?: string;
  /** Menu heading; also the accessible name when no label is attached. */
  heading?: string;
  "aria-label"?: string;
  "aria-invalid"?: boolean;
  "aria-describedby"?: string;
};

const triggerBase =
  `flex h-9 min-w-0 items-center gap-1.5 rounded-full pl-3.5 pr-3 text-left text-[13px] ${fieldState} aria-expanded:border-primary aria-expanded:ring-[3px] aria-expanded:ring-primary/20`;

export function Select({
  value,
  defaultValue,
  onChange,
  children,
  disabled,
  className = "",
  id,
  heading,
  ...aria
}: SelectProps) {
  const all = readOptions(children);
  const options = all.filter((o) => !o.hidden);
  const current = String(value ?? defaultValue ?? "");
  const selected = all.find((o) => o.value === current) ?? options[0];
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [pos, setPos] = useState<{
    left: number;
    width: number;
    top?: number;
    bottom?: number;
    maxHeight: number;
  } | null>(null);
  const [host, setHost] = useState<Element | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const listId = useId();
  const title = heading ?? aria["aria-label"];

  const place = useCallback(() => {
    const r = trigger.current?.getBoundingClientRect();
    if (!r) return;
    const below = window.innerHeight - r.bottom;
    const up = below < 240 && r.top > below;
    const maxHeight = Math.max(160, Math.min(320, (up ? r.top : below) - 16));
    setPos({
      left: r.left,
      width: Math.max(r.width, 200),
      maxHeight,
      ...(up
        ? { bottom: window.innerHeight - r.top + 4 }
        : { top: r.bottom + 4 }),
    });
  }, []);

  const openMenu = () => {
    if (disabled) return;
    setHost(trigger.current?.closest("dialog") ?? document.body);
    place();
    setActive(
      Math.max(
        0,
        options.findIndex((o) => o.value === selected?.value),
      ),
    );
    setOpen(true);
  };
  const close = (refocus = true) => {
    setOpen(false);
    if (refocus) trigger.current?.focus();
  };
  const choose = (o: Opt) => {
    if (o.disabled) return;
    if (o.value !== current) onChange?.({ target: { value: o.value } });
    close();
  };

  useLayoutEffect(() => {
    if (open) list.current?.focus();
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (!trigger.current?.contains(t) && !list.current?.contains(t))
        close(false);
    };
    const onScroll = (e: Event) => {
      if (!list.current?.contains(e.target as Node)) place();
    };
    document.addEventListener("mousedown", onDown);
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", place);
    return () => {
      document.removeEventListener("mousedown", onDown);
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", place);
    };
    // close is stable enough for this listener's lifetime.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, place]);

  useEffect(() => {
    if (open)
      list.current
        ?.querySelector(`[data-index="${active}"]`)
        ?.scrollIntoView({ block: "nearest" });
  }, [active, open]);

  const step = (from: number, dir: 1 | -1) => {
    for (let i = 1; i <= options.length; i++) {
      const n = (from + dir * i + options.length) % options.length;
      if (!options[n].disabled) return n;
    }
    return from;
  };

  const onListKey = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") setActive((a) => step(a, 1));
    else if (e.key === "ArrowUp") setActive((a) => step(a, -1));
    else if (e.key === "Home") setActive(step(-1, 1));
    else if (e.key === "End") setActive(step(options.length, -1));
    else if (e.key === "Enter" || e.key === " ")
      options[active] && choose(options[active]);
    else if (e.key === "Escape") {
      // Keep a surrounding dialog open.
      e.stopPropagation();
      close();
    } else if (e.key === "Tab") close(false);
    else if (e.key.length === 1) {
      // Type-ahead: jump to the next option starting with the typed letter.
      const k = e.key.toLowerCase();
      const n = options.findIndex(
        (o, i) =>
          i > active && !o.disabled && o.text.toLowerCase().startsWith(k),
      );
      const m =
        n >= 0
          ? n
          : options.findIndex(
              (o) => !o.disabled && o.text.toLowerCase().startsWith(k),
            );
      if (m >= 0) setActive(m);
      return;
    } else return;
    e.preventDefault();
  };

  return (
    <>
      <button
        ref={trigger}
        id={id}
        type="button"
        disabled={disabled}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-label={aria["aria-label"]}
        aria-invalid={aria["aria-invalid"]}
        aria-describedby={aria["aria-describedby"]}
        onClick={() => (open ? close(false) : openMenu())}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            openMenu();
          }
        }}
        className={`${triggerBase} ${/(^|\s)w-/.test(className) ? "" : "w-full"} ${className}`}
      >
        <span
          className={`min-w-0 flex-1 truncate ${selected?.hidden ? "text-muted-foreground" : ""}`}
        >
          {selected?.label}
        </span>
        <CaretUpDownIcon
          size={13}
          weight="bold"
          aria-hidden
          className="shrink-0 text-muted-foreground"
        />
      </button>
      {open && pos && host
        ? createPortal(
            <div
              className="fixed z-[80] animate-dropdown-in rounded-xl border border-card-border bg-canvas p-1 shadow-pop"
              style={{
                left: pos.left,
                top: pos.top,
                bottom: pos.bottom,
                width: pos.width,
              }}
            >
              {title ? (
                <p className="px-2.5 pb-1 pt-1.5 text-[11px] font-medium text-muted-foreground">
                  {title}
                </p>
              ) : null}
              <ul
                ref={list}
                id={listId}
                role="listbox"
                tabIndex={-1}
                aria-label={title}
                aria-activedescendant={`${listId}-${active}`}
                onKeyDown={onListKey}
                className="overflow-y-auto outline-none"
                style={{ maxHeight: pos.maxHeight }}
              >
                {options.map((o, i) => {
                  const on = o.value === selected?.value;
                  return (
                    <li
                      key={o.value + i}
                      id={`${listId}-${i}`}
                      data-index={i}
                      role="option"
                      aria-selected={on}
                      aria-disabled={o.disabled || undefined}
                      onMouseEnter={() => !o.disabled && setActive(i)}
                      onClick={() => choose(o)}
                      className={`flex cursor-pointer items-center justify-between gap-3 rounded-lg px-2.5 py-1.5 text-[13px] aria-disabled:cursor-not-allowed aria-disabled:opacity-40 ${i === active ? "bg-accent" : ""}`}
                    >
                      <span
                        className={`min-w-0 truncate ${on ? "font-medium text-primary" : "text-foreground"}`}
                      >
                        {o.label}
                      </span>
                      {on ? (
                        <CheckIcon
                          size={13}
                          weight="bold"
                          className="shrink-0 text-primary"
                          aria-hidden
                        />
                      ) : null}
                    </li>
                  );
                })}
              </ul>
            </div>,
            host,
          )
        : null}
    </>
  );
}
