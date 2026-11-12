import { PlusIcon } from "@phosphor-icons/react";
import {
  AvailabilityField,
  Choice,
  Choices,
  ENGINE_ICON,
  ENGINE_LABEL,
  fitReplicas,
  instancesOf,
  sizesFor,
} from "~/components/pyo/Databases";
import { Field, Input, Select, Switch } from "~/components/ui/Field";
import { fetchDatabaseExtensions } from "~/lib/pyo/backend";
import { DNS_LABEL } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import { api, usePyo } from "~/lib/pyo/store";
import type { Database, DatabaseExtension, TemplateDatabase } from "~/lib/pyo/types";

// A template's databases (WordPress: MySQL; Nextcloud: Postgres and KV):
// for each, one of the team's on the service's cluster, or a new one made
// just before the service. Either way the service is connected to it in the
// same request that creates it, so its first deploy has the variables.
//
// Extensions the template needs: Pyo turns on the ones it manages, with a
// new database or, once people agree, on an existing one before connecting.
// The others ("additional") the app's own role creates, so they pass through.

export type DbChoice = {
  mode: "new" | "existing";
  existingId: string;
  /** A new one's name; follows the service's until edited. */
  name: string;
  named: boolean;
  size: string;
  ha: boolean;
  replicas: number;
  /** An existing database: turn on the extensions it's missing. */
  enable: boolean;
};

export const initialDbChoice = (req: TemplateDatabase): DbChoice => ({
  mode: "new",
  existingId: "",
  name: "",
  named: false,
  size: sizesFor(req.engine)[0].value,
  ha: false,
  replicas: fitReplicas(req.engine, 2),
  enable: false,
});

/** Pyo's extension catalog (shared with the Extensions tab); null until it's loaded. */
export function useExtensionCatalog(): DatabaseExtension[] | null {
  const q = useQuery("database-extensions", fetchDatabaseExtensions);
  return q.status === "success" ? q.data.extensions : null;
}

/** The ones Pyo turns on, of those the template needs. */
export const managedOf = (req: TemplateDatabase, catalog: DatabaseExtension[] | null) =>
  (req.extensions ?? []).filter((n) => catalog?.some((e) => e.name === n && e.section === "managed"));

/** Of those, the ones an existing database doesn't have on. */
export const missingOn = (req: TemplateDatabase, db: Database | undefined, catalog: DatabaseExtension[] | null) =>
  db ? managedOf(req, catalog).filter((n) => !db.extensions?.some((x) => x.name === n && x.enabled)) : [];

/** "a", "a and b", "a, b and c". */
const list = (xs: string[]) => (xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs[xs.length - 1]}`);

/** The new database's name: the service's, then "-db" ("-kv" for KV). */
export const dbName = (c: DbChoice, service: string, engine: Database["engine"]) =>
  c.named ? c.name : `${(service || "app").slice(0, 37)}-${engine === "valkey" ? "kv" : "db"}`;

/** The team's databases of the engine on the cluster. */
function candidatesOf(s: ReturnType<typeof usePyo>, engine: Database["engine"], clusterId: string) {
  return s.databases.filter(
    (d) =>
      d.teamId === s.currentTeamId &&
      d.engine === engine &&
      d.clusterId === clusterId &&
      d.state !== "deleting",
  );
}

/** What's wrong with each choice, if anything ("" when nothing). */
export function useDbProblems(
  reqs: TemplateDatabase[],
  choices: DbChoice[],
  clusterId: string,
  service: string,
  catalog: DatabaseExtension[] | null,
): string[] {
  const s = usePyo();
  const names = reqs.map((r, i) => (choices[i] ? dbName(choices[i], service, r.engine) : ""));
  return reqs.map((req, i) => {
    const c = choices[i];
    if (!c) return "";
    if (c.mode === "existing") {
      const found = candidatesOf(s, req.engine, clusterId).find((d) => d.id === c.existingId);
      if (!found) return `Pick a ${ENGINE_LABEL[req.engine]} database.`;
      const missing = missingOn(req, found, catalog);
      return missing.length && !c.enable ? `Turn on ${list(missing)}, or pick another database.` : "";
    }
    const name = names[i];
    if (!DNS_LABEL.test(name) || name.length > 40) return "Lowercase letters, digits and dashes, up to 40.";
    if (s.databases.some((d) => d.teamId === s.currentTeamId && d.name === name)) return "The team has a database with this name.";
    if (names.some((n, j) => j !== i && choices[j]?.mode === "new" && n === name)) return "Give each new database its own name.";
    return "";
  });
}

/** One line for the block's header. */
export function dbSummary(req: TemplateDatabase, c: DbChoice, service: string, existing: Database | undefined) {
  const label = ENGINE_LABEL[req.engine];
  if (c.mode === "existing") return existing ? `Uses ${existing.name}` : `Pick a ${label} database`;
  return `New ${label} database ${dbName(c, service, req.engine)}`;
}

export function TemplateDatabaseFields({
  req,
  clusterId,
  clusterName,
  service,
  value: c,
  onChange,
  problem,
  catalog,
}: {
  req: TemplateDatabase;
  clusterId: string;
  clusterName: string;
  service: string;
  value: DbChoice;
  onChange: (c: DbChoice) => void;
  /** Shown after a first Deploy. */
  problem: string;
  catalog: DatabaseExtension[] | null;
}) {
  const s = usePyo();
  const engine = req.engine;
  const label = ENGINE_LABEL[engine];
  const candidates = candidatesOf(s, engine, clusterId);
  const set = (patch: Partial<DbChoice>) => onChange({ ...c, ...patch });
  const chosen = candidates.some((d) => d.id === c.existingId) ? c.existingId : (candidates[0]?.id ?? "");
  const picked = candidates.find((d) => d.id === chosen);
  const missing = c.mode === "existing" ? missingOn(req, picked, catalog) : [];
  const restarts = catalog?.some((e) => e.restart && missing.includes(e.name));
  const managed = managedOf(req, catalog);
  return (
    <div className="space-y-4">
      <Choices label="Database">
        <Choice
          icon={PlusIcon}
          label="New database"
          detail={`${label}, made with it.`}
          selected={c.mode === "new"}
          onSelect={() => set({ mode: "new" })}
        />
        <Choice
          icon={ENGINE_ICON[engine]}
          label="Existing database"
          detail={candidates.length ? `${candidates.length} on ${clusterName}.` : `None on ${clusterName} yet.`}
          selected={c.mode === "existing"}
          onSelect={() => set({ mode: "existing", existingId: chosen })}
        />
      </Choices>
      {c.mode === "existing" ? (
        candidates.length ? (
          <>
            <Field label={`${label} database`} error={problem || undefined}>
              {(p) => (
                <Select {...p} value={chosen} onChange={(e) => set({ existingId: e.target.value })}>
                  {candidates.map((d) => (
                    <option key={d.id} value={d.id}>
                      {d.name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
            {missing.length && picked ? (
              <div className="rounded-md border border-border px-3 py-2.5">
                <Switch
                  checked={c.enable}
                  onChange={(v) => set({ enable: v })}
                  label={`Turn on ${list(missing)}`}
                  description={`${picked.name} doesn't have ${missing.length > 1 ? "them" : "it"} on.${
                    restarts
                      ? ` Turning ${missing.length > 1 ? "them" : "it"} on restarts this database. ${
                          picked.instances > 1 ? "Replicas restart first and the primary hands over." : "It's unavailable for a moment."
                        }`
                      : ""
                  }`}
                />
              </div>
            ) : null}
          </>
        ) : (
          <p className="text-[12.5px] text-muted-foreground">The team has no {label} database on {clusterName}.</p>
        )
      ) : (
        <>
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field label="Name" error={problem || undefined}>
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={dbName(c, service, engine)}
                  onChange={(e) => set({ name: e.target.value.toLowerCase(), named: true })}
                />
              )}
            </Field>
            <Field label="Size" hint="Per instance.">
              {(p) => (
                <Select {...p} value={c.size} onChange={(e) => set({ size: e.target.value })}>
                  {sizesFor(engine).map((x) => (
                    <option key={x.value} value={x.value}>
                      {x.label} · {x.detail}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          </div>
          <AvailabilityField engine={engine} ha={c.ha} replicas={c.replicas} onChange={(ha, replicas) => set({ ha, replicas })} />
        </>
      )}
      <p className="text-[12px] text-muted-foreground">
        Connected on deploy: {Object.keys(req.env).join(", ")}.
        {c.mode === "new" && managed.length ? ` Made with ${list(managed)} on.` : ""}
      </p>
    </div>
  );
}

/**
 * Makes the new database, if one is chosen, with the extensions Pyo manages
 * on; for an existing one, turns on those it's missing (when agreed). Its id,
 * or the API's error.
 */
export async function prepareDatabase(
  req: TemplateDatabase,
  c: DbChoice,
  clusterId: string,
  service: string,
  existing: Database | undefined,
  catalog: DatabaseExtension[],
): Promise<{ id: string } | { error: string }> {
  if (c.mode === "existing") {
    // Made by an earlier try (so it has them), and not listed yet.
    if (!existing) return c.existingId ? { id: c.existingId } : { error: `Pick a ${ENGINE_LABEL[req.engine]} database.` };
    const missing = missingOn(req, existing, catalog);
    if (missing.length) {
      if (!c.enable) return { error: `Turn on ${list(missing)}, or pick another database.` };
      const on = (existing.extensions ?? []).filter((x) => x.enabled).map((x) => x.name);
      const r = await api.setDatabaseExtensions(existing.id, [...on, ...missing]);
      if ("error" in r) return r;
    }
    return { id: existing.id };
  }
  const extensions = managedOf(req, catalog);
  const r = await api.createDatabase({
    name: dbName(c, service, req.engine),
    clusterId,
    engine: req.engine,
    size: c.size,
    instances: instancesOf(c.ha, c.replicas),
    ...(extensions.length ? { extensions } : {}),
  });
  return "error" in r ? r : { id: r.data.id };
}
