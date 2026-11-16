import type { Icon } from "@phosphor-icons/react";
import { useId, type ReactNode } from "react";
import { optionClass } from "./fieldStyles";

/** Square pick-one tiles in a row: the first choice of a flow. */
export function OptionTiles({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div role="radiogroup" aria-label={label} className="grid max-w-[26rem] grid-cols-2 gap-3">
      {children}
    </div>
  );
}

/** One square tile: an icon (or a brand mark), a name and a short line under it. */
export function OptionTile({
  icon: IconCmp,
  mark,
  label,
  detail,
  selected,
  onSelect,
}: {
  icon?: Icon;
  /** A logo in place of the icon, in its own colors. */
  mark?: ReactNode;
  label: string;
  detail: ReactNode;
  selected: boolean;
  onSelect: () => void;
}) {
  const id = useId();
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-describedby={`${id}-d`}
      onClick={onSelect}
      className={`${optionClass({ selected })} flex aspect-square w-full min-w-0 flex-col items-center justify-center gap-3 p-4`}
    >
      <span
        aria-hidden
        className={`flex h-11 w-11 items-center justify-center rounded-xl border ${selected ? "border-primary/30 bg-primary/10 text-primary" : "border-border bg-muted text-muted-foreground"}`}
      >
        {mark ?? (IconCmp ? <IconCmp size={22} /> : null)}
      </span>
      <span className="min-w-0 text-center">
        <span className="block text-[13.5px] font-medium text-foreground">{label}</span>
        <span id={`${id}-d`} className="mt-1 block text-[12px] leading-[1.1rem] text-muted-foreground">
          {detail}
        </span>
      </span>
    </button>
  );
}
