import { useSyncExternalStore } from "react";
import { RESERVED_PROJECT_SLUGS } from "./paths";
import {
  ApiError,
  fetchState,
  op,
  addDatabaseBackup as addDatabaseBackupRequest,
} from "./backend";
import { NEW_PROJECT, type DeployDraft } from "./draft";
import { newId, slugify } from "./format";
import { isActive, revisionFor } from "./simulation";
import { generateValue, getTemplate, templateArgs, templateRecord, templateRefs } from "./templates";
import { toast } from "./toast";
import type {
  Autoscale,
  ActivityEvent,
  ApiKeyAccess,
  ApiToken,
  ServiceSource,
  Application,
  Builder,
  Certificate,
  OriginPullCertificate,
  FirewallRule,
  FirewallRuleInput,
  TrafficMode,
  WafAction,
  WafCondition,
  WafRule,
  Database,
  WafSettings,
  SecretStore,
  Cluster,
  ClusterServer,
  ControlBackup,
  Updates,
  UpdateSettings,
  ControlBackupSettings,
  DeployHook,
  Deployment,
  DeploymentTrigger,
  Domain,
  EmailSettings,
  EncryptionStatus,
  EnvironmentVariable,
  GithubAppInfo,
  GithubInstallation,
  Integration,
  IpMode,
  OAuthGrant,
  PortMapping,
  Project,
  Registry,
  RegistryLogin,
  Role,
  Team,
  TeamBackups,
  TeamMember,
  TlsMode,
  User,
  Volume,
  Workload,
} from "./types";

// The dashboard's data layer. Route components read state through usePyo()
// and change it through `api`. State is a snapshot from the Rust API
// (GET /api/v1/state), refreshed by polling. Each change is applied locally
// first so the screen updates at once, then sent as an op; the next snapshot
// is the truth, and a rejected op is undone by it.

export type PyoState = {
  /** False until the first snapshot arrives. */
  loaded: boolean;
  user: User;
  teams: Team[];
  members: TeamMember[];
  projects: Project[];
  clusters: Cluster[];
  servers: ClusterServer[];
  /** Every workload the clusters' agents report (system and service). */
  systemWorkloads: Workload[];
  services: Application[];
  deployments: Deployment[];
  volumes: Volume[];
  activity: ActivityEvent[];
  registries: Registry[];
  builders: Builder[];
  integrations: Integration[];
  /** URL the agents of in-cluster installs use; shown in install commands. */
  agentApiUrl: string;
  /** Outgoing email (SMTP), or null when it isn't set up. */
  email: EmailSettings | null;
  /** This installation's GitHub App, or null when it isn't set up. */
  github: GithubAppInfo | null;
  githubInstallations: GithubInstallation[];
  deployHooks: DeployHook[];
  apiTokens: ApiToken[];
  /** Assistants connected over OAuth (MCP). */
  oauthGrants: OAuthGrant[];
  /** How teams' values are encrypted, for the security overview. */
  encryption: EncryptionStatus | null;
  /** Each team's backups of its volumes and databases. */
  teamBackups: TeamBackups[];
  /** Backups of Pyo's own data; null unless you're the installation's admin. */
  controlBackup: ControlBackup | null;
  /** Updates of Pyo; null unless you're the installation's admin. */
  updates: Updates | null;
  /** Logins for private container registries. */
  registryLogins: RegistryLogin[];
  /** Installation-wide analytics settings. */
  /** Installation-wide. `small`: Pyo's server has under 3.5 GB of memory now. */
  observabilitySettings: { ipMode: IpMode; enabled: boolean; small: boolean | null };
  /** Uploaded TLS certificates of the user's teams. */
  certificates: Certificate[];
  /** Client certificates for CDNs, without their keys. */
  originPullCertificates: OriginPullCertificate[];
  /** Firewall rules of the user's teams' clusters and services. */
  firewallRules: FirewallRule[];
  /** Custom WAF rules of the user's teams' clusters and services, in order. */
  wafRules: WafRule[];
  /** Postgres, MySQL and KV databases of the user's teams. */
  databases: Database[];
  /** External secret stores, without their credentials. */
  secretStores: SecretStore[];
  // Per-browser preferences, kept in localStorage.
  currentTeamId: string;
  recentTemplateIds: string[];
  notificationsReadAt: number;
  /** Muted notification topics, per team. Everything is on unless listed here. */
  mutedTopics?: Record<string, NotificationTopic[]>;
  /** Git connections aren't available yet; always empty. */
  gitConnections?: {
    teamId: string;
    provider: string;
    account: string;
    connectedAt: number;
  }[];
};

// ── Notifications ──────────────────────────────────────────────────────────

export type NotificationTopic =
  | "deploy_failed"
  | "deploy_succeeded"
  | "servers"
  | "domains"
  | "changes"
  | "team";

export const NOTIFICATION_TOPICS: {
  id: NotificationTopic;
  label: string;
  description: string;
}[] = [
  {
    id: "deploy_failed",
    label: "Failed deployments",
    description: "A build or rollout fails.",
  },
  {
    id: "deploy_succeeded",
    label: "Successful deployments",
    description: "A deployment goes live or is rolled back.",
  },
  {
    id: "servers",
    label: "Server health",
    description:
      "A cluster loses its agent, degrades, or a server changes scheduling.",
  },
  {
    id: "domains",
    label: "Domains",
    description: "Domains are added, verified or removed.",
  },
  {
    id: "changes",
    label: "Service changes",
    description:
      "Services, environment variables, networking and volumes change.",
  },
  {
    id: "team",
    label: "Team and projects",
    description: "Members, projects or team settings change.",
  },
];

export function topicOf(type: ActivityEvent["type"]): NotificationTopic {
  if (type === "deployment.failed") return "deploy_failed";
  if (type.startsWith("deployment.")) return "deploy_succeeded";
  if (type.startsWith("cluster.") || type.startsWith("server.")) return "servers";
  if (type.startsWith("domain.")) return "domains";
  if (
    type.startsWith("team.") ||
    type.startsWith("member.") ||
    type.startsWith("project.")
  )
    return "team";
  return "changes";
}

export const isMuted = (
  s: PyoState,
  topic: NotificationTopic,
  teamId = s.currentTeamId,
) => (s.mutedTopics?.[teamId] ?? []).includes(topic);

// ── State, preferences and sync ────────────────────────────────────────────

/** Registries images can come from. Pulls are anonymous in this version. */
const REGISTRIES: Registry[] = [
  {
    id: "reg_dockerhub",
    teamId: null,
    name: "Docker Hub",
    host: "docker.io",
    credential: "none",
  },
  {
    id: "reg_ghcr",
    teamId: null,
    name: "GitHub Container Registry",
    host: "ghcr.io",
    credential: "none",
  },
  {
    id: "reg_quay",
    teamId: null,
    name: "Quay",
    host: "quay.io",
    credential: "none",
  },
];

const PREFS_KEY = "pyo:prefs";
type Prefs = Pick<
  PyoState,
  "currentTeamId" | "recentTemplateIds" | "notificationsReadAt" | "mutedTopics"
>;

function loadPrefs(): Prefs {
  const fallback: Prefs = {
    currentTeamId: "",
    recentTemplateIds: [],
    notificationsReadAt: Date.now(),
    mutedTopics: {},
  };
  if (typeof window === "undefined") return fallback;
  try {
    return {
      ...fallback,
      ...(JSON.parse(
        localStorage.getItem(PREFS_KEY) ?? "{}",
      ) as Partial<Prefs>),
    };
  } catch {
    return fallback;
  }
}

function savePrefs(s: PyoState) {
  try {
    const prefs: Prefs = {
      currentTeamId: s.currentTeamId,
      recentTemplateIds: s.recentTemplateIds,
      notificationsReadAt: s.notificationsReadAt,
      mutedTopics: s.mutedTopics,
    };
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    // Preferences only.
  }
}

function empty(): PyoState {
  return {
    loaded: false,
    user: { id: "", name: "", email: "", timezone: "UTC" },
    teams: [],
    members: [],
    projects: [],
    clusters: [],
    servers: [],
    systemWorkloads: [],
    services: [],
    deployments: [],
    volumes: [],
    activity: [],
    registries: REGISTRIES,
    agentApiUrl: "",
    email: null,
    github: null,
    githubInstallations: [],
    deployHooks: [],
    apiTokens: [],
    oauthGrants: [],
    encryption: null,
    controlBackup: null,
    updates: null,
    teamBackups: [],
    certificates: [],
    originPullCertificates: [],
    firewallRules: [],
    wafRules: [],
    databases: [],
    secretStores: [],
    registryLogins: [],
    observabilitySettings: { ipMode: "none", enabled: true, small: null },
    builders: [],
    integrations: [],
    gitConnections: [],
    ...loadPrefs(),
  };
}

let state: PyoState | null = null;
const listeners = new Set<() => void>();

function get(): PyoState {
  if (!state) state = empty();
  return state;
}

function set(next: PyoState) {
  state = next;
  savePrefs(next);
  listeners.forEach((l) => l());
}

/** Copy-on-write local change: `fn` mutates a deep clone, listeners see a new object. */
function update(fn: (s: PyoState) => void) {
  const next = structuredClone(get());
  fn(next);
  set(next);
}

function subscribe(l: () => void) {
  listeners.add(l);
  return () => listeners.delete(l);
}

export function usePyo(): PyoState {
  return useSyncExternalStore(subscribe, get, get);
}

export function getState() {
  return get();
}

/** What an invite op answers: whether it was emailed, and the link to share. */
export type InviteResult = {
  emailed: boolean;
  emailError?: string;
  link: string;
};

/** Ops in flight. A snapshot fetched meanwhile could predate them, so it's dropped. */
let pending = 0;
let generation = 0;

/** Fetch the snapshot and make it the state. */
export async function refresh() {
  const started = generation;
  const snap = (await fetchState()) as Partial<PyoState>;
  if (pending > 0 || started !== generation) return;
  const prev = get();
  const next: PyoState = {
    ...prev,
    ...snap,
    registries: REGISTRIES,
    loaded: true,
  };
  if (!next.teams.some((t) => t.id === next.currentTeamId))
    next.currentTeamId = next.teams[0]?.id ?? "";
  set(next);
}

let poller: ReturnType<typeof setTimeout> | undefined;
let polling = false;

/** Keep the snapshot fresh: every second during a rollout, every 4 otherwise. */
export function startSync() {
  if (polling || typeof window === "undefined") return;
  polling = true;
  const loop = async () => {
    if (!polling) return;
    if (!document.hidden) {
      try {
        await refresh();
      } catch (e) {
        if (e instanceof ApiError && e.status === 401) {
          stopSync();
          window.location.assign(
            `/login?next=${encodeURIComponent(window.location.pathname)}`,
          );
          return;
        }
      }
    }
    const busy = get().deployments.some((d) => isActive(d.status));
    poller = setTimeout(loop, busy ? 1000 : 4000);
  };
  // The snapshot was usually just loaded (the app layout waits for it): next round after a pause.
  poller = setTimeout(loop, get().loaded ? 4000 : 0);
}

export function stopSync() {
  polling = false;
  if (poller) clearTimeout(poller);
  poller = undefined;
}

/** Forget everything (sign-out). */
export function resetState() {
  stopSync();
  state = null;
  listeners.forEach((l) => l());
}

/**
 * Send one op. On success the snapshot is re-read; on failure the error is
 * shown and the snapshot (without the optimistic change) is restored.
 */
async function send<T = Record<string, unknown>>(
  name: string,
  args: Record<string, unknown>,
  quiet = false,
): Promise<(T & { ok: boolean }) | null> {
  pending += 1;
  generation += 1;
  try {
    const res = await op<T>(name, args);
    return res;
  } catch (e) {
    if (!quiet)
      toast({
        tone: "error",
        title: e instanceof Error ? e.message : "That didn't work.",
      });
    return null;
  } finally {
    pending -= 1;
    void refresh().catch(() => undefined);
  }
}

/** For forms that ask as you type: a failure comes back as its message, no toast. */
async function attempt<T>(
  name: string,
  args: Record<string, unknown>,
): Promise<{ data: T & { ok: boolean } } | { error: string }> {
  pending += 1;
  generation += 1;
  try {
    return { data: await op<T>(name, args) };
  } catch (e) {
    return { error: e instanceof Error ? e.message : "That didn't work." };
  } finally {
    pending -= 1;
    void refresh().catch(() => undefined);
  }
}

// ── Selectors (pure) ───────────────────────────────────────────────────────

/** Slugs a team can't take because a top-level route already owns them. */
const RESERVED_SLUGS = new Set([
  "profile",
  "login",
  "signup",
  "logout",
  "api",
  "new",
  "teams",
  "assets",
  "install",
  // Pyo registry keeps Pyo's own builds under pyo/.
  "pyo",
]);

/** A URL slug for a team name that no other team or top-level route uses. */
function teamSlug(s: PyoState, name: string, selfId?: string) {
  const base = slugify(name) || "team";
  const taken = (x: string) =>
    RESERVED_SLUGS.has(x) ||
    s.teams.some((t) => t.slug === x && t.id !== selfId);
  let slug = base;
  for (let i = 2; taken(slug); i++) slug = `${base}-${i}`;
  return slug;
}

/** Namespaces the API refuses (kube-* is refused too). */
const RESERVED_NAMESPACES = new Set(["default", "pyo-system"]);

/** The project a deploy creates when it doesn't pick one: named after the app, on a free namespace. */
export function newProjectFor(
  s: PyoState,
  serviceName: string,
  teamId = s.currentTeamId,
) {
  const projects = projectsIn(s, teamId);
  const base = slugify(serviceName) || "project";
  const taken = (x: string) =>
    RESERVED_NAMESPACES.has(x) ||
    RESERVED_PROJECT_SLUGS.includes(x) ||
    x.startsWith("kube-") ||
    projects.some((p) => p.namespace === x || p.name.toLowerCase() === x);
  let ns = base;
  for (let i = 2; taken(ns); i++) ns = `${base.slice(0, 36)}-${i}`;
  return { name: ns, namespace: ns };
}

/** The project a draft deploys into, existing or about to be created. */
export function draftProject(
  s: PyoState,
  d: Pick<DeployDraft, "projectId" | "name" | "teamId">,
): { name: string; namespace: string; isNew: boolean } | undefined {
  if (d.projectId === NEW_PROJECT)
    return {
      ...newProjectFor(s, d.name, d.teamId || s.currentTeamId),
      isNew: true,
    };
  const p = s.projects.find((x) => x.id === d.projectId);
  return p ? { name: p.name, namespace: p.namespace, isNew: false } : undefined;
}

export const teamBySlug = (s: PyoState, slug: string) =>
  s.teams.find((t) => t.slug === slug);
export const currentTeam = (s: PyoState) =>
  s.teams.find((w) => w.id === s.currentTeamId) ?? s.teams[0];
export const projectsIn = (s: PyoState, wsId = s.currentTeamId) =>
  s.projects.filter((p) => p.teamId === wsId);
export const servicesIn = (s: PyoState, wsId = s.currentTeamId) =>
  s.services.filter((a) => a.teamId === wsId);
export const clustersIn = (s: PyoState, wsId = s.currentTeamId) =>
  s.clusters.filter((c) => c.teamId === wsId);
// Clusters and servers are addressed by name in URLs (/acme/clusters/edge-sgp,
// /acme/clusters/sgp/servers/sgp-worker-2); ids resolve too.
export const clusterByRef = (
  s: PyoState,
  ref: string,
  teamId = s.currentTeamId,
) => {
  const cs = clustersIn(s, teamId);
  return cs.find((c) => c.name === ref) ?? cs.find((c) => c.id === ref);
};
export const serverByRef = (
  s: PyoState,
  ref: string,
  teamId = s.currentTeamId,
) => {
  const ids = new Set(clustersIn(s, teamId).map((c) => c.id));
  const ns = s.servers.filter((n) => ids.has(n.clusterId));
  return ns.find((n) => n.name === ref) ?? ns.find((n) => n.id === ref);
};
/** URL segment for a cluster or server: its name. */
// Projects and services are addressed by name in URLs: /<project>/<service>,
// where <project> is its namespace (unique in the team, and it never changes)
// and <service> its name (unique in the project). Ids work too.

/** A project by its namespace or id. */
export const projectByRef = (
  s: PyoState,
  ref: string,
  teamId = s.currentTeamId,
) =>
  s.projects.find((p) => p.teamId === teamId && p.namespace === ref) ??
  s.projects.find((p) => p.id === ref);

/** A service by its name in a project (by namespace or id), or by its id. */
export const serviceByRef = (s: PyoState, projectRef: string, ref: string) => {
  const byId = s.services.find((a) => a.id === ref);
  if (byId) return byId;
  const p = projectByRef(s, projectRef);
  return p
    ? s.services.find((a) => a.projectId === p.id && a.name === ref)
    : undefined;
};

/** The project id a project page's params name (its namespace, or its id). */
export const projectIdOf = (params: { projectId: string }) =>
  projectByRef(getState(), params.projectId)?.id ?? params.projectId;

/** The service id a service page's params name (its name in the project, or an id). */
export const serviceIdOf = (params: { projectId: string; serviceId: string }) =>
  serviceByRef(getState(), params.projectId, params.serviceId)?.id ?? params.serviceId;

/** Route params for a project's pages. */
export const projectParams = (id: string, s: PyoState = getState()) => ({
  projectId: s.projects.find((p) => p.id === id)?.namespace ?? id,
});

/** Route params for a service's pages. */
export const serviceParams = (id: string, s: PyoState = getState()) => {
  const a = s.services.find((x) => x.id === id);
  const p = a && s.projects.find((x) => x.id === a.projectId);
  return {
    projectId: p?.namespace ?? a?.projectId ?? id,
    serviceId: a?.name ?? id,
  };
};

/** A project's page, team-relative: "/nextjs", or "/nextjs/settings". */
export const projectPath = (id: string, tab = "", s: PyoState = getState()) =>
  `/${projectParams(id, s).projectId}${tab ? `/${tab}` : ""}`;

/** A service's page, team-relative: "/nextjs/web", or "/nextjs/web/logs". */
export const servicePath = (id: string, tab = "", s: PyoState = getState()) => {
  const { projectId, serviceId } = serviceParams(id, s);
  return `/${projectId}/${serviceId}${tab ? `/${tab}` : ""}`;
};

/** Each engine's section of the dashboard: /postgres, /mysql, /kv. */
export const DB_SECTION = { postgres: "postgres", mysql: "mysql", valkey: "kv" } as const;
export type DbSection = (typeof DB_SECTION)[keyof typeof DB_SECTION];
export const DB_SECTION_LABEL: Record<DbSection, string> = { postgres: "Postgres", mysql: "MySQL", kv: "KV" };
export const isDbSection = (x: string | undefined): x is DbSection =>
  x === "postgres" || x === "mysql" || x === "kv";

/** A database's page by engine, team-relative: "/postgres/mdb_1", or "/kv/mdb_3/logs". */
export const databaseHref = (engine: Database["engine"], id: string, tab = "") =>
  `/${DB_SECTION[engine]}/${id}${tab ? `/${tab}` : ""}`;

/** A database's page by id (its engine from the store). */
export const databasePath = (id: string, tab = "", s: PyoState = getState()) =>
  databaseHref(s.databases.find((d) => d.id === id)?.engine ?? "postgres", id, tab);

export const clusterRef = (s: PyoState, id: string) =>
  s.clusters.find((c) => c.id === id)?.name ?? id;
export const serverRef = (s: PyoState, id: string) =>
  s.servers.find((n) => n.id === id)?.name ?? id;
/** A server's page: /clusters/<cluster>/<server> (team-relative). */
export const serverHref = (s: PyoState, serverId: string) => {
  const n = s.servers.find((x) => x.id === serverId || x.name === serverId);
  return n ? `/clusters/${clusterRef(s, n.clusterId)}/${n.name}` : "/clusters";
};
export const serversOf = (s: PyoState, clusterId: string) =>
  s.servers.filter((n) => n.clusterId === clusterId);
export const volumesIn = (s: PyoState, wsId = s.currentTeamId) =>
  s.volumes.filter((v) => v.teamId === wsId);
export const activityIn = (s: PyoState, wsId = s.currentTeamId) =>
  s.activity.filter((a) => a.teamId === wsId);
export const gitConnectionsIn = (s: PyoState, wsId = s.currentTeamId) =>
  (s.gitConnections ?? []).filter((c) => c.teamId === wsId);
export const buildersIn = (s: PyoState, wsId = s.currentTeamId) =>
  s.builders.filter((b) => b.teamId === wsId);
export const integrationIn = (
  s: PyoState,
  kind: Integration["kind"],
  wsId = s.currentTeamId,
) => s.integrations.find((i) => i.teamId === wsId && i.kind === kind);
export const membersIn = (s: PyoState, wsId = s.currentTeamId) =>
  s.members.filter((m) => m.teamId === wsId);

export function deploymentsWhere(
  s: PyoState,
  f: {
    teamId?: string;
    projectId?: string;
    serviceId?: string;
    clusterId?: string;
  },
) {
  return s.deployments
    .filter(
      (d) =>
        (!f.teamId || d.teamId === f.teamId) &&
        (!f.projectId || d.projectId === f.projectId) &&
        (!f.serviceId || d.serviceId === f.serviceId) &&
        (!f.clusterId || d.clusterId === f.clusterId),
    )
    .sort((a, b) => b.createdAt - a.createdAt);
}

export const activeDeploymentFor = (s: PyoState, serviceId: string) =>
  s.deployments.find((d) => d.serviceId === serviceId && isActive(d.status));

export function appWorkload(a: Application, namespace: string): Workload {
  const status: Workload["status"] =
    a.workloadKind === "CronJob"
      ? a.status === "stopped"
        ? "stopped"
        : "healthy"
      : a.status === "running"
        ? "healthy"
        : a.status === "deploying" || a.status === "restoring"
          ? "progressing"
          : a.status;
  return {
    id: `wl_${a.id}`,
    clusterId: a.clusterId,
    name: a.name,
    namespace,
    kind: a.workloadKind,
    status,
    replicas: {
      desired: a.status === "stopped" ? 0 : desiredReplicas(a),
      ready: a.runtime.readyReplicas,
    },
    cpuMillis: a.runtime.cpuMillis,
    memoryMi: a.runtime.memoryMi,
    serviceId: a.id,
    projectId: a.projectId,
    createdAt: a.createdAt,
  };
}

/**
 * Workloads on a team's clusters, as each cluster's agent last reported them.
 * Apps that were deployed but aren't in a report yet are filled in from the app.
 */
export function workloadsOf(
  s: PyoState,
  clusterId?: string,
  wsId = s.currentTeamId,
) {
  const clusterIds = new Set(clustersIn(s, wsId).map((c) => c.id));
  const reported = s.systemWorkloads.filter(
    (w) =>
      clusterIds.has(w.clusterId) && (!clusterId || w.clusterId === clusterId),
  );
  const seen = new Set(reported.map((w) => w.serviceId).filter(Boolean));
  const pending = s.services
    .filter(
      (a) =>
        clusterIds.has(a.clusterId) &&
        (!clusterId || a.clusterId === clusterId) &&
        a.currentDeploymentId &&
        !seen.has(a.id),
    )
    .map((a) =>
      appWorkload(
        a,
        s.projects.find((p) => p.id === a.projectId)?.namespace ?? "default",
      ),
    );
  return [...pending, ...reported];
}

export function clusterSummary(s: PyoState, c: Cluster) {
  const servers = serversOf(s, c.id);
  const ready = servers.filter((n) => n.status === "ready");
  const sum = (f: (n: ClusterServer) => number, list = servers) =>
    list.reduce((acc, n) => acc + f(n), 0);
  const workloads = workloadsOf(s, c.id, c.teamId);
  return {
    servers: servers.length,
    readyServers: ready.length,
    controlPlane: servers.filter((n) => n.roles.includes("control-plane")).length,
    workers: servers.filter(
      (n) => !n.roles.includes("control-plane") || n.roles.includes("worker"),
    ).length,
    cpu: {
      capacity: sum((n) => n.capacity.cpu),
      used: sum((n) => n.usage.cpu, ready),
    },
    memory: {
      capacity: sum((n) => n.capacity.memoryGi),
      used: sum((n) => n.usage.memoryGi, ready),
    },
    pods: sum((n) => n.podCount),
    workloads: workloads.length,
    unhealthyWorkloads: workloads.filter(
      (w) => w.status === "degraded" || w.status === "failed",
    ).length,
  };
}

export function projectStats(s: PyoState, p: Project) {
  const apps = s.services.filter((a) => a.projectId === p.id);
  const deps = deploymentsWhere(s, { projectId: p.id });
  const lastAct =
    activityIn(s, p.teamId).find((e) => e.projectId === p.id)?.at ??
    p.updatedAt;
  const failing = apps.filter((a) => a.status === "failed").length;
  const degraded = apps.filter((a) => a.status === "degraded").length;
  const health: "healthy" | "issues" | "empty" = !apps.length
    ? "empty"
    : failing || degraded
      ? "issues"
      : "healthy";
  return {
    apps,
    applications: apps.filter((a) => a.kind === "application").length,
    services: apps.filter((a) => a.kind === "service").length,
    running: apps.filter((a) => a.status === "running").length,
    failing,
    degraded,
    deploying: apps.filter((a) => a.status === "deploying").length,
    stopped: apps.filter((a) => a.status === "stopped").length,
    latest: deps[0],
    lastActivity: Math.max(lastAct, deps[0]?.createdAt ?? 0),
    clusters: [...new Set(apps.map((a) => a.clusterId))]
      .map((id) => s.clusters.find((c) => c.id === id)?.name)
      .filter(Boolean) as string[],
    health,
  };
}

// ── Internal helpers ───────────────────────────────────────────────────────

function log(
  s: PyoState,
  e: Omit<ActivityEvent, "id" | "at" | "actor" | "details"> &
    Partial<Pick<ActivityEvent, "actor" | "details">>,
) {
  s.activity.unshift({
    id: newId("evt"),
    at: Date.now(),
    actor: { name: s.user.name, kind: "user" },
    details: {},
    ...e,
  });
}

const findService = (s: PyoState, id: string) => {
  const a = s.services.find((x) => x.id === id);
  if (!a) throw new Error(`Unknown service ${id}`);
  return a;
};

/** The stages an image deploy reports, in order. */
const IMAGE_STAGES = [
  "queued",
  "applying",
  "scheduling",
  "pulling",
  "starting",
  "waiting_ready",
] as const;

/** Optimistic queued deployment; the API's record replaces it on the next snapshot. */
function queueDeployment(
  s: PyoState,
  serviceId: string,
  o: {
    trigger: DeploymentTrigger;
    source?: ServiceSource;
    rollbackOf?: string;
    retryOf?: string;
  },
): Deployment {
  const a = findService(s, serviceId);
  const now = Date.now();
  const source = o.source ?? a.source;
  const id = newId("dpl");
  const { revision, revisionLabel } = o.rollbackOf
    ? (() => {
        const prev = s.deployments.find((d) => d.id === o.rollbackOf)!;
        return { revision: prev.revision, revisionLabel: prev.revisionLabel };
      })()
    : revisionFor(source, id);
  const dep: Deployment = {
    id,
    teamId: a.teamId,
    projectId: a.projectId,
    serviceId,
    clusterId: a.clusterId,
    source,
    revision,
    revisionLabel,
    trigger: o.trigger,
    actor: s.user.name,
    status: "queued",
    stages: IMAGE_STAGES.map((stage) =>
      stage === "queued" ? { stage, startedAt: now } : { stage },
    ),
    stageStartedAt: now,
    createdAt: now,
    logs: [],
    events: [],
    rollbackOf: o.rollbackOf,
    retryOf: o.retryOf,
    prevServiceStatus: a.status,
    config: {
      replicas: a.replicas,
      resources: { ...a.resources },
      envKeys: a.env.map((e) => e.key),
    },
  };
  s.deployments.unshift(dep);
  a.status = "deploying";
  a.pendingChanges = false;
  a.updatedAt = now;
  return dep;
}

/** Send an app's whole configuration; with a deployment id, roll it out too. */
/** How many replicas it should have now: autoscaling moves this on its own. */
export function desiredReplicas(a: Application) {
  return a.autoscale ? (a.runtime.desired ?? a.replicas) : a.replicas;
}

function saveService(
  serviceId: string,
  deploy?: { deploymentId: string; trigger: DeploymentTrigger },
) {
  const app = get().services.find((a) => a.id === serviceId);
  if (!app) return Promise.resolve(null);
  return send("saveService", {
    service: app,
    deploymentId: deploy?.deploymentId,
    trigger: deploy?.trigger,
  });
}

// ── Public operations ──────────────────────────────────────────────────────

export const api = {
  // Teams
  switchTeam(id: string) {
    update((s) => {
      s.currentTeamId = id;
    });
  },
  createTeam(input: { name: string; description: string }) {
    const id = newId("team");
    update((s) => {
      const now = Date.now();
      s.teams.push({
        id,
        name: input.name.trim(),
        slug: teamSlug(s, input.name),
        description: input.description.trim(),
        createdAt: now,
        preferences: {
          requireDeployApproval: false,
          autoDeployOnPush: true,
          retainDeployments: 20,
        },
      });
      s.members.push({
        id: newId("mem"),
        teamId: id,
        name: s.user.name,
        email: s.user.email,
        role: "owner",
        status: "active",
        joinedAt: now,
      });
      s.currentTeamId = id;
    });
    void send("createTeam", {
      id,
      name: input.name.trim(),
      description: input.description.trim(),
    });
    return id;
  },
  updateTeam(
    id: string,
    patch: Partial<
      Pick<Team, "name" | "description" | "defaultClusterId" | "preferences">
    >,
  ) {
    update((s) => {
      const w = s.teams.find((x) => x.id === id)!;
      Object.assign(w, patch);
      if (patch.name) w.slug = teamSlug(s, patch.name, id);
    });
    return send<{ slug?: string }>("updateTeam", {
      id,
      ...patch,
      defaultClusterId:
        "defaultClusterId" in patch
          ? (patch.defaultClusterId ?? "")
          : undefined,
    });
  },
  deleteTeam(id: string) {
    return send("deleteTeam", { id });
  },
  /** Resolves with whether the invite was emailed, and its link to share. */
  inviteMember(email: string, role: Role) {
    const s0 = get();
    const id = newId("mem");
    update((s) => {
      s.members.push({
        id,
        teamId: s.currentTeamId,
        name: "",
        email,
        role,
        status: "invited",
        joinedAt: Date.now(),
      });
    });
    return send<InviteResult>("inviteMember", {
      id,
      teamId: s0.currentTeamId,
      email,
      role,
    });
  },
  /** A new invite link (the old one stops working), mailed unless `mail` is false. */
  resendInvite(id: string, mail = true) {
    return send<InviteResult>("resendInvite", { id, send: mail });
  },
  saveEmailSettings(input: {
    host: string;
    port: number;
    security: string;
    username: string;
    password?: string;
    fromEmail: string;
    fromName: string;
    publicUrl: string;
  }) {
    return send("saveEmailSettings", input);
  },
  saveBackupSettings(
    input: Omit<ControlBackupSettings, "hasSecret"> & { secretKey?: string },
  ) {
    return send("saveBackupSettings", input);
  },
  removeBackupSettings() {
    return send("removeBackupSettings", {});
  },
  async runBackup() {
    const res = await send("runBackup", {});
    if (res?.ok)
      toast({
        title: "Backup started",
        description: "It shows up below when it's done.",
      });
  },
  saveTeamBackupSettings(
    teamId: string,
    input: Omit<ControlBackupSettings, "hasSecret"> & { secretKey?: string },
  ) {
    return send("saveTeamBackupSettings", { teamId, ...input });
  },
  /** What a team backs up; applied at once, sent in the background. */
  setTeamBackupSelection(
    teamId: string,
    mode: "all" | "selected",
    exceptions: string[],
  ) {
    update((s) => {
      const tb = s.teamBackups.find((t) => t.teamId === teamId);
      if (!tb?.settings) return;
      tb.settings.mode = mode;
      tb.settings.exceptions = exceptions;
      for (const t of tb.targets) {
        const listed = exceptions.includes(t.id);
        t.included = mode === "selected" ? listed : !listed;
      }
    });
    return send("setTeamBackupSelection", { teamId, mode, exceptions });
  },
  /** Check the admin login, make the backup user, save. Throws the API's error. */
  async addDatabaseBackup(input: Record<string, unknown>) {
    const res = await addDatabaseBackupRequest(input);
    await refresh();
    return res;
  },
  updateDatabaseBackup(
    id: string,
    input: { enabled: boolean; schedule: string; hour: number; keep: number },
  ) {
    update((s) => {
      for (const tb of s.teamBackups) {
        const d = tb.databases.find((x) => x.id === id);
        if (d) Object.assign(d, input);
      }
    });
    return send("updateDatabaseBackup", { id, ...input });
  },
  deleteDatabaseBackup(id: string) {
    update((s) => {
      for (const tb of s.teamBackups)
        tb.databases = tb.databases.filter((x) => x.id !== id);
    });
    return send("deleteDatabaseBackup", { id });
  },
  async runDatabaseBackup(id: string) {
    const res = await send("runDatabaseBackup", { id });
    if (res?.ok) toast({ title: "Backup started" });
  },
  removeTeamBackupSettings(teamId: string) {
    return send("removeTeamBackupSettings", { teamId });
  },
  async runTeamBackup(teamId: string, target?: string) {
    const res = await send<{ queued?: number }>("runTeamBackup", {
      teamId,
      target,
    });
    if (res?.ok)
      toast({
        title:
          res.queued === 1 ? "Backup started" : `${res.queued} backups started`,
        description: "Each runs on its cluster. Progress shows below.",
      });
  },
  async revealBackupPassword(teamId: string) {
    const res = await send<{ password?: string }>("revealBackupPassword", {
      teamId,
    });
    return res?.ok ? (res.password ?? "") : null;
  },
  removeEmailSettings() {
    update((s) => {
      s.email = null;
    });
    return send("removeEmailSettings", {});
  },
  /** The form to post to GitHub to create the app. */
  githubManifest(publicUrl: string, org?: string) {
    return send<{ action: string; manifest: string; webhook: boolean }>(
      "githubManifest",
      { publicUrl, org: org || undefined },
    );
  },
  removeGithubApp() {
    return send("removeGithubApp", {});
  },
  githubInstallUrl() {
    return send<{ url: string }>("githubInstallUrl", {
      teamId: get().currentTeamId,
    });
  },
  removeGithubInstallation(id: number) {
    update((s) => {
      s.githubInstallations = s.githubInstallations.filter((g) => g.id !== id);
    });
    return send("removeGithubInstallation", {
      teamId: get().currentTeamId,
      id,
    });
  },
  /** Resolves with the new hook's URL path (shown once, revealable later). */
  createDeployHook(projectId: string, serviceId: string | null, name: string) {
    return send<{ path: string }>("createDeployHook", {
      id: newId("hook"),
      projectId,
      serviceId: serviceId,
      name,
    });
  },
  revealDeployHook(id: string) {
    return send<{ path: string }>("revealDeployHook", { id }, true);
  },
  removeDeployHook(id: string) {
    update((s) => {
      s.deployHooks = s.deployHooks.filter((h) => h.id !== id);
    });
    return send("removeDeployHook", { id });
  },
  /** Resolves with the new token, shown once. */
  createApiToken({
    expiresInDays,
    ...input
  }: {
    name: string;
    access: ApiKeyAccess;
    teamIds: string[] | null;
    /** Null: never expires. */
    expiresInDays: 30 | 90 | 365 | null;
  }) {
    return send<{ token: string }>("createApiToken", {
      id: newId("tok"),
      ...input,
      ...(expiresInDays ? { expiresInDays } : {}),
    });
  },
  revokeApiToken(id: string) {
    update((s) => {
      s.apiTokens = s.apiTokens.filter((t) => t.id !== id);
    });
    return send("revokeApiToken", { id });
  },
  /** Resolves with what the certificate covers and anything worth knowing about it. */
  uploadCertificate(input: {
    name: string;
    certificate: string;
    privateKey: string;
  }) {
    const id = newId("crt");
    return send<{ hostnames: string[]; warnings: string[] }>(
      "uploadCertificate",
      { id, teamId: get().currentTeamId, ...input },
    ).then((r) => (r ? { ...r, id } : null));
  },
  replaceCertificate(
    id: string,
    input: { certificate: string; privateKey: string },
  ) {
    return send<{
      hostnames: string[];
      warnings: string[];
      updated: number;
    }>("replaceCertificate", { id, ...input });
  },
  /** Resolves with the certificate and key: the only time they exist. */
  createOriginPullCertificate(input: {
    teamId: string;
    name: string;
    commonName: string;
    years: number;
  }) {
    return send<{
      id: string;
      certificate: string;
      privateKey: string;
      caCertificate: string;
    }>("createOriginPullCertificate", input);
  },
  /** The certificates a cluster requires on HTTPS; none turns mTLS off. */
  setClusterTrafficMode(clusterId: string, mode: TrafficMode) {
    update((s) => {
      const c = s.clusters.find((x) => x.id === clusterId);
      if (c) c.trafficMode = mode;
    });
    return send("setClusterTrafficMode", { clusterId, mode });
  },
  /** Add a firewall access rule to a service (it goes last). */
  addFirewallRule(input: { serviceId: string } & FirewallRuleInput) {
    return attempt("addFirewallRule", { id: newId("fwr"), ...input });
  },
  updateFirewallRule(id: string, patch: Partial<FirewallRuleInput>) {
    return attempt("updateFirewallRule", { id, ...patch });
  },
  /** A service's firewall access rules in this order (every one of them). */
  reorderFirewallRules(serviceId: string, ids: string[]) {
    update((s) => {
      const pos = new Map(ids.map((id, i) => [id, i]));
      const mine = s.firewallRules.filter((r) => r.serviceId === serviceId).sort((a, b) => (pos.get(a.id) ?? 0) - (pos.get(b.id) ?? 0));
      let i = 0;
      s.firewallRules = s.firewallRules.map((r) => (r.serviceId === serviceId ? mine[i++] : r));
    });
    return attempt("reorderFirewallRules", { serviceId, ids });
  },
  removeFirewallRule(id: string) {
    update((s) => {
      s.firewallRules = s.firewallRules.filter((x) => x.id !== id);
    });
    return send("removeFirewallRule", { id });
  },
  /** Databases: create, change, delete, connect to a service. */
  createDatabase(input: {
    name: string;
    clusterId: string;
    engine: Database["engine"];
    size: string;
    instances: number;
    pooler?: boolean;
    rest?: boolean;
    cpuMillis?: number;
    memoryMi?: number;
    storageGi?: number;
    readReplicas?: number;
    /** Postgres: extensions turned on with it. */
    extensions?: string[];
  }) {
    const id = newId("mdb");
    return attempt("createDatabase", { id, teamId: get().currentTeamId, ...input }).then((r) => ("error" in r ? r : { data: { ...r.data, id } }));
  },
  updateDatabase(
    id: string,
    patch: Partial<{ size: string; instances: number; storageGi: number; version: string; pooler: boolean; rest: boolean; deletionProtection: boolean; readReplicas: number; cpuMillis: number; memoryMi: number }>,
  ) {
    return attempt<{ readReplicas?: number }>("updateDatabase", { id, ...patch });
  },
  /** A Postgres database's extensions or a KV database's modules: the whole list that's on. */
  setDatabaseExtensions(id: string, extensions: string[]) {
    return attempt<{ restarts: boolean }>("setDatabaseExtensions", { id, extensions });
  },
  setDatabaseBackups(
    id: string,
    input: { enabled: boolean; retentionDays?: number; hour?: number; schedule?: "daily" | "weekly"; keep?: number },
  ) {
    return attempt("setDatabaseBackups", { id, ...input });
  },
  /** A new database from `sourceId`'s backups, at `targetTime` (UTC, RFC 3339) or the latest. */
  restoreDatabase(sourceId: string, name: string, targetTime?: string) {
    const id = newId("mdb");
    return attempt("restoreDatabase", { id, sourceId, name, ...(targetTime ? { targetTime } : {}) }).then((r) =>
      "error" in r ? r : { data: { ...r.data, id } },
    );
  },
  deleteDatabase(id: string) {
    return attempt("deleteDatabase", { id });
  },
  /** `prefix`: its variables as PREFIX_DATABASE_URL…, beside another database's. */
  connectDatabase(id: string, serviceId: string, prefix = "") {
    return attempt<{ variables: string[] }>("connectDatabase", { id, serviceId, ...(prefix.trim() ? { prefix: prefix.trim() } : {}) });
  },
  disconnectDatabase(id: string, serviceId: string) {
    return attempt("disconnectDatabase", { id, serviceId });
  },
  revealDatabasePassword(id: string) {
    return attempt<{ password: string; token: string | null; readonlyToken?: string | null }>("revealDatabasePassword", { id });
  },
  /** A token for pushing the team's own images to Pyo registry: shown once. */
  createRegistryToken(teamId: string, input: { name: string; scope: "push" | "pull"; expiresInDays: 30 | 90 | null }) {
    return attempt<{ token: string; username: string; address: string | null; path: string }>("createRegistryToken", {
      id: newId("rtk"),
      teamId,
      name: input.name,
      scope: input.scope,
      ...(input.expiresInDays ? { expiresInDays: input.expiresInDays } : {}),
    });
  },
  revokeRegistryToken(id: string) {
    return send("revokeRegistryToken", { id });
  },
  /** A service's web application firewall settings (all its domains). */
  setServiceWaf(serviceId: string, settings: WafSettings) {
    update((s) => {
      const a = s.services.find((x) => x.id === serviceId);
      if (a) {
        a.waf = { ...a.waf, ...settings };
        if (settings.mode === "off") a.wafState = undefined;
        else if (!a.wafState) a.wafState = { state: "installing", message: "" };
      }
    });
    return attempt("setServiceWaf", { serviceId, ...settings });
  },
  /** Add a custom WAF rule to a service. */
  addWafRule(input: {
    serviceId: string;
    name: string;
    match: "all" | "any";
    conditions: WafCondition[];
    action: WafAction;
  }) {
    return attempt("addWafRule", { id: newId("wfr"), ...input });
  },
  updateWafRule(
    id: string,
    patch: Partial<Pick<WafRule, "name" | "match" | "conditions" | "action" | "enabled">>,
  ) {
    return attempt("updateWafRule", { id, ...patch });
  },
  /** Put a service's custom WAF rules in this order (every rule's id). */
  reorderWafRules(serviceId: string, ids: string[]) {
    update((s) => {
      for (const r of s.wafRules) {
        const i = ids.indexOf(r.id);
        if (i >= 0) r.position = i;
      }
      s.wafRules = [...s.wafRules].sort((a, b) => a.position - b.position);
    });
    return attempt("reorderWafRules", { serviceId, ids });
  },
  removeWafRule(id: string) {
    update((s) => {
      s.wafRules = s.wafRules.filter((x) => x.id !== id);
    });
    return send("removeWafRule", { id });
  },
  /** Websites read the visitor's address from PROXY protocol (a load balancer in front). */
  setClusterWebProxyProtocol(clusterId: string, on: boolean) {
    update((s) => {
      const c = s.clusters.find((x) => x.id === clusterId);
      if (c) c.webProxyProtocol = on;
    });
    return send("setClusterWebProxyProtocol", { clusterId, on });
  },
  /** The gateway answers public traffic on the cluster's servers, or not. */
  setClusterPublicTraffic(clusterId: string, on: boolean) {
    update((s) => {
      const c = s.clusters.find((x) => x.id === clusterId);
      if (c) c.publicTraffic = on;
    });
    return send("setClusterPublicTraffic", { clusterId, on });
  },
  setClusterOriginPull(clusterId: string, certificateIds: string[]) {
    update((s) => {
      const c = s.clusters.find((x) => x.id === clusterId);
      if (c)
        c.originPull = { ...c.originPull, certificateIds, state: "applying" };
    });
    return send("setClusterOriginPull", { clusterId, certificateIds });
  },
  removeOriginPullCertificate(id: string) {
    update((s) => {
      s.originPullCertificates = s.originPullCertificates.filter(
        (c) => c.id !== id,
      );
    });
    return send("removeOriginPullCertificate", { id });
  },
  saveCronJob(input: {
    serviceId: string;
    id?: string;
    name: string;
    schedule: string;
    command: string;
    paused: boolean;
  }) {
    return send<{ id: string }>("saveCronJob", input);
  },
  removeCronJob(serviceId: string, id: string) {
    update((s) => {
      const a = findService(s, serviceId);
      a.cronJobs = (a.cronJobs ?? []).filter((j) => j.id !== id);
    });
    return send("removeCronJob", { serviceId, id });
  },
  runCronJob(serviceId: string, id: string) {
    return send("runCronJob", { serviceId, id });
  },
  saveSecretStore(input: {
    id?: string;
    teamId: string;
    kind: string;
    name: string;
    addr: string;
    options: Record<string, string>;
    credential?: string;
  }) {
    return send<{ id: string }>("saveSecretStore", input);
  },
  removeSecretStore(id: string) {
    update((s) => {
      s.secretStores = s.secretStores.filter((x) => x.id !== id);
    });
    return send("removeSecretStore", { id });
  },
  /** The key names at a path; never their values. */
  checkSecretImport(storeId: string, path: string) {
    return send<{ keys: string[] }>("checkSecretImport", { storeId, path });
  },
  removeCertificate(id: string) {
    update((s) => {
      s.certificates = s.certificates.filter((c) => c.id !== id);
    });
    return send("removeCertificate", { id });
  },
  revokeOauthGrant(id: string) {
    update((s) => {
      s.oauthGrants = s.oauthGrants.filter((g) => g.id !== id);
    });
    return send("revokeOauthGrant", { id });
  },
  sendTestEmail() {
    return send<{ to: string }>("sendTestEmail", {});
  },
  updateMemberRole(id: string, role: Role) {
    update((s) => {
      s.members.find((m) => m.id === id)!.role = role;
    });
    void send("updateMemberRole", { id, role });
  },
  removeMember(id: string) {
    update((s) => {
      s.members = s.members.filter((m) => m.id !== id);
    });
    void send("removeMember", { id });
  },
  updateProfile(input: { name: string; timezone: string }) {
    update((s) => {
      Object.assign(s.user, input);
    });
    return send("updateProfile", input);
  },

  // Projects
  createProject(input: { name: string; description: string; namespace: string }) {
    return api.startProject(input).id;
  },
  /** `done`: resolves once the API has it (true) or refused it (false). */
  startProject(input: { name: string; description: string; namespace: string }) {
    const id = newId("prj");
    const teamId = get().currentTeamId;
    update((s) => {
      const now = Date.now();
      s.projects.push({
        id,
        teamId,
        name: input.name.trim(),
        namespace: input.namespace,
        description: input.description.trim(),
        createdAt: now,
        updatedAt: now,
      });
    });
    const done = send("createProject", {
      id,
      teamId,
      name: input.name.trim(),
      namespace: input.namespace,
      description: input.description.trim(),
    }).then((r) => !!r);
    return { id, done };
  },
  updateProject(
    id: string,
    patch: Partial<Pick<Project, "name" | "description">>,
  ) {
    update((s) => {
      Object.assign(
        s.projects.find((x) => x.id === id)!,
        patch,
        { updatedAt: Date.now() },
      );
    });
    void send("updateProject", { id, ...patch });
  },
  /** Deletes its services too. Resolves true once the API has it. */
  deleteProject(id: string, deleteVolumes = false) {
    return send(
      "deleteProject",
      deleteVolumes ? { id, deleteVolumes } : { id },
    ).then((r) => !!r);
  },

  // Applications
  /**
   * `done`: resolves once the API has it (true) or refused it (false).
   * `databases`: databases to connect it to before its first deploy,
   * each with the service's own names for their variables.
   */
  createService(
    draft: DeployDraft,
    opts: { databases?: { id: string; env?: Record<string, string> }[] } = {},
  ): {
    serviceId: string;
    deploymentId: string;
    done: Promise<boolean>;
  } {
    const serviceId = newId("svc");
    let deploymentId = "";
    const volumes: Volume[] = [];
    // No project picked: create one named after the app, before the app itself.
    const fresh =
      draft.projectId === NEW_PROJECT
        ? {
            id: newId("prj"),
            teamId: draft.teamId,
            description: "",
            ...newProjectFor(get(), draft.name, draft.teamId),
          }
        : undefined;
    const d = fresh ? { ...draft, projectId: fresh.id } : draft;
    update((s) => {
      const now = Date.now();
      if (fresh) s.projects.push({ ...fresh, createdAt: now, updatedAt: now });
      const t = getTemplate(d.templateId);
      const source: ServiceSource =
        d.sourceType === "git"
          ? {
              type: "git",
              repoUrl: d.git.repoUrl.trim(),
              branch: d.git.branch.trim() || "main",
              commit: d.git.commit.trim(),
              builder: "dockerfile",
              dockerfilePath: d.git.dockerfilePath.trim() || "Dockerfile",
              buildContext: d.git.buildContext.trim() || ".",
              buildArgs: d.git.buildArgs.map(({ key, value }) => ({
                key,
                value,
              })),
              private: d.git.private,
              webhook: d.git.installationId ? "active" : "unavailable",
              ...(d.git.installationId
                ? { installationId: d.git.installationId, autoDeploy: true }
                : {}),
            }
          : d.sourceType === "image"
            ? {
                type: "image",
                ...d.image,
                credentialId: undefined,
                ...(d.companionOf ? { companionOf: d.companionOf } : {}),
              }
            : {
                type: "template" as const,
                templateId: t!.id,
                image: t!.image,
                reference: String(d.templateValues.version ?? t!.version),
                // Where it came from, for upgrades later.
                ...templateRecord(t!, d.templateValues),
              };
      const env: EnvironmentVariable[] = d.env.map((e) => {
        const made = t?.env.find((x) => x.key === e.key);
        return {
        id: newId("env"),
        key: e.key,
        value:
          e.secret && !e.value && made?.generate
            ? `${made.prefix ?? ""}${generateValue(made.format)}`
            : templateRefs(e.value, d.name, t),
        secret: e.secret,
        updatedAt: now,
        };
      });
      const domains: Domain[] = d.domains.map((x) => ({
        id: newId("dom"),
        hostname: x.hostname,
        portName: x.portName,
        status: "pending_dns",
        tls: x.tlsMode === "off" ? "disabled" : "pending",
        tlsMode: x.tlsMode ?? "auto",
        redirectHttps: x.tlsMode !== "off" && x.redirectHttps,
        createdAt: now,
      }));
      const ports: PortMapping[] = d.ports.map((p) => ({
        ...p,
        id: newId("port"),
        exposure: d.domains.some((x) => x.portName === p.name)
          ? "http"
          : p.exposure === "http"
            ? "internal"
            : p.exposure,
        publicPort:
          p.exposure === "nodeport"
            ? (p.publicPort ?? 30000 + Math.floor(Math.random() * 2767))
            : p.exposure === "loadbalancer"
              ? (p.publicPort ?? p.containerPort)
              : undefined,
      }));
      const a: Application = {
        id: serviceId,
        teamId: d.teamId,
        projectId: d.projectId,
        clusterId: d.clusterId,
        // One way of placing it, whichever was chosen.
        location:
          d.placement === "location" ? d.location || undefined : undefined,
        // Tags its cluster has are met by being in it; the rest are server tags.
        serverTags: (() => {
          if (d.placement !== "tags") return undefined;
          const ofCluster =
            get().clusters.find((c) => c.id === d.clusterId)?.tags ?? [];
          const rest = d.serverTags.filter((t) => !ofCluster.includes(t));
          return rest.length ? rest : undefined;
        })(),
        servers:
          d.placement === "servers" && d.serverNames.length
            ? d.serverNames
            : undefined,
        name: d.name,
        description: d.description,
        kind: t?.kind ?? "application",
        workloadKind: d.cron
          ? "CronJob"
          : (t?.workloadKind ?? d.workloadKind) === "StatefulSet"
            ? "StatefulSet"
            : "Deployment",
        ...(d.cron
          ? { schedule: d.schedule.trim(), command: d.command.trim() }
          : {}),
        // A template's entrypoint and arguments, with the deploy form's
        // values for {{key}}.
        ...(d.container && (d.container.entrypoint?.length || d.container.command?.length)
          ? { container: d.container }
          : t && (t.args?.length || t.entrypoint?.length) && !d.cron
          ? {
              container: {
                ...(t.entrypoint?.length ? { entrypoint: [...t.entrypoint] } : {}),
                ...(t.args?.length ? { command: templateArgs(t.args, d.templateValues) } : {}),
              },
            }
          : {}),
        source,
        status: "deploying",
        env,
        // A cron job finishes and stops, so nothing can reach it.
        ports: d.cron ? [] : ports,
        domains: d.cron ? [] : domains,
        resources: { ...d.resources },
        replicas: d.replicas,
        healthCheck: d.cron
          ? { ...d.healthCheck, type: "none" }
          : { ...d.healthCheck },
        runtime: { readyReplicas: 0, cpuMillis: 0, memoryMi: 0, restarts: 0 },
        pendingChanges: false,
        createdAt: now,
        updatedAt: now,
      };
      s.services.push(a);
      const cluster = s.clusters.find((c) => c.id === d.clusterId);
      for (const v of d.volumes) {
        const vol: Volume = {
          id: newId("vol"),
          teamId: d.teamId,
          clusterId: d.clusterId,
          projectId: d.projectId,
          serviceId,
          name: v.name,
          storageClass:
            v.storageClass ||
            cluster?.capabilities.storageClasses.find((c) => c.isDefault)
              ?.name ||
            "local-path",
          sizeGi: v.sizeGi,
          mountPath: v.mountPath,
          accessMode: "ReadWriteOnce",
          status: "pending",
          backup: { status: "unsupported" },
          createdAt: now,
        };
        s.volumes.push(vol);
        volumes.push(vol);
      }
      if (t)
        s.recentTemplateIds = [
          t.id,
          ...s.recentTemplateIds.filter((x) => x !== t.id),
        ].slice(0, 6);
      deploymentId = queueDeployment(s, serviceId, { trigger: "manual" }).id;
    });
    const app = get().services.find((a) => a.id === serviceId);
    // Ops are separate requests, so the project has to exist before the app's arrives.
    const done = (async () => {
      if (fresh && !(await send("createProject", fresh))) return false;
      return !!(await send("createService", {
        service: app,
        deploymentId,
        volumes,
        ...(opts.databases?.length ? { databases: opts.databases } : {}),
      }));
    })();
    return { serviceId, deploymentId, done };
  },
  updateService(
    id: string,
    patch: Partial<
      Pick<
        Application,
        | "name"
        | "description"
        | "replicas"
        | "resources"
        | "healthCheck"
        | "source"
        | "clusterId"
        | "autoRollback"
        | "serverTags"
        | "secretImports"
        | "schedule"
        | "command"
      >
    >,
    opts: { requiresRedeploy?: boolean; label?: string } = {},
  ) {
    update((s) => {
      const a = findService(s, id);
      Object.assign(a, patch, { updatedAt: Date.now() });
      if (opts.requiresRedeploy) a.pendingChanges = true;
    });
    return saveService(id);
  },
  setEnv(id: string, env: EnvironmentVariable[], _changed: string[]) {
    update((s) => {
      const a = findService(s, id);
      a.env = env;
      a.pendingChanges = true;
      a.updatedAt = Date.now();
    });
    return saveService(id);
  },
  setPorts(id: string, ports: PortMapping[]) {
    update((s) => {
      const a = findService(s, id);
      a.ports = ports;
      a.pendingChanges = true;
    });
    return saveService(id);
  },
  addDomain(
    id: string,
    input: {
      hostname: string;
      portName: string;
      redirectHttps: boolean;
      tlsMode?: TlsMode;
      certificateId?: string;
    },
  ) {
    update((s) => {
      const a = findService(s, id);
      a.domains.push({
        id: newId("dom"),
        ...input,
        status: "pending_dns",
        tls: input.tlsMode === "off" ? "disabled" : "pending",
        createdAt: Date.now(),
      });
      const port = a.ports.find((p) => p.name === input.portName);
      if (port) port.exposure = "http";
    });
    return saveService(id);
  },
  /** HTTPS settings of a domain; applied live once the service is deployed. */
  updateDomain(
    id: string,
    domainId: string,
    patch: {
      tlsMode?: TlsMode;
      redirectHttps?: boolean;
      certificateId?: string;
    },
  ) {
    update((s) => {
      const a = findService(s, id);
      const d = a.domains.find((x) => x.id === domainId);
      if (!d) return;
      Object.assign(d, patch);
      if (patch.tlsMode === "off") d.tls = "disabled";
      else if (patch.tlsMode) d.tls = "pending";
    });
    return saveService(id);
  },
  removeDomain(id: string, domainId: string) {
    update((s) => {
      const a = findService(s, id);
      a.domains = a.domains.filter((x) => x.id !== domainId);
      for (const p of a.ports)
        if (
          p.exposure === "http" &&
          !a.domains.some((x) => x.portName === p.name)
        )
          p.exposure = "internal";
    });
    return saveService(id);
  },
  /** Default-deny networking for a project, on every cluster it runs on. */
  setProjectIsolation(id: string, isolate: boolean) {
    update((s) => {
      const p = s.projects.find((x) => x.id === id);
      if (p) p.networkIsolation = isolate;
    });
    return send("setProjectIsolation", { id, isolate });
  },
  /** A new encryption key for the team; its values are re-sealed. */
  async rotateDataKey(teamId: string) {
    const res = await send<{ resealed?: number }>("rotateDataKey", { teamId });
    if (res?.ok)
      toast({
        title: "Encryption key rotated",
        description: `${res.resealed ?? 0} records re-encrypted with the new key.`,
      });
    return res?.ok ?? false;
  },
  /** A secret's value, for admins; Pyo logs each reveal. */
  async revealSecret(id: string, envId: string) {
    const res = await send<{ value?: string }>("revealSecret", {
      serviceId: id,
      envId,
    });
    return res?.ok ? (res.value ?? "") : null;
  },
  /** Resolves the hostname from the API server. */
  async verifyDomain(id: string, domainId: string) {
    update((s) => {
      const d = findService(s, id).domains.find((x) => x.id === domainId);
      if (d) {
        d.status = "verifying";
        d.error = undefined;
      }
    });
    const res = await send<{ message?: string; addresses?: string[] }>(
      "verifyDomain",
      { serviceId: id, domainId },
    );
    const host =
      get()
        .services.find((a) => a.id === id)
        ?.domains.find((d) => d.id === domainId)?.hostname ?? "Domain";
    if (res?.ok)
      toast({
        title: `${host} resolves`,
        description: res.addresses?.length
          ? `To ${res.addresses.join(", ")}`
          : undefined,
      });
    else if (res)
      toast({
        tone: "error",
        title: res.message ?? `${host} doesn't resolve yet`,
      });
  },
  restartService(id: string) {
    void send("restartService", { id });
    toast({
      title: `Restarting ${get().services.find((a) => a.id === id)?.name ?? "app"}`,
    });
  },
  /** Applied by the app's next deploy. */
  addVolume(
    serviceId: string,
    input: { name: string; mountPath: string; sizeGi: number },
  ) {
    return send("addVolume", { id: newId("vol"), serviceId: serviceId, ...input });
  },
  /** Unmounted on the service's next deploy; the volume stays. */
  detachVolume(id: string) {
    update((s) => {
      const v = s.volumes.find((x) => x.id === id);
      if (v) v.serviceId = undefined;
    });
    return send("detachVolume", { id });
  },
  /**
   * Mounted on the service's next deploy. A service of another project
   * gets it moved there first (`moving` until its cluster is done).
   */
  attachVolume(id: string, serviceId: string, mountPath: string) {
    return send<{ moving?: boolean }>("attachVolume", { id, serviceId, mountPath });
  },
  /** Replaces its data with a snapshot's; its service is stopped meanwhile. */
  restoreVolume(id: string, snapshotId: string) {
    update((s) => {
      const v = s.volumes.find((x) => x.id === id);
      if (v) {
        v.restore = { id: "", snapshotId, startedAt: Date.now() };
        v.restoreError = undefined;
        const a = s.services.find((x) => x.id === v.serviceId);
        if (a && a.status !== "stopped") a.status = "restoring";
      }
    });
    return send("restoreVolume", { id, snapshotId });
  },
  /** Grow it (never shrink); on classes that can. */
  resizeVolume(id: string, sizeGi: number) {
    return send("resizeVolume", { id, sizeGi });
  },
  /** For good. `clusterId`: one Pyo has no record of, on that cluster. */
  deleteVolume(id: string, clusterId?: string) {
    update((s) => {
      const v = s.volumes.find((x) => x.id === id);
      if (v) v.deleting = true;
    });
    return send("deleteVolume", clusterId ? { id, clusterId } : { id });
  },
  /** Resize in place: no new rollout, pods keep running. */
  scaleService(id: string, replicas: number) {
    update((s) => {
      findService(s, id).replicas = replicas;
    });
    return send("scaleService", { id, replicas });
  },
  /** Null turns autoscaling off, keeping the replicas it had reached. */
  setAutoscale(id: string, autoscale: Autoscale | null) {
    update((s) => {
      const a = findService(s, id);
      a.autoscale = autoscale ?? undefined;
      if (autoscale) a.replicas = autoscale.min;
    });
    return send("setAutoscale", { id, autoscale });
  },
  stopService(id: string) {
    update((s) => {
      const a = findService(s, id);
      a.status = "stopped";
    });
    void send("stopService", { id });
  },
  /** Resolves once the API has it (true) or refused it (false). */
  deleteService(id: string, deleteVolumes = false) {
    update((s) => {
      s.services = s.services.filter((x) => x.id !== id);
      for (const v of s.volumes)
        if (v.serviceId === id) {
          v.serviceId = undefined;
          if (deleteVolumes) v.deleting = true;
        }
    });
    return send(
      "deleteService",
      deleteVolumes ? { id, deleteVolumes } : { id },
    ).then((r) => !!r);
  },

  // Deployments
  deploy(serviceId: string, opts: { trigger?: DeploymentTrigger } = {}) {
    const trigger = opts.trigger ?? "redeploy";
    let id = "";
    update((s) => {
      id = queueDeployment(s, serviceId, { trigger }).id;
    });
    void send("deploy", { serviceId: serviceId, deploymentId: id, trigger });
    return id;
  },
  retry(deploymentId: string) {
    let id = "";
    let serviceId = "";
    update((s) => {
      const d = s.deployments.find((x) => x.id === deploymentId)!;
      serviceId = d.serviceId;
      id = queueDeployment(s, d.serviceId, {
        trigger: "retry",
        source: d.source,
        retryOf: d.id,
      }).id;
    });
    void send("deploy", {
      serviceId: serviceId,
      deploymentId: id,
      trigger: "retry",
      retryOf: deploymentId,
    });
    return id;
  },
  rollback(toDeploymentId: string) {
    let id = "";
    let serviceId = "";
    update((s) => {
      const d = s.deployments.find((x) => x.id === toDeploymentId)!;
      serviceId = d.serviceId;
      id = queueDeployment(s, d.serviceId, {
        trigger: "rollback",
        source: d.source,
        rollbackOf: d.id,
      }).id;
    });
    void send("deploy", {
      serviceId: serviceId,
      deploymentId: id,
      trigger: "rollback",
      rollbackOf: toDeploymentId,
    });
    return id;
  },
  cancel(deploymentId: string) {
    void send("cancel", { deploymentId });
  },

  // Infrastructure
  /** Creates the cluster record and returns its agent token (shown once). */
  /** Resolves with the agent token and the one-line command that sets up the cluster. */
  async createCluster(input: {
    name: string;
    location: string;
    tags?: string[];
    /** The machine Pyo runs on. */
    thisMachine?: boolean;
  }): Promise<{ id: string; token: string; command: string } | null> {
    const id = newId("cl");
    const res = await send<{ token: string; command: string }>(
      "createCluster",
      { id, teamId: get().currentTeamId, ...input },
    );
    return res ? { id, token: res.token, command: res.command } : null;
  },
  /**
   * The add-cluster form's cluster, made once its name is valid: it comes
   * with the agent token and the command. Name and tags are kept up to date
   * with `updateClusterDraft` after that.
   */
  draftCluster(input: {
    id: string;
    name: string;
    tags: string[];
    thisMachine: boolean;
    /** Over the public internet, at this URL (not the tailnet). */
    publicApiUrl?: string;
  }) {
    return attempt<{ token: string; command: string; tailscale: boolean; tailnet: boolean }>("createCluster", {
      ...input,
      teamId: get().currentTeamId,
      location: "",
    });
  },
  /** Add cluster's cluster, taken back: only while its agent hasn't connected. */
  discardClusterDraft(id: string) {
    return send("discardClusterDraft", { id }, true);
  },
  updateClusterDraft(id: string, name: string, tags: string[]) {
    return attempt("updateCluster", { id, name, tags });
  },
  /** joinServer for a form that asks as you type. */
  draftJoin(clusterId: string, serverName: string, tags: string[]) {
    return attempt<{
      command: string;
      scriptUrl: string;
      expiresAt: number;
      tailscale: boolean;
    }>("joinServer", { clusterId, serverName, tags });
  },
  /**
   * A builder on a new server, made as you fill in the form: asking again
   * with the same id replaces it until its server has joined.
   */
  draftServerBuilder(input: {
    id: string;
    name: string;
    clusterId: string;
    server: string;
  }) {
    return attempt<{ command: string }>("createBuilder", {
      ...input,
      kind: "server",
      teamId: get().currentTeamId,
    });
  },
  /** `thisMachine`: the command is run where Pyo runs. */
  async rotateAgentToken(
    id: string,
    thisMachine = false,
  ): Promise<{ token: string; command: string } | null> {
    const res = await send<{ token: string; command: string }>(
      "rotateAgentToken",
      { id, thisMachine },
    );
    return res ? { token: res.token, command: res.command } : null;
  },
  updateCluster(
    id: string,
    patch: Partial<Pick<Cluster, "name" | "location" | "tags">>,
  ) {
    update((s) => {
      Object.assign(
        s.clusters.find((c) => c.id === id)!,
        patch,
      );
    });
    return send("updateCluster", { id, ...patch });
  },
  /** `force`: one unreachable for 10 minutes, with its services. */
  removeCluster(id: string, force = false) {
    return send("removeCluster", force ? { id, force } : { id });
  },
  /** The command that joins a new server to the cluster (its token lasts an hour). */
  joinServer(clusterId: string, serverName: string, tags: string[] = []) {
    return send<{
      command: string;
      scriptUrl: string;
      expiresAt: number;
      tailscale: boolean;
    }>("joinServer", { clusterId, serverName, tags });
  },
  /** Labelled on the cluster within a heartbeat; running services aren't moved. */
  setServerTags(serverId: string, tags: string[]) {
    update((s) => {
      const n = s.servers.find((x) => x.id === serverId);
      if (n) n.tags = tags;
    });
    return send("setServerTags", { serverId, tags });
  },
  /** A GPU server: also run regular services, or keep it for GPU work. */
  setServerRegularServices(serverId: string, on: boolean) {
    update((s) => {
      const n = s.servers.find((x) => x.id === serverId);
      if (n) n.regularServices = on;
    });
    return send("setServerRegularServices", { serverId, on });
  },
  /**
   * A server's region: a name ("US East") or "City, CC". "" goes back to
   * detecting it (or the cluster's). Replicas spread across regions, and
   * traffic stays in one.
   */
  setServerLocation(serverId: string, location: string) {
    update((s) => {
      const n = s.servers.find((x) => x.id === serverId);
      const c = n && s.clusters.find((x) => x.id === n.clusterId);
      if (n) {
        n.location = location || c?.location;
        n.locationSource = location ? "manual" : "cluster";
      }
    });
    return send("setServerLocation", { serverId, location });
  },
  setServerSchedulable(serverId: string, schedulable: boolean) {
    update((s) => {
      const n = s.servers.find((x) => x.id === serverId)!;
      n.schedulable = schedulable;
      if (schedulable) n.maintenance = false;
    });
    void send("setServerSchedulable", { serverId, schedulable });
  },
  setServerMaintenance(serverId: string, on: boolean, _reason = "") {
    update((s) => {
      const n = s.servers.find((x) => x.id === serverId)!;
      n.maintenance = on;
      n.schedulable = !on;
    });
    void send("setServerMaintenance", { serverId, on });
  },
  /** Drain a server and take it out of its cluster; `force` deletes one that's gone. */
  async removeServer(serverId: string, force: boolean) {
    const r = await send("removeServer", { serverId, force });
    if (r)
      update((s) => {
        const n = s.servers.find((x) => x.id === serverId);
        if (n)
          n.removal = {
            state: force ? "removing" : "draining",
            force,
            startedAt: Date.now(),
          };
      });
    return r;
  },
  async cancelServerRemoval(serverId: string) {
    const r = await send("cancelServerRemoval", { serverId });
    if (r)
      update((s) => {
        const n = s.servers.find((x) => x.id === serverId);
        if (n) {
          n.removal = null;
          n.schedulable = true;
        }
      });
    return r;
  },

  // Builders and integration keys
  /** A builder on a new server comes with the command that sets it up. */
  async createBuilder(input: {
    name: string;
    kind: Builder["kind"];
    clusterId?: string;
    server?: string;
  }): Promise<{ id: string; command?: string } | null> {
    const id = newId("bld");
    const r = await send<{ command?: string }>("createBuilder", {
      id,
      teamId: get().currentTeamId,
      ...input,
    });
    return r ? { id, command: r.command } : null;
  },
  async rotateBuilderToken(id: string): Promise<string | null> {
    const r = await send<{ command: string }>("rotateBuilderToken", { id });
    return r?.command ?? null;
  },
  removeBuilder(id: string) {
    update((s) => {
      const b = s.builders.find((x) => x.id === id);
      if (b) b.status = "removing";
    });
    return send("removeBuilder", { id });
  },
  saveUpdateSettings(input: UpdateSettings) {
    update((s) => {
      if (s.updates) s.updates.settings = input;
    });
    return send("saveUpdateSettings", input);
  },
  /** Ask GitHub now; resolves with what it found. */
  checkForUpdates() {
    return send<{ error: string | null; version: string | null }>(
      "checkForUpdates",
      {},
    );
  },
  startUpdate(version: string) {
    return send<{ id: string }>("startUpdate", { version });
  },
  saveObservabilitySettings(ipMode: IpMode) {
    update((s) => {
      s.observabilitySettings.ipMode = ipMode;
    });
    return send("saveObservabilitySettings", { ipMode });
  },
  /** Observability on or off, for the whole installation (its admin). */
  setObservability(enabled: boolean) {
    update((s) => {
      s.observabilitySettings.enabled = enabled;
    });
    return send("setObservability", { enabled });
  },
  /** Checks the login with the registry first; resolves with whether it could. */
  saveRegistryLogin(input: {
    host: string;
    username: string;
    password: string;
  }) {
    return send<{ host: string; verified: boolean }>("saveRegistryLogin", {
      id: newId("rgl"),
      teamId: get().currentTeamId,
      ...input,
    });
  },
  removeRegistryLogin(id: string) {
    update((s) => {
      s.registryLogins = s.registryLogins.filter((l) => l.id !== id);
    });
    return send("removeRegistryLogin", { id });
  },
  saveIntegration(kind: Integration["kind"], secret: string) {
    return send("saveIntegration", {
      teamId: get().currentTeamId,
      kind,
      secret,
    });
  },
  removeIntegration(kind: Integration["kind"]) {
    update((s) => {
      s.integrations = s.integrations.filter(
        (i) => !(i.teamId === s.currentTeamId && i.kind === kind),
      );
    });
    return send("removeIntegration", { teamId: get().currentTeamId, kind });
  },

  // Preferences (this browser)
  setNotificationTopic(topic: NotificationTopic, on: boolean) {
    update((s) => {
      const muted = new Set(s.mutedTopics?.[s.currentTeamId] ?? []);
      if (on) muted.delete(topic);
      else muted.add(topic);
      s.mutedTopics = { ...s.mutedTopics, [s.currentTeamId]: [...muted] };
    });
  },
  markNotificationsRead() {
    update((s) => {
      s.notificationsReadAt = Date.now();
    });
  },
};
