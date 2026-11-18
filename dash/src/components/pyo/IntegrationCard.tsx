import type { ReactNode } from "react";
import { Badge, Dot, type Tone } from "~/components/ui/Badge";
import { LogoTile } from "~/components/pyo/IntegrationDialog";

// One integration on Integrations, the same size as the rest: its logo,
// name and what it's for, how it stands, and one action (Set up or Manage).

export type IntegrationCardProps = {
  id: string;
  name: string;
  icon: ReactNode;
  /** What it's for, in one short line. */
  description: string;
  /** Connected, Not set up, Needs attention, or a step under way. */
  status: { tone: Tone; label: string };
  /** One fact beside the action: the tailnet, a count, or what needs a look. */
  detail?: ReactNode;
  /** Set up or Manage (or why there's no button). */
  action: ReactNode;
  /** Just linked to: a ring for a moment. */
  highlight?: boolean;
};

export function IntegrationCard({
  id,
  name,
  icon,
  description,
  status,
  detail,
  action,
  highlight,
}: IntegrationCardProps) {
  return (
    <section
      id={id}
      aria-labelledby={`${id}-name`}
      className={`flex min-w-0 scroll-mt-4 flex-col rounded-2xl border border-frame-border bg-muted p-[5px] shadow-card transition-shadow duration-500 ${highlight ? "ring-2 ring-ring ring-offset-2 ring-offset-canvas" : ""}`}
    >
      <div className="flex flex-1 items-start gap-3 rounded-xl border border-frame-border bg-card px-3 py-3">
        <LogoTile icon={icon} />
        <div className="min-w-0 flex-1">
          <div className="flex items-center justify-between gap-2">
            <h2 id={`${id}-name`} className="truncate text-[14px] font-medium leading-5">
              {name}
            </h2>
            <Badge tone={status.tone}>
              <Dot tone={status.tone} />
              {status.label}
            </Badge>
          </div>
          <p className="mt-0.5 line-clamp-2 text-[12.5px] leading-[18px] text-muted-foreground">
            {description}
          </p>
        </div>
      </div>
      <footer className="flex min-h-10 items-center gap-3 px-2 pb-0.5 pt-1.5">
        <span className="min-w-0 flex-1 truncate text-[12px] text-muted-foreground">{detail}</span>
        {action}
      </footer>
    </section>
  );
}
