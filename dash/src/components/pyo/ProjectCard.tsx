import { Segmented } from "~/components/ui/Field";
import {
  ListBulletsIcon,
  PackageIcon,
  SquaresFourIcon,
} from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { plural, timeAgo } from "~/lib/pyo/format";
import { fetchAnalytics } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { isActive, STAGE_LABEL } from "~/lib/pyo/simulation";
import {
  activeDeploymentFor,
  deploymentsWhere,
  usePyo,
  type projectStats,
  projectParams,
  serviceParams,
} from "~/lib/pyo/store";
import { getTemplate } from "~/lib/pyo/templates";
import { serviceLabel, ServiceStatusBadge, serviceTone, Dot, waitingForRoom } from "~/components/ui/Badge";
import { Tooltip } from "~/components/ui/Tooltip";
import { PlaceFlags } from "~/components/ui/Flag";
import { placesOf } from "~/lib/pyo/location";
import type {
  Application,
  ServiceSource,
  Deployment,
  Project,
} from "~/lib/pyo/types";
import type { Icon } from "@phosphor-icons/react";
import {
  siAmazonwebservices,
  siAstro,
  siBun,
  siDeno,
  siDigitalocean,
  siDjango,
  siDocker,
  siDotnet,
  siElixir,
  siFlask,
  siGithub,
  siGitlab,
  siGo,
  siGooglecloud,
  siGrafana,
  siHtml5,
  siJfrog,
  siLaravel,
  siMinio,
  siMongodb,
  siMysql,
  siNextdotjs,
  siNginx,
  siNodedotjs,
  siOpenjdk,
  siPhp,
  siPostgresql,
  siPrometheus,
  siPython,
  siReact,
  siRedhat,
  siRedis,
  siRemix,
  siRuby,
  siRubyonrails,
  siRust,
  siSvelte,
  siVite,
  type SimpleIcon,
} from "simple-icons";
import {
  BrandMark,
  GITHUB_LOGO,
  CATEGORY_ICONS,
} from "./common";
import { currentSlug } from "~/lib/pyo/paths";

// Vercel-style project and service summaries, shared by the overview, the
// Projects page and each project's page.

type Stats = ReturnType<typeof projectStats>;

/** A repository's web page, and its branch and commit pages (GitHub, GitLab; other hosts just the repo). */
export function gitLinks(repoUrl: string, branch: string, commit?: string) {
  const repo = repoUrl.replace(/\.git$/, "").replace(/\/$/, "");
  const host = /^https?:\/\/([^/]+)/.exec(repo)?.[1] ?? "";
  const sep = host === "gitlab.com" ? "/-" : host === "github.com" ? "" : null;
  return {
    repo,
    branch:
      sep === null ? repo : `${repo}${sep}/tree/${encodeURIComponent(branch)}`,
    commit:
      sep === null || !commit ? undefined : `${repo}${sep}/commit/${commit}`,
  };
}

/**
 * An image as people write it: Docker Hub images without the registry
 * ("valkey/valkey", "redis"), others in full ("ghcr.io/owner/app"). The tag
 * isn't part of it (the label shows it separately).
 */
export function imageLabel(image: string) {
  const parts = image
    .replace(/@.*$/, "")
    .replace(/:[^/]*$/, "")
    .split("/");
  const hosted = parts.length > 1 && /[.:]/.test(parts[0]);
  if (hosted && parts[0] !== "docker.io") return parts.join("/");
  if (hosted) parts.shift();
  if (parts[0] === "library") parts.shift();
  return parts.join("/");
}

/** The registry host of an image ("docker.io" when it names none). */
export function imageHost(image: string) {
  const first = image.split("/")[0];
  return image.includes("/") && (/[.:]/.test(first) || first === "localhost")
    ? first.toLowerCase()
    : "docker.io";
}

/** The registry's logo, for the ones simple-icons has; undefined otherwise (private ones). */
export function registryIcon(host: string): SimpleIcon | undefined {
  if (host === "docker.io" || host === "index.docker.io") return siDocker;
  if (host === "ghcr.io") return siGithub;
  if (host === "registry.gitlab.com") return siGitlab;
  if (host === "quay.io") return siRedhat;
  if (host === "public.ecr.aws" || /\.ecr\.[\w-]+\.amazonaws\.com$/.test(host))
    return siAmazonwebservices;
  if (
    host === "gcr.io" ||
    host.endsWith(".gcr.io") ||
    host.endsWith("-docker.pkg.dev")
  )
    return siGooglecloud;
  if (host === "registry.digitalocean.com") return siDigitalocean;
  if (host.endsWith(".jfrog.io")) return siJfrog;
  return undefined;
}

/** The registry's web page for an image, for registries that have one. */
export function imagePage(image: string) {
  const ref = image.replace(/[@:][^/]*$/, "");
  const [first, ...rest] = ref.split("/");
  // No registry host (no dot or port in the first part): Docker Hub.
  if (!rest.length || !/[.:]/.test(first) || first === "docker.io") {
    const path = first === "docker.io" ? rest : [first, ...rest];
    const p = path[0] === "library" ? path.slice(1) : path;
    return p.length === 1
      ? `https://hub.docker.com/_/${p[0]}`
      : `https://hub.docker.com/r/${p.join("/")}`;
  }
  const path = rest.join("/");
  switch (first) {
    case "ghcr.io":
      return `https://ghcr.io/${path}`; // redirects to the GitHub package page
    case "quay.io":
      return `https://quay.io/repository/${path}`;
    case "registry.gitlab.com":
      return `https://gitlab.com/${rest.slice(0, 2).join("/")}/container_registry`;
    case "public.ecr.aws":
      return `https://gallery.ecr.aws/${path}`;
    case "mcr.microsoft.com":
      return `https://mcr.microsoft.com/artifact/mar/${path}`;
    case "gcr.io":
      return `https://gcr.io/${path}`;
    default:
      return undefined; // private registries have no public page
  }
}

const PILL =
  "inline-flex max-w-full items-center gap-1.5 rounded-full bg-muted px-2.5 py-1 text-[12px] font-medium text-foreground/90";

/** A rounded label, a link (above a card's own link) when it has somewhere to go. */
export function Pill({
  href,
  title,
  className = "",
  children,
}: {
  href?: string;
  title?: string;
  className?: string;
  children: ReactNode;
}) {
  return href ? (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      title={title}
      className={`${PILL} relative z-10 hover:bg-accent hover:text-foreground ${className}`}
    >
      {children}
    </a>
  ) : (
    <span title={title} className={`${PILL} ${className}`}>
      {children}
    </span>
  );
}

/**
 * Where the project's latest deploy came from: repo, image or template, as
 * one label that links to it. With `revision` (a commit, or an image tag)
 * it's shown after the name, and a commit links to its page.
 */
export function SourcePill({
  source,
  revision,
  className,
}: {
  source: ServiceSource;
  revision?: string;
  /** "min-w-0" in a row: the pill shrinks and its name truncates. */
  className?: string;
}) {
  const t =
    source.type === "template" ? getTemplate(source.templateId) : undefined;
  const label =
    source.type === "git"
      ? source.repoUrl
          .replace(/^https?:\/\/(www\.)?(github\.com\/)?/, "")
          .replace(/\.git$/, "")
      : source.type === "image"
        ? imageLabel(source.image)
        : source.image
          ? imageLabel(source.image)
          : (t?.name ?? "Template");
  const git =
    source.type === "git"
      ? gitLinks(source.repoUrl, source.branch, revision)
      : undefined;
  const href = git
    ? (git.commit ?? git.repo)
    : source.type === "image" || source.type === "template"
      ? imagePage(source.image)
      : undefined;
  // The full name on hover, since a narrow card truncates it.
  const full =
    source.type === "git"
      ? `${label}${revision ? `@${revision}` : ""}`
      : source.image
        ? `${source.image}${source.reference ? `:${source.reference}` : ""}`
        : label;
  return (
    <Pill href={href} title={full} className={className}>
      {source.type === "git" ? (
        <BrandMark icon={GITHUB_LOGO} size={12} />
      ) : (
        <RegistryMark image={source.image} />
      )}
      <span className="truncate">{label}</span>
      {revision ? (
        <span className="shrink-0 font-mono text-[11px] text-muted-foreground">
          {revision.startsWith("sha256:")
            ? revision.slice(7, 19)
            : /^[0-9a-f]{8,40}$/.test(revision)
              ? revision.slice(0, 7)
              : revision.slice(0, 24)}
        </span>
      ) : null}
    </Pill>
  );
}

/** The project's public URL, like Vercel's production domain (the service count is shown below it). */
export function projectSubtitle(st: Stats) {
  const domain = st.apps
    .flatMap((a) => a.domains)
    .find((d) => d.status === "active");
  return domain
    ? domain.hostname
    : st.apps.length
      ? "No domain"
      : "No services yet";
}

/**
 * "3d ago on main", "Deploying now", "Deploy failed 2h ago"… With `settled`
 * (where a status label already says a deploy is running) it skips the
 * running deploy and shows the last finished one, or when the first deploy
 * is still running, when it started.
 */
export function LastDeploy({
  st,
  settled,
}: {
  st: { latest?: Deployment };
  settled?: boolean;
}) {
  const s = usePyo();
  const running = settled && st.latest && isActive(st.latest.status);
  const d = running
    ? deploymentsWhere(s, { serviceId: st.latest!.serviceId }).find(
        (x) => !isActive(x.status),
      )
    : st.latest;
  if (!d)
    return (
      <span className="text-muted-foreground">
        {running
          ? `Started ${timeAgo(st.latest!.createdAt)}`
          : "No deployments yet"}
      </span>
    );
  if (d.status === "failed")
    return (
      <span className="text-destructive">
        Deploy failed {timeAgo(d.createdAt)}
      </span>
    );
  if (isActive(d.status))
    return <span className="text-muted-foreground">Deploying now</span>;
  return (
    <span className="text-muted-foreground">
      {timeAgo(d.createdAt)}
      {d.source.type === "git"
        ? ` on ${d.source.branch}`
        : ` · ${d.revisionLabel}`}
    </span>
  );
}

/**
 * A linked summary card. On hover the edge firms up, a soft shadow shows,
 * and (dark) the fill steps up. No transitions. Shared by project, service,
 * template and cluster cards.
 */
export const CARD_HOVER =
  "hover:border-border-strong hover:shadow-[0_10px_24px_-14px_rgb(0_0_0/0.35)] dark:hover:bg-secondary/70 dark:hover:shadow-[0_10px_24px_-12px_rgb(0_0_0/0.8)]";
const CARD = `group/card relative flex flex-col rounded-[14px] border border-card-border bg-card p-5 ${CARD_HOVER} has-[a:focus-visible]:ring-2 has-[a:focus-visible]:ring-primary/40`;

/** Requests over the last 24 hours, for projects with a live domain. */
export function ProjectTraffic({ p, st }: { p: Project; st: Stats }) {
  const s = usePyo();
  const live = st.apps.some((a) => a.domains.some((d) => d.status === "active"));
  const on = s.observabilitySettings.enabled;
  const q = useQuery(
    `project-traffic:${p.id}:${live && on}`,
    () =>
      live && on
        ? fetchAnalytics({ team: s.currentTeamId, project: p.id, site: "all", range: "24h" })
        : Promise.resolve(null),
    { refreshMs: 60_000 },
  );
  if (!live) return <TrafficEmpty />;
  if (!on) return null;
  const data = q.status === "success" ? q.data : null;
  if (!data) return null;
  const total = data.summary.requests;
  return (
    <TrafficLine
      values={data.series.map((pt) => pt.requests)}
      label={`${total} requests in the last 24 hours`}
    />
  );
}

const TW = 140;
const TH = 30;

/** The empty state's gentle wave: a few overlapping sines, so it doesn't look like real data. */
const EMPTY_WAVE = Array.from({ length: 25 }, (_, i): [number, number] => {
  const t = (i / 24) * Math.PI * 2;
  return [(i / 24) * TW, TH / 2 + Math.sin(t * 1.5) * 4 + Math.sin(t * 3.2 + 1) * 2.5];
}).reduce((d, [x, y], i, xy) => {
  if (!i) return `M ${x} ${y}`;
  const [px, py] = xy[i - 1];
  const mx = (px + x) / 2;
  return `${d} C ${mx} ${py} ${mx} ${y} ${x} ${y}`;
}, "");

/** No traffic yet: a dashed wave with a glow passing along it. */
function TrafficEmpty() {
  return (
    <div className="relative shrink-0" style={{ width: TW, height: TH }} role="img" aria-label="No traffic data yet">
      <div aria-hidden className="absolute inset-0 [mask-image:linear-gradient(to_right,transparent,black_10%,black_90%,transparent)]">
        <svg viewBox={`0 0 ${TW} ${TH}`} className="absolute inset-0 h-full w-full">
          <path d={EMPTY_WAVE} fill="none" stroke="hsl(var(--muted-foreground) / 0.4)" strokeWidth={1.5} strokeDasharray="3 3" strokeLinecap="round" />
        </svg>
        <svg viewBox={`0 0 ${TW} ${TH}`} className="empty-wave-scan absolute inset-0 h-full w-full">
          <path d={EMPTY_WAVE} fill="none" stroke="hsl(var(--muted-foreground))" strokeOpacity={0.9} strokeWidth={1.5} strokeDasharray="3 3" strokeLinecap="round" />
        </svg>
      </div>
      <span aria-hidden className="absolute left-1/2 top-1/2 inline-flex -translate-x-1/2 -translate-y-1/2 items-center gap-1 whitespace-nowrap rounded-full border border-border/60 bg-card px-2 py-px text-[10px] text-muted-foreground shadow-sm">
        <span className="relative flex h-1.5 w-1.5">
          <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-muted-foreground/50 [animation-duration:2.4s] motion-reduce:hidden" />
          <span className="relative inline-flex h-1.5 w-1.5 rounded-full bg-muted-foreground/70" />
        </span>
        No data yet
      </span>
    </div>
  );
}

/** A smooth line with a soft fill under it. */
function TrafficLine({ values, label }: { values: number[]; label: string }) {
  const max = Math.max(0, ...values);
  if (values.length < 2 || max === 0) return <TrafficEmpty />;
  const xy = values.map((v, i): [number, number] => [
    (i / (values.length - 1)) * TW,
    TH - 2 - (v / max) * (TH - 5),
  ]);
  const line = xy.reduce((d, [x, y], i) => {
    if (!i) return `M ${x} ${y}`;
    const [px, py] = xy[i - 1];
    const mx = (px + x) / 2;
    return `${d} C ${mx} ${py} ${mx} ${y} ${x} ${y}`;
  }, "");
  return (
    <svg width={TW} height={TH} viewBox={`0 0 ${TW} ${TH}`} role="img" aria-label={label} className="shrink-0 overflow-visible">
      <path d={`${line} L ${TW} ${TH} L 0 ${TH} Z`} fill="var(--series-1)" fillOpacity={0.12} />
      <path
        d={line}
        fill="none"
        stroke="var(--series-1)"
        strokeWidth={1.5}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}

export function ProjectCard({
  p,
  st,
  menu,
}: {
  p: Project;
  st: Stats;
  menu?: ReactNode;
}) {
  const s = usePyo();
  return (
    <article className={CARD}>
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="min-w-0">
            <h3 className="flex min-w-0 items-center gap-1.5 text-[14px] font-semibold">
              {/* The link covers the card; the menu sits above it. */}
              <Link
                to="/$team/$projectId"
                params={{ team: currentSlug(), ...projectParams(p.id) }}
                className="truncate after:absolute after:inset-0 after:rounded-[14px] focus:outline-none"
              >
                {p.name}
              </Link>
            </h3>
          </div>
        </div>
        <div className="flex shrink-0 items-start gap-1">
          <ProjectTraffic p={p} st={st} />
          {menu ? (
            <div className="relative z-10 -mr-2 -mt-1.5">{menu}</div>
          ) : null}
        </div>
      </div>
      <div className="mt-auto pt-12">
        <p className="flex items-center gap-2 text-[12.5px] text-muted-foreground">
          {st.apps.length ? (
            <>
              <span>{plural(st.apps.length, "service")}</span>
              <span aria-hidden className="text-muted-foreground/40">
                ·
              </span>
              <span className="inline-flex items-center gap-1.5">
                <Dot
                  tone={
                    st.failing
                      ? "danger"
                      : st.running === st.apps.length
                        ? "success"
                        : st.deploying
                          ? "progress"
                          : "warning"
                  }
                />
                {st.running === st.apps.length
                  ? "all running"
                  : st.failing
                    ? `${st.failing} failing`
                    : `${st.running} running`}
              </span>
            </>
          ) : (
            "No services yet"
          )}
        </p>
        {st.apps.length ? (
          <div className="mt-3 flex items-center justify-between gap-3">
            <ServiceStack apps={st.apps} projectId={p.id} size="md" />
            <Places
              places={[...new Set(st.apps.flatMap((a) => servicePlaces(s, a)))]}
            />
          </div>
        ) : null}
      </div>
    </article>
  );
}

/** The service's public URL, or its in-cluster address when it has none. */
export function serviceSubtitle(a: Application, namespace: string) {
  return (
    a.domains.find((d) => d.status === "active")?.hostname ??
    `${a.name}.${namespace}`
  );
}

/** One service inside a project, laid out like ProjectCard. */
export function AppCard({
  a,
  namespace,
  latest,
  menu,
}: {
  a: Application;
  namespace: string;
  latest?: Deployment;
  menu?: ReactNode;
}) {
  const s = usePyo();
  return (
    <article className={CARD}>
      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-center gap-3">
          {/* Status as a dot on the icon; above the card's link so its tooltip shows. */}
          <span className="relative shrink-0">
            <ServiceLogo app={a} />
            <span className="absolute -bottom-1 -right-1 z-10">
              <Tooltip content={waitingForRoom(a) ? "Waiting for room" : serviceLabel[a.status]}>
                <span
                  role="img"
                  aria-label={waitingForRoom(a) ? "Waiting for room" : serviceLabel[a.status]}
                  className="flex rounded-full bg-card p-[2px]"
                >
                  <Dot tone={serviceTone[a.status]} className="h-2 w-2" />
                </span>
              </Tooltip>
            </span>
          </span>
          <div className="min-w-0">
            <h3 className="flex min-w-0 items-center gap-1.5 text-[14px] font-semibold">
              {/* The link covers the card; the menu sits above it. */}
              <Link
                to="/$team/$projectId/$serviceId"
                params={{ team: currentSlug(), ...serviceParams(a.id) }}
                className="truncate after:absolute after:inset-0 after:rounded-[14px] focus:outline-none focus-visible:underline"
              >
                {a.name}
              </Link>
            </h3>
            <p className="truncate text-[13px] text-muted-foreground">
              {serviceSubtitle(a, namespace)}
            </p>
          </div>
        </div>
        {menu ? (
          <div className="relative z-10 -mr-2 -mt-1.5">{menu}</div>
        ) : null}
      </div>
      <div className="mt-auto pt-12">
        <p className="truncate text-[13px]">
          <LastDeploy st={{ latest }} settled />
        </p>
        {/* The source gives way (truncates); the status and flags keep their size. */}
        <div className="mt-2.5 flex min-w-0 items-center gap-2">
          <SourcePill source={a.source} className="min-w-0" />
          {/* Running is the norm: the dot says it. Anything else is worth a label. */}
          {a.status === "running" ? null : (
            <span className="flex shrink-0">
              {a.status === "deploying" ? (
                <DeployingStatus serviceId={a.id} />
              ) : (
                <ServiceStatusBadge status={a.status} waiting={a.runtime.waiting} />
              )}
            </span>
          )}
          <span className="ml-auto flex shrink-0">
            <Places places={servicePlaces(s, a)} />
          </span>
        </div>
      </div>
    </article>
  );
}

/**
 * "Deploying" in the status slot, linking (above the card's link) to the
 * running deploy, with its stage on hover.
 */
export function DeployingStatus({ serviceId }: { serviceId: string }) {
  const d = activeDeploymentFor(usePyo(), serviceId);
  if (!d) return <ServiceStatusBadge status="deploying" />;
  return (
    <Tooltip content={STAGE_LABEL[d.status]} focusable={false}>
      <Link
        to="/$team/deployments/$deploymentId"
        params={{ team: currentSlug(), deploymentId: d.id }}
        aria-label={`Deploying: ${STAGE_LABEL[d.status]}`}
        className="relative z-10 inline-flex rounded-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-primary/40"
      >
        <ServiceStatusBadge status="deploying" />
      </Link>
    </Tooltip>
  );
}

export type View = "grid" | "list";

/** Grid or list, remembered per page in localStorage. */
export function useStoredView(key: string): [View, (v: View) => void] {
  const [view, setView] = useState<View>(() => {
    try {
      return (localStorage.getItem(key) as View) || "grid";
    } catch {
      return "grid";
    }
  });
  const persist = (v: View) => {
    setView(v);
    try {
      localStorage.setItem(key, v);
    } catch {
      // Preference only.
    }
  };
  return [view, persist];
}

export function ViewToggle({
  value,
  onChange,
}: {
  value: View;
  onChange: (v: View) => void;
}) {
  return (
    <Segmented
      label="View"
      className="shrink-0"
      value={value}
      onChange={onChange}
      options={[
        { value: "grid", label: <SquaresFourIcon size={14} />, ariaLabel: "Grid view", title: "Grid view" },
        { value: "list", label: <ListBulletsIcon size={14} />, ariaLabel: "List view", title: "List view" },
      ]}
    />
  );
}

/** The logo of an image's registry, or a package icon for ones without one. */
function RegistryMark({ image }: { image?: string }) {
  const icon = image ? registryIcon(imageHost(image)) : undefined;
  return icon ? (
    <BrandMark icon={icon} size={12} />
  ) : (
    <PackageIcon size={12} className="shrink-0" />
  );
}

/** Frameworks the build detects (Railpack), and Dockerfile builds, by their logo. */
const FRAMEWORKS: [RegExp, SimpleIcon][] = [
  [/next/i, siNextdotjs],
  [/astro/i, siAstro],
  [/remix/i, siRemix],
  [/svelte/i, siSvelte],
  [/vite/i, siVite],
  [/react/i, siReact],
  [/django/i, siDjango],
  [/flask/i, siFlask],
  [/laravel/i, siLaravel],
  [/rails/i, siRubyonrails],
  [/deno/i, siDeno],
  [/bun/i, siBun],
  [/node/i, siNodedotjs],
  [/python/i, siPython],
  [/^go\b|golang/i, siGo],
  [/rust/i, siRust],
  [/ruby/i, siRuby],
  [/php/i, siPhp],
  [/\.net|dotnet/i, siDotnet],
  [/java/i, siOpenjdk],
  [/elixir/i, siElixir],
  [/static|html/i, siHtml5],
  [/dockerfile/i, siDocker],
];

/** Well-known images, by their product's logo. */
const IMAGES: Record<string, SimpleIcon> = {
  nginx: siNginx,
  redis: siRedis,
  postgres: siPostgresql,
  mysql: siMysql,
  mongo: siMongodb,
  grafana: siGrafana,
  prometheus: siPrometheus,
  minio: siMinio,
  node: siNodedotjs,
  python: siPython,
  golang: siGo,
};

/** What a service runs, as a logo: the framework its build detected, its template's
 * product, a well-known image, or else its registry or Git host. */
function serviceMark(
  a: Application,
  built?: string,
): { brand?: Pick<SimpleIcon, "hex" | "path">; Icon?: Icon } {
  const fw = built ? FRAMEWORKS.find(([re]) => re.test(built))?.[1] : undefined;
  if (fw) return { brand: fw };
  const src = a.source;
  // A template no longer in the catalog falls back to its image, below.
  const t = src.type === "template" ? getTemplate(src.templateId) : undefined;
  if (t) {
    return t.logo ? { brand: t.logo } : { Icon: CATEGORY_ICONS[t.category] };
  }
  if (src.type === "git")
    return { brand: /gitlab\.com/.test(src.repoUrl) ? siGitlab : siGithub };
  if (!src.image) return { Icon: PackageIcon };
  const name = imageLabel(src.image).split("/").pop() ?? "";
  const brand = IMAGES[name] ?? registryIcon(imageHost(src.image));
  return brand ? { brand } : { Icon: PackageIcon };
}

/** Where a deployment came from, as its logo: the Git host for a repo, the registry for an image. */
export function SourceMark({
  source,
  size = 13,
}: {
  source: ServiceSource;
  size?: number;
}) {
  if (source.type === "git")
    return (
      <BrandMark
        icon={/gitlab\.com/.test(source.repoUrl) ? siGitlab : siGithub}
        size={size}
      />
    );
  const icon = source.image ? registryIcon(imageHost(source.image)) : undefined;
  return icon ? (
    <BrandMark icon={icon} size={size} />
  ) : (
    <PackageIcon size={size} className="shrink-0" />
  );
}

const STACK_MAX = 5;

/** The project's services as overlapping round logos, each linking to its service. */
/** `size`: "md" for project cards, "sm" (default) inline in lists. */
export function ServiceStack({
  apps,
  projectId,
  size = "sm",
}: {
  apps: Application[];
  projectId: string;
  size?: "sm" | "md";
}) {
  const deployments = usePyo().deployments;
  if (!apps.length) return null;
  const built = (a: Application) => {
    const d = deployments.find((x) => x.id === a.currentDeploymentId);
    return d?.source.type === "git" ? d.source.buildpackLanguage : undefined;
  };
  const shown = apps.slice(0, STACK_MAX);
  const more = apps.length - shown.length;
  const dims = size === "md" ? "h-8 w-8 -ml-1" : "h-7 w-7 -ml-2";
  const px = size === "md" ? 15 : 14;
  const circle = `relative z-10 flex ${dims} items-center justify-center rounded-full border border-border bg-accent ring-2 ring-card dark:bg-popover transition-transform hover:z-20 hover:-translate-y-0.5 first:ml-0`;
  return (
    <span className="flex items-center">
      {shown.map((a) => {
        const m = serviceMark(a, built(a));
        return (
          <Tooltip key={a.id} content={a.name} focusable={false}>
            <Link
              to="/$team/$projectId/$serviceId"
              params={{ team: currentSlug(), ...serviceParams(a.id) }}
              aria-label={a.name}
              className={circle}
            >
              {m.brand ? (
                <BrandMark icon={m.brand} size={px} />
              ) : m.Icon ? (
                <m.Icon size={px} className="text-foreground/80" />
              ) : null}
            </Link>
          </Tooltip>
        );
      })}
      {more > 0 ? (
        <Link
          to="/$team/$projectId"
          params={{ team: currentSlug(), ...projectParams(projectId) }}
          aria-label={`${more} more services`}
          className={`${circle} text-[10.5px] font-semibold text-muted-foreground`}
        >
          +{more}
        </Link>
      ) : null}
    </span>
  );
}

/** A service's logo as a tile (same choice as the project card's stack). */
export function ServiceLogo({ app }: { app: Application }) {
  const deployments = usePyo().deployments;
  const d = deployments.find((x) => x.id === app.currentDeploymentId);
  const m = serviceMark(
    app,
    d?.source.type === "git" ? d.source.buildpackLanguage : undefined,
  );
  return (
    <span
      aria-hidden
      className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md border border-border bg-muted/60 text-foreground/85"
    >
      {m.brand ? (
        <BrandMark icon={m.brand} size={13} />
      ) : m.Icon ? (
        <m.Icon size={13} />
      ) : null}
    </span>
  );
}

/**
 * Where a service runs: the places of the servers its replicas are on, else
 * (not running) where it's pinned, else its cluster's places.
 */
export function servicePlaces(
  s: ReturnType<typeof usePyo>,
  a: Application,
): string[] {
  const cluster = s.clusters.find((c) => c.id === a.clusterId);
  const onNodes = (a.runtime.replicas ?? []).map(
    (r) =>
      s.servers.find((n) => n.clusterId === a.clusterId && n.name === r.server)
        ?.location ??
      cluster?.location ??
      "",
  );
  const places = onNodes.length
    ? onNodes
    : a.location
      ? [a.location]
      : cluster
        ? placesOf(cluster, s.servers).map((pl) => pl.location)
        : [];
  return [...new Set(places.filter(Boolean))];
}

/** Flags of places, above a card's own link so their tooltips show. */
function Places({ places }: { places: string[] }) {
  if (!places.length) return null;
  return (
    <span className="relative z-10 flex shrink-0">
      <PlaceFlags places={places} size={26} />
    </span>
  );
}
