import { Tooltip } from "~/components/ui/Tooltip";
import type { DatabaseBackupRun } from "~/lib/pyo/types";

// A database's backups as a GitHub-style grid: one square a day for the
// last 26 weeks, Sunday on top. A square's color says how that day went;
// its tooltip lists each backup with its time, status, size and duration.

const WEEKS = 26;
const DAY = 86_400_000;
const MONTHS = [
  "Jan",
  "Feb",
  "Mar",
  "Apr",
  "May",
  "Jun",
  "Jul",
  "Aug",
  "Sep",
  "Oct",
  "Nov",
  "Dec",
];

export const formatBytes = (b: number | null) =>
  b === null
    ? "—"
    : b < 1024
      ? `${b} B`
      : b < 1024 * 1024
        ? `${Math.max(1, Math.round(b / 1024))} KB`
        : b < 1024 ** 3
          ? `${(b / 1024 / 1024).toFixed(1)} MB`
          : `${(b / 1024 ** 3).toFixed(2)} GB`;

export const formatDuration = (r: DatabaseBackupRun) => {
  if (!r.startedAt || !r.finishedAt)
    return r.status === "running" ? "running" : "—";
  const s = Math.max(0, Math.round((r.finishedAt - r.startedAt) / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
};

const dayKey = (t: number) => {
  const d = new Date(t);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
};

function cellClass(runs: DatabaseBackupRun[]) {
  if (!runs.length) return "bg-foreground/[0.06]";
  const ok = runs.filter((r) => r.status === "succeeded").length;
  const bad = runs.some((r) => r.status === "failed");
  if (runs.some((r) => r.status === "running" || r.status === "queued"))
    return "bg-primary/70";
  if (bad && ok) return "bg-warning/80";
  if (bad) return "bg-destructive/80";
  return ok >= 3 ? "bg-success" : ok === 2 ? "bg-success/75" : "bg-success/50";
}

const STATUS_LABEL: Record<DatabaseBackupRun["status"], string> = {
  queued: "Queued",
  running: "Running",
  succeeded: "Succeeded",
  failed: "Failed",
};

/** The day's runs, for its tooltip. */
function DayTip({ d, list }: { d: Date; list: DatabaseBackupRun[] }) {
  return (
    <span className="block min-w-44 space-y-1 text-left">
      <span className="block font-medium">
        {d.toLocaleDateString(undefined, {
          weekday: "short",
          month: "short",
          day: "numeric",
          year: "numeric",
        })}
      </span>
      {list.length ? (
        list.map((r) => (
          <span key={r.id} className="block text-[11px] leading-4">
            <span
              className={
                r.status === "failed"
                  ? "text-destructive"
                  : r.status === "succeeded"
                    ? "text-success"
                    : ""
              }
            >
              {STATUS_LABEL[r.status]}
            </span>{" "}
            at{" "}
            {new Date(r.createdAt).toLocaleTimeString(undefined, {
              hour: "2-digit",
              minute: "2-digit",
            })}
            <span className="opacity-70">
              {" "}
              · {formatBytes(r.sizeBytes)} · {formatDuration(r)}
            </span>
            {r.error ? (
              <span className="block opacity-70">{r.error.slice(0, 120)}</span>
            ) : null}
          </span>
        ))
      ) : (
        <span className="block text-[11px] opacity-70">No backups</span>
      )}
    </span>
  );
}

/**
 * `weeks` columns ending with this one. Fixed 11px squares by default;
 * `fluid` stretches them to fill the width (squares stay square).
 */
export function BackupHeatmap({
  runs,
  weeks: count = WEEKS,
  fluid = false,
}: {
  runs: DatabaseBackupRun[];
  weeks?: number;
  fluid?: boolean;
}) {
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  // Start on the Sunday count-1 weeks back, so the last column is this week.
  const start = new Date(
    today.getTime() - (today.getDay() + (count - 1) * 7) * DAY,
  );
  const byDay = new Map<string, DatabaseBackupRun[]>();
  for (const r of runs) {
    const k = dayKey(r.createdAt);
    byDay.set(k, [...(byDay.get(k) ?? []), r]);
  }
  const weeks = Array.from({ length: count }, (_, w) =>
    Array.from(
      { length: 7 },
      (_, d) => new Date(start.getTime() + (w * 7 + d) * DAY),
    ),
  );
  const columns = fluid
    ? { gridTemplateColumns: `28px repeat(${count}, minmax(0, 1fr))` }
    : { gridTemplateColumns: `28px repeat(${count}, 11px)` };
  const gap = fluid ? "gap-[3px] sm:gap-1" : "gap-[3px]";

  return (
    <div className={fluid ? "w-full" : "overflow-x-auto"}>
      <div
        className={`${fluid ? "flex w-full" : "inline-flex"} flex-col gap-1`}
      >
        {/* Month labels over the week a month starts in. */}
        <div
          className={`grid ${gap} text-[10.5px] text-muted-foreground`}
          style={columns}
        >
          <span />
          {weeks.map((days, w) => {
            const first = days.find((d) => d.getDate() === 1);
            return (
              <span key={w} className="overflow-visible whitespace-nowrap">
                {first || w === 0 ? MONTHS[(first ?? days[0]).getMonth()] : ""}
              </span>
            );
          })}
        </div>
        <div
          className={`grid grid-flow-col grid-rows-7 ${gap}`}
          style={columns}
        >
          {["", "Mon", "", "Wed", "", "Fri", ""].map((l, i) => (
            <span
              key={`l${i}`}
              className="flex items-center justify-end pr-1.5 text-[10px] leading-none text-muted-foreground"
            >
              {l}
            </span>
          ))}
          {weeks.flatMap((days) =>
            days.map((d) => {
              const future = d.getTime() > today.getTime();
              const list = byDay.get(dayKey(d.getTime())) ?? [];
              const cell = (
                <span
                  className={`block aspect-square w-full rounded-[3px] ${future ? "opacity-0" : cellClass(list)} ${fluid ? "transition-[filter] hover:brightness-125" : ""}`}
                />
              );
              return (
                <div
                  key={d.getTime()}
                  className="min-w-0 [&>span]:!flex [&>span]:w-full"
                >
                  {future ? (
                    cell
                  ) : (
                    <Tooltip
                      focusable={false}
                      content={<DayTip d={d} list={list} />}
                    >
                      {cell}
                    </Tooltip>
                  )}
                </div>
              );
            }),
          )}
        </div>
        <div className="mt-1 flex items-center justify-end gap-3 text-[10.5px] text-muted-foreground">
          <span className="inline-flex items-center gap-1">
            <span className="h-[9px] w-[9px] rounded-[2px] bg-foreground/[0.06]" />{" "}
            None
          </span>
          <span className="inline-flex items-center gap-1">
            <span className="h-[9px] w-[9px] rounded-[2px] bg-success/50" />
            <span className="h-[9px] w-[9px] rounded-[2px] bg-success" /> Backed
            up
          </span>
          <span className="inline-flex items-center gap-1">
            <span className="h-[9px] w-[9px] rounded-[2px] bg-warning/80" />{" "}
            Some failed
          </span>
          <span className="inline-flex items-center gap-1">
            <span className="h-[9px] w-[9px] rounded-[2px] bg-destructive/80" />{" "}
            Failed
          </span>
        </div>
      </div>
    </div>
  );
}
