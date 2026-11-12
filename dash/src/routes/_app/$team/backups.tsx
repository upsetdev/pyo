import {
  ArrowClockwiseIcon,
  CheckCircleIcon,
  ClockCounterClockwiseIcon,
  DatabaseIcon,
  DotsThreeIcon,
  HardDrivesIcon,
  PlusIcon,
  TrashIcon,
  WarningCircleIcon,
  ArrowCounterClockwiseIcon,
  GearSixIcon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { AppLink } from "~/components/ui/AppLink";
import { Badge, Dot, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Checkbox, Segmented, Select, Switch } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";
import { BackupHeatmap, formatBytes } from "~/components/pyo/BackupHeatmap";
import { BackupForm } from "~/components/pyo/ControlBackupSettings";
import {
  AddDatabaseDialog,
  DatabaseRow,
  ENGINES,
} from "~/components/pyo/DatabaseBackups";
import { RestoreSteps } from "~/components/pyo/BackupRestore";
import { BrandMark } from "~/components/pyo/common";
import { dateTime, timeAgo } from "~/lib/pyo/format";
import { api, databaseHref, usePyo } from "~/lib/pyo/store";
import type { Database, TeamBackupRun, TeamBackups } from "~/lib/pyo/types";

// The team's backups on one screen: how things stand (with every database's
// backups in one graph), the databases, the volumes, and what ran lately.
// Adding goes through "Add backup"; restoring and where backups go have
// their own buttons beside it.
export const Route = createFileRoute("/_app/$team/backups")({
  head: () => ({ meta: seo({ title: "Backups" }) }),
  component: () => (
    <Page width="max-w-4xl">
      <Backups />
    </Page>
  ),
});

const STATUS: Record<TeamBackupRun["status"], { tone: Tone; label: string }> = {
  queued: { tone: "neutral", label: "Queued" },
  running: { tone: "progress", label: "Backing up" },
  succeeded: { tone: "success", label: "Done" },
  failed: { tone: "danger", label: "Failed" },
};

const duration = (r: TeamBackupRun) => {
  if (!r.finishedAt) return null;
  const s = Math.max(0, Math.round((r.finishedAt - r.createdAt) / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
};

function Backups() {
  const s = usePyo();
  const teamId = s.currentTeamId;
  const tb = s.teamBackups.find((t) => t.teamId === teamId);
  const role = s.members.find(
    (m) => m.teamId === teamId && m.email === s.user.email,
  )?.role;
  const admin = role === "owner" || role === "admin";
  const canRun = admin || role === "developer";
  const [adding, setAdding] = useState<"database" | "volume" | null>(null);
  const [panel, setPanel] = useState<"destination" | "restore" | null>(null);

  return (
    <div className="space-y-5">
      <PageHeader
        title="Backups"
        description="Backups of your databases and volumes."
        actions={
          tb?.settings ? (
            <>
              <Button
                variant="secondary"
                size="icon"
                aria-label="Restore"
                title="Restore"
                onClick={() => setPanel("restore")}
              >
                <ArrowCounterClockwiseIcon size={15} />
              </Button>
              <Button
                variant="secondary"
                size="icon"
                aria-label="Destination"
                title="Destination"
                onClick={() => setPanel("destination")}
              >
                <GearSixIcon size={15} />
              </Button>
              {admin ? (
                <Menu
                  label="Add backup"
                  width="w-52"
                  items={[
                    {
                      label: "Database",
                      icon: <DatabaseIcon size={14} />,
                      hint: "dump",
                      onSelect: () => setAdding("database"),
                    },
                    {
                      label: "Volume",
                      icon: <HardDrivesIcon size={14} />,
                      hint: "files",
                      onSelect: () => setAdding("volume"),
                    },
                  ]}
                  trigger={(t) => (
                    <Button {...t} variant="secondary" size="sm">
                      <PlusIcon size={13} weight="bold" /> Add backup
                    </Button>
                  )}
                />
              ) : null}
            </>
          ) : undefined
        }
      />

      {!tb?.settings ? (
        <Panel>
          <EmptyState
            icon={ClockCounterClockwiseIcon}
            title="No backup store"
            description="Add an S3 bucket."
          />
        </Panel>
      ) : (
        <>
          <Overview tb={tb} canRun={canRun} />
          <div className="space-y-5">
            <Databases
              tb={tb}
              admin={admin}
              canRun={canRun}
              onAdd={() => setAdding("database")}
            />
            <Volumes
              tb={tb}
              admin={admin}
              canRun={canRun}
              onAdd={() => setAdding("volume")}
            />
          </div>
          <History runs={tb.history} />
          <AddDatabaseDialog
            open={adding === "database"}
            onClose={() => setAdding(null)}
          />
          <VolumesDialog
            tb={tb}
            open={adding === "volume"}
            onClose={() => setAdding(null)}
          />
          <SettingsDialog
            tb={tb}
            admin={admin}
            section={panel ?? "destination"}
            open={panel !== null}
            onClose={() => setPanel(null)}
          />
        </>
      )}
    </div>
  );
}

/** At a glance, and every database's backups in one graph. */
function Overview({ tb, canRun }: { tb: TeamBackups; canRun: boolean }) {
  const last = tb.history[0];
  const failing = tb.databases.filter((d) => d.runs[0]?.status === "failed");
  const volumes = tb.targets.filter((t) => t.included);
  const latest = new Map<string, number>();
  for (const r of [...tb.history].reverse())
    if (r.status === "succeeded" && r.sizeBytes !== null)
      latest.set(`${r.serviceId}:${r.volumeId ?? ""}:${r.label}`, r.sizeBytes);
  const total = [...latest.values()].reduce((a, b) => a + b, 0);
  const runs = tb.databases.flatMap((d) => d.runs);
  const tiles: { label: string; value: ReactNode; note: ReactNode }[] = [
    {
      label: "Last backup",
      value: last ? timeAgo(last.finishedAt ?? last.createdAt) : "Never",
      note: last ? (
        <span className="inline-flex min-w-0 items-center gap-1.5">
          <Dot tone={STATUS[last.status].tone} />
          <span className="truncate">
            {STATUS[last.status].label} · {last.label}
          </span>
        </span>
      ) : (
        "Add one with Add backup"
      ),
    },
    {
      label: "Databases",
      value: tb.databases.length,
      note: failing.length ? (
        <span className="text-destructive">{failing.length} failing</span>
      ) : tb.databases.length ? (
        "All backing up"
      ) : (
        "None yet"
      ),
    },
    {
      label: "Volumes",
      value: volumes.length,
      note: tb.settings?.enabled
        ? `${tb.settings.schedule === "weekly" ? "Weekly" : "Daily"} at ${String(tb.settings.hour).padStart(2, "0")}:00 UTC`
        : volumes.length
          ? "By hand only"
          : "None yet",
    },
    {
      label: "Stored",
      value: total ? formatBytes(total) : "—",
      note:
        tb.settings?.destination === "s3"
          ? `Bucket ${tb.settings.bucket}`
          : "On this server",
    },
  ];
  return (
    <Panel
      title="Overview"
      actions={
        canRun && (tb.databases.length || volumes.length) ? (
          <Button
            size="xs"
            variant="secondary"
            onClick={() => {
              for (const d of tb.databases) void api.runDatabaseBackup(d.id);
              if (volumes.length) void api.runTeamBackup(tb.teamId);
            }}
          >
            <ArrowClockwiseIcon size={12} /> Back up everything now
          </Button>
        ) : undefined
      }
    >
      <div className="grid grid-cols-2 gap-px border-b border-border bg-border sm:grid-cols-4 [&>*]:bg-card">
        {tiles.map((t) => (
          <div key={t.label} className="min-w-0 px-4 py-3.5">
            <p className="text-[12px] text-muted-foreground">{t.label}</p>
            <p className="mt-1 truncate text-[20px] font-semibold tracking-tight tabular-nums">
              {t.value}
            </p>
            <p className="mt-0.5 truncate text-[12px] text-muted-foreground">
              {t.note}
            </p>
          </div>
        ))}
      </div>
      <div className="px-4 py-4">
        <p className="mb-2 text-[12px] font-medium text-muted-foreground">
          Database backups, last 12 months
        </p>
        <BackupHeatmap runs={runs} weeks={52} fluid />
      </div>
    </Panel>
  );
}

function Databases({
  tb,
  admin,
  canRun,
  onAdd,
}: {
  tb: TeamBackups;
  admin: boolean;
  canRun: boolean;
  onAdd: () => void;
}) {
  const s = usePyo();
  // A Postgres database Pyo runs backs itself up (point-in-time); shown, not run here.
  const pitr = s.databases.filter(
    (m) => m.teamId === tb.teamId && m.engine === "postgres" && m.backup?.enabled,
  );
  return (
    <Panel title="Databases">
      {tb.databases.length || pitr.length ? (
        <ul className="list-rows">
          {pitr.map((m) => (
            <PitrRow key={m.id} d={m} />
          ))}
          {tb.databases.map((d) => (
            <DatabaseRow
              key={d.id}
              d={d}
              admin={admin}
              canRun={canRun}
              graph={false}
            />
          ))}
        </ul>
      ) : (
        <EmptyState
          icon={DatabaseIcon}
          title="No databases backed up"
          description="PostgreSQL, MySQL, MariaDB, MongoDB or Redis / Valkey."
          action={
            admin ? (
              <Button size="sm" onClick={onAdd}>
                Add database
              </Button>
            ) : undefined
          }
        />
      )}
    </Panel>
  );
}

function PitrRow({ d }: { d: Database }) {
  const first = Date.parse(d.status?.backup?.firstRecoverabilityPoint ?? "");
  const failed = !!d.status?.backup?.message;
  return (
    <li className="px-4 py-3">
      <div className="flex items-start gap-3">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-border bg-muted/60">
          <BrandMark icon={ENGINES.postgres.icon} size={17} />
        </span>
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-center gap-2 text-[14px] font-medium">
            <AppLink href={databaseHref(d.engine, d.id, "backups")} className="hover:text-primary">
              {d.name}
            </AppLink>
            <Badge>Point-in-time</Badge>
            {failed ? <Badge tone="danger">Last backup failed</Badge> : null}
          </p>
          <p className="mt-0.5 text-[12px] text-muted-foreground">
            Postgres ·{" "}
            {Number.isNaN(first)
              ? "no restore point yet"
              : `restorable to any moment since ${dateTime(first)}`}
          </p>
        </div>
      </div>
    </li>
  );
}

function Volumes({
  tb,
  admin,
  canRun,
  onAdd,
}: {
  tb: TeamBackups;
  admin: boolean;
  canRun: boolean;
  onAdd: () => void;
}) {
  const cfg = tb.settings!;
  const included = tb.targets.filter((t) => t.included);
  const exclude = (id: string) =>
    void api.setTeamBackupSelection(
      tb.teamId,
      cfg.mode,
      cfg.mode === "all"
        ? [...cfg.exceptions, id]
        : cfg.exceptions.filter((e) => e !== id),
    );
  return (
    <Panel
      title="Volumes"
      description={
        cfg.enabled
          ? `${cfg.schedule === "weekly" ? "Weekly" : "Daily"} at ${String(cfg.hour).padStart(2, "0")}:00 UTC, keeping ${cfg.keep}`
          : "Backed up by hand"
      }
    >
      {included.length ? (
        <ul className="list-rows">
          {included.map((t) => {
            const runs = tb.history.filter(
              // A volume's runs wherever it was attached.
              (h) =>
                t.volumeId
                  ? h.volumeId === t.volumeId
                  : h.serviceId === t.serviceId && !h.volumeId,
            );
            const last = runs[0];
            const good = runs.find((h) => h.status === "succeeded");
            const busy =
              last?.status === "running" || last?.status === "queued";
            return (
              <li key={t.id} className="flex items-center gap-3 px-4 py-3">
                <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-border bg-muted/60 text-muted-foreground">
                  <HardDrivesIcon size={16} />
                </span>
                <div className="min-w-0 flex-1">
                  <p className="flex items-center gap-2 truncate text-[14px] font-medium">
                    {t.label}
                    {last && last.status !== "succeeded" ? (
                      <Dot tone={STATUS[last.status].tone} />
                    ) : null}
                  </p>
                  <p className="truncate text-[12px] text-muted-foreground">
                    {last?.status === "failed"
                      ? "Last backup failed"
                      : good
                        ? `${formatBytes(good.sizeBytes)} · ${timeAgo(good.finishedAt ?? good.createdAt)}`
                        : "Not backed up yet"}
                  </p>
                </div>
                {canRun ? (
                  <Button
                    variant="secondary"
                    size="xs"
                    disabled={busy}
                    onClick={() => void api.runTeamBackup(tb.teamId, t.id)}
                  >
                    {busy ? "Backing up…" : "Back up now"}
                  </Button>
                ) : null}
                {admin ? (
                  <Menu
                    label={`Actions for ${t.label}`}
                    items={[
                      {
                        label: "Stop backing up",
                        icon: <TrashIcon size={14} />,
                        danger: true,
                        onSelect: () => exclude(t.id),
                      },
                    ]}
                    trigger={(p) => (
                      <Button
                        {...p}
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7"
                      >
                        <DotsThreeIcon size={16} weight="bold" />
                      </Button>
                    )}
                  />
                ) : null}
              </li>
            );
          })}
        </ul>
      ) : (
        <EmptyState
          icon={HardDrivesIcon}
          title={tb.targets.length ? "No volumes backed up" : "No volumes yet"}
          description={
            tb.targets.length
              ? `${tb.targets.length} volume${tb.targets.length === 1 ? "" : "s"} can be backed up.`
              : "Volumes of deployed services show up here."
          }
          action={
            admin && tb.targets.length ? (
              <Button size="sm" onClick={onAdd}>
                Add volume
              </Button>
            ) : undefined
          }
        />
      )}
    </Panel>
  );
}

const SHOWN = 8;

function History({ runs }: { runs: TeamBackupRun[] }) {
  const [all, setAll] = useState(false);
  const list = all ? runs : runs.slice(0, SHOWN);
  return (
    <Panel
      title="Recent backups"
      footer={
        runs.length > SHOWN ? (
          <button
            type="button"
            onClick={() => setAll((v) => !v)}
            className="font-medium text-foreground/85 transition-colors hover:text-foreground"
          >
            {all ? "Show less" : `Show all ${runs.length}`}
          </button>
        ) : undefined
      }
    >
      {runs.length ? (
        <ul className="list-rows">
          {list.map((r) => {
            const Icon =
              r.status === "succeeded"
                ? CheckCircleIcon
                : r.status === "failed"
                  ? WarningCircleIcon
                  : ArrowClockwiseIcon;
            const KindIcon =
              r.kind === "database" ? DatabaseIcon : HardDrivesIcon;
            return (
              <li
                key={r.id}
                className="grid grid-cols-[16px_minmax(0,1fr)_auto] items-center gap-x-3 px-4 py-2.5 md:grid-cols-[16px_minmax(0,1.4fr)_minmax(0,2fr)_auto]"
              >
                <Icon
                  size={15}
                  className={
                    r.status === "succeeded"
                      ? "text-success"
                      : r.status === "failed"
                        ? "text-destructive"
                        : "animate-spin text-primary"
                  }
                />
                <p className="flex min-w-0 items-center gap-1.5 truncate text-[13px] font-medium">
                  <KindIcon
                    size={12}
                    className="shrink-0 text-muted-foreground"
                  />
                  <span className="truncate">{r.label}</span>
                </p>
                <p className="col-start-2 truncate text-[12px] text-muted-foreground md:col-start-auto">
                  {r.status === "failed"
                    ? r.error
                    : r.status === "succeeded"
                      ? `${formatBytes(r.sizeBytes)} · ${formatBytes(r.addedBytes)} new${duration(r) ? ` · took ${duration(r)}` : ""}`
                      : "Running on its cluster…"}
                </p>
                <p className="row-start-1 text-right text-[12px] text-muted-foreground md:row-start-auto">
                  {timeAgo(r.createdAt)} ·{" "}
                  {r.trigger === "manual" ? "manual" : "scheduled"}
                </p>
              </li>
            );
          })}
        </ul>
      ) : (
        <EmptyState
          icon={ClockCounterClockwiseIcon}
          title="No backups yet"
          description="Backups show up here as they run."
        />
      )}
    </Panel>
  );
}

/** Which volumes, and when: "Add backup → Volume". */
function VolumesDialog({
  tb,
  open,
  onClose,
}: {
  tb: TeamBackups;
  open: boolean;
  onClose: () => void;
}) {
  const cfg = tb.settings!;
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [auto, setAuto] = useState(cfg.mode === "all");
  const [schedule, setSchedule] = useState(cfg.enabled ? cfg.schedule : "off");
  const [hour, setHour] = useState(cfg.hour);
  const [keep, setKeep] = useState(cfg.keep);
  const [saving, setSaving] = useState(false);
  // Fresh from the current state each time it opens.
  const [seen, setSeen] = useState(false);
  if (open && !seen) {
    setSeen(true);
    setPicked(new Set(tb.targets.filter((t) => t.included).map((t) => t.id)));
    setAuto(cfg.mode === "all");
    setSchedule(cfg.enabled ? cfg.schedule : "off");
    setHour(cfg.hour);
    setKeep(cfg.keep);
  }
  if (!open && seen) setSeen(false);

  const save = async () => {
    setSaving(true);
    const ids = tb.targets.map((t) => t.id);
    await api.setTeamBackupSelection(
      tb.teamId,
      auto ? "all" : "selected",
      auto
        ? ids.filter((i) => !picked.has(i))
        : ids.filter((i) => picked.has(i)),
    );
    await api.saveTeamBackupSettings(tb.teamId, {
      destination: cfg.destination,
      localAvailable: cfg.localAvailable,
      enabled: schedule !== "off",
      schedule: schedule === "weekly" ? "weekly" : "daily",
      hour,
      keep,
      endpoint: cfg.endpoint,
      region: cfg.region,
      bucket: cfg.bucket,
      prefix: cfg.prefix,
      accessKey: cfg.accessKey,
    });
    setSaving(false);
    onClose();
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      width="max-w-lg"
      title="Back up volumes"
      description="Files are copied as-is."
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
      footer={
        <>
          <Button type="button" variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={saving}>
            {saving ? "Saving…" : "Save"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {tb.targets.length ? (
          <ul className="list-rows overflow-hidden rounded-xl border border-border">
            {tb.targets.map((t) => (
              <li key={t.id} className="px-3 py-2.5">
                <Checkbox
                  checked={picked.has(t.id)}
                  onChange={(on) =>
                    setPicked((p) => {
                      const n = new Set(p);
                      if (on) n.add(t.id);
                      else n.delete(t.id);
                      return n;
                    })
                  }
                  label={t.label}
                />
              </li>
            ))}
          </ul>
        ) : (
          <p className="text-[13px] text-muted-foreground">No volumes yet.</p>
        )}
        <Switch
          checked={auto}
          onChange={setAuto}
          label="Also back up new volumes"
          description="Includes volumes of services added later."
        />
        <div className="space-y-2">
          <p className="text-[13px] font-medium">Schedule</p>
          <div className="flex flex-wrap items-center gap-2">
            <Segmented
              label="How often"
              value={schedule}
              onChange={setSchedule}
              options={[
                { value: "off", label: "Off" },
                { value: "daily", label: "Daily" },
                { value: "weekly", label: "Weekly" },
              ]}
            />
            {schedule !== "off" ? (
              <>
                <div className="w-32">
                  <Select
                    aria-label="Hour (UTC)"
                    value={String(hour)}
                    onChange={(e) => setHour(Number(e.target.value))}
                  >
                    {Array.from({ length: 24 }, (_, h) => (
                      <option key={h} value={h}>
                        {String(h).padStart(2, "0")}:00 UTC
                      </option>
                    ))}
                  </Select>
                </div>
                <div className="w-28">
                  <Select
                    aria-label="Keep"
                    value={String(keep)}
                    onChange={(e) => setKeep(Number(e.target.value))}
                  >
                    {[7, 14, 30, 90].map((n) => (
                      <option key={n} value={n}>
                        Keep {n}
                      </option>
                    ))}
                  </Select>
                </div>
              </>
            ) : null}
          </div>
        </div>
      </div>
    </Dialog>
  );
}

/** The header's restore and gear buttons: how to restore, and where backups go. */
function SettingsDialog({
  tb,
  admin,
  section: tab,
  open,
  onClose,
}: {
  tb: TeamBackups;
  admin: boolean;
  section: "destination" | "restore";
  open: boolean;
  onClose: () => void;
}) {
  const repository = tb.settings!.repository;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      width="max-w-2xl"
      title={tab === "destination" ? "Destination" : "Restore"}
      description={
        tab === "destination"
          ? undefined
          : "Standard restic snapshots, restorable without Pyo."
      }
    >
      <div className="space-y-4">
        {tab === "destination" ? (
          <BackupForm
            key={JSON.stringify(tb.settings)}
            saved={tb.settings}
            canEdit={admin}
            hideSchedule
            bare
            title="Destination"
            encryption="Encrypted before it leaves the server, except Postgres and MySQL databases Pyo runs."
            onSave={(input) => api.saveTeamBackupSettings(tb.teamId, input)}
            onRun={() => void api.runTeamBackup(tb.teamId)}
            onRemove={() => void api.removeTeamBackupSettings(tb.teamId)}
          />
        ) : (
          <RestoreSteps
            teamId={tb.teamId}
            repository={repository}
            admin={admin}
          />
        )}
      </div>
    </Dialog>
  );
}
