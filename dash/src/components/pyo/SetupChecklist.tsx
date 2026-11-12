import {
  CheckCircleIcon,
  CircleIcon,
  CircleNotchIcon,
} from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { Panel } from "~/components/ui/Panel";

export type SetupCheck = {
  label: string;
  done: boolean;
  detail?: ReactNode;
  /** Reported, and not there: nothing to wait for. */
  missing?: boolean;
};

/**
 * What a new cluster, server or builder has reported so far, ticked off as
 * it comes in. The first step still to come spins.
 */
export function SetupChecklist({
  checks,
  title,
  doneTitle = "Connected",
}: {
  checks: SetupCheck[];
  title: string;
  doneTitle?: string;
}) {
  const first = checks.findIndex((c) => !c.done && !c.missing);
  const allDone = checks.every((c) => c.done || c.missing);
  return (
    <Panel title={allDone ? doneTitle : title}>
      <ul className="list-rows">
        {checks.map((ch, i) => (
          <li key={ch.label} className="flex items-start gap-3 px-4 py-3">
            {ch.done ? (
              <CheckCircleIcon
                size={17}
                weight="fill"
                className="mt-px shrink-0 text-success"
              />
            ) : i === first ? (
              <CircleNotchIcon
                size={17}
                className="mt-px shrink-0 animate-spin text-muted-foreground"
              />
            ) : (
              <CircleIcon
                size={17}
                className="mt-px shrink-0 text-muted-foreground/60"
              />
            )}
            <div className="min-w-0">
              <p className="text-[13px] font-medium">{ch.label}</p>
              {ch.detail ? (
                <p className="text-[12px] text-muted-foreground">{ch.detail}</p>
              ) : null}
            </div>
          </li>
        ))}
      </ul>
    </Panel>
  );
}
