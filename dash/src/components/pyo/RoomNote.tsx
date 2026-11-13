import { WarningIcon } from "@phosphor-icons/react";

/** More replicas than the servers have room for: they're accepted, and wait. */
export function RoomNote({ text }: { text: string | null | undefined }) {
  if (!text) return null;
  return (
    <p
      role="status"
      className="flex gap-2 rounded-lg border border-warning/30 bg-warning/[0.07] px-3 py-2.5 text-[12.5px] leading-5 text-foreground/90"
    >
      <WarningIcon size={15} className="mt-0.5 shrink-0 text-warning" />
      <span>{text}</span>
    </p>
  );
}
