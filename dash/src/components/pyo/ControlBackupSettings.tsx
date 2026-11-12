import {
  ArrowClockwiseIcon,
  CheckCircleIcon,
  LockKeyIcon,
  WarningCircleIcon,
} from "@phosphor-icons/react";
import { useState } from "react";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented, Select, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { dateTime, timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { ControlBackupRun, ControlBackupSettings } from "~/lib/pyo/types";

// Backups of Pyo's own data (projects, services, settings, deployment
// history) to S3-compatible storage. Installation-wide, for its admin.

const PROVIDERS = [
  {
    id: "aws",
    label: "AWS S3",
    endpoint: "https://s3.us-east-1.amazonaws.com",
    region: "us-east-1",
  },
  {
    id: "r2",
    label: "Cloudflare R2",
    endpoint: "https://<account-id>.r2.cloudflarestorage.com",
    region: "auto",
  },
  {
    id: "b2",
    label: "Backblaze B2",
    endpoint: "https://s3.us-west-004.backblazeb2.com",
    region: "us-west-004",
  },
  {
    id: "minio",
    label: "MinIO / other",
    endpoint: "https://minio.example.com",
    region: "us-east-1",
  },
];

const STATUS: Record<
  ControlBackupRun["status"],
  { tone: Tone; label: string }
> = {
  queued: { tone: "neutral", label: "Queued" },
  running: { tone: "progress", label: "Backing up" },
  succeeded: { tone: "success", label: "Done" },
  failed: { tone: "danger", label: "Failed" },
};

const size = (b: number | null) =>
  b === null
    ? ""
    : b < 1024 * 1024
      ? `${Math.max(1, Math.round(b / 1024))} KB`
      : `${(b / 1024 / 1024).toFixed(1)} MB`;

export function ControlBackupSection() {
  const s = usePyo();
  if (!s.user.isAdmin)
    return (
      <Panel>
        <p className="px-4 py-4 text-[13px] text-muted-foreground">
          Only the installation's admin manages backups.
        </p>
      </Panel>
    );
  const cb = s.controlBackup;
  return (
    <div className="space-y-4">
      <BackupForm
        key={JSON.stringify(cb?.settings)}
        saved={cb?.settings ?? null}
        title="Backups of Pyo"
        encryption="Encrypted with PYO_SECRET_KEY. Keep that key safe: restoring needs it."
        onSave={(input) => api.saveBackupSettings(input)}
        onRun={() => void api.runBackup()}
        onRemove={() => void api.removeBackupSettings()}
      />
      {cb?.settings ? <History runs={cb.history} /> : null}
    </div>
  );
}

/** Where backups go and when; shared by Pyo's and teams' backups. */
export function BackupForm({
  saved,
  title,
  description,
  encryption,
  onSave,
  onRun,
  onRemove,
  canEdit = true,
  hideSchedule = false,
  bare = false,
}: {
  saved: ControlBackupSettings | null;
  title: string;
  description?: string;
  encryption: string;
  onSave: (
    input: Omit<ControlBackupSettings, "hasSecret"> & { secretKey?: string },
  ) => Promise<{ ok: boolean } | null>;
  onRun: () => void;
  onRemove: () => void;
  canEdit?: boolean;
  /** Only where backups go (the schedule lives elsewhere). */
  hideSchedule?: boolean;
  /**
   * Inside a dialog that already names it: no card or title of its own, and
   * the destination choice shown open rather than folded behind "Change".
   */
  bare?: boolean;
}) {
  const [f, setF] = useState<Omit<ControlBackupSettings, "hasSecret">>(
    saved ?? {
      destination: "local",
      localAvailable: true,
      enabled: true,
      schedule: "daily",
      hour: 2,
      keep: 14,
      endpoint: PROVIDERS[0].endpoint,
      region: PROVIDERS[0].region,
      bucket: "",
      prefix: "pyo-backups",
      accessKey: "",
    },
  );
  const [secret, setSecret] = useState("");
  // Local is the default: the destination stays folded away until changed.
  const [whereOpen, setWhere] = useState(
    saved?.destination === "s3" && !saved.bucket,
  );
  const where = bare || whereOpen;
  const [saving, setSaving] = useState(false);
  const [removing, setRemoving] = useState(false);
  const set = (p: Partial<typeof f>) => setF((x) => ({ ...x, ...p }));

  const save = async () => {
    setSaving(true);
    const res = await onSave({ ...f, secretKey: secret || undefined });
    setSaving(false);
    if (res?.ok) {
      setSecret("");
      toast({
        title: "Backups saved",
        description: "Test file written and removed.",
      });
    }
  };

  const note = (
    <span className="inline-flex items-start gap-1.5">
      <LockKeyIcon size={13} className="mt-[3px] shrink-0" />
      {encryption}
    </span>
  );
  const body = (
    <>
      <fieldset
        disabled={!canEdit}
        className={`min-w-0 space-y-4 disabled:opacity-70 ${bare ? "" : "p-4"}`}
      >
        {hideSchedule ? null : (
          <>
            <Switch
              checked={f.enabled}
              onChange={(enabled) => set({ enabled })}
              label="Schedule"
              description="Off: back up by hand."
            />
            {f.enabled ? (
              <div className="grid gap-4 sm:grid-cols-3">
                <Field label="How often">
                  {(p) => (
                    <Segmented
                      label="How often"
                      value={f.schedule}
                      onChange={(schedule) => set({ schedule })}
                      options={[
                        { value: "daily", label: "Daily" },
                        { value: "weekly", label: "Weekly" },
                      ]}
                      {...p}
                    />
                  )}
                </Field>
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
                <Field label="Keep the newest">
                  {(p) => (
                    <Select
                      {...p}
                      value={String(f.keep)}
                      onChange={(e) => set({ keep: Number(e.target.value) })}
                    >
                      {[7, 14, 30, 90].map((n) => (
                        <option key={n} value={n}>
                          {n} backups
                        </option>
                      ))}
                    </Select>
                  )}
                </Field>
              </div>
            ) : null}
          </>
        )}

        <div className={hideSchedule ? "" : "border-t border-border pt-4"}>
          {bare ? null : (
          <button
            type="button"
            onClick={() => setWhere((w) => !w)}
            aria-expanded={where}
            className="flex w-full items-center justify-between gap-3 text-left"
          >
            <span className="min-w-0">
              <span className="block text-[13px] font-medium">Destination</span>
              <span className="block truncate text-[12px] text-muted-foreground">
                {f.destination === "s3"
                  ? `S3-compatible · ${f.bucket || "no bucket yet"} at ${f.endpoint.replace(/^https?:\/\//, "")}`
                  : "This server · Pyo's backup store (default)"}
              </span>
            </span>
            <span className="shrink-0 text-[12.5px] font-medium text-muted-foreground">
              {where ? "Done" : "Change"}
            </span>
          </button>
          )}
          {where ? (
            <div className={`space-y-3 ${bare ? "" : "mt-3"}`}>
              <Segmented
                label="Destination"
                value={f.destination}
                onChange={(destination) => set({ destination })}
                options={[
                  {
                    value: "local",
                    label: "This server",
                    disabled:
                      !saved?.localAvailable && f.destination !== "local",
                  },
                  { value: "s3", label: "S3-compatible" },
                ]}
              />
              {f.destination === "local" ? (
                <p className="text-[12.5px] text-muted-foreground">
                  On this server only. Add an S3 bucket for off-site copies.
                </p>
              ) : (
                <div>
                  <div className="mb-3 flex flex-wrap gap-1.5">
                    {PROVIDERS.map((p) => (
                      <button
                        key={p.id}
                        type="button"
                        onClick={() =>
                          set({ endpoint: p.endpoint, region: p.region })
                        }
                        className={`rounded-full border px-2.5 py-1 text-[12px] font-medium transition-colors ${f.endpoint === p.endpoint ? "border-foreground/20 bg-accent text-foreground" : "border-border text-muted-foreground hover:text-foreground"}`}
                      >
                        {p.label}
                      </button>
                    ))}
                  </div>
                  <div className="grid gap-4 sm:grid-cols-2">
                    <Field label="Endpoint" className="sm:col-span-2">
                      {(p) => (
                        <Input
                          {...p}
                          mono
                          value={f.endpoint}
                          onChange={(e) => set({ endpoint: e.target.value })}
                          placeholder="https://s3.us-east-1.amazonaws.com"
                        />
                      )}
                    </Field>
                    <Field label="Bucket">
                      {(p) => (
                        <Input
                          {...p}
                          mono
                          value={f.bucket}
                          onChange={(e) => set({ bucket: e.target.value })}
                          placeholder="my-backups"
                        />
                      )}
                    </Field>
                    <Field label="Region">
                      {(p) => (
                        <Input
                          {...p}
                          mono
                          value={f.region}
                          onChange={(e) => set({ region: e.target.value })}
                          placeholder="us-east-1"
                        />
                      )}
                    </Field>
                    <Field label="Folder">
                      {(p) => (
                        <Input
                          {...p}
                          mono
                          value={f.prefix}
                          onChange={(e) => set({ prefix: e.target.value })}
                          placeholder="pyo-backups"
                        />
                      )}
                    </Field>
                    <Field label="Access key ID">
                      {(p) => (
                        <Input
                          {...p}
                          mono
                          value={f.accessKey}
                          onChange={(e) => set({ accessKey: e.target.value })}
                        />
                      )}
                    </Field>
                    <Field
                      label="Secret access key"
                      hint={
                        saved?.hasSecret
                          ? "Saved, encrypted. Leave empty to keep it."
                          : undefined
                      }
                      className="sm:col-span-2"
                    >
                      {(p) => (
                        <Input
                          {...p}
                          mono
                          type="password"
                          value={secret}
                          onChange={(e) => setSecret(e.target.value)}
                          placeholder={saved?.hasSecret ? "••••••••" : ""}
                        />
                      )}
                    </Field>
                  </div>
                </div>
              )}
            </div>
          ) : null}
        </div>
      </fieldset>
      {canEdit ? (
        <div
          className={`flex items-center justify-between gap-2 border-t border-border ${bare ? "mt-5 pt-4" : "px-4 py-3"}`}
        >
          {saved ? (
            <Button variant="ghost" size="sm" onClick={() => setRemoving(true)}>
              Turn off backups
            </Button>
          ) : (
            <span />
          )}
          <Button
            size="sm"
            disabled={
              saving ||
              (f.destination === "s3" &&
                (!f.bucket || !f.accessKey || (!secret && !saved?.hasSecret)))
            }
            onClick={() => void save()}
          >
            {saving
              ? "Checking…"
              : f.destination === "s3"
                ? "Save and test"
                : "Save"}
          </Button>
        </div>
      ) : null}
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title="Turn off backups?"
        description="Existing backups stay."
        confirmLabel="Turn off"
        onConfirm={onRemove}
      />
    </>
  );
  if (bare)
    return (
      <div>
        {body}
        <p className="mt-4 text-[12px] leading-5 text-muted-foreground">
          {note}
        </p>
      </div>
    );
  return (
    <Panel
      title={title}
      description={description}
      actions={
        saved && !hideSchedule ? (
          <Button variant="secondary" size="xs" onClick={onRun}>
            Back up now
          </Button>
        ) : undefined
      }
      footer={note}
    >
      {body}
    </Panel>
  );
}

function History({ runs }: { runs: ControlBackupRun[] }) {
  const last = runs.find((r) => r.status === "succeeded");
  return (
    <Panel
      title="Recent backups"
      description={
        last
          ? `Last good backup ${timeAgo(last.finishedAt ?? last.createdAt)}.`
          : "None yet."
      }
    >
      {runs.length ? (
        <ul className="list-rows">
          {runs.map((r) => {
            const st = STATUS[r.status];
            const Icon =
              r.status === "succeeded"
                ? CheckCircleIcon
                : r.status === "failed"
                  ? WarningCircleIcon
                  : ArrowClockwiseIcon;
            return (
              <li key={r.id} className="flex items-start gap-3 px-4 py-3">
                <Icon
                  size={15}
                  className={`mt-0.5 shrink-0 ${r.status === "succeeded" ? "text-success" : r.status === "failed" ? "text-destructive" : "text-muted-foreground"}`}
                />
                <div className="min-w-0 flex-1">
                  <p className="flex flex-wrap items-center gap-2 text-[13px]">
                    <span className="font-medium">{dateTime(r.createdAt)}</span>
                    <Badge tone={st.tone}>{st.label}</Badge>
                    <span className="text-muted-foreground">
                      {r.trigger === "manual" ? "Manual" : "Scheduled"}
                    </span>
                  </p>
                  <p className="truncate text-[12px] text-muted-foreground">
                    {r.status === "failed"
                      ? r.error
                      : r.objectKey
                        ? `${size(r.sizeBytes)} · ${r.tables} tables · ${r.objectKey}`
                        : "Waiting for the worker…"}
                  </p>
                </div>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="px-4 py-6 text-center text-[13px] text-muted-foreground">
          First run at the scheduled hour.
        </p>
      )}
    </Panel>
  );
}
