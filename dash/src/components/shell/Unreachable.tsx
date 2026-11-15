import { CircleNotchIcon } from "@phosphor-icons/react";
import { usePyo } from "~/lib/pyo/store";

/** A slim bar while Pyo can't be reached: what's shown may be out of date. */
export function Unreachable() {
  const s = usePyo();
  if (!s.unreachable) return null;
  return (
    <div
      role="status"
      className="flex items-center justify-center gap-2 border-b border-warning/30 bg-warning/10 px-4 py-1.5 text-[12.5px] font-medium text-warning"
    >
      <CircleNotchIcon size={12} className="animate-spin" aria-hidden />
      Can&apos;t reach Pyo. Trying again…
    </div>
  );
}
