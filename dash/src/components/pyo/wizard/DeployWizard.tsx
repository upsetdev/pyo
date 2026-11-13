import { currentSlug } from "~/lib/pyo/paths";
import { Layered } from "~/components/ui/Layered";
import {
  ArrowLeftIcon,
  ArrowRightIcon,
  BracketsCurlyIcon,
  CaretRightIcon,
  CpuIcon,
  MapPinIcon,
  SlidersHorizontalIcon,
  DatabaseIcon,
  GitBranchIcon,
  GlobeIcon,
  PackageIcon,
  PlugsIcon,
  RocketLaunchIcon,
  TimerIcon,
} from "@phosphor-icons/react";
import { ProjectIcon } from "~/components/ui/ProjectIcon";
import { placesOf, locationLabel } from "~/lib/pyo/location";
import { Location } from "~/components/ui/Flag";
import { Link, useNavigate } from "@tanstack/react-router";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
  type ReactNode,
} from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { freeRoom, fitting, needsConfirm, roomWarning } from "~/lib/pyo/capacity";
import { Input, Select, Segmented } from "~/components/ui/Field";
import { Page } from "~/components/ui/Panel";
import { ENGINE_LABEL } from "~/components/pyo/Databases";
import {
  companionDraft,
  draftFromTemplate,
  emptyDraft,
  NEW_PROJECT,
  parseImageInput,
  suggestName,
  validateSection,
  type DeployDraft,
  type Issues,
  type Section,
  type SourceType,
  imageDefaults,
} from "~/lib/pyo/draft";
import { cpu, mem } from "~/lib/pyo/format";
import { STAGE_LABEL, stagePlan } from "~/lib/pyo/simulation";
import {
  api,
  buildersIn,
  clustersIn,
  currentTeam,
  draftProject,
  newProjectFor,
  usePyo,
} from "~/lib/pyo/store";
import { companionService, findTemplate, getTemplate } from "~/lib/pyo/templates";
import { toast } from "~/lib/pyo/toast";
import type { DeploymentStage, Template } from "~/lib/pyo/types";
import {
  BrandMark,
  DOCKER_LOGO,
  GIT_LOGO,
  IconTile,
  TemplateLogo,
} from "../common";
import { TemplateGallery } from "../TemplateGallery";
import {
  DomainsStep,
  EnvStep,
  GeneralFields,
  NetworkingStep,
  ResourcesStep,
  ScheduleStep,
  SourceStep,
  TemplateFields,
  VolumesStep,
  sizeName,
} from "./steps";
import { Block, DeploySummary, LogoTile, useDraftSections, type Collapsible } from "./sections";
import {
  dbSummary,
  initialDbChoice,
  prepareDatabase,
  TemplateDatabaseFields,
  useDbProblems,
  useExtensionCatalog,
  type DbChoice,
} from "./TemplateDatabase";
import { fetchDatabaseExtensions } from "~/lib/pyo/backend";
import { providerOfService } from "~/lib/pyo/refs";
import { Logo } from "~/components/site/Logo";
import { ComposeImport } from "./ComposeImport";

// New deploy in two screens. Start: paste a repository or image, or pick a
// template. Configure: one page where only name, project and cluster need a
// decision; everything else has defaults and sits in collapsed sections
// whose summary shows what will be used.

/** From the URL: ?template=mariadb, ?repo=https://github.com/o/r, ?image=nginx:1.27 (and ?project=). */
type Initial = {
  source?: SourceType | "compose";
  template?: string;
  project?: string;
  repo?: string;
  image?: string;
  cron?: boolean;
};

export function DeployWizard({ initial }: { initial: Initial }) {
  const s = usePyo();
  const ws = currentTeam(s);
  const clusters = clustersIn(s);
  const defaultCluster =
    (ws?.defaultClusterId &&
      clusters.find(
        (c) => c.id === ws.defaultClusterId && c.status !== "unreachable",
      )?.id) ||
    clusters.find((c) => c.status === "connected")?.id ||
    "";
  const storageClassOf = (clusterId: string) =>
    clusters
      .find((c) => c.id === clusterId)
      ?.capabilities.storageClasses.find((x) => x.isDefault)?.name ?? "";

  // Coming from a project deploys into it; otherwise the service gets a project of its own.
  const base = (): DeployDraft =>
    emptyDraft(ws?.id ?? "", initial.project ?? NEW_PROJECT, defaultCluster);
  const withStorage = (d: DeployDraft): DeployDraft => ({
    ...d,
    volumes: d.volumes.map((v) => ({
      ...v,
      storageClass: v.storageClass || storageClassOf(d.clusterId),
    })),
  });
  const fromTemplate = (t: Template) =>
    withStorage(draftFromTemplate(t, base()));

  const preset = findTemplate(initial.template);
  // A link with ?repo= or ?image= opens straight on Configure, like pasting it.
  const imported = (patch?: Partial<DeployDraft>) => {
    if (!patch) return null;
    const d = { ...base(), ...patch, cron: !!initial.cron };
    return { ...d, name: suggestName(d) };
  };
  const [draft] = useState<DeployDraft | null>(() =>
    preset
      ? fromTemplate(preset)
      : initial.repo
        ? imported(gitImport(initial.repo).patch)
        : initial.image
          ? imported(imageImport(initial.image, s.registries).patch)
          : null,
  );

  // The URL says what's being deployed, so the page can be linked and
  // reloaded: each choice goes into it, and the page (keyed on it) follows.
  const navigate = useNavigate({ from: "/$team/deploy/new" });
  const go = (p: Partial<Initial>) =>
    void navigate({
      search: (prev) => ({ project: prev.project, cron: prev.cron, ...p }),
      replace: true,
    });

  if (!draft && initial.source === "compose" && !initial.cron)
    return <ComposeImport onBack={() => go({ source: undefined })} />;

  if (!draft)
    return (
      <StartScreen
        cron={!!initial.cron}
        initialKind={initial.source === "git" ? "git" : "image"}
        focusImport={initial.source === "git" || initial.source === "image"}
        recentIds={s.recentTemplateIds}
        onKind={(k) => go({ source: k })}
        compose={!initial.cron}
        onTemplate={(t) => go({ template: t.id })}
        onImport={(patch) =>
          go(
            patch.sourceType === "git"
              ? { repo: gitParam(patch) }
              : { image: imageParam(patch) },
          )
        }
        registries={s.registries}
      />
    );

  return (
    <Configure
      key={draft.sourceType + (draft.templateId ?? "")}
      initial={draft}
      onBack={() =>
        go({
          source:
            draft.sourceType === "template" ? undefined : draft.sourceType,
        })
      }
    />
  );
}

/** ?repo= for a Git draft: the URL, with "/tree/<branch>" unless it's main. */
export function gitParam(patch: Partial<DeployDraft>) {
  const g = patch.git;
  return g
    ? `${g.repoUrl}${g.branch && g.branch !== "main" ? `/tree/${g.branch}` : ""}`
    : "";
}

/** ?image= for an image draft: "repo:tag" or "repo@sha256:…". */
export function imageParam(patch: Partial<DeployDraft>) {
  const i = patch.image;
  return i
    ? `${i.image}${i.reference.startsWith("sha256:") ? "@" : ":"}${i.reference}`
    : "";
}

/** A Git URL as a draft: ".../tree/<branch>" picks that branch, else main. */
export function gitImport(raw: string): {
  patch?: Partial<DeployDraft>;
  error?: string;
} {
  const [, base = raw, branch = "main"] =
    /^(.*?)\/tree\/([^?#]+)/.exec(raw.trim()) ?? [];
  const url = base.trim().replace(/\/+$/, "");
  if (!/^https?:\/\/[^\s/]+\/\S+$/.test(url))
    return {
      error:
        "Enter the repository's HTTPS URL, e.g. https://github.com/owner/repo.",
    };
  const probe = emptyDraft("", "", "");
  // Port 8080 with a TCP check until the build reveals the EXPOSE'd port.
  return {
    patch: {
      sourceType: "git",
      git: {
        ...probe.git,
        repoUrl: url,
        branch: branch.replace(/\/+$/, "") || "main",
        builder: "dockerfile",
      },
      ...imageDefaults(""),
    },
  };
}

/** An image reference as a draft. */
export function imageImport(
  raw: string,
  registries: { id: string; host: string }[],
): { patch?: Partial<DeployDraft>; error?: string } {
  if (!raw.trim()) return { error: "Enter an image, e.g. nginx:1.27." };
  const probe = emptyDraft("", "", "");
  const parsed = parseImageInput(raw, registries);
  const issues = validateSection(
    "source",
    { ...probe, sourceType: "image", image: { ...probe.image, ...parsed } },
    { clusters: [], takenNames: [] },
  );
  if (issues.image || issues.reference)
    return { error: issues.image ?? issues.reference ?? "" };
  return {
    patch: {
      sourceType: "image",
      image: { ...probe.image, ...parsed, private: false },
      ...imageDefaults(parsed.image),
    },
  };
}

// ── Start ──────────────────────────────────────────────────────────────────

/** Paste a repository or an image. Also the first step of onboarding. */
export function ImportPanel({
  title = "Import",
  hint,
  initialKind,
  onKind,
  compose = false,
  focusImport,
  onImport,
  registries,
}: {
  title?: ReactNode;
  /** In place of the note under the form. */
  hint?: ReactNode;
  initialKind: "git" | "image";
  onKind: (k: "git" | "image" | "compose") => void;
  /** Offer a Docker Compose import (it opens a page of its own). */
  compose?: boolean;
  focusImport: boolean;
  onImport: (patch: Partial<DeployDraft>) => void;
  registries: { id: string; host: string }[];
}) {
  const s = usePyo();
  const [kind, setKind] = useState<"git" | "image">(initialKind);
  const [value, setValue] = useState("");
  const [error, setError] = useState("");

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const r = imageImport(value, registries);
    if (r.patch) onImport(r.patch);
    else setError(r.error ?? "");
  };

  const [repo, setRepo] = useState("");
  const hasBuilder = buildersIn(s).some((b) => b.status === "ready");
  const submitGit = (e: FormEvent) => {
    e.preventDefault();
    const r = gitImport(repo);
    if (r.patch) onImport(r.patch);
    else setError(r.error ?? "");
  };

  const switchKind = (k: "git" | "image") => {
    setKind(k);
    setValue("");
    setError("");
    onKind(k);
  };

  const field = (icon: ReactNode, input: ReactNode) => (
    <div className="flex flex-col gap-2 sm:flex-row">
      <div className="relative flex-1">
        <span className="pointer-events-none absolute left-3.5 top-1/2 -translate-y-1/2 text-muted-foreground">
          {icon}
        </span>
        {input}
      </div>
      <Button type="submit" size="lg">
        Continue <ArrowRightIcon size={14} />
      </Button>
    </div>
  );

  return (
    <Layered
      title={title}
      action={
        <Segmented
          label="Import from"
          size="xs"
          value={kind}
          onChange={(k) => (k === "compose" ? onKind("compose") : switchKind(k as "git" | "image"))}
          options={[
            {
              value: "git",
              label: (
                <span className="inline-flex items-center gap-1.5">
                  <BrandMark icon={GIT_LOGO} size={13} /> Git
                </span>
              ),
            },
            {
              value: "image",
              label: (
                <span className="inline-flex items-center gap-1.5">
                  <BrandMark icon={DOCKER_LOGO} size={13} /> Image
                </span>
              ),
            },
            ...(compose
              ? [
                  {
                    value: "compose",
                    label: (
                      <span className="inline-flex items-center gap-1.5">
                        <BrandMark icon={DOCKER_LOGO} size={13} /> Docker Compose
                      </span>
                    ),
                  },
                ]
              : []),
          ]}
        />
      }
      footer={
        kind === "git" ? (
          !hasBuilder ? (
            <>
              No builder is ready yet.{" "}
              <Link
                to="/$team/builders"
                params={{ team: currentSlug() }}
                className="font-medium text-foreground hover:text-primary"
              >
                Add a builder
              </Link>{" "}
              to build from Git.
            </>
          ) : (
            "Built with its Dockerfile or Containerfile, or the framework detected."
          )
        ) : (
          "Private images use your registry logins from Integrations."
        )
      }
    >
      {kind === "git" ? (
        <form onSubmit={submitGit} noValidate className="space-y-3 p-5">
          {field(
            <GitBranchIcon size={15} aria-hidden />,
            <Input
              mono
              autoFocus={focusImport}
              value={repo}
              onChange={(e) => {
                setRepo(e.target.value);
                setError("");
              }}
              placeholder="https://github.com/owner/repo"
              aria-label="Repository URL"
              aria-invalid={error ? true : undefined}
              className="h-11 pl-10"
            />,
          )}
          {error ? (
            <p role="alert" className="text-[12.5px] text-destructive">
              {error}
            </p>
          ) : null}
        </form>
      ) : (
        <form onSubmit={submit} noValidate className="space-y-3 p-5">
          {field(
            <PackageIcon size={15} aria-hidden />,
            <Input
              id="import-input"
              mono
              autoFocus={focusImport}
              value={value}
              onChange={(e) => {
                setValue(e.target.value);
                setError("");
              }}
              placeholder="registry/org/app:tag"
              aria-label="Image"
              aria-invalid={error ? true : undefined}
              aria-describedby={error ? "import-error" : undefined}
              className="h-11 pl-10"
            />,
          )}
          {error ? (
            <p
              id="import-error"
              role="alert"
              className="text-[12.5px] text-destructive"
            >
              {error}
            </p>
          ) : null}
        </form>
      )}
    </Layered>
  );
}

function StartScreen({
  cron,
  initialKind,
  onKind,
  compose,
  focusImport,
  recentIds,
  onTemplate,
  onImport,
  registries,
}: {
  cron: boolean;
  initialKind: "git" | "image";
  onKind: (k: "git" | "image" | "compose") => void;
  compose: boolean;
  focusImport: boolean;
  recentIds: string[];
  onTemplate: (t: Template) => void;
  onImport: (patch: Partial<DeployDraft>) => void;
  registries: { id: string; host: string }[];
}) {
  return (
    <Page width="max-w-3xl">
      <div className="flex flex-col items-center pt-4 text-center">
        <span className="flex h-16 w-16 items-center justify-center rounded-full border border-border bg-card text-primary shadow-sm">
          {cron ? (
            <TimerIcon size={26} weight="duotone" />
          ) : (
            <Logo showName={false} className="[&_svg]:h-7 [&_svg]:w-7" />
          )}
        </span>
        <h1 className="mt-5 text-[26px] font-semibold tracking-tight">
          {cron ? "Create a cron job" : "Deploy something new"}
        </h1>
        <p className="mt-1.5 text-[14px] text-muted-foreground">
          {cron
            ? "Bring a repository or an image to run on a schedule."
            : "Bring a repository or an image, or start from a template."}
        </p>
      </div>

      <ImportPanel
        initialKind={initialKind}
        onKind={onKind}
        compose={compose}
        focusImport={focusImport}
        onImport={onImport}
        registries={registries}
      />

      {cron ? null : (
        <Layered
          title="Start from a template"
          action={
            <Link
              to="/$team/templates"
              params={{ team: currentSlug() }}
              className="text-[12.5px] font-medium text-muted-foreground transition-colors hover:text-foreground"
            >
              Browse with details
            </Link>
          }
          bare
        >
          <TemplateGallery
            previews={false}
            simple
            recentIds={recentIds}
            onSelect={onTemplate}
          />
        </Layered>
      )}
    </Page>
  );
}

// ── Configure ──────────────────────────────────────────────────────────────

function Configure({
  initial,
  onBack,
}: {
  initial: DeployDraft;
  onBack: () => void;
}) {
  const s = usePyo();
  const navigate = useNavigate();
  const [d, setDraft] = useState(initial);
  const [attempted, setAttempted] = useState(false);
  const formRef = useRef<HTMLDivElement>(null);
  const set = (patch: Partial<DeployDraft>) =>
    setDraft((cur) => ({ ...cur, ...patch }));

  // The project's services, so the Environment section can flag a
  // ${{service.VAR}} reference that can't be filled in.
  const providers = useMemo(
    () => s.services.filter((a) => a.projectId === d.projectId).map((a) => providerOfService(a)),
    [s.services, d.projectId],
  );
  const sections = useDraftSections(d, set, attempted, [], providers);
  const { t, all, count, shown, header, summaries, rows, icons, runsOn, project } = sections;

  // A template's databases: each picked or made, connected on create.
  const reqs = t?.databases ?? [];
  const [dbs, setDbs] = useState<DbChoice[]>(() =>
    (getTemplate(initial.templateId)?.databases ?? []).map(initialDbChoice),
  );
  const catalog = useExtensionCatalog();
  const dbProblems = useDbProblems(reqs, dbs, d.clusterId, d.name, catalog);
  const [dbError, setDbError] = useState("");
  const [creating, setCreating] = useState(false);
  const clusterName = s.clusters.find((c) => c.id === d.clusterId)?.name ?? "the cluster";

  // A very large replica count asks first (the API holds it back otherwise).
  const free = d.cron
    ? null
    : freeRoom(s.servers, d.clusterId, {
        servers: d.placement === "servers" ? d.serverNames : [],
        serverTags: d.placement === "tags" ? d.serverTags : [],
        gpu: d.resources.gpu,
      });
  const big =
    !d.cron && needsConfirm(d.replicas, 0, free ? fitting(d.resources, 0, free) : null)
      ? (roomWarning("run", d.replicas, d.resources, 0, free) ?? "")
      : null;
  const [asking, setAsking] = useState(false);

  const deploy = async (confirm = false) => {
    setAttempted(true);
    if (count || dbProblems.some(Boolean)) {
      // Bring the first problem into view.
      requestAnimationFrame(() =>
        formRef.current
          ?.querySelector<HTMLElement>("[aria-invalid=true], [data-error=true]")
          ?.scrollIntoView({ behavior: "smooth", block: "center" }),
      );
      return;
    }
    if (big !== null && !confirm) {
      setAsking(true);
      return;
    }
    const databases: { id: string; env: Record<string, string> }[] = [];
    if (t && reqs.length) {
      setCreating(true);
      setDbError("");
      const next = [...dbs];
      // Which of the extensions it needs Pyo turns on: from the catalog.
      let known = catalog;
      if (!known && reqs.some((r) => r.extensions?.length))
        known = await fetchDatabaseExtensions().then((r) => r.extensions, () => null);
      if (!known && reqs.some((r) => r.extensions?.length)) {
        setCreating(false);
        setDbError("Couldn't load the database extensions. Try again.");
        return;
      }
      for (const [i, req] of reqs.entries()) {
        const existing = s.databases.find((x) => x.id === next[i].existingId);
        const r = await prepareDatabase(req, next[i], d.clusterId, d.name, existing, known ?? []);
        if ("error" in r) {
          setCreating(false);
          setDbs(next);
          setDbError(r.error);
          requestAnimationFrame(() => document.getElementById(`deploy-database-${i}`)?.scrollIntoView({ behavior: "smooth", block: "center" }));
          return;
        }
        // Made now: picked from here on, so a retry doesn't make another.
        if (next[i].mode === "new") next[i] = { ...next[i], mode: "existing", existingId: r.id };
        databases.push({ id: r.id, env: req.env });
      }
      setDbs(next);
      setCreating(false);
    }
    // Services that come with it (Plausible's ClickHouse), first: its
    // variables point at them.
    let target = d;
    if (t?.companions.length) {
      setCreating(true);
      // One new project for them all, made first.
      if (d.projectId === NEW_PROJECT) {
        const p = api.startProject({ ...newProjectFor(s, d.name, d.teamId), description: "" });
        if (!(await p.done)) {
          setCreating(false);
          setDbError("Couldn't create the project.");
          return;
        }
        target = { ...d, projectId: p.id };
      }
      for (const c of t.companions) {
        const registryId = parseImageInput(c.image, s.registries).registryId;
        const { done } = api.createService(companionDraft(t, c, target, registryId));
        if (!(await done)) {
          setCreating(false);
          setDbError(`Couldn't create ${companionService(t, c, d.name)}.`);
          return;
        }
      }
      setCreating(false);
    }
    const { deploymentId } = api.createService(target, { databases, confirm });
    toast({ tone: "info", title: `Deploying ${d.name}` });
    void navigate({
      to: "/$team/deployments/$deploymentId",
      params: { team: currentSlug(), deploymentId },
    });
  };

  const plan = stagePlan(
    d.sourceType === "git"
      ? { ...d.git, type: "git", builder: "dockerfile", webhook: "inactive" }
      : { type: "image", ...d.image },
    "manual",
  );

  // Rows open on a click; one with a problem (after Deploy) opens by itself,
  // and so does Volumes for a template that keeps data.
  // A new cron job starts at its schedule.
  const [open, setOpen] = useState<Partial<Record<Collapsible, boolean>>>(
    initial.cron ? { schedule: true } : {},
  );
  const bad = (k: Collapsible) => attempted && !!Object.keys(all[k]).length;
  const isOpen = (k: Collapsible) =>
    bad(k) || (open[k] ?? ((k === "volumes" && !!t?.storage?.required) || (k === "domains" && "domains" in all.domains)));
  const toggle = (k: Collapsible) =>
    setOpen((o) => ({ ...o, [k]: !isOpen(k) }));
  return (
    <Page width="max-w-2xl">
      <div>
        <Button variant="ghost" size="sm" onClick={onBack} className="-ml-3">
          <ArrowLeftIcon size={13} /> Back
        </Button>
        <div className="mt-6 flex flex-col items-center text-center">
          <span className="flex h-16 w-16 items-center justify-center rounded-full border border-border bg-card shadow-sm [&>*]:border-0 [&>*]:bg-transparent">
            {header.icon}
          </span>
          <h1 className="mt-5 max-w-full truncate text-[24px] font-semibold tracking-tight">
            Deploy {header.title}
          </h1>
          <p className="mt-1.5 max-w-md text-[14px] text-muted-foreground">
            {header.sub}
          </p>
        </div>
      </div>

      <div ref={formRef} className="space-y-4">
        <Block
          id="general"
          icon={<ProjectIcon size={15} />}
          title="Project & name"
          description={project ? `${project.isNew ? "New project" : "In"} ${project.name}${d.name ? ` · ${d.name}` : ""}` : "Pick a project"}
          error={attempted && ["name", "projectId"].some((k) => k in all.general)}
        >
          <GeneralFields d={d} set={set} errors={shown("general")} part="name" />
        </Block>
        <Block
          id="placement"
          icon={<MapPinIcon size={15} />}
          title="Where it runs"
          description={runsOn ? `Runs ${runsOn.startsWith("anywhere") ? runsOn : `on ${runsOn}`}` : "Pick where it runs"}
          error={attempted && ["clusterId", "placement"].some((k) => k in all.general)}
        >
          <GeneralFields d={d} set={set} errors={shown("general")} part="placement" />
        </Block>
        {t && t.fields.length ? (
          <Block
            id="template"
            icon={<SlidersHorizontalIcon size={15} />}
            title={`${t.name} settings`}
            description={t.fields.map((f) => f.label).slice(0, 3).join(", ")}
            error={attempted && Object.keys(all.general).some((k) => k.startsWith("field."))}
          >
            <div className="space-y-4">
              <TemplateFields d={d} set={set} errors={shown("general")} />
              {t.hints.length ? (
                <ul className="space-y-1 border-t border-border pt-3 text-[12.5px] text-muted-foreground">
                  {t.hints.map((h) => (
                    <li key={h}>{h}</li>
                  ))}
                </ul>
              ) : null}
            </div>
          </Block>
        ) : null}

        {t
          ? reqs.map((req, i) =>
              dbs[i] ? (
                <Block
                  key={req.engine}
                  id={`database-${i}`}
                  icon={<DatabaseIcon size={15} />}
                  title={reqs.length > 1 ? `${ENGINE_LABEL[req.engine]} database` : "Database"}
                  description={dbSummary(req, dbs[i], d.name, s.databases.find((x) => x.id === dbs[i].existingId))}
                  error={(attempted && !!dbProblems[i]) || !!dbError}
                >
                  <div className="space-y-3">
                    <TemplateDatabaseFields
                      req={req}
                      clusterId={d.clusterId}
                      clusterName={clusterName}
                      service={d.name}
                      value={dbs[i]}
                      onChange={(v) => {
                        setDbs((cur) => cur.map((x, j) => (j === i ? v : x)));
                        setDbError("");
                      }}
                      problem={attempted ? dbProblems[i] : ""}
                      catalog={catalog}
                    />
                    {dbError ? (
                      <p className="text-[12.5px] text-destructive" role="alert">
                        {dbError}
                      </p>
                    ) : null}
                  </div>
                </Block>
              ) : null,
            )
          : null}

        <div className="space-y-2 pt-2">
          {rows.map(({ k, title, body, action, fixed }) => (
            <Block
              key={k}
              id={k}
              icon={icons[k]}
              title={title}
              description={bad(k) ? "Needs attention" : summaries[k]}
              optional={k !== "build" && !(fixed && body)}
              open={fixed ? !!body : isOpen(k)}
              error={bad(k)}
              onToggle={fixed ? undefined : () => toggle(k)}
              action={action}
            >
              {body}
            </Block>
          ))}
        </div>

        <div className="pt-4">
          <DeploySummary
            d={d}
            sections={sections}
            action={
              <>
              <ConfirmDialog
                open={asking}
                onClose={() => setAsking(false)}
                tone="primary"
                title={`Start ${d.replicas} replicas?`}
                description={big ?? ""}
                confirmLabel="Deploy"
                onConfirm={() => {
                  setAsking(false);
                  void deploy(true);
                }}
              />
              <Button size="lg" className="h-12 w-full text-[15px]" disabled={creating} onClick={() => void deploy()}>
                <RocketLaunchIcon size={17} weight="fill" /> {creating ? "Creating…" : "Deploy"}
                {d.name && !creating ? ` ${d.name}` : ""}
              </Button>
              </>
            }
            note={
              attempted && (count || dbProblems.some(Boolean)) ? (
                <p className="text-center text-[12.5px] text-destructive" role="alert">
                  {count ? `Fix ${count} field${count === 1 ? "" : "s"} to deploy.` : dbProblems.find(Boolean)}
                </p>
              ) : (
                <p className="text-center text-[12px] text-muted-foreground">
                  You can change all of this later in the service&apos;s settings.
                </p>
              )
            }
          />
        </div>
      </div>
    </Page>
  );
}


function Card({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="rounded-[14px] border border-card-border bg-card p-5">
      <h2 className="mb-4 text-[15px] font-semibold">{title}</h2>
      {children}
    </section>
  );
}

/** A row of the checkout: its title and what it's set to; click to change it. */
function Fold({
  id,
  icon,
  title,
  summary,
  open,
  error,
  onToggle,
  children,
}: {
  id: string;
  icon: ReactNode;
  title: string;
  summary: string;
  open: boolean;
  error?: boolean;
  onToggle: () => void;
  children: ReactNode;
}) {
  return (
    <section
      id={`deploy-${id}`}
      data-error={error || undefined}
      className="scroll-mt-20"
    >
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className="flex w-full items-center gap-3 px-5 py-3.5 text-left transition-colors hover:bg-accent/30"
      >
        <span
          className={`flex h-7 w-7 shrink-0 items-center justify-center rounded-full border ${error ? "border-destructive/40 text-destructive" : "border-border text-muted-foreground"}`}
        >
          {icon}
        </span>
        <span className="text-[14px] font-medium">{title}</span>
        <span
          className={`ml-auto min-w-0 truncate text-[12.5px] ${error ? "text-destructive" : "text-muted-foreground"}`}
        >
          {error ? "Needs attention" : summary}
        </span>
        <CaretRightIcon
          size={13}
          className={`shrink-0 text-muted-foreground transition-transform duration-200 ${open ? "rotate-90" : ""}`}
        />
      </button>
      {open ? <div className="px-5 pb-5 pt-1">{children}</div> : null}
    </section>
  );
}
