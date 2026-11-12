import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import {
  AvailabilityField,
  ResourceSummary,
  SIZE_SPECS,
  SizeFields,
  customError,
  instancesOf,
  maxReadReplicas,
  replicasValid,
} from "~/components/pyo/Databases";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Stepper, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { currentSlug, teamPath } from "~/lib/pyo/paths";
import { DB_SECTION, api } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Database } from "~/lib/pyo/types";
import { useDatabase } from "./DatabaseLayout";

export function DatabaseSettings() {
  const d = useDatabase();
  return d ? <Settings key={d.id} d={d} /> : null;
}

function Settings({ d }: { d: Database }) {
  const navigate = useNavigate();
  const [size, setSize] = useState<string>(d.size);
  const [cpu, setCpu] = useState(d.cpuMillis ?? SIZE_SPECS[d.size]?.cpu ?? 500);
  const [mem, setMem] = useState(d.memoryMi ?? SIZE_SPECS[d.size]?.mem ?? 1024);
  const custom = size === "custom";
  const sizeError = custom ? customError(d.engine, cpu, mem) : "";
  const [ha, setHa] = useState(d.instances > 1);
  const [replicas, setReplicas] = useState(
    d.instances > 1 ? d.instances - 1 : 2,
  );
  const instances = instancesOf(ha, replicas);
  const [storage, setStorage] = useState(d.storageGi);
  const [pooler, setPooler] = useState(d.pooler);
  const [rest, setRest] = useState(d.rest);
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const dirty =
    size !== d.size ||
    (custom && (cpu !== d.cpuMillis || mem !== d.memoryMi)) ||
    instances !== d.instances ||
    storage !== d.storageGi ||
    pooler !== d.pooler ||
    rest !== d.rest;
  const storageError =
    storage < d.storageGi
      ? `Storage only grows: ${d.storageGi} GiB now.`
      : storage > 1000
        ? "At most 1000 GiB."
        : "";
  const save = async () => {
    setSaving(true);
    const r = await api.updateDatabase(d.id, {
      size,
      ...(custom ? { cpuMillis: cpu, memoryMi: mem } : {}),
      instances,
      storageGi: storage,
      ...(d.engine === "postgres"
        ? { pooler }
        : d.engine === "valkey"
          ? { rest }
          : {}),
    });
    setSaving(false);
    if ("error" in r)
      toast({ tone: "error", title: "Not saved", description: r.error });
    else
      toast({
        title: "Saved",
        description: d.connections.length
          ? "Connected services get changes on their next deploy."
          : undefined,
      });
  };
  return (
    <div className="space-y-6">
      <Panel title="Resources">
        <div className="space-y-5 p-4">
          <AvailabilityField
            engine={d.engine}
            ha={ha}
            replicas={replicas}
            atLeast={d.engine === "mysql" ? d.instances : 1}
            onChange={(h, r) => {
              setHa(h);
              setReplicas(r);
            }}
          />
          <SizeFields
            engine={d.engine}
            size={size}
            cpu={cpu}
            mem={mem}
            onChange={(v) => {
              setSize(v.size);
              setCpu(v.cpu);
              setMem(v.mem);
            }}
          />
          <div>
            <Field
              label="Storage (GiB)"
              hint={storageError ? undefined : "Per instance. Only grows."}
              error={storageError || undefined}
            >
              {(p) => (
                <Input
                  {...p}
                  type="number"
                  min={d.storageGi}
                  max={1000}
                  value={storage}
                  onChange={(e) => setStorage(Number(e.target.value))}
                />
              )}
            </Field>
          </div>
        </div>
        {d.engine === "mysql" ? null : (
        <div className="border-t border-border p-4">
          {d.engine === "postgres" ? (
            <Switch
              checked={pooler}
              onChange={setPooler}
              label="Connection pooler"
              description="Many service connections share a few database connections."
            />
          ) : (
            <Switch
              checked={rest}
              onChange={setRest}
              label="REST API"
              description="For serverless and edge code."
            />
          )}
        </div>
        )}
        <div className="border-t border-border p-4">
          <ResourceSummary
            engine={d.engine}
            cpu={custom ? cpu : (SIZE_SPECS[size]?.cpu ?? 0)}
            mem={custom ? mem : (SIZE_SPECS[size]?.mem ?? 0)}
            storageGi={storage}
            instances={instances}
            readReplicas={d.readReplicas?.instances ?? 0}
            pooler={pooler}
            rest={rest}
          />
        </div>
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={
              !dirty ||
              !!storageError ||
              !!sizeError ||
              !replicasValid(d.engine, ha, replicas) ||
              saving
            }
            onClick={() => void save()}
          >
            {saving ? "Saving…" : "Save changes"}
          </Button>
        </div>
      </Panel>
      {maxReadReplicas(d.engine) ? <ReadReplicas d={d} /> : null}
      <Panel title="Danger zone" tone="danger">
        <div className="space-y-4 p-4">
          <Switch
            checked={d.deletionProtection}
            onChange={async (v) => {
              const r = await api.updateDatabase(d.id, {
                deletionProtection: v,
              });
              if ("error" in r)
                toast({
                  tone: "error",
                  title: "Not saved",
                  description: r.error,
                });
            }}
            label="Deletion protection"
            description="While on, it can't be deleted."
          />
          <div className="flex flex-wrap items-center justify-between gap-3 border-t border-border pt-4">
            <div>
              <p className="text-[13px] font-medium">Delete database</p>
              <p className="text-[12px] text-muted-foreground">
                {d.deletionProtection
                  ? "Turn deletion protection off first."
                  : d.connections.length
                    ? "Disconnect its services first."
                    : "Its data is deleted."}
              </p>
            </div>
            <Button
              variant="secondary"
              size="sm"
              className="text-destructive"
              disabled={d.deletionProtection || d.connections.length > 0}
              onClick={() => setDeleting(true)}
            >
              Delete
            </Button>
          </div>
        </div>
      </Panel>
      <ConfirmDialog
        open={deleting}
        onClose={() => setDeleting(false)}
        title={`Delete ${d.name}?`}
        description="Its data is deleted and can't be brought back."
        confirmText={d.name}
        confirmLabel="Delete database"
        onConfirm={async () => {
          const r = await api.deleteDatabase(d.id);
          if ("error" in r)
            return toast({
              tone: "error",
              title: "Not deleted",
              description: r.error,
            });
          toast({ title: `Deleting ${d.name}` });
          void navigate({
            href: teamPath(currentSlug(), `/${DB_SECTION[d.engine]}`),
          });
        }}
      />
    </div>
  );
}

/** Dedicated read replicas: their own pooler (Postgres) and a read URL. 0: none. */
function ReadReplicas({ d }: { d: Database }) {
  const max = maxReadReplicas(d.engine);
  const current = d.readReplicas?.instances ?? 0;
  const [count, setCount] = useState(current);
  const [saving, setSaving] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const pg = d.engine === "postgres";
  const save = async () => {
    setSaving(true);
    const r = await api.updateDatabase(d.id, { readReplicas: count });
    setSaving(false);
    if ("error" in r) toast({ tone: "error", title: "Not saved", description: r.error });
    // An API older than read replicas ignores the field and still says ok.
    else if (r.data.readReplicas !== count)
      toast({ tone: "error", title: "Not saved", description: "The running API doesn't support read replicas yet." })
    else
      toast({
        title: "Saved",
        description: d.connections.length ? "Connected services get changes on their next deploy." : undefined,
      });
  };
  return (
    <Panel title="Read replicas">
      <div className="flex flex-wrap items-center justify-between gap-4 p-4">
        <p className="min-w-0 flex-1 text-[12.5px] text-muted-foreground">
          {pg
            ? "Serve reads only, through DATABASE_URL_READ, and never become the primary."
            : "Serve reads only, which REDIS_URL sends to them, and never become the primary."}
        </p>
        <Stepper aria-label="Read replicas" min={0} max={max} value={count} onChange={setCount} className="w-[132px]" />
      </div>
      <div className="flex justify-end border-t border-border px-4 py-3">
        <Button
          size="sm"
          disabled={count === current || saving}
          onClick={() => (count === 0 && current > 0 ? setConfirming(true) : void save())}
        >
          {saving ? "Saving…" : "Save changes"}
        </Button>
      </div>
      <ConfirmDialog
        open={confirming}
        onClose={() => setConfirming(false)}
        title="Remove the read replicas?"
        description={pg ? "Connected services lose DATABASE_URL_READ on their next deploy." : "Reads go back to the primary."}
        confirmLabel="Remove read replicas"
        onConfirm={() => {
          setConfirming(false);
          void save();
        }}
      />
    </Panel>
  );
}
