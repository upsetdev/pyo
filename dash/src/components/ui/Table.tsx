import { Tabs } from "./Tabs";
import { fieldState, optionClass } from "./fieldStyles";
import {
  CaretDownIcon,
  CaretUpIcon,
  CheckIcon,
  CopyIcon,
  MagnifyingGlassIcon,
} from "@phosphor-icons/react";
import { useTeamPathname } from "~/lib/pyo/paths";
import { AppLink } from "./AppLink";
import {
  useState,
  type ReactNode,
  type ThHTMLAttributes,
  useEffect,
  useRef,
} from "react";
import { copyText } from "~/lib/pyo/clipboard";

// Tables, tabs and small shared controls.

/** `fluid`: no minimum width, for tables that drop columns on small screens. */
export function Table({
  children,
  className = "",
  fluid,
}: {
  children: ReactNode;
  className?: string;
  fluid?: boolean;
}) {
  return (
    <div className={`relative overflow-x-auto ${className}`}>
      <table
        className={`w-full border-collapse text-left text-[12.5px] ${fluid ? "" : "min-w-[640px]"}`}
      >
        {children}
      </table>
    </div>
  );
}

export function Th({
  children,
  className = "",
  ...props
}: ThHTMLAttributes<HTMLTableCellElement>) {
  return (
    <th
      scope="col"
      className={`h-10 whitespace-nowrap border-b border-card-border bg-muted/60 px-3 text-[11.5px] font-medium text-muted-foreground first:pl-4 last:pr-4 ${className}`}
      {...props}
    >
      {children}
    </th>
  );
}

export function Td({
  children,
  className = "",
  colSpan,
}: {
  children?: ReactNode;
  className?: string;
  colSpan?: number;
}) {
  return (
    <td
      colSpan={colSpan}
      className={`border-b border-divider px-3 py-3 align-middle first:pl-4 last:pr-4 group-last/row:border-b-0 ${className}`}
    >
      {children}
    </td>
  );
}

export function Tr({
  children,
  onClick,
  className = "",
}: {
  children: ReactNode;
  onClick?: () => void;
  className?: string;
}) {
  return (
    <tr
      onClick={onClick}
      className={`group/row transition-colors hover:bg-accent/70 ${onClick ? "cursor-pointer" : ""} ${className}`}
    >
      {children}
    </tr>
  );
}

export type Sort<K extends string> = { key: K; dir: "asc" | "desc" };

export function SortTh<K extends string>({
  label,
  k,
  sort,
  onSort,
  className = "",
}: {
  label: string;
  k: K;
  sort: Sort<K>;
  onSort: (s: Sort<K>) => void;
  className?: string;
}) {
  const active = sort.key === k;
  return (
    <Th
      className={className}
      aria-sort={
        active ? (sort.dir === "asc" ? "ascending" : "descending") : "none"
      }
    >
      <button
        type="button"
        onClick={() =>
          onSort({
            key: k,
            dir: active && sort.dir === "desc" ? "asc" : "desc",
          })
        }
        className={`inline-flex items-center gap-1 hover:text-foreground ${active ? "text-foreground" : ""}`}
      >
        {label}
        {active ? (
          sort.dir === "asc" ? (
            <CaretUpIcon size={10} weight="bold" />
          ) : (
            <CaretDownIcon size={10} weight="bold" />
          )
        ) : null}
      </button>
    </Th>
  );
}

export function sortBy<T, K extends string>(
  rows: T[],
  sort: Sort<K>,
  get: (row: T, key: K) => string | number,
) {
  const m = sort.dir === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const x = get(a, sort.key);
    const y = get(b, sort.key);
    return (
      (typeof x === "number" && typeof y === "number"
        ? x - y
        : String(x).localeCompare(String(y))) * m
    );
  });
}

/** Route-backed tabs (underline). `exact` marks tabs that match only their own path. */
export function RouteTabs({
  tabs,
  label,
}: {
  tabs: {
    label: ReactNode;
    to: string;
    exact?: boolean;
    count?: number;
    also?: string[];
  }[];
  label: string;
}) {
  const pathname = useTeamPathname();
  const active = tabs.find((t) =>
    t.exact
      ? pathname === t.to
      : [t.to, ...(t.also ?? [])].some((to) => pathname === to || pathname.startsWith(`${to}/`)),
  )?.to;
  // Each tab is its own page, so this remounts: remember the last one so the
  // pill slides over from it.
  // Recorded once, as the page opens: by the time this unmounts the path is
  // already the next tab's.
  const [from] = useState(() => lastRouteTab.get(label));
  const [opened] = useState(active);
  useEffect(() => {
    if (opened) lastRouteTab.set(label, opened);
  }, [label, opened]);
  return (
    <Tabs
      label={label}
      value={active}
      from={from}
      items={tabs.map((t) => ({ value: t.to, href: t.to, exact: t.exact, label: t.label, count: t.count }))}
    />
  );
}

const lastRouteTab = new Map<string, string>();

export function SearchInput({
  value,
  onChange,
  placeholder = "Search",
  className = "",
  label,
  shortcut,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
  label?: string;
  /** A key that focuses the field from anywhere on the page, e.g. "/". */
  shortcut?: string;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (!shortcut) return;
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      const typing =
        el &&
        (el.isContentEditable ||
          ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName));
      if (
        e.key === shortcut &&
        !typing &&
        !e.metaKey &&
        !e.ctrlKey &&
        !e.altKey
      ) {
        e.preventDefault();
        ref.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [shortcut]);
  return (
    <div className={`relative ${className}`}>
      <MagnifyingGlassIcon
        size={13}
        className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-muted-foreground"
      />
      <input
        ref={ref}
        type="search"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape" && value) {
            e.preventDefault();
            onChange("");
          }
        }}
        placeholder={placeholder}
        aria-label={label ?? placeholder}
        aria-keyshortcuts={shortcut}
        className={`h-9 w-full rounded-full pl-8 text-[13px] placeholder:text-muted-foreground/80 ${fieldState} ${shortcut ? "pr-10" : "pr-3"}`}
      />
      {shortcut && !value ? (
        <kbd className="pointer-events-none absolute right-2.5 top-1/2 -translate-y-1/2 rounded-md border border-border px-1.5 font-mono text-[11px] leading-5 text-muted-foreground">
          {shortcut}
        </kbd>
      ) : null}
    </div>
  );
}

export function CopyButton({
  value,
  label = "Copy",
  className = "",
}: {
  value: string;
  label?: string;
  className?: string;
}) {
  const [done, setDone] = useState(false);
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={async () => {
        try {
          await copyText(value);
          setDone(true);
          setTimeout(() => setDone(false), 1200);
        } catch {
          // Nothing could copy it.
        }
      }}
      className={`inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-muted-foreground transition hover:bg-accent hover:text-foreground ${className}`}
    >
      {done ? (
        <CheckIcon size={12} className="text-success" />
      ) : (
        <CopyIcon size={12} />
      )}
    </button>
  );
}

/** Filter chip row. */
export function FilterChips<T extends string>({
  value,
  onChange,
  options,
  label,
}: {
  value: T;
  onChange: (v: T) => void;
  options: { value: T; label: string; count?: number }[];
  label: string;
}) {
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className="scrollbar-none flex gap-1 overflow-x-auto"
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          onClick={() => onChange(o.value)}
          className={`${optionClass({ selected: value === o.value, pill: true })} inline-flex h-8 shrink-0 items-center gap-1.5 px-3 text-[12px] font-medium ${value === o.value ? "text-foreground" : "text-muted-foreground hover:text-foreground"}`}
        >
          {o.label}
          {o.count !== undefined ? (
            <span className="tabular-nums text-muted-foreground">
              {o.count}
            </span>
          ) : null}
        </button>
      ))}
    </div>
  );
}
