import {
  ArrowClockwiseIcon,
  ArrowCounterClockwiseIcon,
  CheckIcon,
  DotsThreeIcon,
  EyeIcon,
  GitCommitIcon,
  MinusIcon,
  RocketLaunchIcon,
  StopIcon,
} from "@phosphor-icons/react";
import { copyIdItem } from "~/components/pyo/copyId";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Select } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { ListSection } from "~/components/ui/Layered";
import { EmptyState, NoMatches, Panel } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { duration, timeAgo } from "~/lib/pyo/format";
import { isActive, STAGE_LABEL } from "~/lib/pyo/simulation";
import {
  activeDeploymentFor,
  api,
  membersIn,
  usePyo,
  type PyoState,
} from "~/lib/pyo/store";
import { UserAvatar } from "~/components/ui/UserAvatar";
import { toast } from "~/lib/pyo/toast";
import type { Deployment } from "~/lib/pyo/types";
import { getTemplate } from "~/lib/pyo/templates";
import { deployStatus } from "./lists";
import { imageLabel, SourceMark } from "./ProjectCard";
import { currentSlug } from "~/lib/pyo/paths";

export const TRIGGER_LABEL: Record<Deployment["trigger"], string> = {
  manual: "Manual",
  git_push: "Git push",
  rollback: "Rollback",
  redeploy: "Redeploy",
  config_change: "Config change",
  retry: "Retry",
  deploy_hook: "Deploy hook",
};

/**
 * Who started a deployment, with their Gravatar. Deployments keep only the
 * name, so the picture comes from the team member of that name.
 */
export function DeployActor({
  name,
  size = 16,
}: {
  name: string;
  size?: number;
}) {
  const s = usePyo();
  const m =
    membersIn(s).find((x) => x.name === name) ??
    (s.user.name === name ? s.user : undefined);
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5 align-middle">
      <UserAvatar
        name={name}
        email={m?.email}
        gravatar={m?.gravatar}
        size={size}
      />
      <span className="truncate">{name}</span>
    </span>
  );
}

type StatusFilter = "all" | "active" | "succeeded" | "failed" | "cancelled";

/**
 * The version a one-click rollback goes back to: the newest successful
 * deployment of the service older than the live one whose image is still kept.
 */
export function previousVersion(
  s: PyoState,
  serviceId: string,
): Deployment | undefined {
  const app = s.services.find((a) => a.id === serviceId);
  const live = s.deployments.find((d) => d.id === app?.currentDeploymentId);
  if (!live) return undefined;
  return s.deployments
    .filter(
      (d) =>
        d.serviceId === serviceId &&
        d.id !== live.id &&
        d.status === "succeeded" &&
        !d.imageExpired &&
        d.createdAt < live.createdAt,
    )
    .sort((a, b) => b.createdAt - a.createdAt)[0];
}

/** Shared retry/rollback/cancel actions with confirmation. */
export function useDeploymentActions(s: PyoState) {
  const navigate = useNavigate();
  const [rollbackTo, setRollbackTo] = useState<Deployment | null>(null);
  const [cancelling, setCancelling] = useState<Deployment | null>(null);
  const items = (d: Deployment) => {
    const app = s.services.find((a) => a.id === d.serviceId);
    const busy = app ? !!activeDeploymentFor(s, app.id) : true;
    const current = app?.currentDeploymentId === d.id;
    const previous = current && app ? previousVersion(s, app.id) : undefined;
    const why = busy ? "A deploy is running" : undefined;
    return [
      {
        label: "View details",
        icon: <EyeIcon size={14} />,
        href: `/deployments/${d.id}`,
      },
      copyIdItem("Deployment", d.id),
      ...(isActive(d.status)
        ? [
            {
              label: "Cancel deployment",
              icon: <StopIcon size={14} />,
              onSelect: () => setCancelling(d),
            },
          ]
        : []),
      ...(d.status === "failed" || d.status === "cancelled"
        ? [
            {
              label: "Retry",
              icon: <ArrowClockwiseIcon size={14} />,
              disabled: busy || !app,
              hint: why,
              onSelect: () => {
                const id = api.retry(d.id);
                void navigate({
                  to: "/$team/deployments/$deploymentId",
                  params: { team: currentSlug(), deploymentId: id },
                });
              },
            },
          ]
        : []),
      ...(previous
        ? [
            { type: "separator" as const },
            {
              label: "Roll back to previous version",
              icon: <ArrowCounterClockwiseIcon size={14} />,
              disabled: busy,
              hint: why,
              onSelect: () => setRollbackTo(previous),
            },
          ]
        : []),
      ...(d.status === "succeeded" && !current
        ? [
            { type: "separator" as const },
            {
              label: "Roll back to this version",
              icon: <ArrowCounterClockwiseIcon size={14} />,
              disabled: busy || !app || !!d.imageExpired,
              hint: d.imageExpired ? "Image cleaned up" : why,
              onSelect: () => setRollbackTo(d),
            },
          ]
        : []),
    ];
  };
  const dialogs = (
    <>
      <RollbackDialog
        s={s}
        to={rollbackTo}
        onClose={() => setRollbackTo(null)}
        onDone={(id) =>
          void navigate({
            to: "/$team/deployments/$deploymentId",
            params: { team: currentSlug(), deploymentId: id },
          })
        }
      />
      <ConfirmDialog
        open={!!cancelling}
        onClose={() => setCancelling(null)}
        title="Cancel deployment?"
        description="The live version keeps serving."
        confirmLabel="Cancel deployment"
        onConfirm={() => cancelling && api.cancel(cancelling.id)}
      />
    </>
  );
  return { items, dialogs, rollback: setRollbackTo, cancel: setCancelling };
}

/** What a version is, in one line: its commit or image tag, and when. */
function VersionRow({
  label,
  d,
  tone,
}: {
  label: string;
  d: Deployment;
  tone: "now" | "next";
}) {
  return (
    <div className="flex items-center gap-3 px-4 py-3">
      <span
        className={`w-20 shrink-0 text-[12px] font-medium ${tone === "next" ? "text-primary" : "text-muted-foreground"}`}
      >
        {label}
      </span>
      <div className="min-w-0 flex-1">
        <p className="flex items-center gap-1.5 truncate text-[13px]">
          <span className="text-muted-foreground">
            <SourceMark source={d.source} />
          </span>
          <span className="font-mono text-[12.5px]">
            {d.revisionLabel || d.id}
          </span>
          {d.source.type === "git" ? (
            <span className="truncate text-muted-foreground">
              on {d.source.branch}
            </span>
          ) : null}
        </p>
        <p className="truncate text-[12px] text-muted-foreground">
          <span className="font-mono">{d.id}</span> · {TRIGGER_LABEL[d.trigger]}{" "}
          by <DeployActor name={d.actor} size={14} /> ·{" "}
          {timeAgo(d.finishedAt ?? d.createdAt)}
        </p>
      </div>
    </div>
  );
}

/**
 * Confirm a rollback, showing what's live now and what will be, and what does
 * and doesn't change.
 */
export function RollbackDialog({
  s,
  to,
  onClose,
  onDone,
}: {
  s: PyoState;
  to: Deployment | null;
  onClose: () => void;
  onDone?: (deploymentId: string) => void;
}) {
  const app = to ? s.services.find((a) => a.id === to.serviceId) : undefined;
  const live = s.deployments.find((d) => d.id === app?.currentDeploymentId);
  const busy = app ? !!activeDeploymentFor(s, app.id) : true;
  const name = app?.name ?? "The service";
  return (
    <Dialog
      open={!!to}
      onClose={onClose}
      title={`Roll back ${name}?`}
      description="The live version keeps serving."
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button
            size="sm"
            disabled={busy || !to}
            onClick={() => {
              if (!to) return;
              const id = api.rollback(to.id);
              toast({ tone: "info", title: `Rolling back ${name}` });
              onClose();
              onDone?.(id);
            }}
          >
            <ArrowCounterClockwiseIcon size={13} /> Roll back
          </Button>
        </>
      }
    >
      {to ? (
        <div className="space-y-4">
          <div className="list-rows overflow-hidden rounded-xl border border-border">
            {live ? <VersionRow label="Live now" d={live} tone="now" /> : null}
            <VersionRow label="Goes live" d={to} tone="next" />
          </div>
          <ul className="space-y-2 text-[13px]">
            <li className="flex gap-2.5">
              <CheckIcon
                size={14}
                weight="bold"
                className="mt-0.5 shrink-0 text-success"
              />
              <span>The same image runs again. Nothing is rebuilt.</span>
            </li>
            <li className="flex gap-2.5">
              <CheckIcon
                size={14}
                weight="bold"
                className="mt-0.5 shrink-0 text-success"
              />
              <span>
                Variables, ports and resources come back too.
              </span>
            </li>
            <li className="flex gap-2.5">
              <MinusIcon
                size={14}
                weight="bold"
                className="mt-0.5 shrink-0 text-muted-foreground"
              />
              <span className="text-muted-foreground">
                Volumes and databases keep their current data.
              </span>
            </li>
          </ul>
          {busy ? (
            <p className="text-[12.5px] text-warning">
              A deploy is running. Wait or cancel it.
            </p>
          ) : null}
        </div>
      ) : null}
    </Dialog>
  );
}

/**
 * Vercel-style deployment list: search and a status filter, then one row per
 * deployment with what it is, how it went, what it built and who started it.
 */
export function DeploymentsTable({
  deployments,
  s,
  showService = true,
  filters = true,
  title,
  action,
}: {
  deployments: Deployment[];
  s: PyoState;
  showService?: boolean;
  filters?: boolean;
  title?: ReactNode;
  action?: ReactNode;
}) {
  const [q, setQ] = useState("");
  const [status, setStatus] = useState<StatusFilter>("all");
  const { items, dialogs } = useDeploymentActions(s);

  const rows = deployments.filter((d) => {
    const app = s.services.find((a) => a.id === d.serviceId);
    const ql = q.trim().toLowerCase();
    const ref = d.source.type === "git" ? d.source.branch : d.revisionLabel;
    return (
      (!ql ||
        d.id.includes(ql) ||
        d.revisionLabel.toLowerCase().includes(ql) ||
        ref.toLowerCase().includes(ql) ||
        app?.name.includes(ql)) &&
      (status === "all" ||
        (status === "active" ? isActive(d.status) : d.status === status))
    );
  });

  return (
    <ListSection
      title={title}
      action={action}
      toolbar={
        filters ? (
          <>
            <SearchInput
              value={q}
              onChange={setQ}
              placeholder={
                showService
                  ? "Search by service, branch or ID…"
                  : "Search by branch or ID…"
              }
              className="min-w-0 flex-1 basis-full sm:basis-0"
            />
            <div className="w-40">
              <Select
                value={status}
                onChange={(e) => setStatus(e.target.value as StatusFilter)}
                aria-label="Status"
                heading="Status"
              >
                <option value="all">All statuses</option>
                <option value="succeeded">Ready</option>
                <option value="active">In progress</option>
                <option value="failed">Error</option>
                <option value="cancelled">Canceled</option>
              </Select>
            </div>
          </>
        ) : undefined
      }
    >
      {rows.length ? (
        <ul className="list-rows overflow-hidden rounded-[14px] border border-card-border bg-card">
          {rows.map((d) => {
            const app = s.services.find((a) => a.id === d.serviceId);
            const cluster = s.clusters.find((c) => c.id === d.clusterId);
            const current = app?.currentDeploymentId === d.id;
            const st = deployStatus(d.status);
            return (
              <li
                key={d.id}
                className="relative grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 gap-y-1 px-5 py-3.5 transition-colors hover:bg-accent/40 md:grid-cols-[minmax(0,1.3fr)_minmax(0,0.9fr)_minmax(0,1.2fr)_minmax(0,1fr)_32px]"
              >
                <div className="min-w-0">
                  <Link
                    to="/$team/deployments/$deploymentId"
                    params={{ team: currentSlug(), deploymentId: d.id }}
                    className="block truncate font-mono text-[13px] font-medium after:absolute after:inset-0 focus:outline-none focus-visible:underline"
                  >
                    {d.id}
                  </Link>
                  <p className="flex items-center gap-1.5 truncate text-[12.5px] text-muted-foreground">
                    {showService ? (
                      <span className="truncate">
                        {app?.name ?? "Deleted service"}
                      </span>
                    ) : (
                      <span className="truncate">{cluster?.name}</span>
                    )}
                    {current ? (
                      <span className="shrink-0 rounded-md bg-primary/15 px-1.5 text-[11px] font-medium text-primary">
                        Current
                      </span>
                    ) : null}
                  </p>
                </div>
                <div className="hidden min-w-0 md:block">
                  <p className="flex items-center gap-2 text-[13px]">
                    <Dot tone={st.tone} className="h-2 w-2" />
                    <span
                      className={st.tone === "danger" ? "text-destructive" : ""}
                    >
                      {st.label}
                    </span>
                  </p>
                  <p className="pl-4 text-[12.5px] tabular-nums text-muted-foreground">
                    {d.status === "queued"
                      ? "Waiting to start"
                      : isActive(d.status)
                        ? STAGE_LABEL[d.status]
                        : duration((d.finishedAt ?? d.createdAt) - d.createdAt)}
                  </p>
                </div>
                <div className="hidden min-w-0 md:block">
                  {d.source.type === "git" ? (
                    <>
                      <p className="flex items-center gap-1.5 truncate text-[13px]">
                        <SourceMark source={d.source} />
                        <span className="truncate">{d.source.branch}</span>
                      </p>
                      <p className="flex items-center gap-1.5 truncate text-[12.5px] text-muted-foreground">
                        <GitCommitIcon size={13} className="shrink-0" />
                        <span className="font-mono text-[12px]">
                          {d.revisionLabel}
                        </span>
                      </p>
                    </>
                  ) : (
                    <>
                      <p className="flex items-center gap-1.5 truncate text-[13px]">
                        <SourceMark source={d.source} />
                        <span className="truncate">
                          {d.source.image
                            ? imageLabel(d.source.image)
                            : d.source.type === "template"
                              ? (getTemplate(d.source.templateId)?.name ??
                                "Template")
                              : ""}
                        </span>
                      </p>
                      <p className="truncate pl-[19px] font-mono text-[12px] text-muted-foreground">
                        {d.revisionLabel}
                      </p>
                    </>
                  )}
                </div>
                <p className="hidden min-w-0 items-center justify-end gap-1.5 text-[12.5px] text-muted-foreground md:flex">
                  <span className="shrink-0">{timeAgo(d.createdAt)} by</span>
                  <span className="min-w-0 text-foreground/90">
                    <DeployActor name={d.actor} size={18} />
                  </span>
                </p>
                <div
                  className="relative z-10 justify-self-end"
                  onClick={(e) => e.stopPropagation()}
                >
                  <Menu
                    label={`Actions for ${d.id}`}
                    items={items(d)}
                    trigger={(t) => (
                      <Button
                        {...t}
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7"
                      >
                        <DotsThreeIcon size={16} weight="bold" />
                      </Button>
                    )}
                  />
                </div>
                {/* Phones: status and age under the ID. */}
                <p className="col-span-2 flex items-center gap-2 text-[12.5px] text-muted-foreground md:hidden">
                  <Dot tone={st.tone} className="h-2 w-2" />
                  <span
                    className={
                      st.tone === "danger"
                        ? "text-destructive"
                        : "text-foreground"
                    }
                  >
                    {st.label}
                  </span>
                  · {timeAgo(d.createdAt)}
                </p>
              </li>
            );
          })}
        </ul>
      ) : deployments.length ? (
        <NoMatches
          query={q}
          what="deployments"
          onClear={() => {
            setQ("");
            setStatus("all");
          }}
        />
      ) : (
        <Panel>
          <EmptyState icon={RocketLaunchIcon} title="No deployments yet" />
        </Panel>
      )}
      {dialogs}
    </ListSection>
  );
}
