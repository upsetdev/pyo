import { optionClass } from "~/components/ui/Field";
import {
  ArrowClockwiseIcon,
  ArrowSquareOutIcon,
  CheckCircleIcon,
  CircleNotchIcon,
  CloudArrowUpIcon,
  DatabaseIcon,
  WarningCircleIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import { type ReactNode, useState } from "react";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Segmented, Select, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { dateTime, timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  UpdateRun,
  UpdateSettings,
  Updates,
} from "~/lib/pyo/types";
import { Arrow } from "~/components/ui/Arrow";
import { MenuPath } from "~/components/ui/MenuPath";

// Updates of Pyo itself: the version it runs, the latest release, how it
// updates (an admin, or automatically in a weekly window) and the update
// dialog listing everything that changes. Installation-wide, for its admin.

const DAYS: { value: UpdateSettings["days"][number]; label: string }[] = [
  { value: "mon", label: "Mon" },
  { value: "tue", label: "Tue" },
  { value: "wed", label: "Wed" },
  { value: "thu", label: "Thu" },
  { value: "fri", label: "Fri" },
  { value: "sat", label: "Sat" },
  { value: "sun", label: "Sun" },
];

const RUN: Record<UpdateRun["status"], { tone: Tone; label: string }> = {
  pyo: { tone: "progress", label: "Updating Pyo" },
  agents: { tone: "progress", label: "Updating agents" },
  succeeded: { tone: "success", label: "Done" },
  partial: { tone: "warning", label: "Partly done" },
  failed: { tone: "danger", label: "Failed" },
};

/** For the admin: whether there's something to update (for a quiet dot). */
export function useUpdateAvailable() {
  const u = usePyo().updates;
  return !!u && (u.available || !!u.active);
}

export function UpdatesSection() {
  const s = usePyo();
  const u = s.updates;
  const [open, setOpen] = useState(false);
  if (!s.user.isAdmin || !u)
    return (
      <Panel>
        <p className="px-4 py-4 text-[13px] text-muted-foreground">
          Only the installation's admin updates Pyo.
        </p>
      </Panel>
    );
  return (
    <div className="space-y-4">
      <VersionPanel u={u} onOpen={() => setOpen(true)} />
      <ModeForm key={JSON.stringify(u.settings)} u={u} />
      {u.history.length ? <History runs={u.history} /> : null}
      <UpdateDialog u={u} open={open} onClose={() => setOpen(false)} />
    </div>
  );
}

function VersionPanel({ u, onOpen }: { u: Updates; onOpen: () => void }) {
  const [checking, setChecking] = useState(false);
  const check = async () => {
    setChecking(true);
    const res = await api.checkForUpdates();
    setChecking(false);
    if (!res) return;
    if (res.error)
      toast({ tone: "error", title: "Couldn't check", description: res.error });
    else
      toast({
        title: res.version ? `Latest release: ${res.version}` : "No releases yet",
      });
  };
  const latest = u.latest?.version;
  return (
    <Panel
      title="Version"
      actions={
        u.checksOffByEnv ? null : (
          <Button
            size="sm"
            variant="secondary"
            onClick={check}
            disabled={checking}
          >
            <ArrowClockwiseIcon
              size={13}
              className={checking ? "animate-spin" : ""}
            />
            Check now
          </Button>
        )
      }
    >
      <div className="space-y-3 px-4 py-3">
        <dl className="grid grid-cols-1 gap-3 text-[13px] sm:grid-cols-3">
          <div>
            <dt className="text-[12px] text-muted-foreground">Running</dt>
            <dd className="font-mono font-medium">{u.current}</dd>
          </div>
          <div>
            <dt className="text-[12px] text-muted-foreground">Latest release</dt>
            <dd className="font-mono font-medium">
              {latest ? (
                <a
                  href={u.latest?.url}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-1 hover:text-primary"
                >
                  {latest}
                  <ArrowSquareOutIcon size={12} aria-hidden />
                </a>
              ) : (
                <span className="text-muted-foreground">Unknown</span>
              )}
            </dd>
          </div>
          <div>
            <dt className="text-[12px] text-muted-foreground">Last checked</dt>
            <dd>
              {u.checksOffByEnv
                ? "Off (PYO_UPDATE_CHECK)"
                : !u.settings.check
                  ? "Off"
                  : u.checkedAt
                    ? timeAgo(u.checkedAt)
                    : "Not yet"}
            </dd>
          </div>
        </dl>
        {u.checkError ? (
          <StatusNote
            tone="warning"
            icon={<WarningIcon size={15} />}
            title="The last check didn't work"
          >
            {u.checkError}
          </StatusNote>
        ) : null}
        <Status u={u} onOpen={onOpen} />
      </div>
    </Panel>
  );
}

/** What's going on: an update running, one available, or nothing to do. */
function Status({ u, onOpen }: { u: Updates; onOpen: () => void }) {
  if (u.active)
    return (
      <StatusNote
        tone="progress"
        icon={<CircleNotchIcon size={15} className="animate-spin" />}
        title={`Updating to ${u.active.version}`}
        action={
          <Button size="sm" variant="secondary" onClick={onOpen}>
            Details
          </Button>
        }
      >
        {u.active.message ?? "Starting…"}
      </StatusNote>
    );
  const last = u.history[0];
  const lastFailed =
    last &&
    last.finishedAt &&
    (last.status === "failed" || last.status === "partial");
  if (u.available && u.latest && lastFailed && last.version === u.latest.version)
    return (
      <StatusNote
        tone={last.status === "failed" ? "danger" : "warning"}
        icon={<WarningCircleIcon size={15} />}
        title={
          last.status === "failed"
            ? `Updating to ${last.version} failed`
            : `Updating to ${last.version} didn't finish everywhere`
        }
        action={
          <Button size="sm" onClick={onOpen}>
            {last.status === "failed" ? "Try again" : "Update now"}
          </Button>
        }
      >
        {last.message}
        {u.autoHeld && u.settings.mode === "auto" ? ` ${u.autoHeld}` : ""}
      </StatusNote>
    );
  if (u.available && u.latest)
    return (
      <StatusNote
        tone="neutral"
        icon={<CloudArrowUpIcon size={15} />}
        title={
          u.plan?.pyo.needed
            ? `Pyo ${u.latest.version} is available`
            : `Some agents aren't on ${u.latest.version} yet`
        }
        action={
          <Button size="sm" onClick={onOpen}>
            Update now
          </Button>
        }
      >
        {u.autoHeld && u.settings.mode === "auto"
          ? u.autoHeld
          : u.settings.mode === "auto"
            ? `It's applied automatically in the next window (${windowText(u.settings)}), or now.`
            : "See what changes before you update."}
      </StatusNote>
    );
  if (lastFailed)
    return (
      <StatusNote
        tone={last.status === "failed" ? "danger" : "warning"}
        icon={<WarningCircleIcon size={15} />}
        title={`Updating to ${last.version} ${last.status === "failed" ? "failed" : "didn't finish everywhere"}`}
      >
        {last.message}
      </StatusNote>
    );
  if (u.latest)
    return (
      <StatusNote
        tone="success"
        icon={<CheckCircleIcon size={15} />}
        title="Up to date"
      >
        Pyo and its agents run the latest release.
      </StatusNote>
    );
  return null;
}

function windowText(s: UpdateSettings) {
  const days = DAYS.filter((d) => s.days.includes(d.value))
    .map((d) => d.label)
    .join(", ");
  return `${days || "no day"} ${String(s.hour).padStart(2, "0")}:00 UTC`;
}

function ModeForm({ u }: { u: Updates }) {
  const [f, setF] = useState<UpdateSettings>(u.settings);
  const [saving, setSaving] = useState(false);
  const set = (p: Partial<UpdateSettings>) => setF((x) => ({ ...x, ...p }));
  const dirty = JSON.stringify(f) !== JSON.stringify(u.settings);
  const save = async () => {
    setSaving(true);
    const res = await api.saveUpdateSettings(f);
    setSaving(false);
    if (res?.ok) toast({ title: "Saved" });
  };
  const toggleDay = (d: UpdateSettings["days"][number]) =>
    set({
      days: f.days.includes(d)
        ? f.days.filter((x) => x !== d)
        : DAYS.map((x) => x.value).filter((x) => x === d || f.days.includes(x)),
    });
  return (
    <Panel
      title="How Pyo updates"
      footer={
        <div className="flex justify-end">
          <Button
            size="sm"
            onClick={save}
            disabled={!dirty || saving || (f.mode === "auto" && !f.days.length)}
          >
            Save
          </Button>
        </div>
      }
    >
      <div className="space-y-4 px-4 py-3">
        <Field label="Updates">
          {(p) => (
            <Segmented
              label="Updates"
              value={f.mode}
              onChange={(mode) => set({ mode })}
              options={[
                { value: "manual", label: "Manual" },
                { value: "auto", label: "Automatic" },
              ]}
              {...p}
            />
          )}
        </Field>
        <p className="-mt-2 text-[12.5px] text-muted-foreground">
          {f.mode === "manual"
            ? "You choose when."
            : "Applied in the window below. Some releases wait for you."}
        </p>
        {f.mode === "auto" ? (
          <div className="flex flex-wrap items-end gap-4">
            <div>
              <p className="mb-1.5 text-[12px] font-medium text-muted-foreground">
                Days (UTC)
              </p>
              <div
                role="group"
                aria-label="Days"
                className="flex flex-wrap gap-1"
              >
                {DAYS.map((d) => {
                  const on = f.days.includes(d.value);
                  return (
                    <button
                      key={d.value}
                      type="button"
                      aria-pressed={on}
                      onClick={() => toggleDay(d.value)}
                      className={`${optionClass({ selected: on, pill: true })} h-7 px-2.5 text-[12px] font-medium ${on ? "text-foreground" : "text-muted-foreground hover:text-foreground"}`}
                    >
                      {d.label}
                    </button>
                  );
                })}
              </div>
            </div>
            <Field label="At (UTC)">
              {(p) => (
                <Select
                  {...p}
                  value={String(f.hour)}
                  onChange={(e) => set({ hour: Number(e.target.value) })}
                >
                  {Array.from({ length: 24 }, (_, h) => (
                    <option key={h} value={h}>
                      {String(h).padStart(2, "0")}:00
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          </div>
        ) : null}
        <Switch
          checked={f.check && !u.checksOffByEnv}
          disabled={u.checksOffByEnv}
          onChange={(check) => set({ check })}
          label="Look for new releases"
          description={
            u.checksOffByEnv
              ? "Turned off for this installation with PYO_UPDATE_CHECK=off."
              : "Every 6 hours, from GitHub."
          }
        />
      </div>
    </Panel>
  );
}

function History({ runs }: { runs: UpdateRun[] }) {
  return (
    <Panel title="Recent updates">
      <ul className="list-rows">
        {runs.map((r) => (
          <li key={r.id} className="flex items-start gap-3 px-4 py-3">
            <div className="min-w-0 flex-1">
              <p className="flex flex-wrap items-center gap-2 text-[13px]">
                <span className="font-mono font-medium">
                  {r.fromVersion}
                  <Arrow />
                  {r.version}
                </span>
                <Badge tone={RUN[r.status].tone}>{RUN[r.status].label}</Badge>
                <span className="text-muted-foreground">
                  {dateTime(r.createdAt)} ·{" "}
                  {r.trigger === "auto"
                    ? "Automatic"
                    : (r.startedBy ?? "An admin")}
                </span>
              </p>
              {r.message ? (
                <p className="mt-0.5 text-[12px] text-muted-foreground">
                  {r.message}
                </p>
              ) : null}
            </div>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

/** Everything an update changes, what to know first, then the button. */
export function UpdateDialog({
  u,
  open,
  onClose,
}: {
  u: Updates;
  open: boolean;
  onClose: () => void;
}) {
  const [starting, setStarting] = useState(false);
  const latest = u.latest;
  const plan = u.plan;
  const start = async () => {
    if (!latest) return;
    setStarting(true);
    const res = await api.startUpdate(latest.version);
    setStarting(false);
    if (res?.ok)
      toast({
        title: `Updating to ${latest.version}`,
        description: "Pyo keeps running while it updates.",
      });
  };
  const active = u.active;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      width="max-w-2xl"
      title={
        active
          ? `Updating to ${active.version}`
          : latest
            ? `Update to Pyo ${latest.version}`
            : "Updates"
      }
      description={
        latest?.publishedAt
          ? `Released ${dateTime(Date.parse(latest.publishedAt))}.`
          : undefined
      }
      footer={
        active ? (
          <Button variant="secondary" onClick={onClose}>
            Close
          </Button>
        ) : (
          <>
            <Button variant="secondary" onClick={onClose}>
              Cancel
            </Button>
            <Button
              onClick={start}
              disabled={starting || !u.available || u.blockers.length > 0}
            >
              {latest ? `Update to ${latest.version}` : "Update"}
            </Button>
          </>
        )
      }
    >
      <div className="max-h-[60dvh] space-y-4 overflow-y-auto px-4 py-3 text-[13px]">
        {active ? <Progress run={active} /> : null}
        {latest?.beforeYouUpdate.length ? (
          <section
            aria-label="Before you update"
            className="rounded-lg border border-warning/30 bg-warning/[0.07] px-3 py-2.5"
          >
            <p className="flex items-center gap-1.5 font-medium">
              <WarningIcon size={15} className="text-warning" />
              Before you update
            </p>
            <ul className="mt-1 list-disc space-y-1 pl-5 text-foreground/90">
              {latest.beforeYouUpdate.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          </section>
        ) : null}
        {!active && u.blockers.length ? (
          <StatusNote
            tone="danger"
            icon={<WarningCircleIcon size={15} />}
            title="This update can't start yet"
          >
            <ul className="list-disc space-y-1 pl-4">
              {u.blockers.map((b) => (
                <li key={b}>{b}</li>
              ))}
            </ul>
          </StatusNote>
        ) : null}
        {!active && plan ? (
          <section aria-label="What changes">
            <h3 className="mb-1.5 text-[12px] font-medium text-muted-foreground">
              What changes
            </h3>
            <ul className="list-rows rounded-lg border border-border">
              <Row
                name="Pyo (dashboard, API and background workers)"
                from={plan.pyo.from}
                to={plan.pyo.to}
                needed={plan.pyo.needed}
                note={
                  plan.pyo.needed
                    ? "Updates one replica at a time, so the dashboard stays up."
                    : undefined
                }
              />
              {plan.agents.map((a) => (
                <Row
                  key={a.clusterId}
                  name={`Agent on ${a.name}${a.sameCluster ? " (Pyo's cluster)" : ""}`}
                  sub={a.team}
                  from={a.from || "unknown"}
                  to={a.to}
                  needed={a.needed}
                  note={
                    !a.needed
                      ? undefined
                      : !a.online
                        ? "Not reachable now: it's skipped, update it later."
                        : !a.canUpdate
                          ? "Can't update itself. Skipped, with steps to do it by hand."
                          : "Restarts on the new version. Services keep running."
                  }
                  warn={a.needed && (!a.online || !a.canUpdate)}
                />
              ))}
              {plan.builders.map((b) => (
                <Row
                  key={b.id}
                  name={`Builder ${b.name}`}
                  sub={b.team}
                  from={b.from || "unknown"}
                  to={b.to}
                  needed={b.needed}
                  note={
                    b.needed
                      ? "Moves to the new builder image after its current build."
                      : undefined
                  }
                />
              ))}
              <li className="flex items-start gap-3 px-3 py-2.5">
                <DatabaseIcon
                  size={15}
                  className="mt-0.5 shrink-0 text-muted-foreground"
                />
                <div className="min-w-0 flex-1">
                  <p className="font-medium">Database</p>
                  <p className="text-[12px] text-muted-foreground">
                    {plan.migrations.length
                      ? `${plan.migrations.length} change${plan.migrations.length === 1 ? "" : "s"} (${plan.migrations.join(", ")}). They keep the running version working, so it's safe to roll back.`
                      : "No changes."}
                  </p>
                </div>
              </li>
            </ul>
            {plan.platformImages.length ? (
              <p className="mt-1.5 text-[12px] text-muted-foreground">
                Platform: {plan.platformImages.join(", ")}.
              </p>
            ) : null}
          </section>
        ) : null}
        {!active ? <BackupHint u={u} /> : null}
        {latest?.notes ? (
          <section aria-label="Release notes">
            <h3 className="mb-1.5 flex items-center justify-between text-[12px] font-medium text-muted-foreground">
              Release notes
              <a
                href={latest.url}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1 font-normal hover:text-foreground"
              >
                On GitHub <ArrowSquareOutIcon size={12} aria-hidden />
              </a>
            </h3>
            <pre className="max-h-64 overflow-y-auto whitespace-pre-wrap rounded-lg border border-border bg-muted/40 px-3 py-2 font-sans text-[12.5px] leading-5">
              {latest.notes}
            </pre>
          </section>
        ) : null}
      </div>
    </Dialog>
  );
}

function Row({
  name,
  sub,
  from,
  to,
  needed,
  note,
  warn,
}: {
  name: string;
  sub?: string;
  from: string;
  to: string;
  needed: boolean;
  note?: string;
  warn?: boolean;
}) {
  return (
    <li className="flex items-start gap-3 px-3 py-2.5">
      {needed ? (
        warn ? (
          <WarningIcon size={15} className="mt-0.5 shrink-0 text-warning" />
        ) : (
          <CloudArrowUpIcon size={15} className="mt-0.5 shrink-0 text-info" />
        )
      ) : (
        <CheckCircleIcon size={15} className="mt-0.5 shrink-0 text-success" />
      )}
      <div className="min-w-0 flex-1">
        <p className="font-medium">
          {name}
          {sub ? (
            <span className="font-normal text-muted-foreground"> · {sub}</span>
          ) : null}
        </p>
        {note ? (
          <p className="text-[12px] text-muted-foreground">{note}</p>
        ) : null}
      </div>
      <span className="shrink-0 font-mono text-[12px] text-muted-foreground">
        {needed ? (
          <>
            {from}
            <Arrow />
            <span className="text-foreground">{to}</span>
          </>
        ) : (
          <>{to}, up to date</>
        )}
      </span>
    </li>
  );
}

function BackupHint({ u }: { u: Updates }) {
  const last = u.backup.lastAt;
  const old = !last || Date.now() - last > 24 * 3600_000;
  return (
    <StatusNote
      tone={old ? "warning" : "neutral"}
      icon={<DatabaseIcon size={15} />}
      title={
        last
          ? `Pyo's data was last backed up ${timeAgo(last)}`
          : "Pyo's own data hasn't been backed up yet"
      }
      action={
        <ButtonLink href="/settings/backups" size="sm" variant="secondary">
          Backups
        </ButtonLink>
      }
    >
      {old
        ? "Back up first: database changes can't be undone."
        : "Recent enough to restore from if you need to."}
    </StatusNote>
  );
}

function Progress({ run }: { run: UpdateRun }) {
  const agents = run.agents.filter((a) => a.state !== "none");
  const pyoDone = run.status !== "pyo";
  return (
    <section aria-label="Progress" aria-live="polite" className="space-y-2">
      <ol className="space-y-1.5">
        <Step
          state={pyoDone ? "done" : "running"}
          label={
            <>
              Pyo {run.fromVersion}
              <Arrow />
              {run.version}
            </>
          }
          note={pyoDone ? undefined : (run.message ?? undefined)}
        />
        {run.status === "pyo" ? (
          <Step state="waiting" label="Agents" />
        ) : agents.length ? (
          agents.map((a) => (
            <Step
              key={a.clusterId}
              state={
                a.state === "done"
                  ? "done"
                  : a.state === "failed" || a.state === "skipped"
                    ? "failed"
                    : a.state === "sent"
                      ? "running"
                      : "waiting"
              }
              label={`Agent on ${a.name}`}
              note={a.message}
            />
          ))
        ) : (
          <Step state="running" label="Agents" />
        )}
      </ol>
      <p className="text-[12px] text-muted-foreground">
        You can close this. The update carries on, and{" "}
        <MenuPath steps={["Settings", "Updates"]} /> shows how it went.
      </p>
    </section>
  );
}

function Step({
  state,
  label,
  note,
}: {
  state: "done" | "running" | "waiting" | "failed";
  label: ReactNode;
  note?: string;
}) {
  const icon = {
    done: <CheckCircleIcon size={15} className="text-success" />,
    running: <CircleNotchIcon size={15} className="animate-spin text-info" />,
    waiting: <CircleNotchIcon size={15} className="text-muted-foreground/50" />,
    failed: <WarningCircleIcon size={15} className="text-destructive" />,
  }[state];
  return (
    <li className="flex items-start gap-2">
      <span className="mt-0.5 shrink-0">{icon}</span>
      <div className="min-w-0">
        <p className="font-medium">{label}</p>
        {note ? (
          <p className="text-[12px] text-muted-foreground">{note}</p>
        ) : null}
      </div>
    </li>
  );
}
