import {
  ArrowSquareOutIcon,
  CaretDownIcon,
  CheckCircleIcon,
  CubeIcon,
  DatabaseIcon,
  DownloadSimpleIcon,
  StackIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { z } from "zod";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import { CoolifyMark, DokployMark } from "~/components/pyo/platforms/Marks";
import { GroupLabel } from "~/components/pyo/Databases";
import { AppLink } from "~/components/ui/AppLink";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Checkbox, Field, Input, SecretInput, Segmented, Select } from "~/components/ui/Field";
import { Callout, EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import {
  defaultPicks,
  pickable,
  previewImport,
  startImport,
  type ImportDatabase,
  type ImportPlan,
  type ImportResult,
  type ImportService,
  type ImportStack,
  type Platform,
} from "~/lib/pyo/platformImport";
import { clustersIn, getState, projectPath, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { seo } from "~/lib/seo";

// Moving a team's services from Coolify or Dokploy: the platform's address
// and an API token, a preview of what each thing becomes (pick what to
// bring), then the import. The token is sent with each request, never kept.

export const Route = createFileRoute("/_app/$team/import_/platform")({
  validateSearch: z.object({
    from: z.enum(["coolify", "dokploy"]).optional().catch(undefined),
  }),
  head: () => ({ meta: seo({ title: "Import from another platform" }) }),
  component: PlatformImportPage,
});

const PLATFORMS: Record<Platform, { label: string; Mark: (p: { className?: string }) => ReactNode; placeholder: string; hint: string }> = {
  coolify: {
    label: "Coolify",
    Mark: CoolifyMark,
    placeholder: "https://coolify.example.com",
    hint: "In Coolify, Keys & Tokens. Give it read and read:sensitive.",
  },
  dokploy: {
    label: "Dokploy",
    Mark: DokployMark,
    placeholder: "https://dokploy.example.com",
    hint: "In Dokploy, your profile's API/CLI section.",
  },
};

function PlatformImportPage() {
  const admin = useTeamAdmin();
  return (
    <Page width="max-w-3xl">
      <PageHeader title="Import from another platform" />
      {admin ? (
        <ImportFlow />
      ) : (
        <Panel>
          <EmptyState icon={DownloadSimpleIcon} title="Only owners and admins can import." />
        </Panel>
      )}
    </Page>
  );
}

function ImportFlow() {
  const s = usePyo();
  const search = Route.useSearch();
  const [platform, setPlatform] = useState<Platform>(search.from ?? "coolify");
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const clusters = clustersIn(s);
  const team = s.teams.find((t) => t.id === s.currentTeamId);
  const [clusterId, setClusterId] = useState(team?.defaultClusterId ?? clusters[0]?.id ?? "");
  const [plan, setPlan] = useState<ImportPlan | null>(null);
  const [picks, setPicks] = useState<Set<string>>(new Set());
  const [deploy, setDeploy] = useState(false);
  const [busy, setBusy] = useState<"" | "preview" | "import">("");
  const [error, setError] = useState("");
  const [results, setResults] = useState<ImportResult[] | null>(null);

  const p = PLATFORMS[platform];
  const source = { teamId: s.currentTeamId, platform, url: url.trim(), token: token.trim() };
  const ready = !!source.url && !!source.token && !busy;
  const reset = () => {
    setPlan(null);
    setResults(null);
    setError("");
  };

  const preview = async () => {
    if (!ready) return;
    setBusy("preview");
    reset();
    try {
      const got = await previewImport(source);
      setPlan(got);
      setPicks(defaultPicks(got));
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work.");
    } finally {
      setBusy("");
    }
  };

  const run = async () => {
    if (!plan || !picks.size || busy) return;
    setBusy("import");
    setError("");
    try {
      const r = await startImport({ ...source, clusterId: clusterId || undefined, items: [...picks], deploy });
      setResults(r);
      const made = r.filter((x) => x.state === "created" || x.state === "importing").length;
      toast({ title: made ? `Imported ${made} of ${r.length}` : "Nothing new to import" });
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work.");
    } finally {
      setBusy("");
    }
  };

  const toggle = (key: string, on: boolean) =>
    setPicks((cur) => {
      const next = new Set(cur);
      if (on) next.add(key);
      else next.delete(key);
      return next;
    });

  return (
    <div className="space-y-6">
      <Panel>
        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void preview();
          }}
        >
          <section className="space-y-4 p-6">
            <GroupLabel>From</GroupLabel>
            <Segmented
              label="Platform"
              size="md"
              value={platform}
              onChange={(v) => {
                setPlatform(v);
                reset();
              }}
              options={(Object.keys(PLATFORMS) as Platform[]).map((v) => {
                const { Mark, label } = PLATFORMS[v];
                return {
                  value: v,
                  label: (
                    <span className="flex items-center gap-1.5">
                      <Mark className="size-[15px]" />
                      {label}
                    </span>
                  ),
                };
              })}
            />
            <Field label="Address">
              {(f) => (
                <Input
                  {...f}
                  type="url"
                  inputMode="url"
                  autoComplete="off"
                  spellCheck={false}
                  placeholder={p.placeholder}
                  value={url}
                  onChange={(e) => setUrl(e.target.value)}
                  disabled={!!busy}
                />
              )}
            </Field>
            <Field label="API token" hint={p.hint}>
              {(f) => (
                <SecretInput {...f} autoComplete="off" spellCheck={false} value={token} onChange={(e) => setToken(e.target.value)} disabled={!!busy} />
              )}
            </Field>
            {clusters.length > 1 ? (
              <Field label="Cluster">
                {(f) => (
                  <Select {...f} value={clusterId} onChange={(e) => setClusterId(e.target.value)} disabled={!!busy}>
                    {clusters.map((c) => (
                      <option key={c.id} value={c.id}>
                        {c.name}
                      </option>
                    ))}
                  </Select>
                )}
              </Field>
            ) : null}
          </section>
          <div className="flex justify-end border-t border-border px-6 py-3">
            <Button type="submit" size="sm" variant={plan ? "secondary" : "primary"} disabled={!ready}>
              {busy === "preview" ? "Reading…" : plan ? "Read again" : "Preview"}
            </Button>
          </div>
        </form>
      </Panel>

      {error ? (
        <Callout tone="danger" icon={WarningIcon} title="Can't import">
          {error}
        </Callout>
      ) : null}

      {results ? <Results results={results} /> : null}

      {plan && !results ? (
        <Panel>
          <div className="space-y-5 p-6">
            {plan.warnings.map((w) => (
              <Callout key={w} tone="warning" icon={WarningIcon}>
                {w}
              </Callout>
            ))}
            {plan.projects.length ? (
              plan.projects.map((pr) => (
                <section key={pr.key} className="space-y-1.5">
                  <div className="flex items-center gap-2">
                    <GroupLabel>{pr.name}</GroupLabel>
                    {pr.existingProjectId ? <Badge>Imported before</Badge> : null}
                  </div>
                  <ul className="divide-y divide-border rounded-lg border border-border">
                    {pr.services.map((x) => (
                      <Item key={x.key} item={x} picked={picks.has(x.key)} onPick={(on) => toggle(x.key, on)} />
                    ))}
                    {pr.stacks.map((x) => (
                      <Item key={x.key} item={x} picked={picks.has(x.key)} onPick={(on) => toggle(x.key, on)} />
                    ))}
                    {pr.databases.map((x) => (
                      <Item key={x.key} item={x} picked={picks.has(x.key)} onPick={(on) => toggle(x.key, on)} />
                    ))}
                    {!pr.services.length && !pr.stacks.length && !pr.databases.length ? (
                      <li className="px-3 py-2.5 text-[13px] text-muted-foreground">Empty</li>
                    ) : null}
                  </ul>
                </section>
              ))
            ) : (
              <EmptyState icon={DownloadSimpleIcon} title="Nothing to import" />
            )}
          </div>
          <div className="flex flex-wrap items-center justify-end gap-3 border-t border-border px-6 py-3">
            <span className="mr-auto">
              <Checkbox checked={deploy} onChange={setDeploy} label="Deploy services once imported" disabled={!!busy} />
            </span>
            <Button type="button" size="sm" disabled={!picks.size || !!busy} onClick={() => void run()}>
              {busy === "import" ? "Importing…" : `Import ${picks.size || ""}`.trim()}
            </Button>
          </div>
        </Panel>
      ) : null}
    </div>
  );
}

/** What a thing is and where it comes from, in a few words. */
function describe(item: ImportService | ImportStack | ImportDatabase): { icon: ReactNode; name: string; from: string; badges: string[] } {
  if ("service" in item) {
    const src = item.service.source;
    const from =
      src.type === "git"
        ? `${src.repoUrl.replace(/^https:\/\//, "")} · ${src.branch}${item.builder === "dockerfile" ? " · Dockerfile" : ""}`
        : src.type === "image"
          ? `${src.image}:${src.reference}`
          : item.sourceName;
    const badges = [
      ...item.domains.map((d) => d.hostname),
      item.service.env.length ? `${item.service.env.length} variable${item.service.env.length === 1 ? "" : "s"}` : "",
      item.service.volumes.length ? `${item.service.volumes.length} volume${item.service.volumes.length === 1 ? "" : "s"}` : "",
    ];
    return { icon: <CubeIcon size={15} />, name: item.service.name || item.sourceName, from, badges: badges.filter(Boolean) };
  }
  if ("plan" in item) {
    const n = item.plan.services.length;
    return {
      icon: <StackIcon size={15} />,
      name: item.plan.project.name || item.sourceName,
      from: n ? `Compose · ${n} service${n === 1 ? "" : "s"} · a project of its own` : "Compose",
      badges: item.domains.map((d) => d.hostname),
    };
  }
  const engine = { postgres: "Postgres", mysql: "MySQL", valkey: "KV" } as const;
  const target = item.target ? engine[item.target] : "";
  const from =
    item.action === "import" ? `${target} · data copied` : item.action === "create" ? `${target} · empty` : `${item.engine} · not imported`;
  return { icon: <DatabaseIcon size={15} />, name: item.name, from, badges: [] };
}

function Item({
  item,
  picked,
  onPick,
}: {
  item: ImportService | ImportStack | ImportDatabase;
  picked: boolean;
  onPick: (on: boolean) => void;
}) {
  const [open, setOpen] = useState(false);
  const d = describe(item);
  const before = ("existingId" in item && item.existingId) || ("existingProjectId" in item && item.existingProjectId);
  const can = pickable(item);
  return (
    <li className="px-3 py-2.5">
      <div className="flex items-start gap-2.5">
        <input
          type="checkbox"
          className="mt-1"
          aria-label={`Import ${d.name}`}
          checked={can && picked}
          disabled={!can}
          onChange={(e) => onPick(e.target.checked)}
        />
        <span className="mt-0.5 shrink-0 text-muted-foreground">{d.icon}</span>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="truncate text-[13px] font-medium text-foreground">{d.name}</span>
            {before ? <Badge>Imported before</Badge> : !can ? <Badge tone="warning">Not imported</Badge> : null}
            {d.badges.map((b) => (
              <Badge key={b} mono>
                {b}
              </Badge>
            ))}
          </div>
          <p className="truncate text-[12px] text-muted-foreground">{d.from}</p>
          {item.warnings.length ? (
            <button
              type="button"
              aria-expanded={open}
              onClick={() => setOpen((v) => !v)}
              className="mt-1 flex items-center gap-1 rounded-md text-[12px] font-medium text-warning outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              <CaretDownIcon size={11} weight="bold" className={`transition-transform ${open ? "" : "-rotate-90"}`} />
              {item.warnings.length} note{item.warnings.length === 1 ? "" : "s"}
            </button>
          ) : null}
          {open ? (
            <ul className="mt-1.5 list-disc space-y-1 pl-5 text-[12px] leading-4 text-muted-foreground">
              {item.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          ) : null}
        </div>
      </div>
    </li>
  );
}

const STATE: Record<ImportResult["state"], { label: string; tone: Tone }> = {
  created: { label: "Created", tone: "success" },
  importing: { label: "Copying data", tone: "progress" },
  exists: { label: "Imported before", tone: "neutral" },
  skipped: { label: "Not imported", tone: "warning" },
  failed: { label: "Failed", tone: "danger" },
};

function Results({ results }: { results: ImportResult[] }) {
  const s = getState();
  return (
    <Panel>
      <ul className="divide-y divide-border">
        {results.map((r) => {
          const st = STATE[r.state];
          const project = r.projectId ? s.projects.find((p) => p.id === r.projectId) : undefined;
          return (
            <li key={r.key} className="flex items-start gap-2.5 px-6 py-3">
              {r.state === "created" ? (
                <CheckCircleIcon size={16} weight="fill" className="mt-0.5 shrink-0 text-success" />
              ) : (
                <span className="mt-0.5 size-4 shrink-0" />
              )}
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-[13px] font-medium text-foreground">{r.name}</span>
                  <Badge tone={st.tone}>{st.label}</Badge>
                  {project ? (
                    <AppLink href={projectPath(project.id)} className="inline-flex items-center gap-1 text-[12px] text-primary hover:underline">
                      {project.name}
                      <ArrowSquareOutIcon size={12} />
                    </AppLink>
                  ) : null}
                </div>
                {r.message ? <p className="text-[12px] text-muted-foreground">{r.message}</p> : null}
              </div>
            </li>
          );
        })}
      </ul>
    </Panel>
  );
}
