import {
  CaretDownIcon,
  CaretRightIcon,
  CheckCircleIcon,
  CircleNotchIcon,
  DotsThreeIcon,
  PlusIcon,
  TimerIcon,
  XCircleIcon,
} from "@phosphor-icons/react";
import { createFileRoute, Link } from "@tanstack/react-router";
import { useState } from "react";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { fetchCronRuns, type CronRunRecord } from "~/lib/pyo/backend";
import { dateTime, timeAgo } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import { currentSlug } from "~/lib/pyo/paths";
import { api, serviceParams, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { useCronJobForm } from "~/components/pyo/CronJobForm";
import { AnsiText } from "~/components/pyo/AnsiText";
import type { Application, CronRun, CronTask } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/cron-jobs")({
  head: () => ({ meta: seo({ title: "Cron Jobs" }) }),
  component: CronJobsPage,
});

/** One row: a service's cron job, or a service that is one. */
type Row = {
  key: string;
  name: string;
  schedule: string;
  command?: string;
  paused: boolean;
  lastRun?: CronRun | null;
  app: Application;
  /** Set for a job that runs on a service; unset for a cron job service. */
  task?: CronTask;
};

function rowsOf(apps: Application[]): Row[] {
  return apps.flatMap((app): Row[] => {
    if (app.workloadKind === "CronJob")
      return [
        {
          key: app.id,
          name: app.name,
          schedule: app.schedule ?? "",
          command: app.command,
          paused: app.status === "stopped",
          lastRun: app.runtime.lastRun,
          app,
        },
      ];
    return (app.cronJobs ?? []).map((task) => {
      const live = app.runtime.cronJobs?.find((r) => r.id === task.id);
      return {
        key: `${app.id}:${task.id}`,
        name: task.name,
        schedule: task.schedule,
        command: task.command,
        paused: task.paused,
        lastRun: live?.lastRun,
        app,
        task,
      };
    });
  });
}

function duration(ms: number) {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  return m < 60 ? `${m}m ${s % 60}s` : `${Math.floor(m / 60)}h ${m % 60}m`;
}

const RUN_STATUS = {
  succeeded: {
    Icon: CheckCircleIcon,
    tone: "text-success",
    label: "Succeeded",
  },
  failed: { Icon: XCircleIcon, tone: "text-destructive", label: "Failed" },
  running: {
    Icon: CircleNotchIcon,
    tone: "text-muted-foreground",
    label: "Running",
  },
} as const;

/** One run, with its output on a click. */
function RunRow({ run }: { run: CronRunRecord }) {
  const [open, setOpen] = useState(false);
  const st = RUN_STATUS[run.status];
  const detail = [
    run.finishedAt ? duration(run.finishedAt - run.startedAt) : null,
    run.exitCode != null ? `exit code ${run.exitCode}` : null,
    run.reason,
  ].filter(Boolean);
  return (
    <li>
      <button
        type="button"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        className="flex w-full items-center gap-3 px-3 py-2 text-left text-[12.5px] transition-colors hover:bg-accent/50"
      >
        <st.Icon
          size={14}
          weight="fill"
          className={`shrink-0 ${st.tone} ${run.status === "running" ? "animate-spin" : ""}`}
        />
        <span className={`w-20 shrink-0 ${st.tone}`}>{st.label}</span>
        <span
          className="w-36 shrink-0 tabular-nums"
          title={dateTime(run.startedAt)}
        >
          {timeAgo(run.startedAt)}
        </span>
        <span className="min-w-0 flex-1 truncate text-muted-foreground">
          {detail.join(" · ")}
        </span>
        {open ? (
          <CaretDownIcon size={12} className="shrink-0 text-muted-foreground" />
        ) : (
          <CaretRightIcon
            size={12}
            className="shrink-0 text-muted-foreground"
          />
        )}
      </button>
      {open ? (
        <div className="mx-3 mb-3 overflow-hidden rounded-lg bg-muted/50">
          <dl className="flex flex-wrap gap-x-6 gap-y-1 border-b border-border/60 px-3 py-2 text-[11.5px]">
            <RunTime label="Started" value={exactTime(run.startedAt)} />
            <RunTime
              label="Finished"
              value={run.finishedAt ? exactTime(run.finishedAt) : "Still running"}
            />
            <RunTime
              label="Took"
              value={duration(
                (run.finishedAt ?? Date.now()) - run.startedAt,
              )}
            />
          </dl>
          <pre className="max-h-72 overflow-auto whitespace-pre-wrap break-all p-3 font-mono text-[11.5px] leading-5 text-foreground">
            {run.output?.trim() ? (
              <AnsiText text={run.output.trim()} />
            ) : run.status === "running" ? (
              "Its output shows here once it finishes."
            ) : (
              "No output."
            )}
          </pre>
        </div>
      ) : null}
    </li>
  );
}

/** To the second, in your time zone: "Nov 3, 2026, 09:41:07 GMT+7". */
const exactTime = (at: number) =>
  new Date(at).toLocaleString("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
    timeZoneName: "short",
  });

function RunTime({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex gap-1.5">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="tabular-nums text-foreground">{value}</dd>
    </div>
  );
}

/** A cron job's history: how its recent runs went, and what they printed. */
function RunHistory({ row }: { row: Row }) {
  const cron = row.task?.id ?? "";
  const q = useQuery(
    `cron-runs:${row.app.id}:${cron}`,
    () => fetchCronRuns(row.app.id, cron),
    { refreshMs: 15_000 },
  );
  const [all, setAll] = useState(false);
  if (q.status === "loading")
    return (
      <p className="px-3 py-3 text-[12.5px] text-muted-foreground">
        Loading runs…
      </p>
    );
  if (q.status === "error")
    return (
      <p className="px-3 py-3 text-[12.5px] text-destructive">{q.error}</p>
    );
  const runs = q.data.runs;
  if (!runs.length)
    return (
      <p className="px-3 py-3 text-[12.5px] text-muted-foreground">
        No runs yet. Choose Run now to try it.
      </p>
    );
  const finished = runs.filter((r) => r.status !== "running");
  const ok = finished.filter((r) => r.status === "succeeded").length;
  return (
    <div>
      <p className="px-3 pb-1 pt-2.5 text-[12px] text-muted-foreground">
        {finished.length
          ? `${ok} of the last ${finished.length} runs succeeded`
          : "Running for the first time"}
      </p>
      <ul className="list-rows">
        {(all ? runs : runs.slice(0, 10)).map((r) => (
          <RunRow key={r.job} run={r} />
        ))}
      </ul>
      {runs.length > 10 && (
        <button
          type="button"
          onClick={() => setAll((v) => !v)}
          className="w-full border-t border-border/60 px-3 py-2 text-left text-[12.5px] text-muted-foreground hover:text-foreground"
        >
          {all ? "Show fewer" : `Show all ${runs.length} runs`}
        </button>
      )}
    </div>
  );
}

function LastRun({ run }: { run?: CronRun | null }) {
  if (!run) return <span className="text-muted-foreground">Not run yet</span>;
  const [label, tone] =
    run.succeeded == null
      ? ["Running", "text-foreground"]
      : run.succeeded
        ? ["Succeeded", "text-success"]
        : ["Failed", "text-destructive"];
  return (
    <span>
      <span className={tone}>{label}</span>{" "}
      <span className="text-muted-foreground">{timeAgo(run.startedAt)}</span>
    </span>
  );
}

/** Change a cron job. New ones get their own page (cron-jobs/new). */
function CronJobDialog({
  edit,
  onClose,
}: {
  edit: { app: Application; task: CronTask };
  onClose: () => void;
}) {
  const f = useCronJobForm(edit);
  return (
    <Dialog
      open
      onClose={onClose}
      title={`Edit ${edit.task.name}`}
      description="Runs a command on a schedule."
      onSubmit={async () => {
        if (await f.save()) onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!f.ready}>
            {f.busy ? "Saving…" : "Save"}
          </Button>
        </>
      }
    >
      {f.fields}
    </Dialog>
  );
}

function CronJobsPage() {
  const s = usePyo();
  const team = currentSlug();
  const apps = s.services.filter((a) => a.teamId === s.currentTeamId);
  const rows = rowsOf(apps);
  const [editing, setEditing] = useState<Row | null>(null);
  const [removing, setRemoving] = useState<Row | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const add = (
    <ButtonLink href="/cron-jobs/new" size="sm" variant="secondary">
      <PlusIcon size={13} weight="bold" /> Add cron job
    </ButtonLink>
  );
  const run = async (r: Row) => {
    if (r.task) {
      if (!(await api.runCronJob(r.app.id, r.task.id))) return;
    } else api.restartService(r.app.id);
    toast({ title: `Started ${r.name}` });
  };
  const pause = (r: Row, on: boolean) =>
    r.task
      ? void api.saveCronJob({ serviceId: r.app.id, ...r.task, paused: on })
      : on
        ? api.stopService(r.app.id)
        : void api.scaleService(r.app.id, 1);
  return (
    <Page>
      <PageHeader
        title="Cron Jobs"
        description="Commands that run on a schedule, in UTC."
        actions={add}
      />
      <Panel>
        {rows.length ? (
          <ul className="list-rows">
            {rows.map((r) => (
              <li key={r.key}>
                <div className="flex items-center gap-4 px-4 py-3">
                  <button
                    type="button"
                    onClick={() =>
                      setExpanded(expanded === r.key ? null : r.key)
                    }
                    aria-expanded={expanded === r.key}
                    aria-label={`Runs of ${r.name}`}
                    className="flex h-6 w-6 shrink-0 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                  >
                    {expanded === r.key ? (
                      <CaretDownIcon size={14} />
                    ) : (
                      <CaretRightIcon size={14} />
                    )}
                  </button>
                  <div className="min-w-0 flex-1">
                    <p className="flex items-center gap-2 truncate text-[13px] font-medium">
                      {r.name}
                      {r.paused ? (
                        <span className="rounded bg-muted px-1.5 text-[11px] font-normal text-muted-foreground">
                          Paused
                        </span>
                      ) : null}
                    </p>
                    <p className="truncate text-[12px] text-muted-foreground">
                      <Link
                        to="/$team/$projectId/$serviceId"
                        params={{ team, ...serviceParams(r.app.id) }}
                        className="text-foreground hover:text-primary"
                      >
                        {r.app.name}
                      </Link>
                      {r.command ? (
                        <>
                          {" · "}
                          <span className="font-mono text-[11.5px]">
                            {r.command}
                          </span>
                        </>
                      ) : null}
                    </p>
                  </div>
                  <span className="hidden w-32 shrink-0 font-mono text-[12px] md:block">
                    {r.schedule}
                  </span>
                  <span className="hidden w-40 shrink-0 text-[12.5px] md:block">
                    <LastRun run={r.lastRun} />
                  </span>
                  <Menu
                    label={`Actions for ${r.name}`}
                    items={[
                      { label: "Run now", onSelect: () => void run(r) },
                      {
                        label: r.paused ? "Resume" : "Pause",
                        onSelect: () => pause(r, !r.paused),
                      },
                      ...(r.task
                        ? [
                            {
                              label: "Edit",
                              onSelect: () => setEditing(r),
                            },
                            { type: "separator" as const },
                            {
                              label: "Delete",
                              danger: true,
                              onSelect: () => setRemoving(r),
                            },
                          ]
                        : []),
                    ]}
                    trigger={(t) => (
                      <Button
                        {...t}
                        variant="ghost"
                        size="icon"
                        aria-label={`Actions for ${r.name}`}
                      >
                        <DotsThreeIcon size={16} weight="bold" />
                      </Button>
                    )}
                  />
                </div>
                {expanded === r.key ? (
                  <div className="mx-4 mb-3 overflow-hidden rounded-lg border border-border">
                    <RunHistory row={r} />
                  </div>
                ) : null}
              </li>
            ))}
          </ul>
        ) : (
          <EmptyState
            icon={TimerIcon}
            title="No cron jobs"
            description="Run a command on a schedule."
            action={
              <ButtonLink href="/cron-jobs/new" size="sm">
                Add cron job
              </ButtonLink>
            }
          />
        )}
      </Panel>
      {editing?.task ? (
        <CronJobDialog
          edit={{ app: editing.app, task: editing.task }}
          onClose={() => setEditing(null)}
        />
      ) : null}
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Delete ${removing?.name}?`}
        description="Past runs are removed."
        confirmLabel="Delete"
        onConfirm={() => {
          if (removing?.task)
            void api.removeCronJob(removing.app.id, removing.task.id);
        }}
      />
    </Page>
  );
}
