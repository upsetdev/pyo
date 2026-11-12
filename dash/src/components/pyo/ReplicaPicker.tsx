import { Stepper } from "~/components/ui/Field";

const PRESETS = [1, 2, 3, 5];

/**
 * How many replicas: a stepper and one-click presets.
 * Shared by the Scale dialog and the deploy checkout.
 */
export function ReplicaPicker({
  value,
  onChange,
  max = 50,
  align = "center",
}: {
  value: number;
  onChange: (n: number) => void;
  max?: number;
  align?: "center" | "start";
}) {
  const clamp = (v: number) => Math.max(1, Math.min(max, Math.round(v) || 1));
  const n = value;
  return (
    <div
      className={`flex flex-wrap items-center gap-x-6 gap-y-3 ${align === "center" ? "flex-col justify-center" : ""}`}
    >
      <div className="flex flex-col items-center gap-1.5">
        <Stepper
          aria-label="Replicas"
          min={1}
          max={max}
          value={n}
          onChange={(v) => onChange(clamp(v))}
          className="w-[132px]"
        />
        <span className="text-[12px] text-muted-foreground">
          {n === 1 ? "replica" : "replicas"}
        </span>
      </div>
      <div className="flex gap-2">
        {PRESETS.filter((p) => p <= max).map((p) => (
          <button
            key={p}
            type="button"
            onClick={() => onChange(p)}
            aria-pressed={n === p}
            className={`flex size-9 shrink-0 items-center justify-center rounded-full border text-[13px] font-medium leading-none tabular-nums transition-[border-color,background-color,color,box-shadow,transform] duration-150 ease-out focus-visible:border-primary focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-primary/20 active:scale-[0.94] motion-reduce:transition-none motion-reduce:active:scale-100 ${
              n === p
                ? "border-primary bg-primary/[0.08] text-primary dark:bg-primary/[0.14]"
                : "border-input bg-card text-muted-foreground hover:border-input-hover hover:bg-muted/60 hover:text-foreground"
            }`}
          >
            {p}
          </button>
        ))}
      </div>
    </div>
  );
}
