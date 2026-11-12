import { CaretRightIcon } from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { AppLink } from "~/components/ui/AppLink";
import { Dot, type Tone } from "~/components/ui/Badge";
import { timeAgo } from "~/lib/pyo/format";
import type { PyoState } from "~/lib/pyo/store";
import type { ServiceStatus, Deployment } from "~/lib/pyo/types";
import type { Issue } from "./widgets";
import { currentSlug } from "~/lib/pyo/paths";

// Vercel-style lists shared by the overview and project pages: one bordered
// card, divided rows, a status dot plus a word.

export function SectionTitle({
  id,
  title,
  href,
}: {
  id: string;
  title: string;
  href?: string;
}) {
  return (
    <div className="mb-3 flex items-center justify-between">
      <h2 id={id} className="text-[14px] font-medium">
        {title}
      </h2>
      {href ? (
        <AppLink
          href={href}
          className="text-[12.5px] text-muted-foreground hover:text-foreground"
        >
          View all
        </AppLink>
      ) : null}
    </div>
  );
}

/** Vercel's status words for a deployment. Only a build is "Building":
 * an image deploy goes straight to deploying. */
export function deployStatus(status: Deployment["status"]): {
  label: string;
  tone: Tone;
} {
  if (status === "succeeded") return { label: "Ready", tone: "success" };
  if (status === "failed") return { label: "Error", tone: "danger" };
  if (status === "cancelled") return { label: "Canceled", tone: "neutral" };
  const label =
    status === "queued"
      ? "Queued"
      : status === "building" || status === "pushing"
        ? "Building"
        : status === "rolling_back"
          ? "Rolling back"
          : "Deploying";
  return { label, tone: "progress" };
}

export const APP_STATUS: Record<ServiceStatus, { label: string; tone: Tone }> = {
  running: { label: "Running", tone: "success" },
  deploying: { label: "Deploying", tone: "progress" },
  degraded: { label: "Degraded", tone: "warning" },
  failed: { label: "Down", tone: "danger" },
  stopped: { label: "Stopped", tone: "neutral" },
  restoring: { label: "Restoring", tone: "progress" },
};

export function ListCard({
  children,
  empty,
}: {
  children: ReactNode;
  empty?: string;
}) {
  if (empty)
    return (
      <p className="rounded-xl border border-dashed border-border px-4 py-8 text-center text-[13px] text-muted-foreground">
        {empty}
      </p>
    );
  return (
    <ul className="list-rows overflow-hidden rounded-[14px] border border-card-border bg-card">
      {children}
    </ul>
  );
}

export function DeploymentList({
  deployments,
  s,
  empty = "No deployments yet",
}: {
  deployments: Deployment[];
  s: PyoState;
  empty?: string;
}) {
  if (!deployments.length) return <ListCard empty={empty}>{null}</ListCard>;
  return (
    <ListCard>
      {deployments.map((d) => {
        const app = s.services.find((a) => a.id === d.serviceId);
        const status = deployStatus(d.status);
        const ref = d.source.type === "git" ? d.source.branch : d.revisionLabel;
        return (
          <li key={d.id}>
            <Link
              to="/$team/deployments/$deploymentId"
              params={{ team: currentSlug(), deploymentId: d.id }}
              className="flex items-start gap-3 px-4 py-3 transition-colors hover:bg-accent/40"
            >
              <Dot tone={status.tone} className="mt-[7px] h-2 w-2" />
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[13px] font-medium">
                  {app?.name ?? "Deleted service"}
                </span>
                <span className="block truncate text-[12px] text-muted-foreground">
                  <span
                    className={
                      status.tone === "danger" ? "text-destructive" : ""
                    }
                  >
                    {status.label}
                  </span>{" "}
                  · {ref}
                </span>
              </span>
              <span className="shrink-0 pt-px text-[12px] text-muted-foreground">
                {timeAgo(d.createdAt)}
              </span>
            </Link>
          </li>
        );
      })}
    </ListCard>
  );
}

/** Problems with a dot, title and detail; the first three, with "Show more". */
export function IssueRows({ issues }: { issues: Issue[] }) {
  const [all, setAll] = useState(false);
  return (
    <ListCard>
      {(all ? issues : issues.slice(0, 3)).map((i) => (
        <li key={i.id}>
          <AppLink
            href={i.href}
            className="group flex items-center gap-3 px-4 py-3 transition-colors hover:bg-accent/40"
          >
            <Dot
              tone={i.severity === "critical" ? "danger" : "warning"}
              className="h-2 w-2"
            />
            <span className="sr-only">
              {i.severity === "critical" ? "Critical:" : "Warning:"}
            </span>
            <span className="min-w-0 truncate text-[13px] font-medium sm:shrink-0">
              {i.title}
            </span>
            <span className="hidden min-w-0 truncate text-[13px] text-muted-foreground sm:inline">
              {i.detail}
            </span>
            <CaretRightIcon
              size={13}
              className="ml-auto shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100"
            />
          </AppLink>
        </li>
      ))}
      {issues.length > 3 ? (
        <li>
          <button
            type="button"
            onClick={() => setAll((v) => !v)}
            aria-expanded={all}
            className="w-full px-4 py-2.5 text-left text-[12.5px] text-muted-foreground transition-colors hover:bg-accent/40 hover:text-foreground"
          >
            {all ? "Show less" : `Show ${issues.length - 3} more`}
          </button>
        </li>
      ) : null}
    </ListCard>
  );
}
