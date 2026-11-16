import { Input } from "~/components/ui/Field";
import { Select } from "~/components/ui/Select";
import { LIFETIME_UNITS, MAX_LIFETIME_SECS, type LifetimeUnit } from "~/lib/pyo/sandbox";

// A lifetime typed in: a whole number and a unit. The new sandbox form and
// the team's sandbox limits use it.

export type CustomLifetime = { amount: string; unit: LifetimeUnit };

const unitSecs = (u: LifetimeUnit) => LIFETIME_UNITS.find((x) => x.value === u)!.secs;

/** Its seconds, or null when the amount isn't a whole number above 0. */
export function customSecs(c: CustomLifetime) {
  const n = Number(c.amount);
  if (c.amount.trim() === "" || !Number.isInteger(n) || n < 1) return null;
  return n * unitSecs(c.unit);
}

/** How Pyo reads it, like "3d". */
export const customTtl = (c: CustomLifetime) => `${Number(c.amount)}${c.unit}`;

/** Seconds as the largest unit they divide into. */
export function toCustom(secs: number, units: readonly LifetimeUnit[]): CustomLifetime {
  const unit = units.find((u) => secs % unitSecs(u) === 0) ?? units[units.length - 1];
  return { amount: String(Math.max(1, Math.round(secs / unitSecs(unit)))), unit };
}

/** Why it can't be used, or null. */
export function customError(c: CustomLifetime) {
  const secs = customSecs(c);
  if (secs === null) return "Enter a whole number.";
  if (secs > MAX_LIFETIME_SECS) return "365 days at most.";
  return null;
}

export function CustomLifetimeInput({
  value,
  onChange,
  units,
  id,
  ...aria
}: {
  value: CustomLifetime;
  onChange: (v: CustomLifetime) => void;
  units: readonly LifetimeUnit[];
  id?: string;
  "aria-label"?: string;
  "aria-invalid"?: boolean;
  "aria-describedby"?: string;
}) {
  return (
    <div className="flex gap-2">
      <div className="w-24">
        <Input
          id={id}
          {...aria}
          type="number"
          min={1}
          step={1}
          inputMode="numeric"
          value={value.amount}
          onChange={(e) => onChange({ ...value, amount: e.target.value })}
        />
      </div>
      <div className="w-32">
        <Select
          aria-label="Unit"
          value={value.unit}
          onChange={(e) => onChange({ ...value, unit: e.target.value as LifetimeUnit })}
        >
          {LIFETIME_UNITS.filter((u) => units.includes(u.value)).map((u) => (
            <option key={u.value} value={u.value}>
              {u.many}
            </option>
          ))}
        </Select>
      </div>
    </div>
  );
}
