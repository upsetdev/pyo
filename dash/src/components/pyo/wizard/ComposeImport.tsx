import {
  ArrowLeftIcon,
  ArrowRightIcon,
  GitBranchIcon,
  HardDrivesIcon,
  PackageIcon,
  RocketLaunchIcon,
  UploadSimpleIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Field, Input, Segmented, Select, Switch, Textarea } from "~/components/ui/Field";
import { Layered } from "~/components/ui/Layered";
import { Callout, Page } from "~/components/ui/Panel";
import {
  fetchGithubRepos,
  op,
  previewCompose,
  type ComposePlan,
  type ComposeService,
} from "~/lib/pyo/backend";
import { currentSlug } from "~/lib/pyo/paths";
import { clustersIn, currentTeam, projectParams, refresh, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { GithubRepo } from "~/lib/pyo/types";

// Deploy new › Docker Compose: paste or upload a file, or pick one in a
// repository; see what Pyo makes of it (and what it leaves out); change
// names, public ports and domains; then create it all at once.

type From = "paste" | "repo";

export function ComposeImport({ onBack }: { onBack: () => void }) {
  const s = usePyo();
  const team = currentTeam(s);
  const clusters = clustersIn(s).filter((c) => c.status !== "pending");
  const [clusterId, setClusterId] = useState(
    (team?.defaultClusterId && clusters.some((c) => c.id === team.defaultClusterId)
      ? team.defaultClusterId
      : clusters[0]?.id) ?? "",
  );
  const [from, setFrom] = useState<From>("paste");
  const [file, setFile] = useState("");
  const [env, setEnv] = useState("");
  const [repos, setRepos] = useState<GithubRepo[] | null>(null);
  const [repo, setRepo] = useState("");
  const [branch, setBranch] = useState("");
  const [path, setPath] = useState("compose.yaml");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [plan, setPlan] = useState<ComposePlan | null>(null);
  const upload = useRef<HTMLInputElement>(null);
  const navigate = useNavigate();

  useEffect(() => {
    if (from !== "repo" || repos || !team) return;
    fetchGithubRepos(team.id)
      .then((r) => setRepos(r.repositories))
      .catch(() => setRepos([]));
  }, [from, repos, team]);

  const preview = async (e: FormEvent) => {
    e.preventDefault();
    if (!team) return;
    setBusy(true);
    setError("");
    try {
      const p = await previewCompose(
        from === "paste"
          ? { teamId: team.id, file, env: env || undefined, clusterId: clusterId || undefined }
          : {
              teamId: team.id,
              repo: { fullName: repo, branch, path },
              clusterId: clusterId || undefined,
            },
      );
      setPlan(p);
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const deploy = async () => {
    if (!plan || !team) return;
    setBusy(true);
    setError("");
    try {
      const r = await op<{ projectId: string; services: { name: string }[] }>("importCompose", {
        teamId: team.id,
        clusterId: clusterId || undefined,
        plan,
      });
      await refresh();
      toast({ tone: "success", title: `Deploying ${r.services.length} service${r.services.length === 1 ? "" : "s"}` });
      void navigate({
        to: "/$team/$projectId",
        params: { team: currentSlug(), ...projectParams(r.projectId) },
      });
    } catch (err) {
      setError((err as Error).message);
      setBusy(false);
    }
  };

  if (plan)
    return (
      <Preview
        plan={plan}
        onChange={setPlan}
        busy={busy}
        error={error}
        onBack={() => setPlan(null)}
        onDeploy={deploy}
      />
    );

  const picked = repos?.find((r) => r.fullName === repo);
  return (
    <Page width="max-w-3xl">
      <div>
        <Button variant="ghost" size="sm" onClick={onBack}>
          <ArrowLeftIcon size={13} /> Back
        </Button>
        <h1 className="mt-3 text-[22px] font-semibold tracking-tight">Import Docker Compose</h1>
        <p className="mt-1 text-[13.5px] text-muted-foreground">
          Each Compose service becomes a service in a new project. You review it first.
        </p>
      </div>
      <form onSubmit={preview} noValidate className="space-y-4">
        <Layered
          title="Compose file"
          action={
            <Segmented
              label="Where the file is"
              size="xs"
              value={from}
              onChange={(v) => {
                setFrom(v);
                setError("");
              }}
              options={[
                { value: "paste", label: "Paste or upload" },
                { value: "repo", label: "Repository" },
              ]}
            />
          }
        >
          {from === "paste" ? (
            <div className="space-y-4 p-5">
              <Field label="compose.yaml">
                {(f) => (
                  <Textarea
                    {...f}
                    value={file}
                    onChange={(e) => setFile(e.target.value)}
                    rows={14}
                    spellCheck={false}
                    placeholder={"services:\n  web:\n    image: nginx\n    ports: [\"80:80\"]"}
                    className="font-mono text-[12px]"
                  />
                )}
              </Field>
              <div className="flex flex-wrap items-center gap-2">
                <input
                  ref={upload}
                  type="file"
                  accept=".yml,.yaml,text/yaml"
                  className="hidden"
                  onChange={(e) => {
                    const f = e.target.files?.[0];
                    if (f) void f.text().then(setFile);
                    e.target.value = "";
                  }}
                />
                <Button type="button" variant="secondary" size="sm" onClick={() => upload.current?.click()}>
                  <UploadSimpleIcon size={13} /> Upload a file
                </Button>
              </div>
              <Field label=".env" optional hint="For the file's ${VARIABLES} and env_file: .env.">
                {(f) => (
                  <Textarea
                    {...f}
                    value={env}
                    onChange={(e) => setEnv(e.target.value)}
                    rows={4}
                    spellCheck={false}
                    placeholder="POSTGRES_PASSWORD=…"
                    className="font-mono text-[12px]"
                  />
                )}
              </Field>
            </div>
          ) : (
            <div className="grid gap-4 p-5 sm:grid-cols-2">
              <Field label="Repository" className="sm:col-span-2">
                {(f) => (
                  <Select
                    {...f}
                    value={repo}
                    onChange={(e) => {
                      setRepo(e.target.value);
                      setBranch(repos?.find((r) => r.fullName === e.target.value)?.defaultBranch ?? "");
                    }}
                  >
                    <option value="">{repos === null ? "Loading…" : repos.length ? "Pick a repository" : "No repositories: connect GitHub first"}</option>
                    {(repos ?? []).map((r) => (
                      <option key={r.fullName} value={r.fullName}>
                        {r.fullName}
                      </option>
                    ))}
                  </Select>
                )}
              </Field>
              <Field label="Branch">
                {(f) => <Input {...f} mono value={branch} onChange={(e) => setBranch(e.target.value)} placeholder={picked?.defaultBranch ?? "main"} />}
              </Field>
              <Field label="Path of the file">
                {(f) => <Input {...f} mono value={path} onChange={(e) => setPath(e.target.value)} placeholder="compose.yaml" />}
              </Field>
              <p className="text-[12.5px] text-muted-foreground sm:col-span-2">
                A .env file next to it is read too.
              </p>
            </div>
          )}
        </Layered>
        <Layered title="Where it runs">
          <div className="p-5">
            <Field label="Cluster" hint="Web ports get a free domain on it.">
              {(f) => (
                <Select {...f} value={clusterId} onChange={(e) => setClusterId(e.target.value)}>
                  {clusters.length ? null : <option value="">No clusters yet</option>}
                  {clusters.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          </div>
        </Layered>
        {error ? (
          <p role="alert" className="text-[12.5px] text-destructive">
            {error}
          </p>
        ) : null}
        <div className="flex justify-end">
          <Button
            type="submit"
            size="lg"
            disabled={busy || (from === "paste" ? !file.trim() : !repo || !path.trim())}
          >
            {busy ? "Reading…" : "Preview"} <ArrowRightIcon size={14} />
          </Button>
        </div>
      </form>
    </Page>
  );
}

function Preview({
  plan,
  onChange,
  busy,
  error,
  onBack,
  onDeploy,
}: {
  plan: ComposePlan;
  onChange: (p: ComposePlan) => void;
  busy: boolean;
  error: string;
  onBack: () => void;
  onDeploy: () => void;
}) {
  const general = plan.warnings.filter((w) => !w.service);
  const edit = (i: number, f: (s: ComposeService) => ComposeService) =>
    onChange({ ...plan, services: plan.services.map((s, j) => (j === i ? f(s) : s)) });
  const needsRepo = plan.services.filter((s) => s.source.type === "git" && !s.source.repoUrl);
  return (
    <Page width="max-w-3xl">
      <div>
        <Button variant="ghost" size="sm" onClick={onBack}>
          <ArrowLeftIcon size={13} /> Change the file
        </Button>
        <h1 className="mt-3 text-[22px] font-semibold tracking-tight">Review the import</h1>
        <p className="mt-1 text-[13.5px] text-muted-foreground">
          {plan.services.length} service{plan.services.length === 1 ? "" : "s"}, deployed in
          this order. Nothing is created until you deploy.
        </p>
      </div>
      <Layered title="Project">
        <div className="p-5">
          <Field label="Name">
            {(f) => (
              <Input
                {...f}
                value={plan.project.name}
                onChange={(e) => onChange({ ...plan, project: { ...plan.project, name: e.target.value } })}
              />
            )}
          </Field>
        </div>
      </Layered>
      {general.length ? (
        <Callout tone="warning" icon={WarningIcon} title="Before you deploy">
          <ul className="list-disc space-y-1 pl-4">
            {general.map((w, i) => (
              <li key={i}>{w.message}</li>
            ))}
          </ul>
        </Callout>
      ) : null}
      <Layered title="Services" bare>
        <ul className="list-rows overflow-hidden rounded-[14px] border border-card-border bg-card">
          {plan.services.map((svc, i) => {
            const warnings = plan.warnings.filter((w) => w.service === svc.composeName);
            return (
              <li key={svc.composeName} className="space-y-3 px-4 py-4">
                <div className="flex flex-wrap items-center gap-2">
                  {svc.source.type === "git" ? (
                    <GitBranchIcon size={15} className="text-muted-foreground" />
                  ) : (
                    <PackageIcon size={15} className="text-muted-foreground" />
                  )}
                  <Input
                    aria-label={`Name of ${svc.composeName}`}
                    value={svc.name}
                    onChange={(e) => edit(i, (s) => ({ ...s, name: e.target.value }))}
                    className="h-8 w-48 font-medium"
                  />
                  <span className="min-w-0 truncate font-mono text-[12px] text-muted-foreground">
                    {svc.source.type === "git"
                      ? svc.source.repoUrl
                        ? `built from ${svc.source.buildContext || "the repository root"}`
                        : "built from source: needs its repository"
                      : `${svc.source.image}:${svc.source.reference}`}
                  </span>
                  {svc.replicas !== 1 ? <Badge tone="neutral">{svc.replicas} replicas</Badge> : null}
                </div>
                {svc.ports.length ? (
                  <div className="space-y-2">
                    {svc.ports.map((p, k) => (
                      <div key={p.name} className="flex flex-wrap items-center gap-x-3 gap-y-2 text-[12.5px]">
                        <Switch
                          checked={p.public}
                          label={`${p.containerPort} public`}
                          onChange={(v) =>
                            edit(i, (s) => ({
                              ...s,
                              ports: s.ports.map((x, j) => (j === k ? { ...x, public: v, publicPort: v && x.protocol !== "HTTP" ? (x.publicPort ?? x.containerPort) : x.publicPort } : x)),
                            }))
                          }
                        />
                        <span className="w-24 tabular-nums">
                          {p.containerPort}
                          <span className="text-muted-foreground"> {p.protocol}</span>
                        </span>
                        {p.public && p.protocol === "HTTP" ? (
                          <Input
                            mono
                            aria-label={`Domain for port ${p.containerPort}`}
                            value={p.domain ?? ""}
                            placeholder="app.example.com"
                            onChange={(e) =>
                              edit(i, (s) => ({
                                ...s,
                                ports: s.ports.map((x, j) => (j === k ? { ...x, domain: e.target.value } : x)),
                              }))
                            }
                            className="h-8 min-w-0 flex-1"
                          />
                        ) : (
                          <span className="text-muted-foreground">
                            {p.public
                              ? `public on port ${p.publicPort ?? p.containerPort}, through the gateway`
                              : "inside the project"}
                          </span>
                        )}
                      </div>
                    ))}
                  </div>
                ) : null}
                <div className="flex flex-wrap gap-x-4 gap-y-1 text-[12.5px] text-muted-foreground">
                  <span>
                    {svc.env.length} variable{svc.env.length === 1 ? "" : "s"}
                    {svc.env.some((e) => e.secret) ? ` (${svc.env.filter((e) => e.secret).length} secret)` : ""}
                  </span>
                  {svc.volumes.map((v) => (
                    <span key={v.name} className="inline-flex items-center gap-1">
                      <HardDrivesIcon size={12} /> {v.name} at {v.mountPath}, {v.sizeGi} GiB
                      {v.fromBind ? ", starts empty" : ""}
                    </span>
                  ))}
                  {svc.dependsOn.length ? <span>after {svc.dependsOn.join(", ")}</span> : null}
                </div>
                {warnings.length ? (
                  <ul className="space-y-1 rounded-[10px] border border-warning/30 bg-warning/[0.07] px-3 py-2 text-[12.5px]">
                    {warnings.map((w, k) => (
                      <li key={k} className="flex gap-2">
                        <WarningIcon size={13} className="mt-0.5 shrink-0 text-warning" />
                        <span>
                          <span className="font-mono text-[11.5px] text-muted-foreground">{w.field}</span> {w.message}
                        </span>
                      </li>
                    ))}
                  </ul>
                ) : null}
              </li>
            );
          })}
        </ul>
      </Layered>
      {error ? (
        <p role="alert" className="text-[12.5px] text-destructive">
          {error}
        </p>
      ) : null}
      <div className="flex items-center justify-end gap-3">
        {needsRepo.length ? (
          <span className="text-[12.5px] text-muted-foreground">
            {needsRepo.map((s) => s.name).join(", ")} need{needsRepo.length === 1 ? "s" : ""} its repository: import from the repository instead.
          </span>
        ) : null}
        <Button size="lg" disabled={busy || needsRepo.length > 0} onClick={onDeploy}>
          <RocketLaunchIcon size={14} /> {busy ? "Deploying…" : "Deploy"}
        </Button>
      </div>
    </Page>
  );
}
