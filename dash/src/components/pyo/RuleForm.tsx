import { XIcon } from "@phosphor-icons/react";
import { useState, type FormEvent, type ReactNode } from "react";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented, Select } from "~/components/ui/Field";
import { fieldGroupClass } from "~/components/ui/fieldStyles";
import { Flag } from "~/components/ui/Flag";
import { countryName } from "~/lib/pyo/location";
import { Arrow } from "~/components/ui/Arrow";

// The firewall's and the WAF's rules share one form, as in Cloudflare's
// WAF: say what to match, see it in words, then pick the action, last. Rows in lists read the same way: what a rule
// matches, then its action.

export const COUNTRIES = (() => {
  const codes: string[] = [];
  for (let a = 65; a <= 90; a++)
    for (let b = 65; b <= 90; b++) {
      const c = String.fromCharCode(a, b);
      const name = countryName(c);
      if (name && name !== c && !/Unknown|Region/i.test(name)) codes.push(c);
    }
  return codes
    .filter((c) => !["EU", "EZ", "UN", "QO", "XA", "XB", "ZZ", "DD", "SU", "YU", "CS", "ZR", "TP", "BU", "NT", "YD", "FX", "AN", "UK"].includes(c))
    .filter((c, i, all) => all.findIndex((x) => countryName(x) === countryName(c)) === i)
    .sort((x, y) => countryName(x).localeCompare(countryName(y)));
})();

export const cleanIp = (raw: string) => {
  const t = raw.trim();
  const v4 = /^(\d{1,3})(\.\d{1,3}){3}(\/\d{1,2})?$/.test(t) && t.split("/")[0].split(".").every((o) => Number(o) <= 255) && Number(t.split("/")[1] ?? 32) <= 32;
  const v6 = /^[0-9a-fA-F:]+(\/\d{1,3})?$/.test(t) && t.includes(":") && Number(t.split("/")[1] ?? 128) <= 128;
  return v4 || v6 ? t : null;
};
export const cleanCountry = (raw: string) => {
  const t = raw.trim().toUpperCase();
  return COUNTRIES.includes(t) ? t : null;
};

/** A field's name inside a sentence: "path", but "IP address". */
export const inSentence = (label: string) => (/^IP\b/.test(label) ? label : label.charAt(0).toLowerCase() + label.slice(1));

export type ActionOption<T extends string> = { value: T; label: string; tone: Tone; hint: string };

// ── Pickers ──────────────────────────────────────────────────────────────────

/** Countries as chips with flags, and a search to add more. */
export function CountryPicker({
  value,
  onChange,
  label,
  max = 50,
  disabled = false,
  onEnterEmpty,
}: {
  value: string[];
  onChange: (v: string[]) => void;
  label: string;
  max?: number;
  disabled?: boolean;
  onEnterEmpty?: () => void;
}) {
  const [q, setQ] = useState("");
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const query = q.trim().toLowerCase();
  const found = query
    ? COUNTRIES.filter((c) => !value.includes(c) && (countryName(c).toLowerCase().includes(query) || c.toLowerCase() === query)).slice(0, 8)
    : [];
  const add = (c: string) => {
    if (value.length < max && !value.includes(c)) onChange(max === 1 ? [c] : [...value, c]);
    setQ("");
    setActive(0);
  };
  const listId = `${label.replace(/\W+/g, "-")}-countries`;
  return (
    <div className="relative min-w-[12rem] flex-1">
      <div className={`${fieldGroupClass(false)} h-auto min-h-9 w-full flex-wrap items-center gap-1.5 px-2 py-1.5`}>
        {value.map((c) => (
          <span key={c} className="inline-flex items-center gap-1 rounded-md bg-muted px-1.5 py-0.5 text-[12px]">
            <Flag country={c} size={12} />
            {countryName(c)}
            {disabled ? null : (
              <button type="button" aria-label={`Remove ${countryName(c)}`} onClick={() => onChange(value.filter((x) => x !== c))} className="rounded text-muted-foreground hover:text-foreground">
                <XIcon size={11} />
              </button>
            )}
          </span>
        ))}
        {!disabled && value.length < max ? (
          <input
            role="combobox"
            aria-label={label}
            aria-expanded={open && found.length > 0}
            aria-controls={listId}
            aria-activedescendant={found[active] ? `${listId}-${found[active]}` : undefined}
            value={q}
            placeholder={value.length ? "" : "Search countries"}
            onChange={(e) => {
              setQ(e.target.value);
              setOpen(true);
              setActive(0);
            }}
            onFocus={() => setOpen(true)}
            onBlur={() => setTimeout(() => setOpen(false), 120)}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setActive(Math.min(active + 1, found.length - 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setActive(Math.max(active - 1, 0));
              } else if (e.key === "Enter" || e.key === ",") {
                if (e.metaKey || e.ctrlKey) return;
                e.preventDefault();
                if (found[active]) add(found[active]);
                else if (!q.trim()) onEnterEmpty?.();
              } else if (e.key === "Backspace" && !q && value.length) {
                onChange(value.slice(0, -1));
              }
            }}
            className="min-w-[7rem] flex-1 bg-transparent px-1 text-[12.5px] outline-none placeholder:text-muted-foreground/80"
          />
        ) : null}
      </div>
      {open && found.length ? (
        <ul id={listId} role="listbox" className="absolute z-30 mt-1 max-h-64 w-full overflow-y-auto rounded-lg border border-border bg-popover p-1 shadow-lg">
          {found.map((c, i) => (
            <li
              key={c}
              id={`${listId}-${c}`}
              role="option"
              aria-selected={i === active}
              onMouseDown={(e) => {
                e.preventDefault();
                add(c);
              }}
              className={`flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-[13px] ${i === active ? "bg-accent" : ""}`}
            >
              <Flag country={c} size={14} />
              {countryName(c)}
              <span className="ml-auto font-mono text-[11px] text-muted-foreground">{c}</span>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

export const METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/** Methods as toggle chips. */
export function MethodChips({ value, onChange, single = false }: { value: string[]; onChange: (v: string[]) => void; single?: boolean }) {
  return (
    <div role="group" aria-label="Methods" className="flex min-w-[12rem] flex-1 flex-wrap gap-1">
      {METHODS.map((m) => {
        const on = value.includes(m);
        return (
          <button
            key={m}
            type="button"
            aria-pressed={on}
            onClick={() => onChange(single ? (on ? [] : [m]) : on ? value.filter((x) => x !== m) : [...value, m])}
            className={`rounded-md border px-2 py-1 font-mono text-[11.5px] transition-colors ${
              on ? "border-primary bg-primary/12 text-foreground" : "border-border text-muted-foreground hover:text-foreground"
            }`}
          >
            {m}
          </button>
        );
      })}
    </div>
  );
}

/** A field picker, grouped (Request, Headers, Client, Body). */
export function FieldPicker<T extends string>({
  value,
  groups,
  onChange,
  label,
  disabled = false,
}: {
  value: T;
  groups: { label: string; fields: { value: T; label: string }[] }[];
  onChange: (v: T) => void;
  label: string;
  disabled?: boolean;
}) {
  // The same control as the operator next to it, so a condition row reads as one.
  return (
    <Select aria-label={label} className="w-[140px]" value={value} disabled={disabled} onChange={(e) => onChange(e.target.value as T)}>
      {groups.flatMap((g) => g.fields).map((f) => (
        <option key={f.value} value={f.value}>
          {f.label}
        </option>
      ))}
    </Select>
  );
}

// ── Rows and the dialog ──────────────────────────────────────────────────────

/** One rule in a list: its name and what it matches, then its action. */
export function RuleRow({
  order,
  name,
  summary,
  action,
  dim = false,
  meta,
  controls,
}: {
  /** Its place in the order rules are checked in; null keeps the column empty. */
  order?: number | null;
  name?: ReactNode;
  summary: ReactNode;
  action: { label: string; tone: Tone };
  dim?: boolean;
  /** Small text before the action, like matches in 24h. */
  meta?: ReactNode;
  /** Switches, reorder buttons and the row menu. */
  controls?: ReactNode;
}) {
  return (
    <li className="flex items-center gap-3 px-4 py-2.5 text-[13px]">
      {order !== undefined ? (
        <span className="w-4 shrink-0 self-start pt-px text-right text-[12px] tabular-nums text-muted-foreground" title={order == null ? undefined : "Checked in this order"}>
          {order ?? ""}
        </span>
      ) : null}
      {/* The controls stay at the end of the first line when the rest wraps. */}
      <span className="flex min-w-0 flex-1 flex-wrap items-center gap-x-3 gap-y-1.5">
        <span className={`min-w-[12rem] flex-1 ${dim ? "opacity-60" : ""}`}>
          {name ? <span className="block font-medium">{name}</span> : null}
          <span className={`block break-words ${name ? "mt-0.5 text-[12px] text-muted-foreground" : ""}`}>{summary}</span>
        </span>
        {meta ? <span className="text-[12px] tabular-nums text-muted-foreground">{meta}</span> : null}
        <span className={dim ? "opacity-60" : ""}>
          <Badge tone={action.tone}>{action.label}</Badge>
        </span>
      </span>
      {controls ? <span className="flex shrink-0 items-center gap-1">{controls}</span> : null}
    </li>
  );
}

/**
 * The add/edit dialog: name, "When incoming requests match…", one line in
 * words, then the action. Cmd/Ctrl+Enter saves; Esc closes; full screen on
 * phones.
 */
export function RuleDialog<T extends string>({
  open,
  onClose,
  title,
  name,
  autoName,
  onName,
  nameLabel = "Rule name",
  conditions,
  sentence,
  actions,
  action,
  onAction,
  actionFields,
  submitLabel,
  busy,
  blocked,
  error,
  onSubmit,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  /** What the user typed ("" until they do). */
  name: string;
  /** Shown, and saved, until they type one. */
  autoName: string;
  onName: (v: string) => void;
  nameLabel?: string;
  conditions: ReactNode;
  /** What it matches, in words ("If path starts with /wp-admin"); null while incomplete. */
  sentence: string | null;
  actions: ActionOption<T>[];
  action: T;
  onAction: (a: T) => void;
  /** Settings of the chosen action, under it. */
  actionFields?: ReactNode;
  submitLabel: string;
  busy: boolean;
  /** Why it can't be saved yet, in a few words. */
  blocked: string | null;
  error?: string | null;
  onSubmit: (e: FormEvent) => void;
}) {
  const act = actions.find((a) => a.value === action) ?? actions[0];
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={title}
      width="max-w-2xl"
      fullOnPhone
      onSubmit={(e) => {
        if (!blocked) onSubmit(e);
      }}
      footer={
        <>
          {blocked ? <span className="mr-auto text-[12px] text-muted-foreground">{blocked}</span> : null}
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={busy || !!blocked}>
            {submitLabel}
          </Button>
        </>
      }
    >
      <div
        className="space-y-6"
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            (e.currentTarget.closest("form") as HTMLFormElement | null)?.requestSubmit();
          }
        }}
      >
        <Field label={nameLabel}>
          {(p) => <Input {...p} value={name || autoName} maxLength={80} onChange={(e) => onName(e.target.value)} />}
        </Field>
        <fieldset className="space-y-3">
          <legend className="mb-3 text-[13px] font-medium">When incoming requests match…</legend>
          {conditions}
        </fieldset>
        <p aria-live="polite" className="text-[12.5px] text-muted-foreground">
          {sentence ?? "If …"}
          <Arrow label="then" />
          {act.label}
        </p>
        <fieldset className="space-y-2">
          <legend className="mb-3 text-[13px] font-medium">Then take action</legend>
          <Segmented label="Action" value={action} onChange={onAction} options={actions.map((a) => ({ value: a.value, label: a.label }))} />
          <p className="text-[12px] text-muted-foreground">{act.hint}</p>
          {actionFields}
          {error ? <p className="text-[12px] text-destructive">{error}</p> : null}
        </fieldset>
      </div>
    </Dialog>
  );
}
