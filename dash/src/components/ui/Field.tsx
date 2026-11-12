import { Tabs } from "./Tabs";
import {
  CircleNotchIcon,
  EyeIcon,
  EyeSlashIcon,
  MinusIcon,
  PlusIcon,
} from "@phosphor-icons/react";
import {
  forwardRef,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type InputHTMLAttributes,
  type ReactNode,
  type TextareaHTMLAttributes,
} from "react";

// Form controls. <Field> wires label, hint and error to the control through
// ids, so every input gets an accessible name and description.

export {
  fieldGroupClass,
  fieldState,
  groupInputClass,
  inputClass,
  inputSmClass,
  optionClass,
  textareaClass,
} from "./fieldStyles";
import { fieldState, inputClass, textareaClass } from "./fieldStyles";

type FieldRender = (p: {
  id: string;
  "aria-invalid"?: boolean;
  "aria-describedby"?: string;
}) => ReactNode;

export function Field({
  label,
  hint,
  error,
  required,
  optional,
  children,
  className = "",
}: {
  label: ReactNode;
  hint?: ReactNode;
  error?: string;
  required?: boolean;
  optional?: boolean;
  children: FieldRender;
  className?: string;
}) {
  const id = useId();
  const describedBy =
    [hint ? `${id}-hint` : "", error ? `${id}-err` : ""]
      .filter(Boolean)
      .join(" ") || undefined;
  return (
    <div className={`min-w-0 ${className}`}>
      <label
        htmlFor={id}
        className="mb-1.5 flex items-center gap-1 text-[12px] font-medium text-muted-foreground"
      >
        {label}
        {required ? (
          <span className="text-destructive" aria-hidden>
            *
          </span>
        ) : null}
        {optional ? (
          <span className="font-normal text-muted-foreground">(optional)</span>
        ) : null}
      </label>
      {children({
        id,
        "aria-invalid": error ? true : undefined,
        "aria-describedby": describedBy,
      })}
      {error ? (
        <p id={`${id}-err`} className="mt-1 text-[11.5px] text-destructive">
          {error}
        </p>
      ) : hint ? (
        <p
          id={`${id}-hint`}
          className="mt-1 text-[11.5px] leading-4 text-muted-foreground"
        >
          {hint}
        </p>
      ) : null}
    </div>
  );
}

export const Input = forwardRef<
  HTMLInputElement,
  InputHTMLAttributes<HTMLInputElement> & { mono?: boolean }
>(function Input({ className = "", mono, ...props }, ref) {
  return (
    <input
      ref={ref}
      className={`${inputClass} ${mono ? "font-mono text-[12px]" : ""} ${className}`}
      {...props}
    />
  );
});

export function Textarea({
  className = "",
  ...props
}: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <textarea
      className={`${textareaClass} ${className}`}
      {...props}
    />
  );
}

export { Select } from "./Select";

/**
 * Masked value with an eye toggle inside. Pass `shown` and `onToggle` to
 * control it (e.g. fetch a stored value first); `busy` spins the eye,
 * `toggleDisabled` disables it with that reason as its title.
 */
export const SecretInput = forwardRef<
  HTMLInputElement,
  InputHTMLAttributes<HTMLInputElement> & {
    shown?: boolean;
    onToggle?: () => void;
    busy?: boolean;
    toggleDisabled?: string;
  }
>(function SecretInput(
  { className = "", shown: shownProp, onToggle, busy, toggleDisabled, ...props },
  ref,
) {
  const [own, setOwn] = useState(false);
  const shown = shownProp ?? own;
  return (
    <div className="relative">
      <input
        ref={ref}
        type={shown ? "text" : "password"}
        autoComplete="new-password"
        className={`${inputClass} pr-9 font-mono text-[12px] ${className}`}
        {...props}
      />
      <button
        type="button"
        onClick={() => {
          if (busy || toggleDisabled) return;
          if (onToggle) onToggle();
          else setOwn((v) => !v);
        }}
        aria-disabled={toggleDisabled ? true : undefined}
        title={toggleDisabled}
        aria-label={shown ? "Hide value" : "Show value"}
        aria-pressed={shown}
        aria-busy={busy || undefined}
        className="absolute inset-y-0 right-0 flex w-9 items-center justify-center rounded-r-full text-muted-foreground outline-none hover:text-foreground focus-visible:text-foreground focus-visible:ring-2 focus-visible:ring-ring aria-disabled:cursor-not-allowed aria-disabled:opacity-50 aria-disabled:hover:text-muted-foreground"
      >
        {busy ? (
          <CircleNotchIcon
            size={14}
            className="animate-spin motion-reduce:animate-none"
          />
        ) : shown ? (
          <EyeSlashIcon size={14} />
        ) : (
          <EyeIcon size={14} />
        )}
      </button>
    </div>
  );
});

/**
 * A switch beside a name and description sits centered against them; once
 * the description wraps past two lines, it sits on the name's first line
 * instead. Measures the description element given the ref.
 */
export function useWrapsPastTwoLines<T extends HTMLElement>(dep: unknown) {
  const ref = useRef<T>(null);
  const [long, setLong] = useState(false);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return setLong(false);
    const measure = () => {
      const lh = parseFloat(getComputedStyle(el).lineHeight) || 18;
      setLong(el.getBoundingClientRect().height > lh * 2 + 1);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [dep]);
  return [ref, long] as const;
}

/** The row's alignment for useWrapsPastTwoLines: the switch is a direct child. */
export const switchRowAlign = (long: boolean) =>
  long ? "items-start [&>[role=switch]]:-mt-1" : "items-center";

export function Switch({
  checked,
  onChange,
  label,
  description,
  disabled,
  id,
  ariaLabel,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label?: ReactNode;
  description?: ReactNode;
  disabled?: boolean;
  id?: string;
  /** Its name when it has no label. */
  ariaLabel?: string;
}) {
  const auto = useId();
  const sid = id ?? auto;
  const [descRef, long] = useWrapsPastTwoLines<HTMLSpanElement>(description);
  const toggle = (
    <button
      id={sid}
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label ? undefined : ariaLabel}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`group relative inline-flex h-7 w-[46px] shrink-0 items-center rounded-full p-0.5 shadow-[inset_0_0_0_1px_rgb(0_0_0/0.04)] transition-colors duration-200 ease-out focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-canvas disabled:cursor-not-allowed disabled:opacity-50 motion-reduce:transition-none dark:shadow-[inset_0_0_0_1px_rgb(255_255_255/0.04)] ${
        checked ? "bg-switch-on" : "bg-switch-off"
      }`}
    >
      {/* On/off labels, as iOS draws them: a bar on the green, a ring on the grey. */}
      <span
        aria-hidden
        className={`absolute left-[11px] top-1/2 h-2.5 w-[1.5px] -translate-y-1/2 rounded-full bg-white transition-opacity duration-200 ${checked ? "opacity-100" : "opacity-0"}`}
      />
      <span
        aria-hidden
        className={`absolute right-[7px] top-1/2 size-2.5 -translate-y-1/2 rounded-full border-[1.5px] border-muted-foreground/70 transition-opacity duration-200 ${checked ? "opacity-0" : "opacity-100"}`}
      />
      <span
        className={`relative inline-block size-6 rounded-full shadow-[0_2px_4px_rgb(0_0_0/0.2),0_0_0_0.5px_rgb(0_0_0/0.06)] transition-[translate,background-color] duration-200 ease-out motion-reduce:transition-none ${checked ? "translate-x-[18px] bg-white" : "translate-x-0 bg-switch-knob"}`}
      />
    </button>
  );
  if (!label) return toggle;
  return (
    <div className={`flex justify-between gap-4 ${switchRowAlign(long)}`}>
      <label htmlFor={sid} className="min-w-0 cursor-pointer">
        <span className="block text-[13px] font-medium text-foreground">
          {label}
        </span>
        {description ? (
          <span
            ref={descRef}
            className="mt-0.5 block text-[12px] text-muted-foreground"
          >
            {description}
          </span>
        ) : null}
      </label>
      {toggle}
    </div>
  );
}

/** Segmented single choice: Kumo-style tabs with radio semantics. "xs" is the small size, for toolbars; "md" for a primary choice (larger, evenly sized options). */
export function Segmented<T extends string>({
  value,
  onChange,
  options,
  label,
  size = "sm",
  className = "",
}: {
  value: T;
  onChange: (v: T) => void;
  options: { value: T; label: ReactNode; disabled?: boolean; ariaLabel?: string; title?: string }[];
  label: string;
  size?: "xs" | "sm" | "md";
  className?: string;
}) {
  return (
    <Tabs
      items={options}
      value={value}
      onChange={onChange}
      label={label}
      semantics="radio"
      pill
      size={size === "xs" ? "sm" : size === "md" ? "md" : "base"}
      className={className}
    />
  );
}

const ROLL_MS = 220;
const ROLL_EASE = "cubic-bezier(0.22, 1, 0.36, 1)";
type Roll = { id: number; from: string; to: string; dir: 1 | -1 };

/**
 * The digits of a changed number rolling over: only the digits that differ
 * move, the new one in from below on the way up (from above on the way down)
 * as the old one leaves. A digit gained or lost also grows or shrinks its
 * width. Sits over the input, which hides its text meanwhile.
 */
function RollingDigits({
  from,
  to,
  dir,
  className,
  onDone,
}: Omit<Roll, "id"> & { className: string; onDone: () => void }) {
  const self = useRef<HTMLSpanElement>(null);
  // After the digits' own effects, so their animations exist.
  useLayoutEffect(() => {
    const all = self.current?.getAnimations({ subtree: true }) ?? [];
    Promise.all(all.map((a) => a.finished)).then(onDone, () => {});
  }, []);
  const n = Math.max(from.length, to.length);
  const a = from.padStart(n, " ");
  const b = to.padStart(n, " ");
  return (
    <span ref={self} aria-hidden className={`pointer-events-none absolute inset-0 flex items-center justify-center whitespace-pre ${className}`}>
      {[...b].map((ch, i) =>
        a[i] === ch ? <span key={i}>{ch}</span> : <RollingDigit key={i} out={a[i]} next={ch} dir={dir} />,
      )}
    </span>
  );
}

function RollingDigit({ out, next, dir }: { out: string; next: string; dir: 1 | -1 }) {
  const box = useRef<HTMLSpanElement>(null);
  const enter = useRef<HTMLSpanElement>(null);
  const leave = useRef<HTMLSpanElement>(null);
  useLayoutEffect(() => {
    const timing = { duration: ROLL_MS, easing: ROLL_EASE, fill: "forwards" as const };
    const off = (sign: number) => `translateY(${sign * 0.7}em)`;
    if (next !== " ") enter.current?.animate([{ transform: off(dir), opacity: 0 }, { transform: "none", opacity: 1 }], timing);
    leave.current?.animate([{ transform: "none", opacity: 1 }, { transform: off(-dir), opacity: 0 }], timing);
    if (out === " ") box.current?.animate([{ width: "0ch" }, { width: "1ch" }], timing);
    if (next === " ") box.current?.animate([{ width: "1ch" }, { width: "0ch" }], timing);
  }, [out, next, dir]);
  return (
    <span ref={box} className="relative inline-block text-center">
      {/* In flow, it sizes the column; a lost digit keeps an invisible copy. */}
      <span ref={enter} className={`inline-block ${next === " " ? "invisible" : ""}`}>
        {next === " " ? out : next}
      </span>
      {out === " " ? null : (
        <span ref={leave} className="absolute inset-0">
          {out}
        </span>
      )}
    </span>
  );
}

/**
 * A number in a pill: − and + at the ends, the value in the middle (typed
 * values apply on blur or Enter, clamped to the range). Arrow keys step.
 * A changed value rolls its digits (instant with reduced motion); typing
 * doesn't. For short ranges: replicas, instances, counts.
 */
export function Stepper({
  value,
  onChange,
  min = 0,
  max = Number.MAX_SAFE_INTEGER,
  step = 1,
  disabled,
  size = "md",
  suffix,
  className = "",
  id,
  ...aria
}: {
  value: number;
  onChange: (n: number) => void;
  min?: number;
  max?: number;
  step?: number;
  disabled?: boolean;
  size?: "sm" | "md";
  /** Shown after the value, e.g. "×" or "%". */
  suffix?: ReactNode;
  className?: string;
  id?: string;
  "aria-label"?: string;
  "aria-invalid"?: boolean;
  "aria-describedby"?: string;
}) {
  const [draft, setDraft] = useState(String(value));
  // The text on screen, so a new value can roll from it.
  const shown = useRef(draft);
  const [roll, setRoll] = useState<Roll | null>(null);
  const rolls = useRef(0);
  const write = (text: string) => {
    shown.current = text;
    setDraft(text);
  };
  const reveal = (to: string) => {
    const from = shown.current;
    if (from === to) return;
    write(to);
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return setRoll(null);
    const a = Number(from);
    setRoll({ id: ++rolls.current, from, to, dir: Number.isFinite(a) && a > Number(to) ? -1 : 1 });
  };
  useLayoutEffect(() => reveal(String(value)), [value]);
  const clamp = (n: number) => Math.max(min, Math.min(max, n));
  const set = (n: number) => {
    const v = clamp(n);
    reveal(String(v));
    if (v !== value) onChange(v);
  };
  const commit = () => {
    const n = Number(draft);
    if (draft.trim() === "" || !Number.isFinite(n)) reveal(String(value));
    else set(n);
  };
  const sm = size === "sm";
  const typing = draft !== String(value);
  const digits = `font-medium tabular-nums ${sm ? "text-[12.5px]" : "text-[13px]"}`;
  const btn = `flex h-full shrink-0 items-center justify-center rounded-full text-muted-foreground transition-[color,opacity,transform] duration-150 ease-out hover:text-foreground focus-visible:text-foreground focus-visible:outline-none active:scale-[0.94] disabled:pointer-events-none disabled:opacity-35 motion-reduce:transition-none motion-reduce:active:scale-100 ${sm ? "w-7" : "w-8"}`;
  return (
    <div
      role="group"
      aria-label={aria["aria-label"]}
      aria-disabled={disabled || undefined}
      className={`flex min-w-0 items-center rounded-full px-0.5 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20 aria-disabled:cursor-not-allowed aria-disabled:border-border aria-disabled:bg-muted aria-disabled:text-muted-foreground aria-disabled:shadow-none ${fieldState} ${aria["aria-invalid"] ? "border-destructive ring-[3px] ring-destructive/20" : ""} ${sm ? "h-8" : "h-9"} ${/(^|\s)w-/.test(className) ? "" : "w-full"} ${className}`}
    >
      <button
        type="button"
        tabIndex={-1}
        aria-label="Decrease"
        disabled={disabled || value <= min}
        onClick={() => set(value - step)}
        className={btn}
      >
        <MinusIcon size={sm ? 12 : 13} weight="bold" />
      </button>
      <span className="flex min-w-0 flex-1 items-baseline justify-center">
        <span className="relative inline-flex min-w-0">
          <input
            id={id}
            type="text"
            inputMode="numeric"
            role="spinbutton"
            aria-valuemin={min}
            aria-valuemax={max === Number.MAX_SAFE_INTEGER ? undefined : max}
            aria-valuenow={value}
            aria-label={aria["aria-label"]}
            aria-invalid={aria["aria-invalid"]}
            aria-describedby={aria["aria-describedby"]}
            disabled={disabled}
            value={draft}
            style={{ width: `${Math.max(1, draft.length) + 0.25}ch` }}
            onChange={(e) => write(e.target.value.replace(/[^\d.-]/g, ""))}
            onBlur={commit}
            onKeyDown={(e) => {
              if (e.key === "Enter") commit();
              else if (e.key === "ArrowUp") set(value + step);
              else if (e.key === "ArrowDown") set(value - step);
              else if (e.key === "Home") set(min);
              else if (e.key === "End" && max < Number.MAX_SAFE_INTEGER) set(max);
              else return;
              e.preventDefault();
            }}
            className={`min-w-0 bg-transparent text-center outline-none disabled:cursor-not-allowed disabled:text-muted-foreground ${digits} ${roll ? "!text-transparent" : "text-foreground"} ${typing ? "" : "transition-[width] duration-200 ease-out motion-reduce:transition-none"}`}
          />
          {roll ? (
            <RollingDigits
              key={roll.id}
              from={roll.from}
              to={roll.to}
              dir={roll.dir}
              onDone={() => setRoll((r) => (r?.id === roll.id ? null : r))}
              className={`${digits} ${disabled ? "text-muted-foreground" : "text-foreground"}`}
            />
          ) : null}
        </span>
        {suffix ? (
          <span aria-hidden className={`ml-0.5 font-medium text-muted-foreground ${sm ? "text-[12.5px]" : "text-[13px]"}`}>
            {suffix}
          </span>
        ) : null}
      </span>
      <button
        type="button"
        tabIndex={-1}
        aria-label="Increase"
        disabled={disabled || value >= max}
        onClick={() => set(value + step)}
        className={btn}
      >
        <PlusIcon size={sm ? 12 : 13} weight="bold" />
      </button>
    </div>
  );
}

export function Checkbox({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label className="inline-flex cursor-pointer items-center gap-2 text-[13px] text-foreground">
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
      />
      {label}
    </label>
  );
}
