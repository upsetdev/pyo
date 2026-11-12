import type { Provider } from "~/lib/pyo/refs";
import {
  BracketsCurlyIcon,
  CpuIcon,
  DatabaseIcon,
  GitBranchIcon,
  GlobeIcon,
  HeartbeatIcon,
  PackageIcon,
  PlugsIcon,
  TimerIcon,
} from "@phosphor-icons/react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { fetchBuildFile } from "~/lib/pyo/backend";
import {
  validateSection,
  type DeployDraft,
  type Issues,
  type Section,
} from "~/lib/pyo/draft";
import { cpu, describeCron, mem } from "~/lib/pyo/format";
import { Button } from "~/components/ui/Button";
import { clustersIn, draftProject, usePyo } from "~/lib/pyo/store";
import { locationLabel, placesOf } from "~/lib/pyo/location";
import { refsIn } from "~/lib/pyo/refs";
import { CaretRightIcon } from "@phosphor-icons/react";
import { BLOCK } from "./style";
import { getTemplate } from "~/lib/pyo/templates";
import { BrandMark, DOCKER_LOGO, GIT_LOGO, IconTile, TemplateLogo } from "../common";
import {
  DomainsStep,
  EnvStep,
  HealthStep,
  NetworkingStep,
  ResourcesStep,
  ScheduleStep,
  SourceStep,
  VolumesStep,
  sizeName,
} from "./steps";

// What a draft's settings are, section by section: the wizard's Configure
// screen and the deploy canvas's side panel show the same ones, with the
// same checks and summaries.

export type Collapsible =
  | "build"
  | "schedule"
  | "env"
  | "resources"
  | "health"
  | "volumes"
  | "domains"
  | "networking";

export const SECTION_CHECKS: Record<Collapsible | "general", Section[]> = {
  general: ["basics", "target"],
  build: ["source"],
  env: ["env"],
  schedule: ["schedule"],
  resources: ["resources"],
  health: ["health"],
  volumes: ["volumes"],
  domains: ["domains"],
  networking: ["networking"],
};


/**
 * `attempted`: show problems (after a first Deploy). `otherNames`: names of
 * other drafts going into the same project, which are taken too.
 */
/** "owner/name" of a GitHub repository URL. */
function githubRepo(url: string) {
  const m = /^https?:\/\/github\.com\/([\w.-]+)\/([\w.-]+?)(?:\.git)?\/?$/i.exec(
    url.trim(),
  );
  return m ? `${m[1]}/${m[2]}` : null;
}

/**
 * The file a Git build of the repository would use by default: "Dockerfile"
 * or "Containerfile", null for neither (zero config), undefined while it isn't
 * known (no team, not on GitHub, or GitHub couldn't be asked).
 */
function useBuildFile(
  team: string,
  repoUrl: string,
  branch: string,
  context: string,
) {
  const repo = githubRepo(repoUrl);
  const key =
    team && repo && branch.trim()
      ? JSON.stringify([team, repo, branch.trim(), context.trim() || "."])
      : "";
  const [found, setFound] = useState<{
    key: string;
    file: "Dockerfile" | "Containerfile" | null;
  }>();
  useEffect(() => {
    if (!key) return;
    let cancelled = false;
    // Typing a URL changes it on every key: ask once it settles.
    const t = setTimeout(() => {
      const [team, repo, branch, context] = JSON.parse(key) as string[];
      fetchBuildFile({ team, repo, branch, context })
        .then((r) => !cancelled && setFound({ key, file: r.file }))
        .catch(() => {});
    }, 400);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [key]);
  return key && found?.key === key ? found.file : undefined;
}

export function useDraftSections(
  d: DeployDraft,
  set: (patch: Partial<DeployDraft>) => void,
  attempted: boolean,
  otherNames: string[] = [],
  providers?: Provider[],
) {
  const s = usePyo();
  // The builder decides when it has the code: the Dockerfile, else the
  // Containerfile, otherwise Railpack's zero-config detection. For a GitHub
  // repository, the wizard looks ahead.
  const customPath =
    d.git.dockerfilePath.trim() && d.git.dockerfilePath.trim() !== "Dockerfile";
  const found = useBuildFile(
    d.sourceType === "git" && !customPath ? d.teamId : "",
    d.git.repoUrl,
    d.git.branch,
    d.git.buildContext,
  );
  const buildText = customPath
    ? `Dockerfile at ${d.git.dockerfilePath.trim()}`
    : found === undefined
      ? "Auto-detect"
      : (found ?? "Zero config");
  const t = getTemplate(d.templateId);
  const clusters = clustersIn(s);
  // A new project has no services yet, so any name is free there.
  const ctx = useMemo(
    () => ({
      clusters,
      takenNames: s.services
        .filter((a) => a.projectId === d.projectId)
        .map((a) => a.name)
        .concat(otherNames),
      takenHosts: Object.fromEntries(
        s.services.flatMap((a) =>
          a.domains.map((x) => [x.hostname.toLowerCase(), a.name]),
        ),
      ),
      providers,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- names by value
    [clusters, d.projectId, s.services, otherNames.join(), providers],
  );
  const issuesOf = (k: Collapsible | "general"): Issues =>
    SECTION_CHECKS[k].reduce<Issues>(
      (acc, sec) => ({ ...acc, ...validateSection(sec, d, ctx) }),
      {},
    );
  const all = {
    general: issuesOf("general"),
    build: issuesOf("build"),
    schedule: issuesOf("schedule"),
    env: issuesOf("env"),
    resources: issuesOf("resources"),
    health: issuesOf("health"),
    volumes: issuesOf("volumes"),
    domains: issuesOf("domains"),
    networking: issuesOf("networking"),
  };
  const count = Object.values(all).reduce(
    (n, i) => n + Object.keys(i).length,
    0,
  );
  const shown = (k: keyof typeof all) => (attempted ? all[k] : {});

  const repo = d.git.repoUrl
    .replace(/^https:\/\/(www\.)?github\.com\//, "")
    .replace(/\.git$/, "");
  const imageShort = `${d.image.image.replace(/^docker\.io\/(library\/)?/, "")}${d.image.reference.startsWith("sha256:") ? "@" : ":"}${d.image.reference}`;
  const header =
    d.sourceType === "template"
      ? {
          icon: t ? (
            <TemplateLogo template={t} size="lg" />
          ) : (
            <IconTile icon={PackageIcon} size="lg" />
          ),
          title: t?.name ?? "Template",
          sub: `${t?.image}:${d.templateValues.version ?? t?.version}`,
        }
      : d.sourceType === "git"
        ? {
            icon: <LogoTile icon={GIT_LOGO} />,
            title: repo,
            sub: `${d.git.branch} · ${buildText}`,
          }
        : {
            icon: <LogoTile icon={DOCKER_LOGO} />,
            title: imageShort,
            sub:
              s.registries.find((r) => r.id === d.image.registryId)?.name ??
              "",
          };

  const secrets = d.env.filter((e) => e.secret).length;
  const exposed =
    d.domains.length ||
    d.ports.some(
      (p) => p.exposure === "nodeport" || p.exposure === "loadbalancer",
    );
  const summaries: Record<Collapsible, string> = {
    schedule: d.cron ? describeCron(d.schedule) || `${d.schedule} UTC` : "Not a cron job: it runs continuously",
    build:
      d.sourceType === "git"
        ? `${d.git.branch}${d.git.commit ? ` @ ${d.git.commit.slice(0, 7)}` : ""} · ${buildText}${d.git.private ? " · private" : ""}`
        : `${imageShort}${d.image.private ? " · private" : ""}`,
    env: d.env.length
      ? `${d.env.length} variable${d.env.length === 1 ? "" : "s"}${secrets ? ` · ${secrets} secret${secrets === 1 ? "" : "s"}` : ""}`
      : "None",
    resources: `${sizeName(d.resources) ?? `${cpu(d.resources.cpuRequest)} · ${mem(d.resources.memoryRequest)}`} · ${d.replicas} replica${d.replicas === 1 ? "" : "s"}`,
    health:
      d.healthCheck.type === "http"
        ? `HTTP ${d.healthCheck.path} on ${d.healthCheck.port}`
        : d.healthCheck.type === "tcp"
          ? `TCP on ${d.healthCheck.port}`
          : d.healthCheck.type === "exec"
            ? `Command: ${d.healthCheck.command}`
            : "None",
    volumes: d.volumes.length
      ? d.volumes.map((v) => `${v.mountPath} · ${v.sizeGi} GB`).join(", ")
      : "None",
    domains: d.domains.some((x) => x.hostname)
      ? d.domains
          .map((x) => x.hostname)
          .filter(Boolean)
          .join(", ")
      : d.domains.length
        ? `${d.domains.length} domain${d.domains.length === 1 ? "" : "s"}`
        : "None",
    networking: d.ports.length
      ? `${exposed ? "Public" : "Private"} · ${d.ports.map((p) => `${p.containerPort}/${p.protocol}`).join(", ")}`
      : "No ports",
  };

  // A cron job is added and removed from its block's header; once added its
  // fields show straight away (no switch inside).
  const cronAction = (
    <Button
      size="xs"
      variant={d.cron ? "ghost" : "secondary"}
      onClick={(e) => {
        e.stopPropagation();
        set({ cron: !d.cron });
      }}
    >
      {d.cron ? "Remove" : "Add cron job"}
    </Button>
  );
  const sections: { k: Collapsible; title: string; body: ReactNode; action?: ReactNode; fixed?: boolean }[] = [
    ...(d.sourceType !== "template"
      ? [
          {
            k: "build" as const,
            title: d.sourceType === "git" ? "Build" : "Image",
            body: <SourceStep d={d} set={set} errors={shown("build")} />,
          },
        ]
      : []),
    ...(d.sourceType !== "template"
      ? [
          {
            k: "schedule" as const,
            title: "Cron job",
            body: d.cron ? <ScheduleStep d={d} set={set} errors={shown("schedule")} /> : null,
            action: cronAction,
            fixed: true,
          },
        ]
      : []),
    {
      k: "env",
      title: "Environment variables",
      body: <EnvStep d={d} set={set} errors={shown("env")} />,
    },
    {
      k: "resources",
      title: "Resources",
      body: <ResourcesStep d={d} set={set} errors={shown("resources")} />,
    },
    {
      k: "health",
      title: "Health check",
      body: <HealthStep d={d} set={set} errors={shown("health")} />,
    },
    {
      k: "volumes",
      title: "Volumes",
      body: <VolumesStep d={d} set={set} errors={shown("volumes")} />,
    },
    {
      k: "domains",
      title: "Domains",
      body: <DomainsStep d={d} set={set} errors={shown("domains")} />,
    },
    {
      k: "networking",
      title: "Networking",
      body: <NetworkingStep d={d} set={set} errors={shown("networking")} />,
    },
  ];
  // Nothing can reach a cron job, and it needs no health check.
  const rows = sections.filter(
    (r) => !d.cron || (r.k !== "domains" && r.k !== "networking" && r.k !== "health"),
  );

  const icons: Record<Collapsible, ReactNode> = {
    build:
      d.sourceType === "git" ? (
        <GitBranchIcon size={15} />
      ) : (
        <PackageIcon size={15} />
      ),
    schedule: <TimerIcon size={15} />,
    env: <BracketsCurlyIcon size={15} />,
    resources: <CpuIcon size={15} />,
    health: <HeartbeatIcon size={15} />,
    volumes: <DatabaseIcon size={15} />,
    domains: <GlobeIcon size={15} />,
    networking: <PlugsIcon size={15} />,
  };


  // Where it will run, in words.
  const cluster = s.clusters.find((c) => c.id === d.clusterId);
  const runsOn = !cluster
    ? ""
    : d.placement === "tags"
      ? d.serverTags.length
        ? `servers tagged ${d.serverTags.join(" + ")}`
        : "tagged servers"
      : d.placement === "servers"
        ? d.serverNames.join(", ") || "picked servers"
        : d.location
          ? locationLabel(d.location)
          : placesOf(cluster, s.servers).length > 1
            ? `anywhere in ${cluster.name}`
            : locationLabel(placesOf(cluster, s.servers)[0]?.location ?? "");
  const project = draftProject(s, d);

  return { t, all, count, shown, header, summaries, rows, icons, exposed, buildText, runsOn, cluster, project };
}

export function LogoTile({ icon }: { icon: Parameters<typeof BrandMark>[0]["icon"] }) {
  return (
    <span
      aria-hidden
      className="flex h-10 w-10 shrink-0 items-center justify-center rounded-md border border-border bg-muted/60 text-foreground/85"
    >
      <BrandMark icon={icon} size={20} />
    </span>
  );
}

/**
 * One part of the checkout: a header row (icon, title, what it's set to or
 * "Optional", a problem badge) over its fields. With `onToggle` the header
 * opens and closes it.
 */
export function Block({
  id,
  icon,
  title,
  description,
  optional = false,
  error = false,
  open = true,
  onToggle,
  action,
  children,
}: {
  /** A button on the header's right (beside, not inside, the toggle). */
  action?: ReactNode;
  id?: string;
  icon: ReactNode;
  title: string;
  description?: ReactNode;
  optional?: boolean;
  error?: boolean;
  open?: boolean;
  onToggle?: () => void;
  children?: ReactNode;
}) {
  const head = (
    <>
      <span
        aria-hidden
        className={`flex h-8 w-8 shrink-0 items-center justify-center rounded-lg ${error ? "bg-destructive/10 text-destructive" : "bg-muted text-foreground/75"}`}
      >
        {icon}
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-2">
          <span className="truncate text-[14px] font-semibold">{title}</span>
          {optional && !error ? (
            <span className="shrink-0 rounded-full bg-muted px-1.5 py-px text-[10.5px] font-medium text-muted-foreground">
              Optional
            </span>
          ) : null}
        </span>
        {description ? (
          <span className="block truncate text-[12.5px] text-muted-foreground">{description}</span>
        ) : null}
      </span>
      {error ? (
        <span className="flex shrink-0 items-center gap-1.5 rounded-full bg-destructive/10 px-2 py-0.5 text-[11.5px] font-medium text-destructive">
          <span className="h-1.5 w-1.5 rounded-full bg-destructive" aria-hidden />
          Needs attention
        </span>
      ) : null}
      {onToggle ? (
        <CaretRightIcon
          size={13}
          aria-hidden
          className={`shrink-0 text-muted-foreground transition-transform duration-200 ${open ? "rotate-90" : ""}`}
        />
      ) : null}
    </>
  );
  return (
    <section
      id={id ? `deploy-${id}` : undefined}
      aria-label={title}
      data-error={error || undefined}
      className={`${BLOCK} scroll-mt-20 ${error ? "border-destructive/50" : ""}`}
    >
      <div className="flex items-center">
        {onToggle ? (
          <button
            type="button"
            onClick={onToggle}
            aria-expanded={open}
            className="flex min-w-0 flex-1 items-center gap-3 px-4 py-3 text-left transition-colors hover:bg-accent/30"
          >
            {head}
          </button>
        ) : (
          <div className="flex min-w-0 flex-1 items-center gap-3 px-4 py-3">{head}</div>
        )}
        {action ? <div className="shrink-0 pr-3">{action}</div> : null}
      </div>
      {open && children ? (
        <div className="px-4 pb-4 pt-1">{children}</div>
      ) : null}
    </section>
  );
}

/** What Deploy will create, as aligned key/value rows. */
export function DeploySummary({
  d,
  sections,
  action,
  note,
}: {
  d: DeployDraft;
  sections: ReturnType<typeof useDraftSections>;
  /** The Deploy button, set apart at the bottom. */
  action?: ReactNode;
  note?: ReactNode;
}) {
  const { header, runsOn, project, exposed, cluster } = sections;
  const r = d.resources;
  const refs = d.env.filter((e) => refsIn(e.value).length).length;
  const secrets = d.env.filter((e) => e.secret).length;
  const domains = d.domains.map((x) => x.hostname).filter(Boolean);
  const rows: [string, ReactNode][] = [
    ["Service", <span className="font-mono">{d.name || "—"}</span>],
    ["Source", header.title],
    ["Project", project ? `${project.name}${project.isNew ? " (new)" : ""}` : "—"],
    ["Runs on", runsOn ? `${runsOn}${cluster && !runsOn.includes(cluster.name) ? ` · ${cluster.name}` : ""}` : "—"],
    ["Reachable", domains.length ? domains.join(", ") : exposed ? "Public" : "Inside the project only"],
    ...(d.cron ? ([["Cron job", describeCron(d.schedule) || `${d.schedule} UTC`]] as [string, ReactNode][]) : []),
    ["Replicas", String(d.replicas)],
    ["CPU", `${cpu(r.cpuRequest)} reserved · up to ${cpu(r.cpuLimit)}`],
    ["Memory", `${mem(r.memoryRequest)} reserved · up to ${mem(r.memoryLimit)}`],
    ...(r.gpu ? ([["GPU", String(r.gpu)]] as [string, ReactNode][]) : []),
    ["Volumes", d.volumes.length ? d.volumes.map((v) => `${v.mountPath} · ${v.sizeGi} GB`).join(", ") : "None"],
    [
      "Variables",
      d.env.length
        ? `${d.env.length}${secrets ? ` · ${secrets} secret` : ""}${refs ? ` · ${refs} connected` : ""}`
        : "None",
    ],
  ];
  return (
    <section aria-label="Summary" className={BLOCK}>
      <div className="flex items-center gap-3 px-4 pb-1 pt-3">
        <span className="min-w-0 flex-1">
          <span className="block text-[14px] font-semibold">Summary</span>
          <span className="block text-[12.5px] text-muted-foreground">What Deploy creates</span>
        </span>
      </div>
      <dl className="grid grid-cols-[7.5rem_minmax(0,1fr)] gap-x-4 px-4 py-3 text-[13px]">
        {rows.map(([k, v]) => (
          <div key={k} className="contents">
            <dt className="py-1.5 text-muted-foreground">{k}</dt>
            <dd className="min-w-0 truncate py-1.5 text-right tabular-nums">{v}</dd>
          </div>
        ))}
      </dl>
      {action || note ? (
        <div className="space-y-2 bg-muted/50 px-4 py-4">
          {action}
          {note}
        </div>
      ) : null}
    </section>
  );
}
