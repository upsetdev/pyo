import { useLocation, useParams } from "@tanstack/react-router";
import { currentTeam, getState } from "./store";

// Dashboard pages live under /:teamSlug (/acme/projects, /acme/analytics).
// Components write paths relative to the team ("/projects") and AppLink adds
// the slug; pages about the person (/profile) and server routes stay global.

const GLOBAL =
  /^\/(profile|api-keys|mcp|oauth|login|signup|logout|api)(?=[/?#]|$)/;

export function teamPath(slug: string, path: string) {
  if (!path.startsWith("/") || GLOBAL.test(path) || !slug) return path;
  return path === "/"
    ? `/${slug}`
    : /^\/[?#]/.test(path)
      ? `/${slug}${path.slice(1)}`
      : `/${slug}${path}`;
}

/** The team in the URL, or the last-used team on pages outside one. */
export function useTeamSlug(): string {
  const params = useParams({ strict: false }) as { team?: string };
  return params.team ?? currentTeam(getState())?.slug ?? "";
}

/** The current path without the team prefix, e.g. "/projects/prj_1". */
export function useTeamPathname(): string {
  const slug = useTeamSlug();
  const pathname = useLocation({
    select: (l) => l.pathname.replace(/\/$/, ""),
  });
  const rest =
    slug && (pathname === `/${slug}` || pathname.startsWith(`/${slug}/`))
      ? pathname.slice(slug.length + 1)
      : pathname;
  return rest || "/";
}

/**
 * The current team's slug, for typed router links (`to="/$team/$projectId/$serviceId"`).
 * The $team route syncs the store to the URL before rendering, so this
 * matches the URL on every team page.
 */
export const currentSlug = () => currentTeam(getState())?.slug ?? "";

/**
 * Project namespaces that would shadow a team page in /<project>/... URLs
 * (the API refuses them too).
 */
export const RESERVED_PROJECT_SLUGS = [
  "activity",
  "analytics",
  "api-keys",
  "backups",
  "builders",
  "certificates",
  "cron-jobs",
  "clusters",
  "databases",
  "deploy",
  "deployments",
  "domains",
  "images",
  "import",
  "integrations",
  "kv",
  "mcp",
  "members",
  "mtls",
  "mysql",
  "new",
  "notifications",
  "observability",
  "onboarding",
  "postgres",
  "projects",
  "registry",
  "security",
  "servers",
  "services",
  "settings",
  "templates",
];

/** Service names that would shadow a project page in /<project>/<service> URLs. */
export const RESERVED_SERVICE_NAMES = [
  "analytics",
  "deployments",
  "networking",
  "services",
  "settings",
  "storage",
];

/** A cluster's tabs: servers can't be named these (a server is /clusters/<cluster>/<server>). */
export const CLUSTER_TABS = [
  "servers",
  "workloads",
  "networking",
  "storage",
  "settings",
];
