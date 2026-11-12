import { DownloadSimpleIcon, WarningIcon } from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import {
  AvailabilityField,
  Choice,
  Choices,
  ENGINE_ICON,
  GroupLabel,
  ResourceSummary,
  SIZE_SPECS,
  SizeFields,
  customError,
  fitReplicas,
  fitSize,
  instancesOf,
  maxReadReplicas,
  replicasValid,
} from "~/components/pyo/Databases";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Input, Select, Stepper, Switch } from "~/components/ui/Field";
import { Callout, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { DNS_LABEL } from "~/lib/pyo/format";
import { nameConflict } from "~/lib/pyo/databaseNames";
import { currentSlug, teamPath } from "~/lib/pyo/paths";
import { DB_SECTION, api, clustersIn, databaseHref, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Database } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// A new, empty Postgres, MySQL or KV database, on one of the team's
// connected clusters (its first database there sets databases up first).
// Each engine has its page (/postgres/new, /mysql/new, /kv/new); picking
// another engine goes to its page with the name and cluster kept. Imports
// have their own page (/import), which shares the fields here.
export const newDatabaseRoute = (engine: Database["engine"]) => ({
  head: () => ({ meta: seo({ title: "New database" }) }),
  component: () => <NewDatabase engine={engine} />,
});

/** The name and cluster typed so far, carried to another engine's page. */
let carried: { name: string; clusterId: string } | null = null;

type Engine = Database["engine"];

const ENGINES: { value: Engine; label: string; detail: string }[] = [
  { value: "postgres", label: "Postgres", detail: "Relational SQL database." },
  { value: "mysql", label: "MySQL", detail: "Relational SQL database." },
  { value: "valkey", label: "KV", detail: "Redis-compatible key-value store." },
];

/** Whether the signed-in user is an owner or admin of the current team (imports need it). */
export function useTeamAdmin() {
  const s = usePyo();
  const role = s.members.find((m) => m.teamId === s.currentTeamId && m.email === s.user.email)?.role;
  return role === "owner" || role === "admin";
}

/** What's wrong with a new database's name ("" when fine). `team`: the team's databases. */
export function databaseNameError(engine: Engine, name: string, team: Database[]) {
  if (!DNS_LABEL.test(name) || name.length > 40) return "Lowercase letters, digits and dashes, up to 40.";
  if (team.some((d) => d.name === name)) return "The team has a database with this name.";
  return nameConflict(engine, name, team);
}

/** A new database's fields, as the form edits them. */
export function useDatabaseDraft(engine: Engine, init?: { name: string; clusterId: string } | null) {
  const s = usePyo();
  const ready = clustersIn(s).filter((c) => c.status === "connected" || c.status === "degraded");
  const [name, setName] = useState(() => init?.name ?? "");
  const [clusterId, setClusterId] = useState(() => init?.clusterId ?? "");
  const [size, setSize] = useState(() => fitSize(engine, "small"));
  const [cpu, setCpu] = useState(250);
  const [mem, setMem] = useState(1024);
  const [storage, setStorage] = useState(10);
  const [readReplicas, setReadReplicas] = useState(0);
  const [ha, setHa] = useState(false);
  const [replicas, setReplicas] = useState(2);
  const [pooler, setPooler] = useState(true);
  const [rest, setRest] = useState(false);
  const cluster = clusterId || ready[0]?.id || "";
  const nameError = name ? databaseNameError(engine, name, s.databases.filter((d) => d.teamId === s.currentTeamId)) : "";
  const custom = size === "custom";
  const spec = custom ? { cpu, mem, storage } : SIZE_SPECS[size];
  const sizeError = custom ? customError(engine, cpu, mem) || (storage >= 1 && storage <= 1000 ? "" : "Storage is 1 to 1000 GiB.") : "";
  const maxRead = maxReadReplicas(engine);
  const valid = !!name && !nameError && !sizeError && !!cluster && replicasValid(engine, ha, replicas);
  return {
    ready,
    name,
    setName,
    clusterId,
    setClusterId,
    cluster,
    size,
    cpu,
    mem,
    storage,
    setStorage,
    readReplicas,
    setReadReplicas,
    ha,
    replicas,
    pooler,
    setPooler,
    rest,
    setRest,
    nameError,
    custom,
    spec,
    sizeError,
    maxRead,
    valid,
    setSize: (v: { size: string; cpu: number; mem: number }) => {
      setSize(v.size);
      setCpu(v.cpu);
      setMem(v.mem);
    },
    setAvailability: (h: boolean, r: number) => {
      setHa(h);
      setReplicas(r);
    },
    /** Keeps what fits when the engine changes: KV and MySQL have fewer replicas; MySQL needs 1 GiB. */
    fitEngine: (next: Engine) => {
      setReplicas((r) => fitReplicas(next, r));
      setSize((v) => fitSize(next, v));
      setMem((m) => Math.max(m, next === "mysql" ? 1024 : 0));
    },
    /** The fields createDatabase takes. */
    fields: () => ({
      name,
      clusterId: cluster,
      engine,
      size,
      instances: instancesOf(ha, replicas),
      ...(custom ? { cpuMillis: cpu, memoryMi: mem, storageGi: storage } : {}),
      ...(readReplicas ? { readReplicas: Math.min(readReplicas, maxRead) } : {}),
      ...(engine === "postgres" ? { pooler } : engine === "valkey" ? { rest } : {}),
    }),
  };
}

export type DatabaseDraft = ReturnType<typeof useDatabaseDraft>;

/** Name and cluster, side by side. */
export function NameAndCluster({ engine, draft: f }: { engine: Engine; draft: DatabaseDraft }) {
  return (
    <div className="grid items-start gap-4 sm:grid-cols-2">
      <Field label="Name" error={f.nameError || undefined}>
        {(p) => (
          <Input {...p} mono value={f.name} placeholder={engine === "valkey" ? "kv" : "main"} onChange={(e) => f.setName(e.target.value.toLowerCase())} />
        )}
      </Field>
      <Field label="Cluster">
        {(p) => (
          <Select {...p} value={f.cluster} onChange={(e) => f.setClusterId(e.target.value)} disabled={!f.ready.length}>
            {f.ready.length ? (
              f.ready.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))
            ) : (
              <option value="" hidden>
                None connected
              </option>
            )}
          </Select>
        )}
      </Field>
    </div>
  );
}

/** No connected cluster to put it on. */
export function NoCluster() {
  return (
    <Callout tone="warning" icon={WarningIcon}>
      <span className="flex flex-wrap items-center justify-between gap-2">
        No connected cluster yet.
        <ButtonLink href="/clusters" size="xs" variant="secondary">
          Clusters
        </ButtonLink>
      </span>
    </Callout>
  );
}

/** Size (with storage when custom) and availability. */
export function SizeAndAvailability({ engine, draft: f }: { engine: Engine; draft: DatabaseDraft }) {
  return (
    <>
      <SizeFields engine={engine} size={f.size} cpu={f.cpu} mem={f.mem} onChange={f.setSize} />
      {f.custom ? (
        <Field label="Storage (GiB)" hint="Per instance. Only grows later." error={f.storage >= 1 && f.storage <= 1000 ? undefined : "1 to 1000 GiB."}>
          {(p) => <Input {...p} type="number" min={1} max={1000} value={f.storage} onChange={(e) => f.setStorage(Number(e.target.value))} />}
        </Field>
      ) : null}
      <AvailabilityField engine={engine} ha={f.ha} replicas={f.replicas} onChange={f.setAvailability} />
    </>
  );
}

function NewDatabase({ engine }: { engine: Engine }) {
  const navigate = useNavigate();
  const admin = useTeamAdmin();
  const f = useDatabaseDraft(engine, carried);
  useEffect(() => {
    carried = null;
  }, []);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const canCreate = f.valid && !saving;

  const create = async () => {
    setSaving(true);
    setError("");
    const r = await api.createDatabase(f.fields());
    setSaving(false);
    if ("error" in r) return setError(r.error);
    toast({ title: `Creating ${f.name}`, description: "It's ready in a minute or two." });
    void navigate({ href: teamPath(currentSlug(), databaseHref(engine, r.data.id)) });
  };

  return (
    <Page width="max-w-2xl">
      <PageHeader
        title="New database"
        actions={
          admin && engine !== "valkey" ? (
            <ButtonLink href={`/import?engine=${engine}`} size="sm" variant="secondary">
              <DownloadSimpleIcon size={14} />
              Import
            </ButtonLink>
          ) : null
        }
      />
      <Panel>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            if (canCreate) void create();
          }}
        >
          <div className="space-y-5 p-5">
            <div>
              <GroupLabel>Engine</GroupLabel>
              <Choices label="Engine" cols="sm:grid-cols-3">
                {ENGINES.map((x) => (
                  <Choice
                    key={x.value}
                    icon={ENGINE_ICON[x.value]}
                    label={x.label}
                    detail={x.detail}
                    selected={engine === x.value}
                    onSelect={() => {
                      if (x.value === engine) return;
                      carried = { name: f.name, clusterId: f.clusterId };
                      void navigate({ href: teamPath(currentSlug(), `/${DB_SECTION[x.value]}/new`), replace: true });
                      f.fitEngine(x.value);
                    }}
                  />
                ))}
              </Choices>
            </div>
            <NameAndCluster engine={engine} draft={f} />
            {!f.ready.length ? <NoCluster /> : null}
            <SizeAndAvailability engine={engine} draft={f} />
          </div>
          {engine === "mysql" ? null : (
            <div className="space-y-5 border-t border-border p-5">
              <Field label="Read replicas" hint="Serve reads only and never become the primary.">
                {(p) => (
                  <Stepper {...p} min={0} max={f.maxRead} value={Math.min(f.readReplicas, f.maxRead)} onChange={f.setReadReplicas} className="w-[140px]" />
                )}
              </Field>
              {engine === "postgres" ? (
                <Switch checked={f.pooler} onChange={f.setPooler} label="Connection pooler" description="Many service connections share a few database connections." />
              ) : (
                <Switch checked={f.rest} onChange={f.setRest} label="REST API" description="For serverless and edge code." />
              )}
            </div>
          )}
          <div className="border-t border-border p-5">
            <ResourceSummary
              engine={engine}
              cpu={f.spec?.cpu ?? 0}
              mem={f.spec?.mem ?? 0}
              storageGi={f.spec?.storage ?? 0}
              instances={instancesOf(f.ha, f.replicas)}
              readReplicas={Math.min(f.readReplicas, f.maxRead)}
              pooler={f.pooler}
              rest={f.rest}
            />
          </div>
          <div className="flex flex-wrap items-center justify-end gap-2 border-t border-border px-5 py-3">
            {error ? (
              <p className="mr-auto text-[12.5px] text-destructive" role="alert">
                {error}
              </p>
            ) : null}
            <ButtonLink href={`/${DB_SECTION[engine]}`} variant="ghost" size="sm">
              Cancel
            </ButtonLink>
            <Button type="submit" size="sm" disabled={!canCreate}>
              {saving ? "Creating…" : "Create"}
            </Button>
          </div>
        </form>
      </Panel>
    </Page>
  );
}
