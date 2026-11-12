import { fieldGroupClass } from "./fieldStyles";
import { TagIcon, XIcon } from "@phosphor-icons/react";
import { Button } from "./Button";
import { Dialog } from "./Dialog";
import { useState } from "react";

/** Lowercase letters, digits and dashes, up to 32 (they become label names). */
export const TAG = /^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?$/;

/** A tag as typed, tidied, or null when it can't be one. */
export function cleanTag(raw: string) {
  const t = raw.trim().toLowerCase().replace(/\s+/g, "-");
  return TAG.test(t) ? t : null;
}

/**
 * Tags as chips with a field to add more: Enter, comma or Tab adds what's
 * typed, Backspace on an empty field removes the last. `suggestions` are
 * offered below while typing.
 */
export function TagInput({
  value,
  onChange,
  placeholder = "Add a tag",
  suggestions = [],
  max = 10,
  id,
  "aria-label": ariaLabel,
  clean = cleanTag,
  invalid = "Lowercase letters, digits and dashes, up to 32.",
  disabled = false,
  onEnterEmpty,
  autoFocus,
}: {
  value: string[];
  onChange: (tags: string[]) => void;
  placeholder?: string;
  suggestions?: string[];
  max?: number;
  id?: string;
  "aria-label"?: string;
  /** What's typed, tidied, or null when it can't be a tag. */
  clean?: (raw: string) => string | null;
  /** Said when `clean` refuses what's typed. */
  invalid?: string;
  disabled?: boolean;
  /** Enter with nothing typed (a form can add a row). */
  onEnterEmpty?: () => void;
  autoFocus?: boolean;
}) {
  const [draft, setDraft] = useState("");
  const [bad, setBad] = useState(false);
  const add = (raw: string) => {
    const t = clean(raw);
    if (!raw.trim()) return true;
    if (!t) {
      setBad(true);
      return false;
    }
    if (!value.includes(t) && value.length < max) onChange([...value, t]);
    setDraft("");
    setBad(false);
    return true;
  };
  const q = draft.trim().toLowerCase();
  const offered = q
    ? suggestions.filter((s) => s.includes(q) && !value.includes(s)).slice(0, 5)
    : [];
  return (
    <div className="relative">
      <div
        className={`${fieldGroupClass(!!bad)} h-auto min-h-9 w-full flex-wrap items-center gap-1.5 px-2 py-1.5`}
      >
        {value.map((t) => (
          <span
            key={t}
            className="inline-flex items-center gap-1 rounded-md bg-muted px-1.5 py-0.5 font-mono text-[12px]"
          >
            {t}
            <button
              type="button"
              disabled={disabled}
              aria-label={`Remove ${t}`}
              onClick={() => onChange(value.filter((x) => x !== t))}
              className="rounded text-muted-foreground hover:text-foreground"
            >
              <XIcon size={11} />
            </button>
          </span>
        ))}
        {value.length < max && !disabled ? (
          <input
            id={id}
            autoFocus={autoFocus}
            aria-label={ariaLabel}
            value={draft}
            placeholder={value.length ? "" : placeholder}
            onChange={(e) => {
              setDraft(e.target.value);
              setBad(false);
            }}
            onKeyDown={(e) => {
              if (
                (e.key === "Enter" ||
                  e.key === "," ||
                  (e.key === "Tab" && draft.trim())) &&
                draft.trim()
              ) {
                e.preventDefault();
                add(draft);
              } else if (e.key === "Enter") {
                e.preventDefault();
                if (!e.metaKey && !e.ctrlKey) onEnterEmpty?.();
              } else if (e.key === "Backspace" && !draft && value.length) {
                onChange(value.slice(0, -1));
              }
            }}
            onBlur={() => add(draft)}
            onPaste={(e) => {
              const text = e.clipboardData.getData("text");
              if (!/[\s,]/.test(text.trim())) return;
              e.preventDefault();
              const parts = text.split(/[\s,]+/).filter(Boolean);
              const next = [...value];
              let refused = false;
              for (const part of parts) {
                const t = clean(part);
                if (!t) refused = true;
                else if (!next.includes(t) && next.length < max) next.push(t);
              }
              onChange(next);
              setBad(refused);
            }}
            className="min-w-[6rem] flex-1 bg-transparent px-1 font-mono text-[12.5px] outline-none placeholder:font-sans placeholder:text-muted-foreground/80 focus-visible:outline-none"
          />
        ) : null}
      </div>
      {bad ? (
        <p className="mt-1 text-[12px] text-destructive">{invalid}</p>
      ) : null}
      {offered.length ? (
        <div className="absolute z-20 mt-1 flex flex-wrap gap-1 rounded-lg border border-border bg-popover p-1.5 shadow-lg">
          {offered.map((s) => (
            <button
              key={s}
              type="button"
              onMouseDown={(e) => {
                e.preventDefault();
                add(s);
              }}
              className="rounded-md px-1.5 py-0.5 font-mono text-[12px] hover:bg-accent"
            >
              {s}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

/** Tags shown read-only, as small chips. */
export function Tags({
  tags,
  className = "",
}: {
  tags?: string[];
  className?: string;
}) {
  if (!tags?.length) return null;
  return (
    <span className={`inline-flex flex-wrap items-center gap-1 ${className}`}>
      {tags.map((t) => (
        <span
          key={t}
          className="rounded-md bg-muted px-1.5 py-px font-mono text-[11.5px] text-foreground/85"
        >
          {t}
        </span>
      ))}
    </span>
  );
}

/**
 * Tags beside a name, with a small button to edit them in a dialog. Admins
 * see the button; `onSave` resolves truthy when saved.
 */
export function TagsBeside({
  tags,
  name,
  suggestions = [],
  hint,
  onSave,
}: {
  tags?: string[];
  name: string;
  suggestions?: string[];
  hint?: string;
  onSave?: (tags: string[]) => unknown;
}) {
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<string[]>([]);
  return (
    <>
      <Tags tags={tags} />
      {onSave ? (
        <button
          type="button"
          onClick={() => {
            setDraft(tags ?? []);
            setOpen(true);
          }}
          className="inline-flex items-center gap-1 rounded-md border border-dashed border-border px-1.5 py-px text-[11.5px] font-normal text-muted-foreground transition-colors hover:border-foreground/30 hover:text-foreground"
        >
          <TagIcon size={11} /> {tags?.length ? "Edit" : "Add tag"}
        </button>
      ) : null}
      <Dialog
        open={open}
        onClose={() => setOpen(false)}
        title={`Tags for ${name}`}
        description={hint}
        onSubmit={async () => {
          if (await onSave?.(draft)) setOpen(false);
        }}
        footer={
          <>
            <Button variant="ghost" size="sm" onClick={() => setOpen(false)}>
              Cancel
            </Button>
            <Button type="submit" size="sm">
              Save
            </Button>
          </>
        }
      >
        <TagInput
          value={draft}
          onChange={setDraft}
          suggestions={suggestions}
          aria-label="Tags"
        />
      </Dialog>
    </>
  );
}
