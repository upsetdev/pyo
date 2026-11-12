import { CircleNotchIcon, WarningCircleIcon } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { CopyButton } from "~/components/ui/Table";

/**
 * An install command that follows the form above it. Until the inputs are
 * valid it says what's missing; while a new one is on its way the old one
 * dims and can't be copied, so what you copy always matches the form.
 */
export function LiveCommand({
  command,
  stale = false,
  error,
  waiting,
  children,
}: {
  command?: string;
  stale?: boolean;
  error?: string;
  /** Shown instead of a command when the inputs aren't valid yet. */
  waiting?: string;
  /** Notes under the command. */
  children?: ReactNode;
}) {
  if (error)
    return (
      <p className="flex items-start gap-2 rounded-lg border border-destructive/30 bg-destructive/[0.06] px-3 py-2.5 text-[12.5px] text-destructive">
        <WarningCircleIcon size={15} className="mt-px shrink-0" />
        {error}
      </p>
    );
  if (!command)
    return (
      <div className="flex min-h-[60px] items-center gap-2 rounded-lg border border-dashed border-border px-3 py-2.5 text-[12.5px] text-muted-foreground">
        {waiting ? (
          waiting
        ) : (
          <>
            <CircleNotchIcon size={14} className="animate-spin" /> Preparing the
            command…
          </>
        )}
      </div>
    );
  return (
    <div className="space-y-2">
      <div
        aria-busy={stale}
        className={`flex items-start gap-2 rounded-lg border border-border bg-muted/40 p-3 transition-opacity ${stale ? "pointer-events-none opacity-45" : ""}`}
      >
        <code className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[11.5px] leading-[18px] text-foreground">
          {command}
        </code>
        {stale ? (
          <CircleNotchIcon
            size={14}
            className="m-[5px] shrink-0 animate-spin text-muted-foreground"
          />
        ) : (
          <CopyButton value={command} label="Copy command" />
        )}
      </div>
      {children}
    </div>
  );
}
