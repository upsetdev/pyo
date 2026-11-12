import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented } from "~/components/ui/Field";
import { dateTime } from "~/lib/pyo/format";
import { currentSlug, teamPath } from "~/lib/pyo/paths";
import { api, databaseHref, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Database } from "~/lib/pyo/types";
import { databaseNameError } from "./NewDatabase";

// Restoring a Postgres or MySQL database's backups into a new database (its
// Backups tab's Restore…): same cluster, size and availability, at the
// latest moment or one picked in the window the backups cover.

/** An RFC 3339 time from the cluster as epoch ms; null when empty or bad. */
const ms = (t?: string) => {
  const n = t ? Date.parse(t) : NaN;
  return Number.isNaN(n) ? null : n;
};
const pad = (n: number) => String(n).padStart(2, "0");
/** Epoch ms as a datetime-local value, in local time. */
const localInput = (at: number) => {
  const d = new Date(at);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

/** What a database's backups cover: from `first` to `last` (null: now, Postgres). */
export function restoreWindow(d: Database) {
  const st = d.status?.backup;
  const first = ms(st?.firstRecoverabilityPoint);
  const last = d.engine === "mysql" ? ms(st?.lastSuccessfulBackup) : null;
  const on = !!d.backup && "retentionDays" in d.backup && d.backup.enabled;
  return { first, last, usable: on && first !== null && (d.engine !== "mysql" || last !== null) };
}

export function RestoreDialog({ open, onClose, d }: { open: boolean; onClose: () => void; d: Database }) {
  const s = usePyo();
  const navigate = useNavigate();
  const win = restoreWindow(d);
  const [name, setName] = useState(() => `${d.name}-restored`.slice(0, 40));
  const [moment, setMoment] = useState<"latest" | "time">("latest");
  const [at, setAt] = useState(() => localInput(Date.now() - 60 * 60_000));
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const nameError = name ? databaseNameError(d.engine, name, s.databases.filter((x) => x.teamId === d.teamId)) : "";
  const target = Date.parse(at);
  const now = win.last ?? Date.now();
  const timeError =
    moment === "latest" || win.first === null
      ? ""
      : Number.isNaN(target) || target < win.first - 60_000 || target > now
        ? `From ${dateTime(win.first)} to ${win.last === null ? "now" : `${dateTime(win.last)}, the latest backup`}.`
        : "";
  const canRestore = win.usable && !!name && !nameError && !timeError && !saving;

  const restore = async () => {
    if (!canRestore) return;
    setSaving(true);
    setError("");
    // A minute picked at the window's start: its first second covered.
    const t = moment === "time" && win.first !== null ? new Date(Math.max(target, win.first)).toISOString() : undefined;
    const r = await api.restoreDatabase(d.id, name, t);
    setSaving(false);
    if ("error" in r) return setError(r.error);
    toast({ title: `Restoring into ${name}`, description: "It's ready in a few minutes." });
    onClose();
    void navigate({ href: teamPath(currentSlug(), databaseHref(d.engine, r.data.id)) });
  };

  return (
    <Dialog
      open={open}
      onClose={saving ? () => undefined : onClose}
      title={`Restore ${d.name}`}
      description="Into a new database, with the same cluster, size and availability."
      onSubmit={() => void restore()}
      footer={
        <>
          {error ? (
            <p className="mr-auto text-[12.5px] text-destructive" role="alert">
              {error}
            </p>
          ) : null}
          <Button type="button" variant="secondary" size="sm" onClick={onClose} disabled={saving}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!canRestore}>
            {saving ? "Restoring…" : "Restore"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Field label="New database" error={nameError || undefined}>
          {(p) => <Input {...p} mono value={name} onChange={(e) => setName(e.target.value.toLowerCase())} />}
        </Field>
        <div className="space-y-3">
          <Segmented
            label="Moment"
            value={moment}
            onChange={setMoment}
            options={[
              { value: "latest", label: win.last === null ? "Latest" : "Latest backup" },
              { value: "time", label: "A moment" },
            ]}
          />
          {moment === "time" && win.first !== null ? (
            <Field label="Date and time" hint={timeError ? undefined : "Your local time."} error={timeError || undefined}>
              {(p) => (
                <Input {...p} type="datetime-local" min={localInput(win.first ?? 0)} max={localInput(now)} value={at} onChange={(e) => setAt(e.target.value)} />
              )}
            </Field>
          ) : null}
        </div>
      </div>
    </Dialog>
  );
}
