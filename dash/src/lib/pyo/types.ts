// Pyo domain model. Every screen reads these shapes from the mock store in
// ./store.ts; a future Rust control plane is expected to return the same
// resources (ids are prefixed strings, timestamps are epoch milliseconds).

export type ID = string;
export type Timestamp = number;

// ── Team ──────────────────────────────────────────────────────────────

/** How a cluster routes requests: same server, same region, or any. */
export type TrafficMode = "local" | "region" | "spread";

export type Role = "owner" | "admin" | "developer" | "viewer";

export type Team = {
  id: ID;
  name: string;
  slug: string;
  description: string;
  createdAt: Timestamp;
  /** Default target for new deploys in this team. */
  defaultClusterId?: ID;
  preferences: {
    retainDeployments: number;
    /** What each of the team's clusters keeps of its requests. */
    requestLogs?: { maxMb: number; maxDays: number };
  };
  /** On a hosted Pyo: its agents update on their own, or when it chooses. */
  agentUpdates?: "auto" | "manual";
  /** Analytics kept shorter than the installation does (3, 6 or 12 months). Null: as long as the installation. */
  analyticsMonths?: number | null;
  /** The longest the team may keep its analytics, in months (the installation's). */
  analyticsMonthsLimit?: number;
  /** Members must use two-factor authentication to open the team. */
  requireTwoFactor?: boolean;
  /** What the audit log keeps of addresses. */
  auditIps?: AuditIps;
  /** The team's Tailscale (Integrations › Tailscale). Absent when it isn't set up. */
  tailscale?: TeamTailscale | null;
};

/** Where a cluster stands with its team's tailnet. */
export type ClusterTailnetState = "holder" | "member" | "moving" | "elsewhere" | "waiting";

/** A team's Tailscale: one cluster (the holder) keeps its client, and acts for every cluster of the team. Never a secret. */
export type TeamTailscale = {
  /** The tailnet ("tail1234.ts.net"), once seen. */
  suffix: string | null;
  holderId: ID | null;
  holderName: string | null;
  holderConnected: boolean;
  /** Why it isn't working. */
  problem: "holder_gone" | "client_refused" | null;
  clientId: string;
  /** The secret's last characters. */
  hint: string;
  /** Pyo's tagged devices on the tailnet (tag:pyo), as last seen. */
  devices: number;
  /** OAuth clients clusters kept before, to delete in the Tailscale console. `suffix`: on another tailnet. */
  oldClients: { clientId: string; hint: string; clusterId: string; cluster: string; suffix?: string; error?: string }[];
  /** Servers of removed clusters still on a tailnet. */
  leftovers: { cluster: string; suffix: string | null; servers: string[] }[];
  /** The team's clusters, oldest first. `servers`: on a tailnet, `onTailnet`: on the team's. */
  clusters: { id: ID; state: ClusterTailnetState; suffix: string | null; servers: number; onTailnet: number }[];
};

export type AuditIps = "full" | "anonymized" | "none";

/** The account's two-factor authentication (an authenticator app). */
export type TwoFactorStatus = {
  enabled: boolean;
  recoveryCodesLeft: number;
  /** The installation, or Pyo Cloud's staff rule, requires it of this account. */
  required: boolean;
  /** Required and not on yet: only the profile works until it is. */
  setupNeeded: boolean;
  /** Teams this account can't open until it's on. */
  lockedTeams: { id: ID; name: string; slug: string }[];
};

/** The installation's security settings (its admin). */
export type SecuritySettings = {
  requireTwoFactor: boolean;
  auditDays: number;
};

/** Where the installation's admins may sign in from (its admin). Neither: anywhere. */
export type AdminSignIn = {
  tailnet: boolean;
  /** IP addresses and ranges, IPv4 or IPv6. */
  addresses: string[];
  /** This browser's address, as Pyo sees it. */
  here: string | null;
  /** This browser came over the tailnet. */
  hereTailnet: boolean;
  /** Pyo has a tailnet name, or this browser came over the tailnet. */
  tailnetAvailable: boolean;
  /** PYO_ADMIN_SIGNIN_ANYWHERE is set on Pyo: the limit is ignored. */
  anywhere: boolean;
};

/** One entry of a team's audit log. */
export type AuditEvent = {
  id: ID;
  at: Timestamp;
  action: string;
  title: string;
  actor: { id: ID | null; name: string; email: string };
  via: "dashboard" | "api" | "mcp" | "system";
  /** The API key's or assistant's name. */
  key: string | null;
  ip: string | null;
  userAgent: string;
  target: { kind?: string; id?: string; label?: string };
  outcome: "ok" | "failed";
};

export type User = {
  id: ID;
  name: string;
  email: string;
  timezone: string;
  /** Administers this installation (outgoing email and the like). */
  isAdmin?: boolean;
  /** Still on the installer's printed sign-in (auth responses only). */
  mustSetUp?: boolean;
  /** sha256 of the email, Gravatar's key for the profile picture. */
  gravatar?: string;
  /** Two-factor authentication (snapshot and /me only). */
  twoFactor?: TwoFactorStatus;
};

/** The installation's outgoing email. Non-admins only see `fromEmail`. */
export type EmailSettings = {
  host?: string;
  port?: number;
  security?: "starttls" | "tls" | "none";
  username?: string;
  passwordSet?: boolean;
  fromEmail: string;
  fromName?: string;
  publicUrl?: string;
};

export type TeamMember = {
  id: ID;
  teamId: ID;
  name: string;
  email: string;
  role: Role;
  status: "active" | "invited";
  joinedAt: Timestamp;
  /** sha256 of the email, Gravatar's key for the profile picture. */
  gravatar?: string;
  /** Uses two-factor authentication. */
  twoFactor?: boolean;
};

// ── Projects ───────────────────────────────────────────────────────────────

export type Project = {
  id: ID;
  teamId: ID;
  name: string;
  /** Kubernetes namespace its workloads use on every cluster they target. */
  namespace: string;
  description: string;
  /** Default-deny networking: only its own services and the ingress reach it. */
  networkIsolation?: boolean;
  createdAt: Timestamp;
  updatedAt: Timestamp;
};

/** What a cluster's agent sees of its protections. */
export type ClusterSecurity = {
  /** k3s Secrets encryption provider, "" when off, null when unknown. */
  secretsEncryption: string | null;
  namespaces: number;
  podSecurityEnforced: number;
  podSecurityRestricted: number;
  networkIsolated: number;
};

/** Backups of Pyo's own data (installation admins only). */
export type ControlBackupSettings = {
  /** "local": this cluster's backup store (default); "s3": a bucket. */
  destination: "local" | "s3";
  /** Whether this installation runs the backup store. */
  localAvailable: boolean;
  enabled: boolean;
  schedule: "daily" | "weekly";
  /** UTC hour. */
  hour: number;
  keep: number;
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  accessKey: string;
  hasSecret: boolean;
};
export type ControlBackupRun = {
  id: ID;
  status: "queued" | "running" | "succeeded" | "failed";
  trigger: "schedule" | "manual";
  objectKey: string | null;
  sizeBytes: number | null;
  tables: number | null;
  error: string | null;
  createdAt: Timestamp;
  finishedAt: Timestamp | null;
};
export type ControlBackup = {
  settings: ControlBackupSettings | null;
  history: ControlBackupRun[];
};

/** Updates of Pyo itself (installation admin only). */
export type UpdateSettings = {
  /** "manual": an admin starts each update. "auto": in the window. */
  mode: "manual" | "auto";
  /** Days of the maintenance window (UTC). */
  days: ("mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun")[];
  /** Hour of the maintenance window, 0-23 (UTC). */
  hour: number;
  /** Look for new releases. */
  check: boolean;
  /** Send a random install ID and the version with the check. */
  usage: boolean;
};
export type UpdateAgentStep = {
  clusterId?: ID;
  name?: string;
  from?: string | null;
  state: "waiting" | "sent" | "done" | "failed" | "skipped" | "none";
  message?: string;
};
export type UpdateRun = {
  id: ID;
  version: string;
  fromVersion: string;
  trigger: "manual" | "auto";
  status: "pyo" | "agents" | "succeeded" | "partial" | "failed";
  startedBy: string | null;
  agents: UpdateAgentStep[];
  message: string | null;
  createdAt: Timestamp;
  updatedAt: Timestamp;
  finishedAt: Timestamp | null;
};
export type Updates = {
  /** The version this Pyo runs. */
  current: string;
  settings: UpdateSettings;
  /** PYO_UPDATE_CHECK=off on this installation. */
  checksOffByEnv: boolean;
  checkedAt: Timestamp | null;
  checkError: string | null;
  latest: {
    version: string;
    url: string;
    publishedAt: string | null;
    /** Markdown, as published on GitHub. */
    notes: string;
    beforeYouUpdate: string[];
    manualOnly: boolean;
    minFrom: string | null;
  } | null;
  /** Something can be updated (Pyo, or an agent). */
  available: boolean;
  plan: {
    pyo: { from: string; to: string; needed: boolean };
    agents: {
      clusterId: ID;
      name: string;
      team: string;
      from: string;
      to: string;
      needed: boolean;
      canUpdate: boolean;
      online: boolean;
      sameCluster: boolean;
    }[];
    builders: {
      id: ID;
      name: string;
      team: string;
      kind: string;
      from: string;
      to: string;
      needed: boolean;
      automatic: boolean;
    }[];
    /** Migrations of the release this database hasn't run. */
    migrations: string[];
    platformImages: string[];
    /** How many databases run another image on the release (they restart once). */
    databasesRestart?: number;
    /** Every image the release pins, by its key in release.json. */
    images: {
      pyo: string;
      agent: string;
      builder: string;
      flows: string;
      waf: string;
      geoip: string;
      dbController: string;
      kv: string;
      mysqlOperator: string;
      mysqlBackup: string;
      postgres16: string;
      postgres17: string;
      postgres18: string;
    };
  } | null;
  /** Why "Update now" can't start (empty when it can). */
  blockers: string[];
  /** Why automatic updates are waiting for an admin, if they are. */
  autoHeld: string | null;
  active: UpdateRun | null;
  history: UpdateRun[];
  backup: { lastAt: Timestamp | null; configured: boolean };
};

/** A team's backups of its volumes and databases. */
export type TeamBackupTarget = {
  id: ID;
  kind: "volume" | "database";
  /** Null for a volume that isn't attached. */
  serviceId: ID | null;
  clusterId: ID;
  label: string;
  engine: "postgres" | "mysql" | "mariadb" | "mongo" | "redis" | null;
  volumeId: ID | null;
  claim: string | null;
  /** In scheduled and "back up all" runs, per the team's selection. */
  included: boolean;
};
export type TeamBackupRun = {
  id: ID;
  serviceId: ID | null;
  volumeId: ID | null;
  kind: "volume" | "database";
  status: "queued" | "running" | "succeeded" | "failed";
  trigger: "schedule" | "manual";
  label: string;
  snapshotId: string | null;
  sizeBytes: number | null;
  addedBytes: number | null;
  error: string | null;
  createdAt: Timestamp;
  finishedAt: Timestamp | null;
};
export type DbEngine = "postgres" | "mysql" | "mariadb" | "mongo" | "redis";
/** A backup of a database, for its activity graph. */
export type DatabaseBackupRun = {
  id: ID;
  status: "queued" | "running" | "succeeded" | "failed";
  trigger: "schedule" | "manual";
  sizeBytes: number | null;
  addedBytes: number | null;
  snapshotId: string | null;
  error: string | null;
  createdAt: Timestamp;
  startedAt: Timestamp | null;
  finishedAt: Timestamp | null;
};
/** A database the team backs up, as its read-only backup user. */
export type BackupDatabase = {
  id: ID;
  clusterId: ID;
  serviceId: ID | null;
  name: string;
  engine: DbEngine;
  host: string;
  port: number;
  database: string;
  user: string;
  enabled: boolean;
  schedule: "daily" | "weekly";
  hour: number;
  keep: number;
  /** The KV database it is, registered by its Backups tab. */
  databaseId?: ID | null;
  createdAt: Timestamp;
  /** The last year, newest first. */
  runs: DatabaseBackupRun[];
};
export type TeamBackups = {
  teamId: ID;
  databases: BackupDatabase[];
  settings:
    | (ControlBackupSettings & {
        repository: string;
        /** "all": everything but exceptions; "selected": only exceptions. */
        mode: "all" | "selected";
        exceptions: ID[];
      })
    | null;
  targets: TeamBackupTarget[];
  history: TeamBackupRun[];
};

/** How Pyo encrypts teams' values (envelope encryption). */
export type EncryptionStatus = {
  enabled: boolean;
  cipher: string;
  provider: "local" | "openbao" | null;
  kekId: string | null;
  teams: {
    teamId: ID;
    keyCreatedAt: Timestamp;
    keyRewrappedAt: Timestamp | null;
    kekId: string;
  }[];
};

// ── Infrastructure ─────────────────────────────────────────────────────────

export type ClusterStatus =
  "connected" | "degraded" | "unreachable" | "pending";

export type StorageClass = {
  name: string;
  provisioner: string;
  isDefault: boolean;
  /** Volumes on this class survive losing a server. local-path does not. */
  replicated: boolean;
  reclaimPolicy: "Delete" | "Retain";
  volumeBindingMode: "WaitForFirstConsumer" | "Immediate";
  /** Its volumes can grow. */
  allowVolumeExpansion?: boolean;
};

export type Cluster = {
  id: ID;
  teamId: ID;
  name: string;
  description: string;
  /** For finding and choosing clusters, e.g. "production", "gpu". */
  tags?: string[];
  distribution: "k3s";
  version: string;
  status: ClusterStatus;
  location: string;
  /** The cluster Pyo runs on ("This machine"). */
  sameCluster?: boolean;
  /** The CDN certificates its HTTPS requires, and whether that's in place. */
  /** How requests are routed: kept in their region (default) or spread. */
  trafficMode?: TrafficMode;
  /** Websites (ports 80 and 443) read PROXY protocol from a load balancer in front. */
  webProxyProtocol?: boolean;
  /** The gateway answers public traffic on its servers (all but build-only ones); on by default. */
  publicTraffic?: boolean;
  /** How it takes web traffic: public ports, only the tailnet, or Cloudflare Tunnel. */
  traffic?: ClusterTraffic;
  /** Its Cloudflare Tunnel; absent when it has none. */
  tunnel?: ClusterTunnel | null;
  /** Its public IPv4 address, for DNS records (never a private one); null without one. */
  publicIp?: string | null;
  /** Its Tailscale IPv4 address that answers websites; null without one. */
  tailnetIp?: string | null;
  /** Its tailnet's DNS name, for tailnet domains ("tail1234.ts.net"); null when not on Tailscale. */
  tailnet?: string | null;
  /** Tailscale on this cluster (its agent keeps the client); null when not set up. */
  tailscale?: ClusterTailscale | null;
  /** Where its free domains point, as chosen or by default. */
  freeDomainAddress?: FreeDomainAddress | "tunnel";
  /** As chosen in its settings; null: the default. */
  freeDomainAddressSet?: FreeDomainAddress | null;
  /** The address its free domains point at now; null: no free domains. */
  freeDomainIp?: string | null;
  /** What its free domains end in ("203.0.113.7.nip.io", or the tunnel's default domain); null: none. */
  freeDomainSuffix?: string | null;
  /** Its public IP is only what the internet sees (no server has it): ports may not be forwarded. */
  behindNat?: boolean;
  originPull?: {
    /** What it was last asked to require. */
    certificateIds?: ID[];
    /** What the cluster has confirmed it requires. */
    enforcedIds?: ID[];
    state?: "applying" | "active" | "failed";
    error?: string | null;
  };
  /** Added without a name: it's named after its server when the agent reports in. */
  nameFromHost?: boolean;
  /** Volumes on it that Pyo has no record of. */
  strayVolumes?: StrayVolume[];
  /** Where its previews get their URLs; null: nip.io on its address. */
  previewDomain?: ClusterPreviewDomain | null;
  connection: {
    method: "agent" | "kubeconfig";
    /** Display only; never a credential. */
    apiEndpoint: string;
    agentVersion?: string;
  };
  /** Until then its install command works, while its agent hasn't connected. */
  connectExpiresAt?: Timestamp | null;
  /** Its databases restart once its install is this Pyo's (the refresh command). */
  databasesRestart?: boolean;
  /** Its agent's last update (a hosted Pyo's cluster page). */
  agentUpdate?: {
    status: "pending" | "sent" | "done" | "failed";
    message?: string | null;
    at: Timestamp;
  } | null;
  capabilities: {
    /** The gateway: "envoy", or "none" until it answers. */
    ingressController: "envoy" | "none";
    /** Why the gateway failed to install, while it isn't running. */
    gatewayError?: string | null;
    /** Servers where another program (k3s's Traefik…) has the gateway's ports. */
    gatewayPortsTaken?: { server: string; ports: number[]; by?: string }[];
    /** k3s ServiceLB (klipper-lb) or another LoadBalancer implementation. */
    loadBalancer: "servicelb" | "metallb" | "none";
    loadBalancerIPs: string[];
    certManager: boolean;
    metricsServer: boolean;
    storageClasses: StorageClass[];
    /** What the agent found about the cluster's security; absent until it reports. */
    security?: ClusterSecurity;
    /** NVIDIA GPU support; absent until the agent reports. */
    gpu?: ClusterGpu | null;
    /** The agent can switch how traffic is routed. */
    trafficModes?: boolean;
    /** The agent can keep traffic on the same server ("local"). */
    trafficLocal?: boolean;
    /** The agent can run a Cloudflare Tunnel connector. */
    tunnel?: boolean;
    /** What the agent may do on the cluster; null until it reports it. */
    agentScope?: AgentScope | null;
    /** The agent's update protocol (0: it can't update Pyo or itself). */
    updates?: number;
    /** Whether its install is this Pyo's: "updating" while the agent
     * refreshes it (or the server Pyo runs on runs `command` by itself),
     * "outdated" until `command` runs on the cluster,
     * "roles" when it's current except for roles only `command` brings
     * (`bundles`: the databases system and the WAF don't wait for them).
     * `command` (any state): the connect command again, for the cluster's
     * first server, which also applies Pyo's latest host settings.
     * `autoUpdate`: how automatic updates stand, as the cluster's first
     * server reports it (absent without them). Only ever shown. */
    agentInstall?: {
      state: "current" | "updating" | "outdated" | "roles";
      message?: string;
      bundles?: boolean;
      command?: string;
      autoUpdate?: {
        state: "on" | "hold" | "error";
        message: string;
        version?: string;
      };
    } | null;
  };
  podCidr: string;
  serviceCidr: string;
  createdAt: Timestamp;
  lastSeenAt: Timestamp;
};

/** How a cluster takes web traffic. */
export type ClusterTraffic = "public" | "tailscale" | "tunnel";

/**
 * "starting" until Cloudflare's first report. "unknown": Pyo tried
 * Cloudflare and failed, or no report came in 5 minutes.
 */
export type TunnelHealth =
  "connected" | "degraded" | "down" | "starting" | "unknown" | "removing";

/** A cluster's Cloudflare Tunnel, as the control plane and the agent see it. */
export type ClusterTunnel = {
  state: "creating" | "ready" | "failed" | "removing";
  error: string | null;
  accountId: string;
  accountName: string;
  tunnelId: string | null;
  tunnelName: string;
  /** Its zone apex: services get names under it. */
  defaultDomain: string | null;
  /** Public TCP and UDP ports stay open on the servers. */
  portsPublic: boolean;
  connectors: { server: string; ready: boolean; reason?: string | null }[];
  /** Cloudflare's data centers it's connected to ("FRA"). */
  colos: string[];
  connections: number;
  health: TunnelHealth;
  /** The cluster's install refused a tunnel object: run the connect command again. */
  refused: boolean;
  /** Rotate was asked for, until the old token's connectors are cut. */
  rotating?: boolean;
  certificate: { id: ID; notAfter: Timestamp } | null;
  checkedAt: Timestamp | null;
};

/** A cluster's wildcard domain for preview URLs. */
export type ClusterPreviewDomain = {
  /** "preview.example.com" */
  domain: string;
  /** The wildcard record to point at the cluster: "*.preview.example.com". */
  record: { name: string; type: "A"; content: string | null };
  /** cloudflare: Pyo makes the record and a wildcard certificate. */
  dns: "cloudflare" | "manual";
  certificate: null | {
    state: "issuing" | "issued" | "failed";
    notAfter: Timestamp | null;
    error: string | null;
  };
  error: string | null;
};

export type ClusterGpu = {
  /** Servers with an NVIDIA GPU set up for containers. */
  servers: number;
  /** GPUs Kubernetes can hand out, across servers. */
  total: number;
  /** The most on one server: what one service can ask for. */
  maxPerServer: number;
  /** GPUs running pods ask for. */
  used?: number;
  models?: string[];
  runtimeClass: boolean;
  /** "ready", "starting", "none", or why it isn't running. */
  devicePlugin: string;
};

export type ServerRoleKind = "control-plane" | "etcd" | "worker";
export type ServerStatus = "ready" | "not_ready" | "unknown";

export type ClusterServer = {
  id: ID;
  clusterId: ID;
  name: string;
  /** Where it is ("City, CC"): its own, or the cluster's. */
  location?: string;
  /** "manual" (set in the dashboard), "geoip" (from its public address) or "cluster" (the cluster's). */
  locationSource?: "manual" | "geoip" | "cluster";
  roles: ServerRoleKind[];
  status: ServerStatus;
  /** False when cordoned. */
  schedulable: boolean;
  maintenance: boolean;
  kubeletVersion: string;
  osImage: string;
  arch: "amd64" | "arm64";
  containerRuntime: string;
  internalIP: string;
  externalIP?: string;
  capacity: { cpu: number; memoryGi: number; pods: number };
  /** What pods can be handed here, and what the ones placed here ask for. */
  allocatable?: { cpu: number; memoryGi: number };
  requested?: { cpu: number; memoryGi: number };
  usage: {
    cpu: number;
    memoryGi: number;
    /** From the agent's kubelet reading; absent from older agents (shown as "—"). */
    rootFs?: { usedGi: number; capacityGi: number } | null;
    imageFs?: { usedGi: number; capacityGi: number } | null;
    /** Network bytes per second since the previous heartbeat. */
    netRxBps?: number | null;
    netTxBps?: number | null;
  };
  podCount: number;
  conditions: {
    type: string;
    status: "True" | "False" | "Unknown";
    message: string;
  }[];
  createdAt: Timestamp;
  /** Being taken out of the cluster. */
  removal?: ServerRemoval | null;
  /** Services can be placed on servers by tag (label `pyo/tag-<tag>`). */
  tags?: string[];
  /** NVIDIA GPUs Kubernetes can hand out here. */
  gpus?: number;
  /** Set up for NVIDIA GPUs, counted or not yet. */
  gpuDetected?: boolean;
  /** e.g. "NVIDIA GeForce RTX 4090". */
  gpuModel?: string;
  /** GPUs running pods ask for here. */
  gpusUsed?: number;
  /** Kept for GPU work (tainted): only GPU services run here. */
  gpuDedicated?: boolean;
  /** A build-only server: only Pyo's builder runs here. */
  builder?: boolean;
  /** Its own setting; null or absent for Pyo's default. */
  regularServices?: boolean | null;
  /** The country the internet sees it in (for a region's flag). */
  publicCountry?: string;
  /** Its address as the internet sees it (found by the agent). */
  publicIp?: string;
  /** Its public IPv4 and IPv6 address as the internet sees them, if found. */
  publicIpv4?: string;
  publicIpv6?: string;
  /** Every address the cluster lists for it, both families. */
  addresses?: string[];
  /** Each GPU's memory, MiB. */
  gpuMemoryMib?: number;
  /**
   * "" (no NVIDIA GPU), "ready", "starting", "no-driver", "no-toolkit" or
   * "no-runtime" (k3s started before the toolkit: restart it).
   */
  gpuStatus?: string;
};

/**
 * draining: its pods are moving to other servers; drained: waiting for the
 * machine to be uninstalled (it then goes offline and is removed);
 * removing: being deleted from the cluster; failed: see `message`.
 */
export type ServerRemoval = {
  state: "draining" | "drained" | "removing" | "failed";
  force: boolean;
  by?: string;
  startedAt?: Timestamp;
  drainedAt?: Timestamp;
  message?: string;
};

export type WorkloadKind =
  "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob";
export type WorkloadStatus =
  "healthy" | "progressing" | "degraded" | "failed" | "stopped" | "completed";

export type Workload = {
  id: ID;
  clusterId: ID;
  name: string;
  namespace: string;
  kind: WorkloadKind;
  status: WorkloadStatus;
  replicas: { desired: number; ready: number };
  cpuMillis: number;
  memoryMi: number;
  /** Set when Pyo manages the workload. */
  serviceId?: ID;
  projectId?: ID;
  /** kube-system and the other system workloads k3s ships with, and Pyo's own. */
  system?: boolean;
  /** Servers its pods run on, as the agent last saw them. */
  servers?: string[];
  /** Why it isn't running, in plain words, when Pyo knows. */
  message?: string | null;
  createdAt: Timestamp;
};

// ── Applications and services ──────────────────────────────────────────────

export type AppKind = "application" | "service";
export type ServiceStatus =
  | "running"
  | "deploying"
  | "degraded"
  | "failed"
  | "stopped"
  /** Stopped for a volume's restore, then starting again. */
  | "restoring";

export type GitSource = {
  type: "git";
  repoUrl: string;
  branch: string;
  /** Empty means the branch head at deploy time. */
  commit: string;
  /** Dockerfile build, or zero config (Railpack) when there is none. */
  builder?: "dockerfile" | "buildpacks" | "railpack";
  /** What the zero-config build detected, e.g. "Next.js" or "Go". */
  buildpackLanguage?: string;
  dockerfilePath: string;
  buildContext: string;
  buildArgs: { key: string; value: string }[];
  private: boolean;
  credentialId?: ID;
  webhook: "active" | "inactive" | "unavailable";
  /** Connected through Pyo's GitHub App: clones with its token (private repos work). */
  installationId?: number;
  /** A repository of one of the team's Git connections (GitLab). */
  gitConnectionId?: ID;
  /** Deploy each push to the branch. Default on for connected repositories. */
  autoDeploy?: boolean;
};

/** This installation's GitHub App (no keys). */
export type GithubAppInfo = {
  slug: string;
  name: string;
  htmlUrl: string;
  owner: string;
  publicUrl: string;
  webhookActive: boolean;
  /** Permissions and events the app lacks, e.g. "Checks: write"; empty when all there. */
  missingPermissions?: string[];
  /** The app's permission settings on GitHub. */
  permissionsUrl?: string;
};

/** A GitHub account or organization a team installed the app on. */
export type GithubInstallation = {
  id: number;
  teamId: ID;
  account: string;
  accountType: "User" | "Organization";
  repositorySelection: "all" | "selected";
  /** Permissions its owner hasn't approved yet; empty when all there. */
  missingPermissions?: string[];
  /** Where its owner approves them on GitHub. */
  approveUrl?: string;
  createdAt: Timestamp;
};

/** A team's GitLab connection (never its token). */
export type GitConnection = {
  id: ID;
  teamId: ID;
  provider: "gitlab";
  url: string;
  /** The group or project the token is for, else its user. */
  account: string;
  publicUrl: string;
  info: {
    user?: string;
    scopes?: string[];
    expiresAt?: string | null;
    version?: string;
    projects?: number;
    problems?: string[];
  };
  createdAt: Timestamp;
  webhooks: { repo: string; active: boolean; error?: string | null }[];
};

/** What Test found. */
export type GitConnectionCheck = {
  working?: boolean;
  user?: string;
  account?: string;
  scopes?: string[];
  expiresAt?: string | null;
  version?: string;
  projects?: number;
  problems: string[];
};

export type GitRepo = {
  connectionId: ID;
  provider: "gitlab";
  host: string;
  fullName: string;
  name: string;
  group: string;
  private: boolean;
  defaultBranch: string;
  htmlUrl: string;
  pushedAt?: string;
};

export type GitBranch = {
  name: string;
  commit: string;
  default: boolean;
  protected: boolean;
};

export type GithubRepo = {
  installationId: number;
  account: string;
  fullName: string;
  name: string;
  private: boolean;
  defaultBranch: string;
  htmlUrl: string;
  pushedAt?: string;
};

export type ImageSource = {
  type: "image";
  image: string;
  /** Tag or sha256 digest. */
  reference: string;
  registryId: ID;
  private: boolean;
  credentialId?: ID;
  /** Deployed by a template next to its own service (Plausible's ClickHouse). */
  companionOf?: TemplateCompanionRecord;
};

/** A deploy-form value as recorded: a sensitive one only says it was set. */
export type TemplateFieldRecord =
  | string
  | number
  | boolean
  | { secret: true; set: boolean; env?: string };

/** What a template service was deployed from, recorded once, when created. */
export type TemplateRecord = {
  /** The template's own version (semver). */
  templateVersion?: string;
  /** The app's version, when the default tag was deployed. */
  appVersion?: string;
  /** Each Compose service's image, by its name there: repository:tag@digest. */
  images?: Record<string, string>;
  /** The deploy form's values, by field key (the version is in `images`). */
  fields?: Record<string, TemplateFieldRecord>;
  /** From the API: its image isn't the one the template gave it any more. */
  customized?: boolean;
};

export type TemplateCompanionRecord = {
  templateId: string;
  templateVersion?: string;
  /** Its name in the template's Compose file. */
  service: string;
  /** The image the template gave it: repository:tag@digest. */
  image: string;
};

export type TemplateSource = TemplateRecord & {
  type: "template";
  templateId: string;
  image: string;
  reference: string;
};

/**
 * Built from a folder sent with `pyo deploy`. On a service: its build
 * settings. On a deployment: also where the folder came from.
 */
export type UploadSource = {
  type: "upload";
  dockerfilePath?: string;
  buildContext?: string;
  buildArgs?: { key: string; value: string }[];
  /** Dockerfile build, or zero config (Railpack) when there is none. */
  builder?: "dockerfile" | "buildpacks" | "railpack";
  /** What the zero-config build detected, e.g. "Next.js" or "Go". */
  buildpackLanguage?: string;
  /** Deployments only: the folder's name, the computer it came from, its size. */
  folder?: string;
  machine?: string;
  files?: number;
  size?: number;
  sha256?: string;
  /** A Git service's working copy, sent with `pyo deploy --local`. */
  fromGit?: boolean;
};

export type ServiceSource = GitSource | ImageSource | TemplateSource | UploadSource;

export type EnvironmentVariable = {
  id: ID;
  key: string;
  value: string;
  secret: boolean;
  /** A secret's value never comes back from the API; this says one is set. */
  hasValue?: boolean;
  updatedAt: Timestamp;
};

export type PortProtocol = "HTTP" | "TCP" | "UDP";
/** ClusterIP only, HTTP route via the gateway, or a Service of type NodePort / LoadBalancer. */
export type Exposure = "internal" | "http" | "nodeport" | "loadbalancer";

export type PortMapping = {
  id: ID;
  name: string;
  containerPort: number;
  protocol: PortProtocol;
  exposure: Exposure;
  /** NodePort (30000-32767) or LoadBalancer port when exposed that way. */
  publicPort?: number;
  /**
   * A public TCP or UDP port ("loadbalancer") goes through the gateway
   * unless this is false. The options below only apply then, and only for TCP.
   */
  proxy?: boolean;
  /** Only these addresses ("allow"), or everyone but them ("deny"). */
  ipFilter?: { mode: "allow" | "deny"; cidrs: string[] } | null;
  /** At most this many open connections on the port, from everyone together. */
  connectionLimit?: number | null;
  /** PROXY protocol to the service. */
  sendProxyProtocol?: "V1" | "V2" | null;
  /** Read the visitor's address from a PROXY protocol header when one is sent. */
  acceptProxyProtocol?: boolean;
};

/** Where a cluster's free domains point: its public IP or its Tailscale IP. */
export type FreeDomainAddress = "public" | "tailscale";

export type DomainStatus = "pending_dns" | "verifying" | "active" | "error";
export type TlsStatus = "pending" | "issued" | "failed" | "disabled";

export type Domain = {
  id: ID;
  hostname: string;
  /** Name of the HTTP port the route targets. */
  portName: string;
  status: DomainStatus;
  tls: TlsStatus;
  /** Which certificate: "auto" picks Let's Encrypt for public domains and a self-signed one for private (tailnet, LAN). */
  tlsMode?: TlsMode;
  /** When the DNS watcher last checked it (while it's waiting for DNS). */
  dnsCheckedAt?: Timestamp;
  /** The uploaded certificate, when tlsMode is "custom". */
  certificateId?: ID;
  /** Why the certificate isn't issued, from cert-manager. */
  tlsError?: string;
  redirectHttps: boolean;
  error?: string;
  createdAt: Timestamp;
  /** Its DNS record in the team's Cloudflare account, when Pyo was asked to make one. */
  dnsRecord?: DnsRecordState;
  /** Through the cluster's Cloudflare Tunnel: "switching" (both ways work) or "on". */
  tunnel?: "switching" | "on";
  /** Its redirects in Pyo Gateway. A domain without a port only redirects. */
  redirects?: DomainRedirect[];
  /** Its header rules in Pyo Gateway. */
  headers?: HeaderRules;
  /** Only this path of the hostname ("/" when absent). The longest path wins. */
  path?: string;
  /** Send /api/x to the service as /x. */
  stripPrefix?: boolean;
  /**
   * false: a path of a hostname another entry holds (its DNS record,
   * certificate, tunnel and redirects). Absent: it holds its hostname.
   */
  primary?: boolean;
};

/** Requests under `path` go to another hostname or path. The query is kept. */
export type DomainRedirect = {
  path: string;
  toHost?: string;
  toPath?: string;
  status: 301 | 302;
  /** Keep the rest of the path after `path`. */
  keepPath: boolean;
};

export type HeaderSet = {
  set?: { name: string; value: string }[];
  remove?: string[];
};

/** Headers set or removed on a domain's responses, and on requests to its service. */
export type HeaderRules = { response?: HeaderSet; request?: HeaderSet };

/** A domain's DNS record in Cloudflare, as Pyo left it. */
export type DnsRecordState = {
  zone: string;
  type: string;
  content: string;
  /** Proxied by Cloudflare now. */
  proxied: boolean;
  /** Proxied was asked for: on once the certificate is issued. */
  proxy: boolean;
  /**
   * "created": Pyo made it. "exists": it already pointed there. "conflict":
   * other records have the name. "switching" and "reverting": moving to the
   * tunnel and back.
   */
  status:
    "created" | "exists" | "conflict" | "failed" | "switching" | "reverting";
  error?: string | null;
  /** The records in the way, on a conflict. */
  existing: CloudflareRecord[];
  /** Pyo made it, so Pyo may delete it. */
  mine: boolean;
  /** A CNAME to the cluster's tunnel, or straight to the cluster. */
  via?: "direct" | "tunnel";
  /** The records it replaced, kept for Switch back. */
  previous?: PreviousRecord[];
  updatedAt: Timestamp;
};

/** A record a tunnel domain replaced. */
export type PreviousRecord = {
  type: string;
  name: string;
  content: string;
  proxied: boolean;
  ttl: number;
  comment?: string | null;
};

export type CloudflareRecord = {
  id: string;
  type: string;
  name: string;
  content: string;
  proxied: boolean;
};

export type CloudflareZone = {
  id: string;
  name: string;
  /** Pyo can create and delete DNS records in it. */
  canEdit: boolean;
  /** The permission the token lacks there. */
  missing?: string;
  /** The Cloudflare account it's in. */
  accountId?: string;
  accountName?: string;
  /** Whether it can use Cloudflare Tunnel, and the tunnel permissions the token lacks. */
  tunnel?: { ready: boolean; missing?: string[] };
};

/** A team's Cloudflare account: its zones, never the token. */
export type CloudflareAccount = {
  teamId: ID;
  zones: CloudflareZone[];
  createdAt: Timestamp;
  testedAt: Timestamp;
  /** The token's last characters: owners and admins only. */
  hint?: string;
  createdBy?: string;
};

export type TlsMode = "auto" | "letsencrypt" | "selfsigned" | "custom" | "off";

/** A certificate the team uploaded for its domains (the key stays on the server). */
export type Certificate = {
  id: ID;
  teamId: ID;
  name: string;
  /** DNS names it's valid for; may include wildcards (*.example.com). */
  hostnames: string[];
  issuer: string;
  notBefore: Timestamp;
  notAfter: Timestamp;
  fingerprint: string;
  createdAt: Timestamp;
  updatedAt: Timestamp;
};

/** A client certificate for a CDN. Its key was shown once and isn't kept. */
export type OriginPullCertificate = {
  id: ID;
  teamId: ID;
  name: string;
  commonName: string;
  /** Public: what servers are told to trust. */
  caCertificate: string;
  fingerprint: string;
  notAfter: Timestamp;
  createdAt: Timestamp;
};

export type HealthCheck = {
  type: "http" | "tcp" | "exec" | "none";
  path: string;
  port: number;
  command: string;
  initialDelaySeconds: number;
  periodSeconds: number;
  /**
   * Pyo picks it from what each build finds: "static" once it found a static
   * site (HTTP check of /). Absent when the user chose it.
   */
  auto?: "default" | "static";
};

export type Resources = {
  cpuRequest: number; // millicores
  cpuLimit: number;
  memoryRequest: number; // MiB
  memoryLimit: number;
  /** Whole NVIDIA GPUs, all on one server; 0 or absent for none. */
  gpu?: number;
};

export type SecretStoreKind =
  | "vault"
  | "openbao"
  | "infisical"
  | "aws"
  | "gcp"
  | "azure"
  | "doppler";

/** An external secret store. Its credential never comes back. */
export type AlertKind = "telegram" | "discord" | "slack" | "webhook";

/** What an alert channel can get, each on or off. */
export type AlertEvent =
  | "deploy_failed"
  | "server_offline"
  | "server_pressure"
  | "backup_failed"
  | "certificate";

/** Where a team's alerts go. The token or URL never leaves the API: `hint` only. */
export type AlertChannel = {
  id: ID;
  teamId: ID;
  kind: AlertKind;
  name: string;
  /** "Chat -100…", "Webhook …a1b2", or a webhook's host. */
  hint: string;
  events: AlertEvent[];
  lastSentAt: number | null;
  lastError: string | null;
  lastErrorAt: number | null;
  createdAt: number;
};

export type SecretStore = {
  id: ID;
  teamId: ID;
  kind: SecretStoreKind;
  name: string;
  addr: string;
  options: {
    mount?: string;
    namespace?: string;
    projectId?: string;
    environment?: string;
    region?: string;
    project?: string;
    config?: string;
  };
  createdAt: Timestamp;
};

export type SecretImport = { storeId: ID; path: string };

export type CronTask = {
  id: ID;
  name: string;
  /** Cron syntax, in UTC. */
  schedule: string;
  command: string;
  paused: boolean;
};

export type CronRun = { startedAt: Timestamp; succeeded?: boolean | null };

export type Autoscale = {
  min: number;
  max: number;
  /** Average CPU use to aim for, as a percentage of the request. */
  targetCpu: number;
  /** The same for memory; off when unset. The busier one decides. */
  targetMemory?: number;
};

/** A service's web application firewall settings (the OWASP rules). */
export type WafSettings = {
  /** "off" for a new service. */
  mode: "off" | "log" | "block";
  /** Strictness: the Core Rule Set's paranoia level, 1 to 4. */
  level: number;
  /** Rule ids not applied. */
  exceptions?: number[];
  /** Inbound anomaly threshold: 5 (0 or unset), 10, 20 or 50. */
  threshold?: number;
};

export type WafField =
  | "path"
  | "query"
  | "method"
  | "host"
  | "header"
  | "userAgent"
  | "cookie"
  | "body"
  | "ip"
  | "country";
export type WafOperator = "equals" | "contains" | "startsWith" | "endsWith" | "in" | "regex";
export type WafAction = "block" | "log" | "allow" | "skipOwasp";

export type WafCondition = {
  field: WafField;
  /** The header's or cookie's name. */
  name?: string;
  op: WafOperator;
  /** One value, or the list for "in". */
  values: string[];
  negate?: boolean;
};

/** A custom WAF rule: a cluster's (serviceId null) or a service's. */
/** A Postgres or MySQL database, or a KV (Valkey, Redis-compatible) database. */
export type Database = {
  id: ID;
  teamId: ID;
  clusterId: ID;
  name: string;
  engine: "postgres" | "mysql" | "valkey";
  version: string;
  size: "small" | "medium" | "large" | "xlarge" | "custom";
  cpu: string;
  memory: string;
  /** Per instance, as numbers (a custom size's, or its preset's). */
  cpuMillis?: number;
  memoryMi?: number;
  storageGi: number;
  instances: number;
  pooler: boolean;
  rest: boolean;
  deletionProtection: boolean;
  /** "active", or "deleting". */
  state: string;
  /** As the cluster reports it. */
  status: {
    phase?: "Creating" | "Ready" | "Degraded" | "Failed" | string;
    message?: string;
    instances?: number;
    readyInstances?: number;
    primary?: string;
    failovers?: number;
    endpoints?: { primary?: string; replicas?: string; pooler?: string; sentinel?: string; rest?: string; read?: string };
    /** Postgres or MySQL with backups on: what they cover. */
    backup?: {
      firstRecoverabilityPoint?: string;
      lastSuccessfulBackup?: string;
      lastArchivedWal?: string;
      lastFailedBackup?: string;
      message?: string;
    };
  };
  /** Postgres and MySQL: point-in-time backups. KV: its scheduled snapshots in Backups, once turned on (else null). */
  backup?:
    | { enabled: boolean; retentionDays: number; hour: number }
    | { enabled: boolean; schedule: "daily" | "weekly"; hour: number; keep: number; backupDatabaseId: ID }
    | null;
  /** Dedicated read replicas (reads only, never the primary); null when off. */
  readReplicas?: {
    instances: number;
    ready: number;
    members: { name: string; ready: boolean; server: string; lagSeconds: number | null }[];
    /** The controller reports them. */
    reported?: boolean;
  } | null;
  /** Postgres: each extension that's on or being turned off, as its cluster reports it; null for other engines. */
  extensions?: DatabaseExtensionState[] | null;
  /** While databases are being set up on its cluster (its first one there); null once they run. */
  setup?: { state: "installing" | "starting" | "failed"; message: string } | null;
  /** Made from another database's backups. */
  restoredFrom?: { sourceId: ID; sourceName: string; targetTime: string | null } | null;
  /** Its latest import (from another database or a dump), if any. */
  import?: DatabaseImport | null;
  createdAt: number;
  /** Its instances and the servers they run on, primary first (absent until the controller reports them). */
  members?: { name: string; role: "primary" | "replica"; ready: boolean; server: string }[];
  connections: { serviceId: ID; variables: string[] }[];
  username: string;
  database: string | null;
  /** What a connected service gets, password masked. */
  variables: { key: string; value: string }[];
};

/** An import into a database: from a URL or an uploaded dump. */
export type DatabaseImport = {
  id: ID;
  state: "pending" | "running" | "succeeded" | "failed";
  /** The step it's on, while it runs. */
  phase: string;
  /** Why it failed. */
  message: string;
  source: "url" | "file";
  /** "shop on db.example.com:5432", or the file's name. */
  from: string;
  /** Made with its database: a failure leaves that one to retry or delete. */
  newDatabase: boolean;
  /** Failed, and its source is still there to try again. */
  retryable: boolean;
  createdAt: number;
  startedAt: number | null;
  finishedAt: number | null;
};

/** A Postgres extension that's on or being turned off. */
export type DatabaseExtensionState = {
  name: string;
  /** On, as asked; false while it's being turned off. */
  enabled: boolean;
  state: "enabled" | "pending" | "restarting" | "disabling" | "failed";
  message: string;
};

/** A Postgres extension or KV module Pyo's databases offer (GET /database-extensions). */
export type DatabaseExtension = {
  name: string;
  engine: "postgres" | "valkey";
  /** "pyo": Pyo turns it on and off. "additional": the database's owner adds it. */
  section: "pyo" | "additional";
  description: string;
  docs: string;
  /** A limited build, shown beside its name ("Apache 2 Edition"). */
  edition?: string;
  /** Obsolete: shown only while the database has it on. */
  hidden?: boolean;
  /** Turning it on or off restarts the database. */
  restart: boolean;
};

/** A service's custom WAF rule. */
export type WafRule = {
  id: ID;
  teamId: ID;
  serviceId: ID;
  /** Its id in findings. */
  number: number;
  name: string;
  enabled: boolean;
  position: number;
  match: "all" | "any";
  conditions: WafCondition[];
  action: WafAction;
  createdBy: string;
  createdAt: Timestamp;
};

/**
 * A service's firewall access rule, in order: allow and block rules are
 * checked from the first, and the first that matches decides; rate limits
 * (websites) apply after, each one that matches counting the request.
 */
export type FirewallRule = {
  id: ID;
  teamId: ID;
  serviceId: ID;
  action: "allow" | "block" | "rateLimit";
  operator: "in" | "notIn";
  /** Address ranges ("203.0.113.0/24") and presets ("cloudflare"), or country codes ("DE"); none: any client. */
  sources: string[];
  /** A rate limit's requests from each client address per `per`. */
  requests: number | null;
  per: "second" | "minute" | null;
  note: string;
  createdBy: string;
  createdAt: Timestamp;
};

/** What adding or changing a firewall access rule sends. */
export type FirewallRuleInput = Pick<FirewallRule, "action" | "operator" | "sources" | "note"> & {
  requests?: number;
  per?: "second" | "minute";
};

export type Application = {
  id: ID;
  teamId: ID;
  projectId: ID;
  clusterId: ID;
  /** The WAF's settings for the service ({} is off). */
  waf?: Partial<WafSettings>;
  /** Set while its WAF is on: "installing" while the WAF starts on its cluster. */
  wafState?: { state: "installing" | "ready" | "failed" | "removing"; message: string };
  /** The CDN in front of its websites: only it reaches them. */
  behindCdn?: "cloudflare" | null;
  name: string;
  description: string;
  kind: AppKind;
  workloadKind: WorkloadKind;
  /** Cron schedule when workloadKind is CronJob, in UTC. */
  schedule?: string;
  /** What a CronJob runs; empty runs the image's default. */
  command?: string;
  /** How its image starts, where it differs from the image's own (Compose imports, templates). */
  container?: { entrypoint?: string[]; command?: string[]; workingDir?: string; runAsUser?: number; runAsGroup?: number };
  /** The Compose file it was made from, and its name there. `pyo deploy -f` keeps it in line with the file. */
  compose?: { file: string; service: string };
  source: ServiceSource;
  status: ServiceStatus;
  currentDeploymentId?: ID;
  env: EnvironmentVariable[];
  ports: PortMapping[];
  domains: Domain[];
  resources: Resources;
  /** Pinned to servers in this place ("City, CC" or "CC"); anywhere in its cluster when unset. */
  location?: string;
  /**
   * Runs only on these servers (by name, in its cluster) and on every
   * server with one of these tags. Absent or null: any server.
   */
  placement?: { servers: string[]; tags: string[] } | null;
  /** How services saved before `placement` kept their tags (read as its tags). */
  serverTags?: string[];
  /** How services saved before `placement` kept their servers. */
  servers?: string[];
  replicas: number;
  /** Replicas follow CPU use, between min and max. */
  autoscale?: Autoscale;
  /** Paths in external stores, read each time it deploys. */
  secretImports?: SecretImport[];
  /** Commands run on a schedule with this service's image and variables. */
  cronJobs?: CronTask[];
  healthCheck: HealthCheck;
  /** Observed at runtime, not configured. */
  runtime: {
    /** How many replicas it should have right now. */
    desired?: number;
    /** This service's cron jobs, as its cluster reports them. */
    cronJobs?: { id: ID; suspended: boolean; lastRun?: CronRun | null }[];
    /** A cron job's latest run; `succeeded` is null while it's running. */
    lastRun?: { startedAt: Timestamp; succeeded?: boolean | null } | null;
    readyReplicas: number;
    cpuMillis: number;
    memoryMi: number;
    restarts: number;
    since?: Timestamp;
    /** Its internal IP in the cluster; may change if it's made again. */
    clusterIp?: string | null;
    /** Each replica (pod), from the agent's heartbeat. */
    replicas?: {
      name: string;
      server: string;
      ready: boolean;
      startedAt?: Timestamp | null;
      /** No server has room for it yet. */
      waiting?: boolean;
      ephemeralMi?: number | null;
      netRxBps?: number | null;
      netTxBps?: number | null;
    }[];
    /** Replicas no server has room for yet: they start when there is. */
    waiting?: number;
    /** Ephemeral storage plus volumes in use, in MiB; null/absent when unknown. */
    storageMi?: number | null;
    /** Network bytes per second, across its replicas. */
    netRxBps?: number | null;
    netTxBps?: number | null;
    /** Served as static files by the cluster's static server: no replicas of its own. */
    static?: boolean;
  };
  /** Configuration saved but not yet rolled out. */
  pendingChanges: boolean;
  /** Put the previous version back when a deploy fails. Default on. */
  autoRollback?: boolean;
  /** A build detected as a static site: served as static files ("static", the default) or run as a service. */
  serveAs?: "static" | "service";
  /** Seconds the gateway waits for a response on its domains (0: no limit). Unset: 300. */
  requestTimeout?: number | null;
  /** A temporary copy for each open pull request (Git services). */
  previews?: PreviewSettings;
  /** Set on a preview copy: the service it previews. Hidden from lists. */
  previewOf?: ID;
  /** Being deleted: listed apart (PyoState.deletingServices) until it's gone. */
  deleting?: boolean;
  /** Why its last delete failed. */
  deleteError?: string;
  createdAt: Timestamp;
  updatedAt: Timestamp;
};

/** A service's preview settings. */
export type PreviewSettings = {
  enabled: boolean;
  /** Which pull requests get one. */
  pulls: "all" | "label";
  /** When pulls is "label". */
  label: string;
  /** At most this many at once (1 to 50). */
  limit: number;
  /** Removed after this many days without a push; 0: never. */
  idleDays: number;
  /** private: on the cluster's tailnet. */
  access: "public" | "private";
  copySecrets: boolean;
  /** Pull requests from forks: not deployed, without secret variables, or with them. */
  forks: "off" | "no-secrets" | "secrets";
  /** Replace the service's variables by key, or add. */
  env: EnvironmentVariable[];
};

export type PreviewStatus =
  | "waiting"
  | "building"
  | "ready"
  | "failed"
  | "removing"
  | "removed";

/** A pull request's (merge request's) preview of a service. */
export type Preview = {
  id: ID;
  teamId: ID;
  /** The service previewed. */
  serviceId: ID;
  /** Its copy: a service with previewOf. */
  previewServiceId: ID | null;
  provider: "github" | "gitlab";
  repo: string;
  number: number;
  title: string;
  /** The pull request's web page. */
  url: string;
  branch: string;
  sha: string;
  author: string;
  fork: boolean;
  /** "web-pr-42" */
  name: string;
  hostname: string | null;
  status: PreviewStatus;
  /** Why it's waiting, failed or removed. */
  message: string | null;
  deploymentId: ID | null;
  pullState: "open" | "closed" | "merged";
  createdAt: Timestamp;
  updatedAt: Timestamp;
};

/** A template-backed (usually stateful) workload such as a database. */
export type Service = Application & { kind: "service"; source: TemplateSource };

// ── Deployments ────────────────────────────────────────────────────────────

export type DeploymentStage =
  | "queued"
  | "building"
  | "pushing"
  | "waiting_database"
  | "applying"
  | "scheduling"
  | "pulling"
  | "starting"
  | "waiting_ready"
  | "rolling_back";

export type DeploymentStatus =
  DeploymentStage | "succeeded" | "failed" | "cancelled";

export type DeploymentTrigger =
  | "manual"
  | "git_push"
  | "rollback"
  | "redeploy"
  | "config_change"
  | "retry"
  | "deploy_hook"
  /** A pull request's preview (previews). */
  | "preview";

/** A personal API token (for MCP clients and scripts); the token itself is shown once. */
/** What a cluster's agent may do there, as it reports. */
export type AgentScope = {
  /** "limited": only what Pyo manages; "full": anything on the cluster. */
  access: "limited" | "full";
  /** Whether the cluster holds the agent to its limits. */
  admission: "enforced" | "missing" | "unsupported";
  /** It may manage servers (labels, pausing, removing). */
  servers: boolean;
  /** Switches the cluster's owner set in pyo-system/pyo-policy. */
  policy: { noTerminal: boolean; noForceRemove: boolean; readOnly: boolean };
  /** The gateway's reach; absent until reported. */
  proxy?: { controllerReadsOtherSecrets: boolean; proxiesHaveCredentials: boolean } | null;
};

export type OAuthGrant = {
  id: ID;
  client: string;
  createdAt: Timestamp;
  lastUsedAt?: Timestamp | null;
  /** What the assistant may do: the same levels as API keys. */
  access?: ApiKeyAccess;
};
/** What an API key may do: look, also deploy, or anything its owner can. */
export type ApiKeyAccess = "read" | "deploy" | "sandboxes" | "full";
export type ApiToken = {
  id: ID;
  name: string;
  prefix: string;
  access: ApiKeyAccess;
  /** Null: all of your teams. */ teamIds: ID[] | null;
  createdAt: Timestamp;
  lastUsedAt?: Timestamp | null;
  /** Null: never expires. */
  expiresAt?: Timestamp | null;
};

/** A secret URL that deploys an app, or every app of a project (serviceId null). */
export type DeployHook = {
  id: ID;
  teamId: ID;
  projectId: ID;
  serviceId?: ID | null;
  name: string;
  createdBy: string;
  createdAt: Timestamp;
  lastUsedAt?: Timestamp | null;
};

export type LogLevel = "debug" | "info" | "warn" | "error";

export type LogEntry = {
  id: ID;
  at: Timestamp;
  level: LogLevel;
  source: string;
  message: string;
};

export type DeploymentEvent = {
  at: Timestamp;
  type: "Normal" | "Warning";
  reason: string;
  object: string;
  message: string;
};

export type Deployment = {
  id: ID;
  teamId: ID;
  projectId: ID;
  serviceId: ID;
  clusterId: ID;
  source: ServiceSource;
  /** Commit sha or image digest that actually ran. */
  revision: string;
  revisionLabel: string;
  trigger: DeploymentTrigger;
  actor: string;
  status: DeploymentStatus;
  stages: {
    stage: DeploymentStage;
    startedAt?: Timestamp;
    finishedAt?: Timestamp;
  }[];
  stageStartedAt: Timestamp;
  createdAt: Timestamp;
  finishedAt?: Timestamp;
  logs: LogEntry[];
  events: DeploymentEvent[];
  failure?: { stage: DeploymentStage; reason: string; message: string };
  rollbackOf?: ID;
  /** Git builds: the builder that built it. */
  builderId?: ID;
  /** On a rollback: the deployment that was live when it started (for undo). */
  rollbackFrom?: ID | null;
  /** Failed or cancelled, and the agent put the previous version back. */
  autoRolledBack?: boolean;
  /** Its image was removed by retention, so it can't be rolled back to. */
  imageExpired?: boolean;
  retryOf?: ID;
  /** Simulation option: fail when this stage completes. */
  failAt?: DeploymentStage;
  /** Simulation bookkeeping: log lines already emitted for the current stage. */
  logCursor?: number;
  /** App status to restore if this rollout fails or is cancelled. */
  prevServiceStatus?: ServiceStatus;
  /** Configuration snapshot used for this rollout. */
  config: { replicas: number; resources: Resources; envKeys: string[] };
};

// ── Templates ──────────────────────────────────────────────────────────────

export type TemplateCategory =
  | "databases"
  | "caches"
  | "storage"
  | "networking"
  | "monitoring"
  | "messaging"
  | "developer-tools"
  | "web"
  | "ai"
  | "other";

export type TemplateField = {
  key: string;
  label: string;
  type: "text" | "number" | "password" | "select" | "boolean";
  default: string | number | boolean;
  required: boolean;
  sensitive?: boolean;
  help?: string;
  options?: string[];
  /** Environment variable the field writes to. */
  envKey?: string;
};

export type TemplatePort = {
  name: string;
  port: number;
  protocol: PortProtocol;
  exposure: Exposure;
};

/**
 * A template, as the dashboard uses it: one entry of pyo-templates' catalog
 * (src/lib/pyo/catalog.json), filled in by templates.ts. The definitions
 * themselves live only in pyo-templates.
 */
export type Template = {
  id: string;
  name: string;
  description: string;
  category: TemplateCategory;
  /** Its product logo, one path in one colour; a category icon without one. */
  logo?: { hex: string; path: string };
  /** The upstream project's website. */
  homepage?: string;
  tier: "official" | "community";
  author: string;
  /** The template's own version (semver), recorded with each service. */
  templateVersion: string;
  /** The app's version the default tag runs, when the template says. */
  appVersion?: string;
  image: string;
  /** The default version (tag): the first of `versions`. */
  version: string;
  versions: { tag: string; digest: string }[];
  kind: AppKind;
  workloadKind: WorkloadKind;
  stateful: boolean;
  featured: boolean;
  /** Left out of galleries; services already using it keep working. */
  hidden: boolean;
  /** Its place in the catalog's order: lower comes first. */
  rank: number;
  ports: TemplatePort[];
  env: {
    key: string;
    value: string;
    secret: boolean;
    generate?: boolean;
    /** What's generated: a 24-character password by default. */
    format?: "long" | "hex-32" | "base64-32";
    /** Put before what's generated ("sk-", "base64:"). */
    prefix?: string;
  }[];
  /** The deploy form's settings; the first is the version picker. */
  fields: TemplateField[];
  resources: Resources;
  replicas: number;
  storage?: { required: boolean; sizeGi: number; mountPath: string };
  healthCheck: HealthCheck;
  hints: string[];
  /** Arguments to the image's own entrypoint (its CMD); `{{key}}` takes a field's value. */
  args?: string[];
  /** Replaces the image's entrypoint. */
  entrypoint?: string[];
  /** NVIDIA GPUs: required, or used when the cluster has them. */
  gpu?: { use: "optional" | "required"; count: number };
  /** Databases it needs, one per engine: picked or made when deploying, and connected. */
  databases: TemplateDatabase[];
  /** Services deployed with it, before it (Plausible's ClickHouse). */
  companions: TemplateCompanion[];
};

/**
 * A template's database: its engine, and `env`, the template's own
 * variable for each of the database's (`{ WORDPRESS_DB_USER: "MYSQL_USER" }`, or
 * several joined by ":": `{ WORDPRESS_DB_HOST: "MYSQL_HOST:MYSQL_PORT" }`),
 * as connectDatabase takes them.
 */
export type TemplateDatabase = {
  /** The catalog's "kv" is the API's "valkey". */
  engine: Database["engine"];
  env: Record<string, string>;
  /** Postgres: extensions turned on with a new database (`extensions` in the catalog). */
  extensions?: string[];
};

/**
 * A service a template deploys next to its own. The template's variables
 * reach it as `${{<name>.HOST}}`; it's deployed as `<service>-<suffix>`.
 */
export type TemplateCompanion = {
  name: string;
  image: string;
  version: { tag: string; digest: string };
  workloadKind: WorkloadKind;
  stateful: boolean;
  replicas: number;
  resources: Resources;
  args?: string[];
  entrypoint?: string[];
  storage?: { required: boolean; sizeGi: number; mountPath: string };
  ports: TemplatePort[];
  healthCheck: HealthCheck;
  env: { key: string; value: string }[];
};

// ── Storage ────────────────────────────────────────────────────────────────

export type VolumeStatus = "pending" | "bound" | "degraded" | "error";

export type Volume = {
  id: ID;
  teamId: ID;
  clusterId: ID;
  projectId?: ID;
  serviceId?: ID;
  name: string;
  storageClass: string;
  sizeGi: number;
  usedGi?: number;
  mountPath?: string;
  accessMode: "ReadWriteOnce" | "ReadWriteMany";
  status: VolumeStatus;
  statusMessage?: string;
  /** Worked out by the API from the team's backup settings and runs. */
  backup: {
    status: "enabled" | "disabled" | "failed" | "unsupported";
    /** Its last good backup. */
    lastAt?: Timestamp;
    /** A backup of it is running. */
    running?: boolean;
    /** When its last backup failed, and why (status "failed"). */
    failedAt?: Timestamp;
    error?: string | null;
    /** "daily", "weekly", or "manual" when the team's schedule is off. */
    schedule?: string;
  };
  /** The server it was made on; a one-server class keeps its data there. */
  server?: string;
  /** Deleted; listed until the cluster confirms. */
  deleting?: boolean;
  /** Why the cluster hasn't deleted it yet (it's asked again). */
  deleteError?: string;
  /** Being restored from a snapshot; its service is stopped meanwhile. */
  restore?: { id: string; snapshotId: string; startedAt: Timestamp };
  /** Why the last restore failed (its data was left as it was). */
  restoreError?: string;
  restoredAt?: Timestamp;
  /** Being moved to another project, to attach it to one of its services. */
  moving?: {
    projectId: ID;
    serviceId: ID;
    mountPath: string;
    startedAt: Timestamp;
  };
  moveError?: string;
  /** Growing to this size (GB) until the cluster reports it. */
  resizeTo?: number;
  resizeError?: string;
  createdAt: Timestamp;
};

/** One of a volume's backups, as restic lists it. */
export type VolumeSnapshot = {
  id: string;
  shortId?: string;
  time: Timestamp;
  sizeBytes?: number | null;
};

/** A volume on a cluster that Pyo has no record of. */
export type StrayVolume = {
  volumeId: string;
  namespace: string;
  name: string;
  sizeGi?: number;
  server?: string;
  deleting?: boolean;
  error?: string;
};

// ── Activity ───────────────────────────────────────────────────────────────

export type ActivityType =
  | "team.created"
  | "team.updated"
  | "member.invited"
  | "member.joined"
  | "project.created"
  | "project.updated"
  | "project.deleted"
  | "service.created"
  | "service.updated"
  | "service.deleted"
  | "service.delete_failed"
  | "service.restarted"
  | "service.stopped"
  | "service.scaled"
  | "deployment.started"
  | "deployment.succeeded"
  | "deployment.failed"
  | "deployment.cancelled"
  | "deployment.rollback"
  | "env.updated"
  | "domain.added"
  | "domain.removed"
  | "domain.verified"
  | "domain.moved"
  | "network.updated"
  | "cluster.registered"
  | "cluster.disconnected"
  | "cluster.degraded"
  | "cluster.unreachable"
  | "cluster.reachable"
  | "server.join"
  | "server.removed"
  | "server.offline"
  | "server.online"
  | "server.pressure"
  | "server.pressure_ended"
  | "backup.failed"
  | "domain.certificate_failed"
  | "volume.created"
  | "volume.attached"
  | "volume.detached"
  | "volume.deleted"
  | "volume.moving"
  | "volume.move_failed"
  | "volume.restoring"
  | "volume.restored"
  | "volume.restore_failed"
  | "volume.snapshot_deleted"
  | "volume.resized"
  | "volume.resize_failed"
  /** Pyo or an agent updated (Settings › Updates). */
  | "pyo.update"
  /** An admin changed an install-wide setting. */
  | "settings.changed";

export type ActivityTarget = {
  kind:
    | "team"
    | "project"
    | "service"
    | "deployment"
    | "cluster"
    | "server"
    | "volume"
    | "domain"
    | "update"
    | "settings";
  id: ID;
  label: string;
};

export type ActivityEvent = {
  id: ID;
  teamId: ID;
  type: ActivityType;
  at: Timestamp;
  actor: { name: string; kind: "user" | "system" };
  status: "success" | "failure" | "warning" | "info";
  title: string;
  target: ActivityTarget;
  projectId?: ID;
  serviceId?: ID;
  clusterId?: ID;
  details: Record<string, string>;
};

// ── Analytics ──────────────────────────────────────────────────────────────

export type TimeRange = "1h" | "24h" | "7d" | "30d";

export type AnalyticsPoint = {
  at: Timestamp;
  requests: number;
  pageviews: number;
  visitors: number;
  bandwidthMb: number;
  errors: number;
  p50: number;
  p95: number;
  p99: number;
};

export type AnalyticsTimeseries = AnalyticsPoint[];

export type AnalyticsSummary = {
  range: TimeRange;
  requests: number;
  /** Unique IP+UA hashes per day; an estimate, not authenticated users. */
  estimatedVisitors: number;
  bandwidthMb: number;
  errorRate: number;
  latency: { p50: number; p95: number; p99: number };
  statusCodes: { code: "2xx" | "3xx" | "4xx" | "5xx"; count: number }[];
  topPaths: { path: string; requests: number }[];
  /** By page views. */
  referrers: { source: string; views: number }[];
  /** By requests; "??" when unknown. */
  countries: { code: string; requests: number }[];
  /** Page views per full URL as requested (query string kept, sensitive values redacted). */
  pages: { url: string; path: string; views: number }[];
  pageviews: number;
  devices: { label: string; views: number }[];
  browsers: { label: string; views: number }[];
  queryParams: { key: string; requests: number; redacted: boolean }[];
};

// ── Builders ───────────────────────────────────────────────────────────────

export type BuilderStatus =
  | "provisioning"
  | "ready"
  | "offline"
  | "error"
  | "removing";

/** Turns Git sources into images: a pod on a cluster server, or on a build-only server. */
export type Builder = {
  id: ID;
  teamId: ID;
  name: string;
  /** cluster: a pod on a server of a cluster; server: a new server that joins a cluster as a build-only server. */
  kind: "cluster" | "server";
  clusterId?: ID | null;
  /** The server a cluster builder runs on. */
  server?: string | null;
  status: BuilderStatus;
  statusMessage?: string | null;
  info: {
    version?: string;
    cpus?: number;
    memoryGi?: number;
    /** Build job running right now. */
    busyWith?: string | null;
    /** A build-only server's hostname. */
    hostname?: string;
  };
  createdAt: Timestamp;
  lastSeenAt?: Timestamp | null;
};

/** Tailscale for one cluster: the client that acts for it (never the secret) and its tailnet. */
export type ClusterTailscale = {
  clientId: string;
  /** The secret's last characters. */
  hint: string;
  /** The tailnet ("tail1234.ts.net"), once the cluster has seen it. */
  suffix: string | null;
  /** On its team's tailnet: the cluster that keeps the team's client. */
  holderId?: ID | null;
  state?: ClusterTailnetState | null;
  problem?: TeamTailscale["problem"];
};

/** Tailscale on the cluster Pyo runs on (Integrations › Tailscale). The client only for admins. */
export type TailscaleInfo = {
  /** The cluster Pyo runs on. */
  clusterId?: string;
  /** The tailnet ("tail1234.ts.net"), once Pyo has seen it. */
  suffix: string | null;
  /** Pyo on the tailnet ("pyo.tail1234.ts.net"), once it answers. */
  dashboard: string | null;
  /** The registry on the tailnet, once it answers. */
  registry: string | null;
  /** Sandboxes' web pages on the tailnet, like the desktop, once it answers. */
  sandboxes?: string | null;
  /** Pyo's own server is on the tailnet. */
  onTailnet: boolean;
  /** Pyo's own server moving k3s onto the tailnet: while it happens, or why it failed. */
  hostMove: { state: "moving" | "failed"; error?: string | null } | null;
  clientId?: string;
  /** The secret's last characters. */
  hint?: string;
  dashboardName?: string | null;
  dashboardError?: string | null;
  registryName?: string | null;
  registryError?: string | null;
  sandboxesName?: string | null;
  sandboxesError?: string | null;
  hostName?: string | null;
};

/** A team's cloud provider (Integrations › Hetzner): the token's end only. */
export type CloudConnection = {
  teamId: string;
  provider: "hetzner";
  /** The token's last characters. */
  hint: string;
  createdAt: Timestamp;
};

export type CloudServerState =
  | "creating"
  | "booting"
  | "joining"
  | "ready"
  | "failed"
  | "removing";

/** A server Pyo made at a cloud provider. */
export type CloudServer = {
  id: string;
  teamId: string;
  provider: "hetzner";
  /** Null once its cluster was removed, and for a sandbox server. */
  clusterId: string | null;
  /** The sandbox server it becomes, instead of a cluster's. */
  sandboxServerId?: string | null;
  name: string;
  /** The cluster's first server (its control plane). */
  firstServer: boolean;
  builder: boolean;
  location: string;
  serverType: string;
  /** A month, before VAT. */
  monthly: number;
  currency: string;
  ipv4: string | null;
  state: CloudServerState;
  error: string | null;
  /** Tailnet Lock waits for its key: run this on a signing device. */
  signCommand: string | null;
  createdBy: string;
  createdAt: Timestamp;
  /** When Pyo stops waiting for it. */
  deadlineAt: Timestamp;
  readyAt: Timestamp | null;
};

export type CloudLocation = {
  name: string;
  city: string;
  country: string;
  networkZone: string;
};

export type CloudServerType = {
  name: string;
  description: string;
  cores: number;
  memoryGb: number;
  diskGb: number;
  cpuType: string;
  architecture: string;
  /** Pyo's servers run x86-64. */
  supported: boolean;
  /** A month per location, before VAT, with its IPv4 address. */
  prices: Record<string, number>;
  /** Locations that can make it now. */
  available: string[];
};

/** GET /cloud/options: read live from the provider. */
export type CloudOptions = {
  provider: "hetzner";
  currency: string;
  locations: CloudLocation[];
  serverTypes: CloudServerType[];
};

/** One check of the Tailscale client (Test). */
export type TailscaleCheck = { name: string; ok: boolean; message: string };

/** What setting up Tailscale with an access token would change (planTailscaleSetup). */
export type TailscaleSetupPlan = {
  changes: string[];
  done: string[];
  /** Policy lines added ("+ …") or changed ("- …"). */
  diff: string[];
  /** Why Pyo can't go on by itself. `diff` then holds the lines to add. */
  blocked: string | null;
  /** The policy as read. Handed back to setUpTailscale. */
  version: string;
};

/** What setting up Tailscale with an access token did (setUpTailscale). */
export type TailscaleSetupOutcome = {
  ok: boolean;
  changed: string[];
  todo: string[];
  error: string | null;
  checks: TailscaleCheck[];
  tokenDeleted: boolean;
  /** The tailnet changed since the review. The token is kept for a new one. */
  reviewAgain: boolean;
};

/** Pyo on the public IP (admins only). */
export type PublicDashboard = {
  open: boolean;
  /** "pyo.203.0.113.7.nip.io", once it answers over HTTPS. */
  host: string | null;
  /** It can be open: Pyo's server has a public IP of its own (not behind a router's NAT). */
  possible: boolean;
  /** Let's Encrypt couldn't reach that IP: ports 80 and 443 are closed in a cloud firewall. */
  blocked?: boolean;
};

// ── Integrations ───────────────────────────────────────────────────────────

/** A team's login for a private container registry (the password stays on the server). */
/** What web analytics keeps of a visitor's IP address (installation-wide). */
export type IpMode = "none" | "anonymized" | "full";

/** How long this installation keeps observability data. */
export type Retention = {
  /** 3, 6, 12 or 24. */
  analyticsMonths: number;
  /** 7 to 90. */
  metricsDays: number;
  /** 30 to 365. */
  buildLogDays: number;
};

export type RegistryLogin = {
  id: ID;
  teamId: ID;
  host: string;
  username: string;
  verified: boolean;
  createdAt: Timestamp;
  lastUsedAt?: Timestamp | null;
};

export type Registry = {
  id: ID;
  teamId: ID | null;
  name: string;
  host: string;
  /** Placeholder for a future encrypted credential reference. */
  credential: "none" | "configured";
};
