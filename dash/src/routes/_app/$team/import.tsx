import { CaretDownIcon, CheckCircleIcon, DownloadSimpleIcon, WarningIcon } from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { z } from "zod";
import { DumpField, UrlField, fileSize, resolveSource, sourceReady, useUploadSignal, type SourceInput } from "~/components/pyo/database/Import";
import { NameAndCluster, NoCluster, SizeAndAvailability, useDatabaseDraft, useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import { ENGINE_ICON, ENGINE_LABEL, GroupLabel, SIZES } from "~/components/pyo/Databases";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Segmented, Select } from "~/components/ui/Field";
import { Callout, EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { newId } from "~/lib/pyo/format";
import { checkImport, importing, importInto, importNew, type ImportChecked, type ImportInto } from "~/lib/pyo/imports";
import { currentSlug, teamPath } from "~/lib/pyo/paths";
import { DB_SECTION, databaseHref, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Database } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// Importing a Postgres or MySQL database, on one page: the engine, where
// from (a connection URL or a dump file) and where to (a new database, or
// an empty one of the team's). ?engine= and ?into=<id> preselect. Once
// started, the database's overview shows how it's going.

export const Route = createFileRoute("/_app/$team/import")({
  validateSearch: z.object({
    engine: z.enum(["postgres", "mysql"]).optional().catch(undefined),
    into: z.string().optional().catch(undefined),
  }),
  head: () => ({ meta: seo({ title: "Import a database" }) }),
  component: ImportPage,
});

type Engine = "postgres" | "mysql";

function ImportPage() {
  const admin = useTeamAdmin();
  return (
    <Page width="max-w-2xl">
      <PageHeader title="Import a database" />
      {admin ? (
        <ImportForm />
      ) : (
        <Panel>
          <EmptyState icon={DownloadSimpleIcon} title="Only owners and admins can import." />
        </Panel>
      )}
    </Page>
  );
}

/** Why a database can't be imported into, as far as the dashboard knows ("" when it can). */
const notEmpty = (d: Database) => (importing(d) ? "Importing" : d.restoredFrom || d.import?.state === "succeeded" ? "Not empty" : "");

function ImportForm() {
  const s = usePyo();
  const navigate = useNavigate();
  const search = Route.useSearch();
  const preset = s.databases.find((d) => d.id === search.into && d.teamId === s.currentTeamId && d.engine !== "valkey");
  const [engine, setEngine] = useState<Engine>(() => (preset?.engine as Engine | undefined) ?? search.engine ?? "postgres");
  const [source, setSource] = useState<SourceInput>({ mode: "url", url: "", file: null });
  const [into, setInto] = useState<"new" | "existing">(preset ? "existing" : "new");
  const [existingId, setExistingId] = useState(preset?.id ?? "");
  const [showOptions, setShowOptions] = useState(false);
  const [newDbId] = useState(() => newId("mdb"));
  const f = useDatabaseDraft(engine);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<number | null>(null);
  const [error, setError] = useState("");
  const [check, setCheck] = useState<{ key: string; state: "checking" | "ok" | "error"; result?: ImportChecked; message?: string } | null>(null);
  const uploadSignal = useUploadSignal();

  const candidates = s.databases
    .filter((d) => d.teamId === s.currentTeamId && d.engine === engine && d.state === "active")
    .map((d) => ({ d, why: notEmpty(d) }));
  const existing = candidates.find((c) => c.d.id === existingId && !c.why)?.d ?? candidates.find((c) => !c.why)?.d;

  const { engine: _engine, ...newFields } = f.fields();
  const target: ImportInto | null =
    into === "new" ? (f.valid ? { newDatabase: { id: newDbId, ...newFields } } : null) : existing ? { databaseId: existing.id } : null;
  const ready = sourceReady(engine, source);
  // A check holds for what it checked: the same engine, URL and target.
  const key = JSON.stringify([engine, source.mode, source.url.trim(), target]);
  const checked = check?.key === key ? check : null;
  const canCheck = source.mode === "url" && ready && !!target && !busy && checked?.state !== "checking";
  const canImport = ready && !!target && !busy && checked?.state !== "checking";

  const runCheck = async () => {
    if (!canCheck || !target) return;
    setError("");
    setCheck({ key, state: "checking" });
    try {
      const result = await checkImport(s.currentTeamId, engine, target, { url: source.url.trim() });
      setCheck({ key, state: "ok", result });
    } catch (e) {
      setCheck({ key, state: "error", message: e instanceof Error ? e.message : "That didn't work." });
    }
  };

  const start = async () => {
    if (!canImport || !target) return;
    setBusy(true);
    setError("");
    try {
      const src = await resolveSource(s.currentTeamId, engine, source, setProgress, uploadSignal());
      setProgress(source.mode === "file" ? 1 : null);
      const r =
        "databaseId" in target
          ? await importInto({ id: target.databaseId, teamId: s.currentTeamId, engine }, src)
          : await importNew(s.currentTeamId, engine, target.newDatabase, src);
      const name = into === "existing" ? existing?.name : f.name;
      toast({
        title: `Importing into ${name}`,
        description: r.extensions.turnedOn.length ? `With ${r.extensions.turnedOn.join(", ")} turned on.` : undefined,
      });
      void navigate({ href: teamPath(currentSlug(), databaseHref(engine, r.databaseId)) });
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work.");
      setBusy(false);
      setProgress(null);
    }
  };

  const sizeText = `${SIZES.find((x) => x.value === f.size)?.label ?? "Custom"} · ${f.ha ? `${f.replicas + 1} instances` : "Single server"}`;

  return (
    <Panel>
      <form
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          void start();
        }}
      >
        <section className="space-y-3 p-6">
          <GroupLabel>Engine</GroupLabel>
          <Segmented
            label="Engine"
            size="md"
            value={engine}
            onChange={(v) => {
              setEngine(v);
              f.fitEngine(v);
            }}
            options={(["postgres", "mysql"] as const).map((v) => {
              const Icon = ENGINE_ICON[v];
              return {
                value: v,
                label: (
                  <span className="flex items-center gap-1.5">
                    <Icon size={15} />
                    {ENGINE_LABEL[v]}
                  </span>
                ),
              };
            })}
          />
        </section>

        <section className="space-y-4 border-t border-border p-6">
          <GroupLabel>From</GroupLabel>
          <Segmented
            label="From"
            size="md"
            value={source.mode}
            onChange={(mode) => setSource({ ...source, mode })}
            options={[
              { value: "url", label: "Connection URL" },
              { value: "file", label: "Dump file" },
            ]}
          />
          {source.mode === "url" ? (
            <UrlField engine={engine} value={source.url} onChange={(url) => setSource({ ...source, url })} disabled={busy} />
          ) : (
            <DumpField engine={engine} file={source.file} onChange={(file) => setSource({ ...source, file })} disabled={busy} progress={progress} />
          )}
        </section>

        <section className="space-y-4 border-t border-border p-6">
          <GroupLabel>Into</GroupLabel>
          <Segmented
            label="Into"
            size="md"
            value={into}
            onChange={setInto}
            options={[
              { value: "new", label: "New database" },
              { value: "existing", label: "Existing database" },
            ]}
          />
          {into === "new" ? (
            <div className="space-y-4">
              <NameAndCluster engine={engine} draft={f} />
              {!f.ready.length ? <NoCluster /> : null}
              <button
                type="button"
                aria-expanded={showOptions}
                onClick={() => setShowOptions((v) => !v)}
                className="flex items-center gap-1.5 rounded-md text-[12.5px] font-medium text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring"
              >
                <CaretDownIcon size={12} weight="bold" className={`transition-transform ${showOptions ? "" : "-rotate-90"}`} />
                Options
                <span className="font-normal">· {sizeText}</span>
              </button>
              {showOptions ? (
                <div className="space-y-5">
                  <SizeAndAvailability engine={engine} draft={f} />
                </div>
              ) : null}
            </div>
          ) : (
            <Field label="Database" hint={candidates.length ? "Only an empty one." : undefined}>
              {(p) => (
                <Select {...p} value={existing?.id ?? ""} onChange={(e) => setExistingId(e.target.value)} disabled={!candidates.length}>
                  {candidates.length ? (
                    candidates.map(({ d, why }) => (
                      <option key={d.id} value={d.id} disabled={!!why}>
                        {why ? `${d.name} · ${why}` : d.name}
                      </option>
                    ))
                  ) : (
                    <option value="" hidden>
                      No {ENGINE_LABEL[engine]} databases
                    </option>
                  )}
                </Select>
              )}
            </Field>
          )}
        </section>

        {checked?.state === "ok" || checked?.state === "error" || error ? (
          <div className="border-t border-border px-6 py-4" aria-live="polite">
            {error || checked?.state === "error" ? (
              <Callout tone="danger" icon={WarningIcon} title="Can't import">
                {error || checked?.message}
              </Callout>
            ) : checked?.result ? (
              <CheckResult engine={engine} result={checked.result} />
            ) : null}
          </div>
        ) : null}

        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-border px-6 py-3">
          <ButtonLink href={`/${DB_SECTION[engine]}`} variant="ghost" size="sm" className="mr-auto">
            Cancel
          </ButtonLink>
          {source.mode === "url" ? (
            <Button type="button" variant="secondary" size="sm" disabled={!canCheck} onClick={() => void runCheck()}>
              {checked?.state === "checking" ? "Checking…" : "Check connection"}
            </Button>
          ) : null}
          <Button type="submit" size="sm" disabled={!canImport}>
            {busy ? (progress !== null && progress < 1 ? "Uploading…" : "Starting…") : "Import"}
          </Button>
        </div>
      </form>
    </Panel>
  );
}

function CheckResult({ engine, result }: { engine: Engine; result: ImportChecked }) {
  const version = result.source?.version;
  const size = result.source?.sizeBytes;
  const on = result.extensions.turnedOn;
  return (
    <div className="flex gap-2.5 text-[13px]" role="status">
      <CheckCircleIcon size={16} weight="fill" className="mt-0.5 shrink-0 text-success" />
      <div className="min-w-0">
        <p className="font-medium text-foreground">Ready to import</p>
        <p className="text-muted-foreground">
          {[version ? `${ENGINE_LABEL[engine]} ${version}` : "", size ? fileSize(size) : ""].filter(Boolean).join(" · ") || "Connected."}
          {on.length ? ` Turns on ${on.join(", ")}.` : ""}
        </p>
      </div>
    </div>
  );
}
