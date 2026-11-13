import { needsOwnDomain, refProblem, SELF, type Provider } from "./refs";
import type { BuildMethod } from "./detect";
import { RESERVED_SERVICE_NAMES } from "./paths";
import { DNS_LABEL, ENV_KEY, HOSTNAME, slugify } from "./format";
import { MAX_REPLICAS } from "./capacity";
import { companionRecord, companionService, getTemplate } from "./templates";
import type {
  Cluster,
  DeploymentStage,
  Exposure,
  HealthCheck,
  PortProtocol,
  Resources,
  Template,
  TemplateCompanion,
  TemplateCompanionRecord,
  TlsMode,
} from "./types";

// One configuration model for every deploy path (template, Git, image). The
// wizard edits a DeployDraft; the review renders it; the store turns it into
// an Application plus its first Deployment.

export type DraftEnv = {
  id: string;
  key: string;
  value: string;
  secret: boolean;
  /** A stored secret: blank keeps it, typing replaces it. */
  hasValue?: boolean;
};
export type DraftPort = {
  id: string;
  name: string;
  containerPort: number;
  protocol: PortProtocol;
  exposure: Exposure;
  publicPort?: number;
  proxy?: boolean;
  ipFilter?: { mode: "allow" | "deny"; cidrs: string[] } | null;
  connectionLimit?: number | null;
  sendProxyProtocol?: "V1" | "V2" | null;
  acceptProxyProtocol?: boolean;
};
export type DraftDomain = {
  id: string;
  hostname: string;
  portName: string;
  redirectHttps: boolean;
  tlsMode?: TlsMode;
};
export type DraftVolume = {
  id: string;
  name: string;
  mountPath: string;
  sizeGi: number;
  storageClass: string;
};

export type SourceType = "template" | "git" | "image";

export type DeployDraft = {
  sourceType: SourceType;
  templateId?: string;
  templateValues: Record<string, string | number | boolean>;
  git: {
    repoUrl: string;
    branch: string;
    commit: string;
    /** "auto" uses the Dockerfile if the repo has one, otherwise buildpacks. */
    builder: "auto" | BuildMethod;
    dockerfilePath: string;
    buildContext: string;
    buildArgs: { id: string; key: string; value: string }[];
    private: boolean;
    /** Picked from the team's GitHub App installations. */
    installationId?: number;
  };
  image: {
    image: string;
    reference: string;
    registryId: string;
    private: boolean;
  };
  name: string;
  description: string;
  teamId: string;
  projectId: string;
  clusterId: string;
  /** Pin to servers in this place; "" for anywhere in the cluster. */
  location: string;
  /** Where it runs: a place (or anywhere) in a cluster, servers with
   * every one of some tags, or servers picked by name. */
  placement: "location" | "tags" | "servers";
  /** Only on servers with all of these tags. */
  serverTags: string[];
  /** Only on these servers. */
  serverNames: string[];
  env: DraftEnv[];
  resources: Resources;
  replicas: number;
  /** Runs on a schedule instead of staying up. */
  cron: boolean;
  schedule: string;
  command: string;
  healthCheck: HealthCheck;
  volumes: DraftVolume[];
  ports: DraftPort[];
  domains: DraftDomain[];
  /** A template companion's own entrypoint and arguments, and workload. */
  container?: { entrypoint?: string[]; command?: string[] };
  workloadKind?: "Deployment" | "StatefulSet";
  /** A template companion: what it records in its source. */
  companionOf?: TemplateCompanionRecord;
  /** Simulation only: make the first rollout fail at this stage. */
};

/** `projectId` of a draft that creates its own project, named after the app. */
export const NEW_PROJECT = "new";

let seq = 0;
export const draftId = () =>
  `d${++seq}${Math.random().toString(36).slice(2, 6)}`;

export function emptyDraft(
  teamId: string,
  projectId = "",
  clusterId = "",
): DeployDraft {
  return {
    sourceType: "git",
    templateValues: {},
    git: {
      repoUrl: "",
      branch: "main",
      commit: "",
      builder: "auto",
      dockerfilePath: "Dockerfile",
      buildContext: ".",
      buildArgs: [],
      private: false,
    },
    image: {
      image: "",
      reference: "latest",
      registryId: "reg_dockerhub",
      private: false,
    },
    name: "",
    description: "",
    teamId,
    projectId,
    clusterId,
    location: "",
    placement: "location",
    serverTags: [],
    serverNames: [],
    env: [],
    resources: {
      cpuRequest: 100,
      cpuLimit: 500,
      memoryRequest: 128,
      memoryLimit: 256,
    },
    replicas: 1,
    cron: false,
    schedule: "0 3 * * *",
    command: "",
    healthCheck: {
      type: "http",
      path: "/healthz",
      port: 8080,
      command: "",
      initialDelaySeconds: 10,
      periodSeconds: 10,
    },
    volumes: [],
    ports: [
      {
        id: draftId(),
        name: "http",
        containerPort: 8080,
        protocol: "HTTP",
        exposure: "internal",
      },
    ],
    domains: [],
  };
}

export function draftFromTemplate(t: Template, base: DeployDraft): DeployDraft {
  const values: DeployDraft["templateValues"] = {};
  for (const f of t.fields) values[f.key] = f.default;
  return {
    ...base,
    sourceType: "template",
    templateId: t.id,
    templateValues: values,
    name: base.name || t.id,
    description: t.description,
    env: t.env.map((e) => ({
      id: draftId(),
      key: e.key,
      value: e.value,
      secret: e.secret,
    })),
    resources: { ...t.resources },
    replicas: t.replicas,
    healthCheck: { ...t.healthCheck },
    volumes: t.storage
      ? [
          {
            id: draftId(),
            name: `${base.name || t.id}-data`,
            mountPath: t.storage.mountPath,
            sizeGi: t.storage.sizeGi,
            storageClass: "",
          },
        ]
      : [],
    ports: t.ports.map((p) => ({
      id: draftId(),
      name: p.name,
      containerPort: p.port,
      protocol: p.protocol,
      exposure: p.exposure === "http" ? "internal" : p.exposure,
    })),
    domains: [],
  };
}

/**
 * A service a template deploys with it (Plausible's ClickHouse), as its own
 * image deploy next to the service `main` describes.
 */
export function companionDraft(
  t: Template,
  c: TemplateCompanion,
  main: DeployDraft,
  registryId: string,
): DeployDraft {
  const name = companionService(t, c, main.name);
  return {
    ...main,
    sourceType: "image",
    templateId: undefined,
    templateValues: {},
    image: { image: c.image, reference: c.version.tag, registryId, private: false },
    name,
    description: `${c.name} for ${main.name}`,
    env: c.env.map((e) => ({ id: draftId(), key: e.key, value: e.value, secret: false })),
    resources: { ...c.resources },
    replicas: c.replicas,
    cron: false,
    healthCheck: { ...c.healthCheck },
    volumes: c.storage
      ? [{ id: draftId(), name: `${name}-data`, mountPath: c.storage.mountPath, sizeGi: c.storage.sizeGi, storageClass: "" }]
      : [],
    ports: c.ports.map((p) => ({
      id: draftId(),
      name: p.name,
      containerPort: p.port,
      protocol: p.protocol,
      exposure: "internal",
    })),
    domains: [],
    container: {
      ...(c.entrypoint ? { entrypoint: [...c.entrypoint] } : {}),
      ...(c.args ? { command: [...c.args] } : {}),
    },
    workloadKind: c.workloadKind === "StatefulSet" ? "StatefulSet" : "Deployment",
    companionOf: companionRecord(t, c),
  };
}

/** Applies template field values onto env vars (fields with envKey). */
export function syncTemplateEnv(d: DeployDraft): DeployDraft {
  const t = getTemplate(d.templateId);
  if (!t) return d;
  const env = d.env.map((e) => {
    const f = t.fields.find((x) => x.envKey === e.key);
    return f ? { ...e, value: String(d.templateValues[f.key] ?? "") } : e;
  });
  return { ...d, env };
}

/** The most GPUs one service may ask for (the API allows the same). */
export const MAX_GPUS = 8;

/** Why a service can't have `gpus` GPUs on `cluster`, if it can't. */
export function gpuError(gpus: number, cluster: Cluster | undefined) {
  if (!gpus) return undefined;
  const g = cluster?.capabilities.gpu;
  if (!cluster) return undefined;
  if (!g?.servers) return `${cluster.name} has no server with an NVIDIA GPU.`;
  if (!g.maxPerServer) return `${cluster.name}'s GPU servers aren't ready yet.`;
  if (gpus > g.maxPerServer)
    return `The most on one server in ${cluster.name} is ${g.maxPerServer}.`;
  return undefined;
}

export type Issues = Record<string, string>;

export type Section =
  | "source"
  | "basics"
  | "target"
  | "env"
  | "resources"
  | "health"
  | "schedule"
  | "volumes"
  | "domains"
  | "networking";
/** `takenHosts`: hostnames other services already serve, with the service's name. */
export type ValidationContext = {
  clusters: Cluster[];
  takenNames: string[];
  takenHosts?: Record<string, string>;
  /** The project's services (and the other drafts), for `${{name.VAR}}` references. */
  providers?: Provider[];
};

/** Issues for one section of the deploy form. */
export function validateSection(
  section: Section,
  d: DeployDraft,
  ctx: ValidationContext,
): Issues {
  return check(section, d, ctx);
}

export function validateAll(d: DeployDraft, ctx: ValidationContext): Issues {
  return (
    [
      "source",
      "basics",
      "target",
      "schedule",
      "env",
      "resources",
      "health",
      "volumes",
      "domains",
      "networking",
    ] as Section[]
  ).reduce<Issues>((acc, sec) => ({ ...acc, ...check(sec, d, ctx) }), {});
}

/**
 * Accepts "org/repo" (GitHub), "github.com/org/repo" or a full HTTPS/SSH URL
 * and returns a normalized repository URL.
 */
export function parseGitInput(raw: string) {
  const v = raw.trim().replace(/\/+$/, "");
  if (!v) return "";
  if (/^(https:\/\/|git@)/.test(v)) return v;
  if (/^[\w.-]+\.[a-z]{2,}\/[\w.-]+\/[\w.-]+/.test(v)) return `https://${v}`;
  if (/^[\w.-]+\/[\w.-]+$/.test(v)) return `https://github.com/${v}`;
  return v;
}

/** Ports of well-known images; anything else gets 8080, the most common app port. */
const KNOWN_PORTS: [RegExp, number, "HTTP" | "TCP"][] = [
  [/(^|\/)(nginx|httpd|caddy|whoami|nginx-unprivileged)$/, 80, "HTTP"],
  [/(^|\/)(grafana|metabase|uptime-kuma)$/, 3000, "HTTP"],
  [/(^|\/)prometheus$/, 9090, "HTTP"],
  [/(^|\/)(redis|valkey|keydb)$/, 6379, "TCP"],
  [/(^|\/)postgres(ql)?$/, 5432, "TCP"],
  [/(^|\/)(mysql|mariadb)$/, 3306, "TCP"],
  [/(^|\/)mongo(db)?$/, 27017, "TCP"],
];

/**
 * Port and health check for an imported image. The check only asks that the
 * port accepts connections: an HTTP path would be a guess.
 */
export function imageDefaults(
  image: string,
): Pick<DeployDraft, "ports" | "healthCheck"> {
  const repo = image.replace(/^docker\.io\/(library\/)?/, "");
  const [, port, protocol] = KNOWN_PORTS.find(([re]) => re.test(repo)) ?? [
    null,
    8080,
    "HTTP" as const,
  ];
  return {
    ports: [
      {
        id: draftId(),
        name: protocol === "HTTP" ? "http" : "tcp",
        containerPort: port,
        protocol,
        exposure: "internal",
      },
    ],
    healthCheck: {
      type: "tcp",
      path: "",
      port,
      command: "",
      initialDelaySeconds: 5,
      periodSeconds: 10,
    },
  };
}

/** Splits "ghcr.io/org/app:1.2", "nginx" or "app@sha256:..." into repository, reference and registry. */
export function parseImageInput(
  raw: string,
  registries: { id: string; host: string }[],
) {
  const v = raw.trim();
  let image = v;
  let reference = "latest";
  const at = v.indexOf("@");
  if (at > 0) {
    image = v.slice(0, at);
    reference = v.slice(at + 1);
  } else {
    const m = v.match(/^(.*):([\w][\w.-]{0,127})$/);
    if (m && !m[2].includes("/")) {
      image = m[1];
      reference = m[2];
    }
  }
  const host =
    image.includes("/") && /[.:]/.test(image.split("/")[0])
      ? image.split("/")[0]
      : "docker.io";
  if (host === "docker.io" && !image.startsWith("docker.io/"))
    image = `docker.io/${image.includes("/") ? image : `library/${image}`}`;
  const registryId =
    registries.find((r) => r.host === host)?.id ?? "reg_dockerhub";
  return { image, reference, registryId };
}

function check(step: Section, d: DeployDraft, ctx: ValidationContext): Issues {
  const e: Issues = {};
  const t = getTemplate(d.templateId);
  if (step === "source") {
    if (d.sourceType === "template" && !t) e.template = "Choose a template.";
    if (d.sourceType === "git") {
      if (!d.git.repoUrl.trim()) e.repoUrl = "Repository URL is required.";
      // Builders fetch over HTTP(S); http:// is for Git servers on a private network.
      else if (
        !/^https?:\/\/[\w.-]+(:\d+)?\/[\w./-]+$/.test(d.git.repoUrl.trim())
      )
        e.repoUrl =
          "Use the repository's HTTPS URL, e.g. https://github.com/org/repo.";
      if (!d.git.branch.trim()) e.branch = "Branch is required.";
      if (d.git.commit && !/^[0-9a-f]{7,40}$/.test(d.git.commit))
        e.commit =
          "Use a 7–40 character commit SHA, or leave empty for the branch head.";
      if (
        /(^|\/)\.\.(\/|$)/.test(d.git.dockerfilePath) ||
        d.git.dockerfilePath.startsWith("/")
      )
        e.dockerfilePath = "Use a path inside the repository.";
      d.git.buildArgs.forEach((a) => {
        if (!ENV_KEY.test(a.key))
          e[`buildArg.${a.id}`] = "Use letters, digits and underscores.";
      });
    }
    if (d.sourceType === "image") {
      if (!d.image.image.trim()) e.image = "Image repository is required.";
      else if (/[A-Z\s]/.test(d.image.image) || d.image.image.includes(":"))
        e.image =
          "Use a lowercase repository without a tag, e.g. ghcr.io/org/app.";
      if (!d.image.reference.trim()) e.reference = "Tag or digest is required.";
      else if (
        d.image.reference.startsWith("sha256:") &&
        !/^sha256:[0-9a-f]{64}$/.test(d.image.reference)
      )
        e.reference = "Digest must be sha256: followed by 64 hex characters.";
    }
  }
  if (step === "basics") {
    if (!d.name) e.name = "Name is required.";
    else if (!DNS_LABEL.test(d.name) || d.name.length > 40)
      e.name =
        "Lowercase letters, digits and dashes, up to 40. Start and end with a letter or digit.";
    else if (ctx.takenNames.includes(d.name))
      e.name = "This project already has a service with that name.";
    else if (RESERVED_SERVICE_NAMES.includes(d.name))
      e.name = "A project page uses that name. Pick another.";
    if (t) {
      for (const f of t.fields) {
        const v = d.templateValues[f.key];
        if (f.required && (v === "" || v === undefined))
          e[`field.${f.key}`] = `${f.label} is required.`;
        if (f.type === "password" && typeof v === "string" && v && v.length < 8)
          e[`field.${f.key}`] =
            "Use at least 8 characters, or leave empty to generate.";
        if (f.type === "number" && (Number.isNaN(Number(v)) || Number(v) <= 0))
          e[`field.${f.key}`] = "Enter a positive number.";
      }
    }
  }
  if (step === "target") {
    if (!d.projectId) e.projectId = "Choose a project.";
    const c = ctx.clusters.find((x) => x.id === d.clusterId);
    if (!c) e.clusterId = "Choose a target cluster.";
    else if (c.status === "unreachable" || c.status === "pending")
      e.clusterId = `${c.name} is ${c.status}. Choose a connected cluster.`;
    if (d.placement === "tags" && !d.serverTags.length)
      e.placement = "Add a tag, or choose another way to place it.";
    if (d.placement === "servers" && !d.serverNames.length)
      e.placement = "Pick at least one server.";
  }
  if (step === "env") {
    const seen = new Set<string>();
    for (const v of d.env) {
      if (!v.key) e[`env.${v.id}`] = "Key is required.";
      else if (!ENV_KEY.test(v.key))
        e[`env.${v.id}`] =
          "Use letters, digits and underscores. Don't start with a digit.";
      else if (seen.has(v.key)) e[`env.${v.id}`] = "Duplicate key.";
      else if (ctx.providers) {
        // ${{self.VAR}}: the service itself. A missing domain shows under Domains.
        const self: Provider = {
          name: SELF,
          envKeys: d.env.map((x) => x.key).filter(Boolean),
          port: d.ports[0]?.containerPort,
          domain: d.domains[0]?.hostname || "domain.invalid",
        };
        // ${{<companion>.VAR}}: a service the template deploys with it.
        const companions: Provider[] = (t?.companions ?? []).map((c) => ({
          name: c.name,
          envKeys: c.env.map((x) => x.key),
          port: c.ports[0]?.port,
        }));
        const why = refProblem(v.value, [...ctx.providers, self, ...companions]);
        if (why) e[`env.${v.id}`] = why;
      }
      seen.add(v.key);
    }
  }
  if (step === "schedule") {
    if (d.cron && d.schedule.trim().split(/\s+/).length !== 5)
      e.schedule = "Use five fields, like 0 3 * * *.";
  }
  if (step === "resources") {
    const r = d.resources;
    if (r.cpuRequest <= 0) e.cpuRequest = "Must be greater than 0.";
    if (r.cpuLimit < r.cpuRequest)
      e.cpuLimit = "Limit must be at least the request.";
    if (r.memoryRequest <= 0) e.memoryRequest = "Must be greater than 0.";
    if (r.memoryLimit < r.memoryRequest)
      e.memoryLimit = "Limit must be at least the request.";
    if (d.replicas < 0 || d.replicas > MAX_REPLICAS || !Number.isInteger(d.replicas))
      e.replicas = "0 or more.";
    if (t?.stateful && d.replicas > 1)
      e.replicas = `${t.name} runs as one replica. This template doesn't set up replication.`;
    const gpu = gpuError(
      r.gpu ?? 0,
      ctx.clusters.find((c) => c.id === d.clusterId),
    );
    if (gpu) e.gpu = gpu;
  }
  if (step === "health") {
    if (d.healthCheck.type === "http" && !d.healthCheck.path.startsWith("/"))
      e.healthPath = "Path must start with /.";
    if (
      (d.healthCheck.type === "http" || d.healthCheck.type === "tcp") &&
      !(d.healthCheck.port > 0 && d.healthCheck.port < 65536)
    )
      e.healthPort = "Enter a valid port.";
    if (d.healthCheck.type === "exec" && !d.healthCheck.command.trim())
      e.healthCommand = "Command is required.";
  }
  if (step === "volumes") {
    const mounts = new Set<string>();
    for (const v of d.volumes) {
      if (!DNS_LABEL.test(v.name))
        e[`vol.${v.id}`] = "Name: lowercase letters, digits and dashes.";
      else if (!v.mountPath.startsWith("/"))
        e[`vol.${v.id}`] = "The folder is an absolute path, e.g. /data.";
      else if (mounts.has(v.mountPath))
        e[`vol.${v.id}`] = "Two volumes use the same folder.";
      else if (!(v.sizeGi >= 1 && v.sizeGi <= 2000))
        e[`vol.${v.id}`] = "Size: 1 to 2000 GB.";
      mounts.add(v.mountPath);
    }
    if (t?.storage?.required && d.volumes.length === 0)
      e.volumes = `${t.name} needs a volume. Without one, its data is lost on every restart.`;
  }
  if (step === "networking") {
    const c = ctx.clusters.find((x) => x.id === d.clusterId);
    const keys = new Set<string>();
    const publicPorts = new Set<string>();
    for (const p of d.ports) {
      const k = `${p.containerPort}/${p.protocol === "UDP" ? "UDP" : "TCP"}`;
      if (!DNS_LABEL.test(p.name))
        e[`port.${p.id}`] = "Port name must be a lowercase DNS label.";
      else if (!(p.containerPort > 0 && p.containerPort < 65536))
        e[`port.${p.id}`] = "Port must be 1–65535.";
      else if (keys.has(k)) e[`port.${p.id}`] = `Port ${k} is defined twice.`;
      else if (p.exposure === "http" && p.protocol !== "HTTP")
        e[`port.${p.id}`] =
          "Websites only carry HTTP. For TCP and UDP, pick Public IP or Port on every server.";
      else if (
        p.exposure === "nodeport" &&
        p.publicPort !== undefined &&
        !(p.publicPort >= 30000 && p.publicPort <= 32767)
      )
        e[`port.${p.id}`] = "Ports on every server must be 30000–32767.";
      else if (
        p.exposure === "loadbalancer" &&
        c &&
        c.capabilities.loadBalancer === "none"
      )
        e[`port.${p.id}`] =
          `${c.name} can't give services a public address. Use “Port on every server” instead.`;
      else if (publicPortError(p)) e[`port.${p.id}`] = publicPortError(p)!;
      else if (p.publicPort && publicPorts.has(`${p.publicPort}/${p.protocol}`))
        e[`port.${p.id}`] = `Public port ${p.publicPort} is used twice.`;
      keys.add(k);
      if (p.publicPort) publicPorts.add(`${p.publicPort}/${p.protocol}`);
    }
  }
  if (step === "domains") {
    const c = ctx.clusters.find((x) => x.id === d.clusterId);
    const hosts = new Set<string>();
    for (const dm of d.domains) {
      if (!HOSTNAME.test(dm.hostname))
        e[`domain.${dm.id}`] = "Enter a valid hostname, e.g. app.example.com.";
      else if (hosts.has(dm.hostname))
        e[`domain.${dm.id}`] = "Duplicate hostname.";
      else if (ctx.takenHosts?.[dm.hostname])
        e[`domain.${dm.id}`] =
          `${dm.hostname} is already used by ${ctx.takenHosts[dm.hostname]}.`;
      else if (
        !d.ports.some((p) => p.name === dm.portName && p.protocol === "HTTP")
      )
        e[`domain.${dm.id}`] = "Route a domain to an HTTP port.";
      hosts.add(dm.hostname);
    }
    if (d.domains.length && c?.capabilities.ingressController === "none")
      e.domains = `${c.name} has no gateway.`;
    else if (!d.domains.length && needsOwnDomain(d.env.map((v) => v.value)))
      e.domains = "Add a domain: it's this service's address.";
  }
  return e;
}

export function suggestName(d: DeployDraft) {
  if (d.sourceType === "git")
    return slugify(
      d.git.repoUrl
        .split("/")
        .pop()
        ?.replace(/\.git$/, "") ?? "",
    );
  if (d.sourceType === "image")
    return slugify(d.image.image.split("/").pop() ?? "");
  return d.templateId ?? "";
}

/** An IP address or range, as the API takes it. */
const CIDR =
  /^(?:(?:\d{1,3}\.){3}\d{1,3}(?:\/\d{1,2})?|[0-9a-fA-F:]*:[0-9a-fA-F:.]*(?:\/\d{1,3})?)$/;

/** Ports the servers need themselves, and the gateway's own (as the API refuses them). */
const SERVER_PORTS: Record<"TCP" | "UDP", number[]> = {
  TCP: [22, 53, 2379, 2380, 6443, 10250],
  UDP: [53, 8472, 51820, 51821],
};

/** What's wrong with a public TCP or UDP port's number and proxy options. */
export function publicPortError(p: DraftPort): string | undefined {
  if (p.exposure !== "loadbalancer") return undefined;
  const proto = p.protocol === "UDP" ? "UDP" : "TCP";
  const n = p.publicPort ?? p.containerPort;
  if (proto === "TCP" && (n === 80 || n === 443))
    return "Ports 80 and 443 are for websites: add a domain instead.";
  if (proto === "TCP" && (n === 10080 || n === 10443 || (n >= 19000 && n <= 19003)))
    return "The gateway uses this port itself.";
  if (SERVER_PORTS[proto].includes(n))
    return "The servers need this port themselves, so it can't be public.";
  if (p.proxy === false || proto === "UDP") return undefined;
  if (p.ipFilter) {
    if (!p.ipFilter.cidrs.length)
      return "Add at least one address to the IP rule, or turn it off.";
    const bad = p.ipFilter.cidrs.find((c) => !CIDR.test(c));
    if (bad) return `${bad} isn't an IP address or range.`;
    if (p.ipFilter.cidrs.length > 100)
      return "An IP rule holds at most 100 addresses.";
  }
  if (
    p.connectionLimit !== undefined &&
    p.connectionLimit !== null &&
    !(p.connectionLimit >= 1 && p.connectionLimit <= 1_000_000)
  )
    return "A connection limit is between 1 and 1,000,000.";
  return undefined;
}
