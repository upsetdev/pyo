import {
  ArrowLeftIcon,
  DatabaseIcon,
  DotsThreeIcon,
  PauseIcon,
  PlayIcon,
  PlusIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { useState, type ReactNode } from "react";
import {
  siMariadb,
  siMongodb,
  siMysql,
  siPostgresql,
  siRedis,
  type SimpleIcon,
} from "simple-icons";
import { AppLink } from "~/components/ui/AppLink";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented, Select, Switch } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { Panel } from "~/components/ui/Panel";
import { timeAgo } from "~/lib/pyo/format";
import { api, databasePath, usePyo } from "~/lib/pyo/store";
import type { Application, BackupDatabase, DbEngine } from "~/lib/pyo/types";
import { BackupHeatmap, formatBytes, formatDuration } from "./BackupHeatmap";
import { BrandMark } from "./common";

// Databases a team backs up, Databasus-style: pick the engine, give an
// admin login once, and Pyo backs it up as a read-only user it creates.

type EngineInfo = {
  label: string;
  icon: SimpleIcon;
  port: number;
  admin: string;
  /** Postgres dumps one database; the others can dump all of them. */
  needsDatabase: boolean;
  /** A service's env: its admin user, password and database. */
  env: { user?: string; password: string; database?: string };
  rights: string;
};

export const ENGINES: Record<DbEngine, EngineInfo> = {
  postgres: {
    label: "PostgreSQL",
    icon: siPostgresql,
    port: 5432,
    admin: "postgres",
    needsDatabase: true,
    env: {
      user: "POSTGRES_USER",
      password: "POSTGRES_PASSWORD",
      database: "POSTGRES_DB",
    },
    rights: "pg_read_all_data (read-only)",
  },
  mysql: {
    label: "MySQL",
    icon: siMysql,
    port: 3306,
    admin: "root",
    needsDatabase: false,
    env: { password: "MYSQL_ROOT_PASSWORD", database: "MYSQL_DATABASE" },
    rights: "SELECT, SHOW VIEW, TRIGGER, LOCK TABLES, EVENT",
  },
  mariadb: {
    label: "MariaDB",
    icon: siMariadb,
    port: 3306,
    admin: "root",
    needsDatabase: false,
    env: { password: "MARIADB_ROOT_PASSWORD", database: "MARIADB_DATABASE" },
    rights: "SELECT, SHOW VIEW, TRIGGER, LOCK TABLES, EVENT",
  },
  mongo: {
    label: "MongoDB",
    icon: siMongodb,
    port: 27017,
    admin: "root",
    needsDatabase: false,
    env: {
      user: "MONGO_INITDB_ROOT_USERNAME",
      password: "MONGO_INITDB_ROOT_PASSWORD",
    },
    rights: "the built-in backup role",
  },
  redis: {
    label: "Redis / Valkey",
    icon: siRedis,
    port: 6379,
    admin: "default",
    needsDatabase: false,
    env: { password: "VALKEY_PASSWORD" },
    rights: "replication commands only (ACL)",
  },
};

/** Which engine a service runs, by template or image. */
function engineOf(a: Application): DbEngine | null {
  const t = a.source.type === "template" ? a.source.templateId : "";
  const img =
    (a.source.type === "git" ? "" : a.source.image)
      .toLowerCase()
      .split("/")
      .pop() ?? "";
  if (t === "postgresql" || img.startsWith("postgres")) return "postgres";
  if (t === "mariadb" || img.startsWith("mariadb")) return "mariadb";
  if (t === "mysql" || img.startsWith("mysql")) return "mysql";
  if (t === "mongodb" || img.startsWith("mongo")) return "mongo";
  if (t === "valkey" || img.startsWith("valkey") || img.startsWith("redis"))
    return "redis";
  return null;
}

const scheduleText = (d: BackupDatabase) =>
  d.enabled
    ? `${d.schedule === "weekly" ? "Weekly" : "Daily"} at ${String(d.hour).padStart(2, "0")}:00 UTC · keeps ${d.keep}`
    : "Paused";

export function DatabaseBackups({
  databases,
  admin,
  canRun,
}: {
  databases: BackupDatabase[];
  admin: boolean;
  canRun: boolean;
}) {
  const [adding, setAdding] = useState(false);
  return (
    <Panel
      title="Databases"

      actions={
        admin ? (
          <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
            <PlusIcon size={12} weight="bold" /> Add database
          </Button>
        ) : undefined
      }
    >
      {databases.length ? (
        <ul className="list-rows">
          {databases.map((d) => (
            <DatabaseRow key={d.id} d={d} admin={admin} canRun={canRun} />
          ))}
        </ul>
      ) : (
        <div className="flex flex-col items-center px-6 py-10 text-center">
          <span className="flex h-10 w-10 items-center justify-center rounded-full border border-border bg-muted/60 text-muted-foreground">
            <DatabaseIcon size={17} />
          </span>
          <p className="mt-3 text-[14px] font-medium">
            No databases backed up yet
          </p>
          <p className="mt-1 max-w-sm text-[12.5px] text-muted-foreground">
            PostgreSQL, MySQL, MariaDB, MongoDB and Redis.
          </p>
          {admin ? (
            <Button size="sm" className="mt-4" onClick={() => setAdding(true)}>
              Add database
            </Button>
          ) : null}
        </div>
      )}
      <AddDatabaseDialog open={adding} onClose={() => setAdding(false)} />
    </Panel>
  );
}

export function DatabaseRow({
  d,
  admin,
  canRun,
  graph = true,
}: {
  d: BackupDatabase;
  admin: boolean;
  canRun: boolean;
  /** Its own activity graph (off where one graph covers them all). */
  graph?: boolean;
}) {
  const [removing, setRemoving] = useState(false);
  const e = ENGINES[d.engine];
  const last = d.runs[0];
  const good = d.runs.find((r) => r.status === "succeeded");
  const busy = last?.status === "running" || last?.status === "queued";
  return (
    <li className={graph ? "space-y-3 px-4 py-4" : "px-4 py-3"}>
      <div className="flex items-start gap-3">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-border bg-muted/60">
          <BrandMark icon={e.icon} size={17} />
        </span>
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-center gap-2 text-[14px] font-medium">
            {d.databaseId ? (
              <AppLink href={databasePath(d.databaseId, "backups")} className="hover:text-primary">
                {d.name}
              </AppLink>
            ) : (
              d.name
            )}
            {d.databaseId ? <Badge>KV</Badge> : null}
            {!d.enabled ? <Badge>Paused</Badge> : null}
            {last?.status === "failed" ? (
              <Badge tone="danger">Last backup failed</Badge>
            ) : null}
          </p>
          <p className="truncate font-mono text-[11.5px] text-muted-foreground">
            {d.host}:{d.port}
            {d.database ? `/${d.database}` : ""} · as {d.user}
          </p>
          <p className="mt-0.5 text-[12px] text-muted-foreground">
            {d.databaseId ? "KV" : e.label} · {scheduleText(d)} ·{" "}
            {good
              ? `last ${timeAgo(good.finishedAt ?? good.createdAt)}, ${formatBytes(good.sizeBytes)} in ${formatDuration(good)}`
              : "not backed up yet"}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {canRun ? (
            <Button
              variant="secondary"
              size="xs"
              disabled={busy}
              onClick={() => void api.runDatabaseBackup(d.id)}
            >
              {busy ? "Backing up…" : "Back up now"}
            </Button>
          ) : null}
          {admin ? (
            <Menu
              label={`Actions for ${d.name}`}
              items={[
                {
                  label: d.enabled ? "Pause schedule" : "Resume schedule",
                  icon: d.enabled ? (
                    <PauseIcon size={14} />
                  ) : (
                    <PlayIcon size={14} />
                  ),
                  onSelect: () =>
                    void api.updateDatabaseBackup(d.id, {
                      enabled: !d.enabled,
                      schedule: d.schedule,
                      hour: d.hour,
                      keep: d.keep,
                    }),
                },
                { type: "separator" },
                {
                  label: "Stop backing up",
                  icon: <TrashIcon size={14} />,
                  danger: true,
                  onSelect: () => setRemoving(true),
                },
              ]}
              trigger={(t) => (
                <Button {...t} variant="ghost" size="icon" className="h-7 w-7">
                  <DotsThreeIcon size={16} weight="bold" />
                </Button>
              )}
            />
          ) : null}
        </div>
      </div>
      {graph ? <BackupHeatmap runs={d.runs} /> : null}
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title={`Stop backing up ${d.name}?`}
        description="Its backups stop. Existing ones are kept."
        confirmLabel="Stop backing up"
        onConfirm={() => void api.deleteDatabaseBackup(d.id)}
      />
    </li>
  );
}

function EnginePicker({ onPick }: { onPick: (e: DbEngine) => void }) {
  return (
    <div className="grid gap-2 sm:grid-cols-2">
      {(Object.keys(ENGINES) as DbEngine[]).map((k) => {
        const e = ENGINES[k];
        return (
          <button
            key={k}
            type="button"
            onClick={() => onPick(k)}
            className="flex items-center gap-3 rounded-[14px] border border-card-border bg-card p-3 text-left transition-colors hover:border-border-strong/70 hover:bg-accent/40"
          >
            <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-border bg-muted/60">
              <BrandMark icon={e.icon} size={18} />
            </span>
            <span className="min-w-0">
              <span className="block text-[13.5px] font-medium">{e.label}</span>
              <span className="block truncate text-[11.5px] text-muted-foreground">
                Backs up as: {e.rights}
              </span>
            </span>
          </button>
        );
      })}
    </div>
  );
}

export function AddDatabaseDialog({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const s = usePyo();
  const teamId = s.currentTeamId;
  const clusters = s.clusters.filter((c) => c.teamId === teamId);
  const [engine, setEngine] = useState<DbEngine | null>(null);
  const [source, setSource] = useState<"service" | "external">("service");
  const [serviceId, setServiceId] = useState("");
  const [f, setF] = useState({
    name: "",
    host: "",
    port: 0,
    database: "",
    adminUser: "",
    adminPassword: "",
    clusterId: clusters[0]?.id ?? "",
    enabled: true,
    schedule: "daily" as "daily" | "weekly",
    hour: 2,
    keep: 14,
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const set = (p: Partial<typeof f>) => setF((x) => ({ ...x, ...p }));
  const e = engine ? ENGINES[engine] : null;
  const services = engine
    ? s.services.filter(
        (a) =>
          a.teamId === teamId &&
          engineOf(a) === engine &&
          a.currentDeploymentId,
      )
    : [];

  const reset = () => {
    setEngine(null);
    setServiceId("");
    setError("");
    setBusy(false);
  };
  const close = () => {
    reset();
    onClose();
  };

  const pick = (k: DbEngine) => {
    const info = ENGINES[k];
    setEngine(k);
    const svcs = s.services.filter(
      (a) => a.teamId === teamId && engineOf(a) === k && a.currentDeploymentId,
    );
    setSource(svcs.length ? "service" : "external");
    set({
      port: info.port,
      adminUser: info.admin,
      database: "",
      name: "",
      host: "",
    });
    if (svcs[0]) fillFromService(svcs[0], k);
  };

  /** Fill in a service's address, and its admin user and database from its env. */
  function fillFromService(a: Application, k: DbEngine = engine!) {
    const info = ENGINES[k];
    const ns =
      s.projects.find((p) => p.id === a.projectId)?.namespace ?? "default";
    const env = (key?: string) =>
      key ? a.env.find((x) => x.key === key && !x.secret)?.value : undefined;
    setServiceId(a.id);
    set({
      name: a.name,
      host: `${a.name}.${ns}.svc.cluster.local`,
      port: a.ports[0]?.containerPort ?? info.port,
      adminUser: env(info.env.user) ?? info.admin,
      database: env(info.env.database) ?? "",
      clusterId: a.clusterId,
      adminPassword: "",
    });
  }

  const revealPassword = async () => {
    const a = s.services.find((x) => x.id === serviceId);
    const v = a?.env.find((x) => x.key === e?.env.password);
    if (!a || !v) return;
    const pw = await api.revealSecret(a.id, v.id);
    if (pw !== null) set({ adminPassword: pw });
  };

  const submit = async () => {
    if (!engine) return;
    setBusy(true);
    setError("");
    try {
      await api.addDatabaseBackup({
        teamId,
        engine,
        serviceId: source === "service" ? serviceId || undefined : undefined,
        ...f,
        port: Number(f.port),
      });
      close();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };

  const canSubmit =
    !!engine &&
    f.name.trim() &&
    f.host.trim() &&
    f.port > 0 &&
    f.clusterId &&
    (!e?.needsDatabase || f.database.trim()) &&
    !busy;

  return (
    <Dialog
      open={open}
      onClose={close}
      width="max-w-xl"
      title={e ? `Back up ${e.label}` : "Add a database"}
      description={
        e
          ? `Used once to create a backup user. Not stored.`
          : undefined
      }
      onSubmit={
        engine
          ? (ev) => {
              ev.preventDefault();
              if (canSubmit) void submit();
            }
          : undefined
      }
      footer={
        engine ? (
          <>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={reset}
              disabled={busy}
            >
              <ArrowLeftIcon size={13} /> Engine
            </Button>
            <Button type="submit" size="sm" disabled={!canSubmit}>
              {busy ? "Checking the database…" : "Create backup user and save"}
            </Button>
          </>
        ) : null
      }
    >
      {!engine ? (
        <EnginePicker onPick={pick} />
      ) : (
        <div className="space-y-4">
          <Segmented
            label="Where it is"
            value={source}
            onChange={(v) => {
              setSource(v);
              if (v === "external") {
                setServiceId("");
                set({
                  host: "",
                  name: "",
                  database: "",
                  port: e!.port,
                  adminUser: e!.admin,
                });
              } else if (services[0]) fillFromService(services[0]);
            }}
            options={[
              {
                value: "service",
                label: `Pyo service${services.length ? ` (${services.length})` : ""}`,
                disabled: !services.length,
              },
              { value: "external", label: "Another host" },
            ]}
          />
          {source === "service" ? (
            <Field label="Service">
              {(p) => (
                <Select
                  {...p}
                  value={serviceId}
                  onChange={(ev) => {
                    const a = services.find((x) => x.id === ev.target.value);
                    if (a) fillFromService(a);
                  }}
                >
                  {services.map((a) => (
                    <option key={a.id} value={a.id}>
                      {s.projects.find((p) => p.id === a.projectId)?.name} /{" "}
                      {a.name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          ) : null}
          <div className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_110px]">
            <Field label="Host">
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={f.host}
                  onChange={(ev) => set({ host: ev.target.value })}
                  placeholder="db.example.com"
                />
              )}
            </Field>
            <Field label="Port">
              {(p) => (
                <Input
                  {...p}
                  mono
                  inputMode="numeric"
                  value={f.port || ""}
                  onChange={(ev) =>
                    set({ port: Number(ev.target.value.replace(/\D/g, "")) })
                  }
                />
              )}
            </Field>
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field
              label="Database"
              hint={e!.needsDatabase ? undefined : "Empty: all of them."}
            >
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={f.database}
                  onChange={(ev) => set({ database: ev.target.value })}
                  placeholder={e!.needsDatabase ? "app" : "all"}
                />
              )}
            </Field>
            <Field label="Name in Pyo">
              {(p) => (
                <Input
                  {...p}
                  value={f.name}
                  onChange={(ev) => set({ name: ev.target.value })}
                  placeholder="orders-db"
                />
              )}
            </Field>
            <Field label="Admin user">
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={f.adminUser}
                  onChange={(ev) => set({ adminUser: ev.target.value })}
                />
              )}
            </Field>
            <Field
              label="Admin password"
              hint={
                source === "service" && serviceId ? (
                  <button
                    type="button"
                    className="font-medium text-foreground hover:text-primary"
                    onClick={() => void revealPassword()}
                  >
                    Use the service's password (logged)
                  </button>
                ) : (
                  "Used once to create the backup user."
                )
              }
            >
              {(p) => (
                <Input
                  {...p}
                  mono
                  type="password"
                  value={f.adminPassword}
                  onChange={(ev) => set({ adminPassword: ev.target.value })}
                />
              )}
            </Field>
          </div>
          {clusters.length > 1 ? (
            <Field
              label="Run backups from"
              hint="The cluster must reach the host."
            >
              {(p) => (
                <Select
                  {...p}
                  value={f.clusterId}
                  onChange={(ev) => set({ clusterId: ev.target.value })}
                >
                  {clusters.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          ) : null}
          <div className="space-y-3 rounded-xl border border-border p-3">
            <Switch
              checked={f.enabled}
              onChange={(enabled) => set({ enabled })}
              label="Schedule"
            />
            {f.enabled ? (
              <div className="grid gap-3 sm:grid-cols-3">
                <Segmented
                  label="How often"
                  value={f.schedule}
                  onChange={(schedule) => set({ schedule })}
                  options={[
                    { value: "daily", label: "Daily" },
                    { value: "weekly", label: "Weekly" },
                  ]}
                />
                <Select
                  aria-label="Hour (UTC)"
                  value={String(f.hour)}
                  onChange={(ev) => set({ hour: Number(ev.target.value) })}
                >
                  {Array.from({ length: 24 }, (_, h) => (
                    <option key={h} value={h}>
                      {String(h).padStart(2, "0")}:00 UTC
                    </option>
                  ))}
                </Select>
                <Select
                  aria-label="Keep"
                  value={String(f.keep)}
                  onChange={(ev) => set({ keep: Number(ev.target.value) })}
                >
                  {[7, 14, 30, 90].map((n) => (
                    <option key={n} value={n}>
                      Keep {n}
                    </option>
                  ))}
                </Select>
              </div>
            ) : null}
          </div>
          {error ? <Note>{error}</Note> : null}
        </div>
      )}
    </Dialog>
  );
}

const Note = ({ children }: { children: ReactNode }) => (
  <p
    role="alert"
    className="rounded-lg border border-destructive/30 bg-destructive/[0.06] px-3 py-2 text-[12.5px] text-destructive"
  >
    {children}
  </p>
);
