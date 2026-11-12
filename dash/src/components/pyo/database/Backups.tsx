import { WarningIcon } from "@phosphor-icons/react";
import { type ReactNode, useState } from "react";
import { AppLink } from "~/components/ui/AppLink";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Input, Select, Switch } from "~/components/ui/Field";
import { Callout, KeyValue, Panel } from "~/components/ui/Panel";
import { dateTime } from "~/lib/pyo/format";
import { formatBytes } from "~/components/pyo/BackupHeatmap";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Database } from "~/lib/pyo/types";
import { useDatabase } from "./DatabaseLayout";
import { RestoreDialog, restoreWindow } from "./Restore";

export function DatabaseBackupsPage() {
  const d = useDatabase();
  if (!d) return null;
  return d.engine === "valkey" ? <KvBackups key={d.id} d={d} /> : <Backups key={d.id} d={d} />;
}

/** An RFC 3339 time from the cluster as epoch ms; null when empty or bad. */
const ms = (t?: string) => {
  const n = t ? Date.parse(t) : NaN;
  return Number.isNaN(n) ? null : n;
};
const when = (t?: string) => {
  const n = ms(t);
  return n === null ? "—" : dateTime(n);
};
const pad = (n: number) => String(n).padStart(2, "0");

/** The team's Backups destination as this database sees it. */
function useDestination(d: Database) {
  const s = usePyo();
  const settings = s.teamBackups.find((t) => t.teamId === d.teamId)?.settings ?? null;
  const cluster = s.clusters.find((c) => c.id === d.clusterId);
  return {
    settings,
    usable: !!settings && (settings.destination === "s3" || !!cluster?.sameCluster),
    label: !settings
      ? null
      : settings.destination === "local"
        ? "This server"
        : `${settings.bucket}${settings.prefix ? `/${settings.prefix}` : ""}`,
  };
}

function NoDestination({ hasSettings }: { hasSettings: boolean }) {
  return (
    <Panel title="Backups">
      <div className="flex flex-wrap items-center justify-between gap-3 p-4">
        <p className="text-[13px] text-muted-foreground">
          {hasSettings
            ? "The team's backups go to this server, which only Pyo's own cluster reaches. Use an S3-compatible bucket for this database."
            : "Backups go where the team's backups go. Set that up first."}
        </p>
        <ButtonLink href="/backups" size="sm" variant="secondary">
          Backups
        </ButtonLink>
      </div>
    </Panel>
  );
}

const DestinationRow = ({ label }: { label: string | null }): [string, ReactNode] => [
  "Destination",
  label ? (
    <AppLink href="/backups" className="hover:text-primary">
      {label}
    </AppLink>
  ) : (
    "—"
  ),
];

function Backups({ d }: { d: Database }) {
  const dest = useDestination(d);
  const cfg = d.backup && "retentionDays" in d.backup ? d.backup : { enabled: false, retentionDays: 7, hour: 3 };
  const [enabled, setEnabled] = useState(cfg.enabled);
  const [days, setDays] = useState(cfg.retentionDays);
  const [hour, setHour] = useState(cfg.hour);
  const [saving, setSaving] = useState(false);
  const [restoring, setRestoring] = useState(false);

  const daysError = days < 1 || days > 90 || !Number.isInteger(days) ? "1 to 90 days." : "";
  const dirty = enabled !== cfg.enabled || days !== cfg.retentionDays || hour !== cfg.hour;

  const save = async () => {
    setSaving(true);
    const r = await api.setDatabaseBackups(d.id, { enabled, retentionDays: days, hour });
    setSaving(false);
    if ("error" in r) toast({ tone: "error", title: "Not saved", description: r.error });
    else toast({ title: "Saved" });
  };

  if (!dest.usable && !cfg.enabled) return <NoDestination hasSettings={!!dest.settings} />;

  const st = d.status?.backup;
  // MySQL uploads its changes with each backup: restorable up to the latest one.
  const mysql = d.engine === "mysql";
  const { first, last, usable } = restoreWindow(d);
  const failed = ms(st?.lastFailedBackup);
  const succeeded = ms(st?.lastSuccessfulBackup);
  const showFailure = !!st?.message || (failed !== null && (succeeded === null || failed > succeeded));

  return (
    <div className="space-y-6">
      {cfg.enabled ? (
        <Panel
          title="Restore"
          actions={
            <Button size="sm" variant="secondary" disabled={!usable} onClick={() => setRestoring(true)}>
              Restore…
            </Button>
          }
        >
          <div className="space-y-3 p-4">
            <p className="text-[13px]">
              {!usable || first === null
                ? "No backup yet. The first runs shortly after backups are turned on."
                : mysql && last !== null
                ? `You can restore to any moment from ${dateTime(first)} to ${dateTime(last)}, its latest backup.`
                : `You can restore to any moment from ${dateTime(first)} to now.`}
            </p>
            {showFailure ? (
              <Callout tone="danger" icon={WarningIcon} title="Last backup failed">
                {st?.message || when(st?.lastFailedBackup)}
              </Callout>
            ) : null}
          </div>
          <KeyValue
            className="border-t border-border py-1"
            items={[
              [mysql ? "Last backup" : "Last base backup", when(st?.lastSuccessfulBackup)],
              // MySQL's changes go up with each backup: the same moment.
              ...(mysql ? [] : [["Last archived change", when(st?.lastArchivedWal)] as [string, string]]),
            ]}
          />
        </Panel>
      ) : null}
      {usable ? <RestoreDialog open={restoring} onClose={() => setRestoring(false)} d={d} /> : null}
      <Panel title="Settings">
        <div className="space-y-5 p-4">
          <Switch
            checked={enabled}
            onChange={setEnabled}
            label="Backups"
            description={
              mysql
                ? "A daily backup with every change since the last, so it can be restored to any moment up to the latest backup."
                : "Every change is kept, so it can be restored to any moment."
            }
          />
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field label="Keep for (days)" error={daysError || undefined}>
              {(p) => (
                <Input
                  {...p}
                  type="number"
                  min={1}
                  max={90}
                  value={days}
                  disabled={!enabled}
                  onChange={(e) => setDays(Number(e.target.value))}
                />
              )}
            </Field>
            <Field label={mysql ? "Daily backup (UTC)" : "Daily base backup (UTC)"}>
              {(p) => (
                <Select {...p} value={hour} disabled={!enabled} onChange={(e) => setHour(Number(e.target.value))}>
                  {Array.from({ length: 24 }, (_, h) => (
                    <option key={h} value={h}>
                      {pad(h)}:00
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          </div>
        </div>
        <KeyValue
          className="border-t border-border py-1"
          items={[DestinationRow({ label: dest.label })]}
        />
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button size="sm" disabled={!dirty || !!daysError || saving} onClick={() => void save()}>
            {saving ? "Saving…" : "Save changes"}
          </Button>
        </div>
      </Panel>

    </div>
  );
}

/** A KV database's snapshots, through Backups: on a schedule, as its own login. */
function KvBackups({ d }: { d: Database }) {
  const s = usePyo();
  const dest = useDestination(d);
  const cfg =
    d.backup && "schedule" in d.backup ? d.backup : { enabled: false, schedule: "daily" as const, hour: 2, keep: 14, backupDatabaseId: null };
  const reg = cfg.backupDatabaseId
    ? s.teamBackups.find((t) => t.teamId === d.teamId)?.databases.find((x) => x.id === cfg.backupDatabaseId)
    : undefined;
  const [enabled, setEnabled] = useState(cfg.enabled);
  const [schedule, setSchedule] = useState<"daily" | "weekly">(cfg.schedule);
  const [hour, setHour] = useState(cfg.hour);
  const [keep, setKeep] = useState(cfg.keep);
  const [saving, setSaving] = useState(false);
  const keepError = keep < 1 || keep > 365 || !Number.isInteger(keep) ? "1 to 365." : "";
  const dirty = enabled !== cfg.enabled || schedule !== cfg.schedule || hour !== cfg.hour || keep !== cfg.keep;
  const save = async () => {
    setSaving(true);
    const r = await api.setDatabaseBackups(d.id, { enabled, schedule, hour, keep });
    setSaving(false);
    if ("error" in r) toast({ tone: "error", title: "Not saved", description: r.error });
    else toast({ title: "Saved" });
  };
  if (!dest.usable && !cfg.enabled) return <NoDestination hasSettings={!!dest.settings} />;
  const runs = reg?.runs ?? [];
  const busy = runs[0]?.status === "running" || runs[0]?.status === "queued";
  return (
    <div className="space-y-6">
      {reg ? (
        <Panel
          title="Snapshots"
          actions={
            <Button size="sm" variant="secondary" disabled={busy} onClick={() => void api.runDatabaseBackup(reg.id)}>
              {busy ? "Backing up…" : "Back up now"}
            </Button>
          }
          footer={
            <AppLink href="/backups" className="font-medium text-foreground/85 hover:text-foreground">
              All backups
            </AppLink>
          }
        >
          {runs.length ? (
            <KeyValue
              className="py-1"
              items={runs.slice(0, 5).map((r): [ReactNode, ReactNode] => [
                dateTime(r.finishedAt ?? r.createdAt),
                r.status === "succeeded" ? (
                  r.sizeBytes != null ? formatBytes(r.sizeBytes) : "Done"
                ) : r.status === "failed" ? (
                  <span className="text-destructive" title={r.error ?? undefined}>
                    Failed
                  </span>
                ) : (
                  "Running…"
                ),
              ])}
            />
          ) : (
            <p className="p-4 text-[13px] text-muted-foreground">None yet.</p>
          )}
        </Panel>
      ) : null}
      <Panel title="Settings">
        <div className="space-y-5 p-4">
          <Switch
            checked={enabled}
            onChange={setEnabled}
            label="Backups"
            description="A snapshot of its data on a schedule, kept in the team's backups."
          />
          <div className="grid items-start gap-4 sm:grid-cols-3">
            <Field label="Schedule">
              {(p) => (
                <Select {...p} value={schedule} disabled={!enabled} onChange={(e) => setSchedule(e.target.value as "daily" | "weekly")}>
                  <option value="daily">Daily</option>
                  <option value="weekly">Weekly</option>
                </Select>
              )}
            </Field>
            <Field label="Time (UTC)">
              {(p) => (
                <Select {...p} value={hour} disabled={!enabled} onChange={(e) => setHour(Number(e.target.value))}>
                  {Array.from({ length: 24 }, (_, h) => (
                    <option key={h} value={h}>
                      {pad(h)}:00
                    </option>
                  ))}
                </Select>
              )}
            </Field>
            <Field label="Keep" error={keepError || undefined}>
              {(p) => (
                <Input {...p} type="number" min={1} max={365} value={keep} disabled={!enabled} onChange={(e) => setKeep(Number(e.target.value))} />
              )}
            </Field>
          </div>
        </div>
        <KeyValue className="border-t border-border py-1" items={[DestinationRow({ label: dest.label })]} />
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button size="sm" disabled={!dirty || !!keepError || saving} onClick={() => void save()}>
            {saving ? "Saving…" : "Save changes"}
          </Button>
        </div>
      </Panel>
    </div>
  );
}
