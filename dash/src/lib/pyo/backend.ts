// Client for the Rust API (crates/pyo-api). Same origin: the API serves
// this dashboard, and Vite proxies /api to it in dev. Sessions are an
// HttpOnly cookie; every request carries x-pyo-csrf, which the API
// requires on anything that changes state.

import type {
  DatabaseExtension,
  AnalyticsSummary,
  AnalyticsTimeseries,
  GithubRepo,
  LogEntry,
  User,
} from "./types";

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function request<T>(
  path: string,
  init: { method?: string; body?: unknown } = {},
): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method: init.method ?? "GET",
      credentials: "same-origin",
      headers: {
        "x-pyo-csrf": "1",
        ...(init.body !== undefined
          ? { "content-type": "application/json" }
          : {}),
      },
      body: init.body !== undefined ? JSON.stringify(init.body) : undefined,
    });
  } catch {
    throw new ApiError("Can't reach the Pyo API. Check your connection.", 0);
  }
  if (res.status === 204) return undefined as T;
  const data = await res.json().catch(() => null);
  if (!res.ok)
    throw new ApiError(
      (data && typeof data.error === "string" && data.error) ||
        `Request failed (${res.status})`,
      res.status,
    );
  return data as T;
}

export const auth = {
  /** The signed-in user, or null. */
  async me(): Promise<User | null> {
    try {
      return await request<User>("/api/auth/me");
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return null;
      throw e;
    }
  },
  /** Whether sign-up is open; with an invite link's secret, its invite. */
  config: (invite?: string) =>
    request<{
      signupOpen: boolean;
      invite: { email: string; team: string } | null;
    }>(
      `/api/auth/config${invite ? `?${new URLSearchParams({ invite })}` : ""}`,
    ),
  /** Join a team with an invite link, signed in with the invited address. */
  acceptInvite: (invite: string) =>
    request<{ team: string }>("/api/auth/invite/accept", {
      method: "POST",
      body: { invite },
    }),
  login: (email: string, password: string) =>
    request<User>("/api/auth/login", {
      method: "POST",
      body: { email, password },
    }),
  signup: (name: string, email: string, password: string, invite?: string) =>
    request<User>("/api/auth/signup", {
      method: "POST",
      body: { name, email, password, invite },
    }),
  /** The first sign-in: replaces the login the installer printed. */
  setup: (name: string, email: string, password: string) =>
    request<User>("/api/auth/setup", {
      method: "POST",
      body: { name, email, password },
    }),
  logout: () => request<void>("/api/auth/logout", { method: "POST" }),
};

/** Options for navigator.credentials, base64url where the browser wants bytes. */
export type PasskeyChallenge = {
  challengeId: string;
  publicKey: Record<string, unknown>;
};

export type Passkey = {
  id: string;
  name: string;
  /** The host name it works at. */
  rpId: string;
  createdAt: number;
  lastUsedAt: number | null;
};

/** Passkeys: sign in (passkeys.rs), and the signed-in user's own. */
export const passkeys = {
  loginStart: () =>
    request<PasskeyChallenge>("/api/auth/passkey/login/start", {
      method: "POST",
    }),
  loginFinish: (challengeId: string, credential: unknown) =>
    request<User>("/api/auth/passkey/login/finish", {
      method: "POST",
      body: { challengeId, credential },
    }),
  list: () => request<Passkey[]>("/api/auth/passkeys"),
  /** Takes the account's password. */
  registerStart: (password: string) =>
    request<PasskeyChallenge>("/api/auth/passkeys/register/start", {
      method: "POST",
      body: { password },
    }),
  registerFinish: (challengeId: string, credential: unknown) =>
    request<{ id: string; name: string; rpId: string }>(
      "/api/auth/passkeys/register/finish",
      { method: "POST", body: { challengeId, credential } },
    ),
  rename: (id: string, name: string) =>
    request<void>(`/api/auth/passkeys/${encodeURIComponent(id)}`, {
      method: "PATCH",
      body: { name },
    }),
  remove: (id: string) =>
    request<void>(`/api/auth/passkeys/${encodeURIComponent(id)}`, {
      method: "DELETE",
    }),
};

/** One write: the op name plus its fields (see Op in crates/pyo-api/src/ops.rs). */
export function op<T = { ok: boolean }>(
  name: string,
  args: Record<string, unknown>,
): Promise<T & { ok: boolean }> {
  return request("/api/v1/ops", {
    method: "POST",
    body: { op: name, ...args },
  });
}

/**
 * An op sent as the page goes away (navigating off, closing the tab):
 * keepalive lets it finish after the page is gone. Nothing waits for it.
 */
export function opOnLeave(name: string, args: Record<string, unknown>) {
  try {
    void fetch("/api/v1/ops", {
      method: "POST",
      credentials: "same-origin",
      keepalive: true,
      headers: { "x-pyo-csrf": "1", "content-type": "application/json" },
      body: JSON.stringify({ op: name, ...args }),
    }).catch(() => undefined);
  } catch {
    // Too late to send: the cleanup in deploy.rs removes it after a day.
  }
}

/** `pyo login`: a one-time code for the CLI waiting on localhost. */
export function approveCli(challenge: string, name: string) {
  return request<{ code: string }>("/api/v1/cli/approve", {
    method: "POST",
    body: { challenge, name },
  });
}

/** Set up backups of a database: Pyo makes its read-only backup user. */
export function addDatabaseBackup(body: Record<string, unknown>) {
  return request<{ ok: boolean; id: string }>("/api/v1/database-backups", {
    method: "POST",
    body,
  });
}

/** The OAuth consent page: who's asking, then the user's answer. */
export const oauth = {
  client: (params: string) =>
    request<{ name: string; redirectHost: string }>(
      `/api/oauth/client?${params}`,
    ),
  approve: (params: Record<string, string>, allow: boolean) =>
    request<{ redirect: string }>("/api/oauth/approve", {
      method: "POST",
      body: { ...params, allow },
    }),
};

/** A tool call an assistant made over MCP. */
export type McpCall = {
  id: number;
  client: string;
  tool: string;
  args: Record<string, unknown>;
  ok: boolean;
  error: string | null;
  durationMs: number;
  createdAt: number;
};
export const fetchMcpCalls = () =>
  request<{ calls: McpCall[] }>("/api/v1/mcp/calls");

/** A container image the team's services have run, with each version (digest) deployed. */
export type ImageVersion = {
  image: string;
  label: string;
  digest: string | null;
  firstDeployed: number;
  lastDeployed: number;
  deployments: number;
  serviceIds: string[];
  live: boolean;
  /** Removed from Pyo's registry by retention. */
  expired: boolean;
  /** Compressed bytes, for images Pyo built. */
  size: number | null;
};
export type TeamImage = {
  /** Pushed by the team to Pyo registry. */
  pushed?: boolean;
  repository: string;
  built: boolean;
  versions: ImageVersion[];
};
/** Pyo registry, as the team sees it (GET /registry). */
export type RegistryInfo = {
  /** In the cluster only: where servers pull from. Users use pushAddress. */
  address: string;
  location: { clusterId: string; server: string | null } | null;
  reachable: boolean;
  usedBytes: number | null;
  repositories: {
    /** As servers pull it (the in-cluster address). */
    repository: string;
    /** A pushed one as named from the tailnet (<pushAddress>/<team>/<project>/<service>); null for built ones or without a tailnet. */
    tailnetRepository?: string | null;
    /** Built by Pyo from Git, or pushed by the team. */
    kind: "built" | "pushed";
    serviceId: string | null;
    tags: number;
    bytes: number;
    lastPushAt: number | null;
  }[];
  retainDeployments: number;
  lastCleanupAt: number | null;
  /** The registry's address from the team's tailnet (push and pull its own images); null without a tailnet. */
  pushAddress: string | null;
  /** The team's part of its image paths: "<slug>" (images are <slug>/<project>/<service>). */
  teamPath: string;
  tokens: RegistryToken[];
};
export type RegistryToken = {
  id: string;
  name: string;
  scope: "push" | "pull";
  createdBy: string;
  createdAt: number;
  expiresAt: number | null;
  lastUsedAt: number | null;
};
export const fetchRegistry = (team: string) =>
  request<RegistryInfo>(`/api/v1/registry?${new URLSearchParams({ team })}`);

export const fetchImages = (team: string) =>
  request<{ images: TeamImage[] }>(
    `/api/v1/images?${new URLSearchParams({ team })}`,
  );

export const fetchState = () =>
  request<Record<string, unknown>>("/api/v1/state");

export type AnalyticsResponse = {
  summary: AnalyticsSummary;
  series: AnalyticsTimeseries;
  previous: Pick<
    AnalyticsSummary,
    "estimatedVisitors" | "pageviews" | "errorRate"
  >;
};

/** Traffic for a team's websites (all, one project's, one service's, or one domain). */
export const fetchAnalytics = (q: {
  team: string;
  project: string;
  site: string;
  range: string;
  service?: string;
}) => request<AnalyticsResponse>(`/api/v1/observability?${new URLSearchParams(q)}`);

/** Connections to a service's public TCP and UDP ports through the gateway. */
export type ConnectionsResponse = {
  range: string;
  ports: {
    port: number;
    protocol: "TCP" | "UDP";
    connections: number;
    clients: number;
    bytesIn: number;
    bytesOut: number;
    durationP50Ms: number;
    durationP95Ms: number;
    blocked: number;
    limited: number;
    unreachable: number;
  }[];
  series: { at: number; connections: number; clients: number; bytes: number }[];
  countries: { code: string; name: string; clients: number; connections: number }[];
  outcomes: { outcome: string; count: number }[];
  recent: {
    at: number;
    port: number;
    protocol: "TCP" | "UDP";
    ip: string;
    country: string;
    durationMs: number;
    bytesIn: number;
    bytesOut: number;
    outcome: "ok" | "blocked" | "limited" | "unreachable";
  }[];
  ipMode: "none" | "anonymized" | "full";
};

export const fetchConnections = (serviceId: string, range: string) =>
  request<ConnectionsResponse>(
    `/api/v1/services/${encodeURIComponent(serviceId)}/connections?${new URLSearchParams({ range })}`,
  );

/** What a service's firewall turned away. */
export type BlockedResponse = {
  range: string;
  total: number;
  /** One point per bucket of the range. */
  series: { at: number; count: number }[];
  rules: { rule: string; count: number }[];
  sources: { ip: string; country: string; countryName: string; count: number; clients: number }[];
  countries: { country: string; count: number }[];
  /** Websites only. */
  paths: { path: string; count: number }[];
  recent: {
    at: number;
    serviceId: string;
    kind: "website" | "tcp" | "udp";
    target: string;
    ip: string;
    country: string;
    rule: string;
  }[];
  ipMode: "none" | "anonymized" | "full";
};

export const fetchFirewallBlocked = (service: string, range: string) =>
  request<BlockedResponse>(
    `/api/v1/firewall/blocked?${new URLSearchParams({ service, range })}`,
  );

/** What a service's web application firewall found. */
export type WafHitsResponse = {
  range: string;
  blocked: number;
  logged: number;
  clients: number;
  /** One point per bucket of the range. */
  series: { at: number; blocked: number; logged: number }[];
  rules: { rule: number; message: string; count: number }[];
  hosts: { host: string; blocked: number; logged: number }[];
  sources: { ip: string; country: string; blocked: number; logged: number }[];
  countries: { country: string; blocked: number; logged: number }[];
  paths: { path: string; blocked: number; logged: number }[];
  /** Each custom rule's matches in the range, by its number. */
  custom?: { rule: number; count: number }[];
  recent: {
    at: number;
    serviceId: string;
    host: string;
    method: string;
    url: string;
    ip: string;
    country: string;
    blocked: boolean;
    rules: number[];
    messages: string[];
    severity: string;
  }[];
};

export const fetchWafHits = (service: string, range: string) =>
  request<WafHitsResponse>(
    `/api/v1/waf/hits?${new URLSearchParams({ service, range })}`,
  );

/** The Postgres extensions and KV modules Pyo's databases offer. */
export const fetchDatabaseExtensions = () => request<{ extensions: DatabaseExtension[] }>("/api/v1/database-extensions");

/** The address Pyo sees this browser at, and its country. */
export const fetchMyAddress = () =>
  request<{ ip: string; country: string }>("/api/v1/firewall/me");

/** One request the web server handled. */
export type AccessLog = {
  at: number;
  host: string;
  method: string;
  path: string;
  url: string;
  status: number;
  durationMs: number;
  bytes: number;
  country: string;
  countryName: string;
  city: string;
  referrer: string;
  device: string;
  browser: string;
  /** As the browser sent it; empty for requests stored before it was kept. */
  userAgent: string;
  /** Per the analytics IP setting: empty when it's "none". */
  ip: string;
};

export type AccessLogSummary = {
  requests: number;
  bytes: number;
  serverErrors: number;
  clientErrors: number;
  p50: number;
  p95: number;
};

/** The web server's request log, newest first; `next` pages back from there. */
export const fetchAccessLogs = (q: {
  team: string;
  site: string;
  range: string;
  status: string;
  method: string;
  q: string;
  before?: number;
}) =>
  request<{
    logs: AccessLog[];
    next: number | null;
    ipMode: string;
    /** Totals for the filters; on the first page only. */
    summary: AccessLogSummary | null;
  }>(
    `/api/v1/access-logs?${new URLSearchParams(
      Object.entries({
        ...q,
        before: String(q.before ?? 0),
        limit: "100",
      }).filter(([, v]) => v !== "" && v !== "all"),
    )}`,
  );

/** One server's or app's samples: CPU in cores, memory in MiB, one value per time. */
export type MetricsEntity = {
  id: string;
  name: string;
  clusterId: string;
  /** null where no sample was taken (the cluster or Pyo was down). */
  cpu: (number | null)[];
  memoryMi: (number | null)[];
  /** Cores: the server's, or the app's limit times its replicas. */
  cpuCapacity: number;
  memoryCapacityMi: number;
  /** Pods on the server, or the app's ready replicas, at the last sample. */
  pods: number;
  restarts: number;
  // Null where unknown (not collected yet): shown as "—".
  /** Network bytes per second (the kubelet's view: the server, or the app's pods). */
  netRxBps?: (number | null)[];
  netTxBps?: (number | null)[];
  /** Servers: root filesystem used, and its size at the last sample. */
  diskUsedGi?: (number | null)[];
  diskCapacityGi?: number | null;
  /** Servers, from Vector's host metrics: load, disk I/O and network cards. */
  load1?: (number | null)[];
  diskReadBps?: (number | null)[];
  diskWriteBps?: (number | null)[];
  nicRxBps?: (number | null)[];
  nicTxBps?: (number | null)[];
  /** Apps: storage used in MiB. */
  storageMi?: (number | null)[];
};

export type MetricsResponse = {
  range: string;
  stepMs: number;
  times: number[];
  servers: MetricsEntity[];
  services: MetricsEntity[];
  /** The gateway on each cluster. */
  ingress?: MetricsEntity[];
};

/** One run of a cron job. */
export type CronRunRecord = {
  job: string;
  startedAt: number;
  finishedAt: number | null;
  status: "running" | "succeeded" | "failed";
  exitCode: number | null;
  reason: string | null;
  output: string | null;
};

/** A cron job's recent runs, newest first. `cron` is empty for a cron job service. */
export const fetchCronRuns = (service: string, cron = "") =>
  request<{ runs: CronRunRecord[] }>(
    `/api/v1/cron-runs?${new URLSearchParams({ service, cron })}`,
  );

/** CPU and memory over time for a team's servers and services (sampled every minute). */
export const fetchMetrics = (q: {
  team: string;
  cluster: string;
  range: string;
}) => request<MetricsResponse>(`/api/v1/metrics?${new URLSearchParams(q)}`);

/** A Docker Compose file as Pyo would import it (crates/pyo-api/src/compose.rs). */
export type ComposePlan = {
  project: { name: string; namespace: string };
  /** In deploy order: dependencies first. */
  services: ComposeService[];
  warnings: ComposeWarning[];
};
export type ComposeService = {
  name: string;
  composeName: string;
  source: {
    type: "image" | "git";
    image: string;
    reference: string;
    repoUrl: string;
    branch: string;
    buildContext: string;
    dockerfilePath: string;
    buildArgs: { key: string; value: string; secret: boolean }[];
    installationId?: number | null;
    private: boolean;
  };
  ports: {
    name: string;
    containerPort: number;
    protocol: "HTTP" | "TCP" | "UDP";
    public: boolean;
    publicPort?: number | null;
    domain?: string | null;
  }[];
  env: { key: string; value: string; secret: boolean }[];
  volumes: { name: string; mountPath: string; sizeGi: number; fromBind: boolean }[];
  healthCheck: Record<string, unknown>;
  replicas: number;
  resources: Record<string, number>;
  container: Record<string, unknown>;
  dependsOn: string[];
};
export type ComposeWarning = { service?: string | null; field: string; message: string };

/** What importing a Compose file would create; creates nothing. */
export const previewCompose = (body: {
  teamId: string;
  file?: string;
  env?: string;
  repo?: { fullName: string; branch: string; path: string };
  clusterId?: string;
  name?: string;
}) => request<ComposePlan>("/api/v1/compose/preview", { method: "POST", body });

/** Repositories the team's GitHub App installations can see. */
export const fetchGithubRepos = (team: string) =>
  request<{ repositories: GithubRepo[] }>(
    `/api/v1/github/repos?${new URLSearchParams({ team })}`,
  );

/** The file a Git build would use by default: the Dockerfile, else the
 * Containerfile, in the build context; null when there's neither. */
export const fetchBuildFile = (q: {
  team: string;
  repo: string;
  branch: string;
  context: string;
}) =>
  request<{ file: "Dockerfile" | "Containerfile" | null }>(
    `/api/v1/github/build-file?${new URLSearchParams(q)}`,
  );

/** Whether the app's site can be shown in a frame (the API asks the site). */
export const fetchPreview = (serviceId: string) =>
  request<{ frameable: boolean; reason?: string }>(
    `/api/v1/services/${encodeURIComponent(serviceId)}/preview`,
  );

/** A database's part, as its logs tell it. */
export type DbRole = "primary" | "replica" | "read" | "pooler" | "read-pooler" | "sentinel" | "proxy" | "backup" | "import" | "other";
export type DbLogLine = {
  at: number;
  level: "debug" | "info" | "warn" | "error";
  source: string;
  role: DbRole;
  message: string;
  /** Structured lines (Postgres): the whole record. */
  raw?: unknown;
};
export type DbEvent = {
  at: number;
  type: "Normal" | "Warning";
  reason: string;
  /** "cluster": reported by the cluster; "pyo": a status change Pyo saw; "team": a change someone made. */
  source: "cluster" | "pyo" | "team";
  object?: string;
  message: string;
  count?: number;
};
type Ser = (number | null)[];

/** A database member's figures over a range (GET /databases/{id}/analytics). */
export type DbMemberSeries = {
  name: string;
  /** "instance", "read", "pooler", "read-pooler", "proxy" or "sentinel". */
  component: string;
  /** Instances: "primary" or "replica". */
  role: string;
  server: string;
  /** Cores. */
  cpu: Ser | null;
  memoryMi: Ser | null;
  diskUsedGi: Ser | null;
  diskCapacityGi: number | null;
  connections: Ser | null;
  /** Replicas and read replicas: seconds behind, the worst in each bucket. */
  lagSeconds: Ser | null;
};

/** A database's health over time; a series is null when it doesn't report it. */
export type DatabaseAnalytics = {
  range: string;
  stepMs: number;
  times: number[];
  engine: "postgres" | "mysql" | "valkey";
  members: DbMemberSeries[];
  /** Per second. */
  transactions?: Ser | null;
  queries?: Ser | null;
  poolerQueries?: Ser | null;
  commands?: Ser | null;
  connections?: {
    primary: Ser | null;
    replicas: Ser | null;
    limit: number | null;
    poolerActive: Ser | null;
    poolerWaiting: Ser | null;
    poolerServersActive?: Ser | null;
    poolerServersIdle?: Ser | null;
    poolerMaxWaitSeconds?: Ser | null;
  };
  cacheHitRatio?: Ser | null;
  keyHitRatio?: Ser | null;
  evictions?: Ser | null;
  routing?: { replica: Ser | null; primary: Ser | null; fallbacks?: Ser | null };
  storage?: { sizeBytes: Ser | null; memoryUsedBytes: Ser | null; maxmemoryBytes: number | null; keys?: Ser | null };
};

export const fetchDatabaseAnalytics = (id: string, range: string) =>
  request<DatabaseAnalytics>(
    `/api/v1/databases/${encodeURIComponent(id)}/analytics?range=${encodeURIComponent(range)}`,
  );

export const fetchDatabaseLogs = (id: string, range: string) =>
  request<{ logs: DbLogLine[]; events: DbEvent[]; fetchedAt: number }>(
    `/api/v1/databases/${encodeURIComponent(id)}/logs?range=${encodeURIComponent(range)}`,
  );

export const fetchServiceLogs = (serviceId: string) =>
  request<{ logs: Omit<LogEntry, "id">[]; fetchedAt: number }>(
    `/api/v1/services/${encodeURIComponent(serviceId)}/logs`,
  );

/** A volume's snapshots, newest first (read from its cluster: up to 2 minutes). */
export const fetchVolumeSnapshots = (volumeId: string) =>
  request<{
    snapshots: {
      id: string;
      shortId?: string;
      time: number;
      sizeBytes?: number | null;
    }[];
  }>(`/api/v1/volumes/${encodeURIComponent(volumeId)}/snapshots`);

/** Delete one of a volume's backups (owners and admins). */
export const deleteVolumeSnapshot = (volumeId: string, snapshotId: string) =>
  request<{ ok: boolean }>(
    `/api/v1/volumes/${encodeURIComponent(volumeId)}/snapshots/${encodeURIComponent(snapshotId)}`,
    { method: "DELETE" },
  );

/** A service's traffic path: domains, DNS, entry servers and replicas by region. */
export type ServiceTraffic = {
  cluster: string;
  /** The cluster's traffic routing. */
  mode: "local" | "region" | "spread";
  domains: { hostname: string; addresses: string[]; error: string | null }[];
  regions: {
    /** "" for servers without a region. */
    name: string;
    servers: {
      name: string;
      ips: string[];
      /** Its public address of each family. */
      ipv4: string | null;
      ipv6: string | null;
      ready: boolean;
      publicTraffic: boolean;
      /** Domains whose DNS points at this server. */
      pointedBy: string[];
      /** Where its requests go: its own replicas, its region's, or any. */
      sendsTo: {
        how: "server" | "region" | "anywhere" | "any";
        replicas: string[];
      } | null;
      /** This server's requests over the stats window. */
      stats?: { requests: number; errors: number; p95Ms: number } | null;
    }[];
    replicas: { name: string; server: string; ready: boolean }[];
    stats: { requests: number; errors: number; avgMs: number; p95Ms: number } | null;
    /** Requests per hour, the last 24 hours, oldest first. */
    series?: number[] | null;
  }[];
  warnings: string[];
  statsAvailable: boolean;
  statsHours: number;
  inRegionRouting: boolean;
};

export const fetchTraffic = (serviceId: string) =>
  request<ServiceTraffic>(`/api/v1/services/${encodeURIComponent(serviceId)}/traffic`);

/** What this Pyo is: its version and the dashboard features turned on (a hosted build's billing). */
export type Meta = {
  version: string;
  features: string[];
  /** Pyo over HTTPS on its tailnet; null when it's only on its own machine. */
  publicUrl: string | null;
};
export const fetchMeta = () => request<Meta>("/api/v1/meta");

/** A team's Pyo Cloud billing (only on a hosted build: see Meta.features). */
export type CloudBilling = {
  servers: number;
  pricePerServerUsd: number;
  totalUsd: number;
  status: "none" | "active" | "past_due" | "paused" | "canceled";
  provider: "polar" | "midtrans" | null;
  seats: number;
  periodEnd: number | null;
  graceUntil: number | null;
  country: string;
  polar: { configured: boolean; portal: boolean };
  /** Present only for visitors in Indonesia. */
  midtrans: {
    configured: boolean;
    pricePerServerIdr: number;
    totalIdr: number;
    /** Servers beyond what's paid this period, payable now (prorated). */
    topUpServers: number;
    topUpIdr: number;
  } | null;
};
export const cloudBilling = {
  get: (team: string) => request<CloudBilling>(`/api/cloud/billing?${new URLSearchParams({ team })}`),
  polarCheckout: (team: string) => request<{ url: string }>("/api/cloud/billing/polar/checkout", { method: "POST", body: { team } }),
  polarPortal: (team: string) => request<{ url: string }>("/api/cloud/billing/polar/portal", { method: "POST", body: { team } }),
  midtransCheckout: (team: string, kind: "month" | "topup") =>
    request<{ token: string; redirectUrl: string }>("/api/cloud/billing/midtrans/checkout", { method: "POST", body: { team, kind } }),
};
