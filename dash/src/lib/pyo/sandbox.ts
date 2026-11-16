// Pyo Sandbox in the dashboard: the control plane's API (crates/pyo-api/src/sandbox)
// and the sandbox server's daemon (crates/pyo-sandbox), which the browser
// calls directly with the client token Pyo signs. The daemon answers only on
// the tailnet, so its calls need the browser to be on the tailnet too. Open
// ports are served on the server's edge proxy instead.

import { request } from "./backend";
import { BASE_TEMPLATE } from "./sandboxTemplates";
import type { CloudServerState } from "./types";

export {
  BASE_TEMPLATE,
  defaultTemplate,
  fieldsSwapped,
  freeLabel,
  freeMemory,
  gettingReady,
  imageRepo,
  isBase,
  serverRefusal,
  templateImage,
  templateStatus,
  usable,
} from "./sandboxTemplates";

// ── Types ──────────────────────────────────────────────────────────────────

export type SandboxState =
  | "creating"
  | "running"
  | "pausing"
  | "paused"
  | "resuming"
  | "stopping"
  | "stopped"
  | "failed";

export type Sandbox = {
  id: string;
  teamId: string;
  projectId: string | null;
  serverId: string | null;
  server: string | null;
  template: string;
  templateId: string | null;
  state: SandboxState;
  /** While it waits for its server to make its template (base, the first time there). */
  preparing?: boolean;
  runtime: "firecracker" | "gvisor" | null;
  cpus: number;
  memoryMib: number;
  diskMib: number;
  network: "internet" | "none" | null;
  isolation: "auto" | "vm" | null;
  labels: Record<string, string>;
  pauseWhenIdleSecs: number | null;
  /** Names only: Pyo never keeps the values. */
  env: string[];
  /** Its server's daemon, `http://<tailnet ip>:7070`. */
  url: string | null;
  /** Its server's edge proxy, where open ports are served. */
  edgeUrl: string | null;
  ports: SandboxPort[];
  /** getSandbox only: a fresh connection when you may use the sandbox. */
  connection?: Connection | null;
  error: string | null;
  createdBy: string | null;
  createdAt: number;
  startedAt: number | null;
  /** Null: it runs until it's deleted. */
  expiresAt: number | null;
  pausedAt: number | null;
  endedAt: number | null;
};

export type SandboxPort = { port: number; public: boolean; url: string | null };

export type SandboxLimits = {
  maxRunning: number;
  maxCpus: number;
  maxMemoryMib: number;
  maxDiskMib: number;
  /** Null: no limit, sandboxes may run until they're deleted. */
  maxTtlSecs: number | null;
};

export type SandboxInUse = {
  sandboxes: number;
  cpus: number;
  memoryMib: number;
  diskMib: number;
};

export type Connection = {
  url: string;
  sandboxId: string;
  token: string;
  /** Milliseconds since 1970. */
  expiresAt: number;
};

export type SandboxCapability = {
  runtime: "firecracker" | "gvisor" | "none";
  kvm: "native" | "nested" | "none";
  bootMs?: number;
  arch: string;
  cpus: number;
  /** Bytes. */
  memory: number;
  disk: number;
  network?: boolean;
  edgeUrl?: string;
  /** The fingerprint of the certificate the edge serves. */
  edgeCert?: string;
  publicIps?: string[];
  reason?: string;
};

/** A server's edge proxy: its domain, certificate and wildcard record. */
export type SandboxEdge = {
  domain: string;
  /** Whether it serves HTTPS now. */
  https: boolean;
  cert: {
    state: "none" | "issuing" | "ready" | "failed";
    notAfter?: number | null;
    issuedAt?: number | null;
    error?: string | null;
    /** Whether the server serves it. */
    served?: boolean;
  };
  /** The connected Cloudflare zone that covers the domain. */
  cloudflare: { teamId: string; zone: string; canEdit: boolean } | null;
  dns: {
    name: string;
    /** What to point it at: the server's public addresses. */
    records: { type: "A" | "AAAA"; content: string }[];
    /** What Pyo made at Cloudflare. */
    made: {
      status: "created" | "exists" | "conflict";
      records: { type: string; content: string }[];
      existing: { type: string; content: string }[];
    } | null;
  };
};

export type SandboxCapacity = {
  memoryTotal: number;
  memoryAvailable: number;
  memoryReserved: number;
  memoryUsed: number;
  memoryCommitted: number;
  running: number;
  paused: number;
  cpusCommitted: number;
  /** Bytes free in the storage pool, and what templates and sandboxes take. */
  diskFree?: number;
  diskTemplates?: number;
  diskSandboxes?: number;
};

export type SandboxServer = {
  id: string;
  /** Null for the installation's, which every team may use. */
  teamId: string | null;
  name: string;
  address: string | null;
  state: "joining" | "ready" | "unreachable" | "refused";
  enabled: boolean;
  capability: SandboxCapability | null;
  capacity: SandboxCapacity | null;
  /** The one line shown next to the runtime badge. */
  summary: string | null;
  daemonVersion: string | null;
  lastReportAt: number | null;
  error: string | null;
  sandboxes: number;
  createdAt: number;
  edgeDomain: string | null;
  edge?: SandboxEdge;
  /** What runs its sandboxes, once it reports. */
  runtime?: "firecracker" | "gvisor" | null;
  /** The server Pyo made for it at a cloud provider, until it's deleted there. */
  cloud?: SandboxServerCloud | null;
};

export type SandboxServerCloud = {
  /** The cloud server's id, `cs_…`. */
  id: string;
  provider: "hetzner";
  location: string;
  serverType: string;
  /** A month, before VAT. */
  monthly: number;
  currency: string;
  ipv4: string | null;
  state: CloudServerState;
  error: string | null;
  signCommand: string | null;
  createdAt: number;
  deadlineAt: number;
};

/** Where Pyo can buy a sandbox server. More plug in here. */
export const CLOUD_PROVIDERS = [
  { value: "hetzner", label: "Hetzner", detail: "Billed to your Hetzner project." },
] as const;

export type CloudProvider = (typeof CLOUD_PROVIDERS)[number]["value"];

export type AddCloudServer = {
  teamId: string;
  name: string;
  provider: CloudProvider;
  location: string;
  serverType: string;
  edgeDomain?: string;
  sshKey?: string;
  confirm: boolean;
};

export type SandboxTemplate = {
  id: string;
  teamId: string | null;
  name: string;
  /** `os` and `release` on Pyo's built-in template. */
  source: { kind: string; image?: string; os?: string; release?: string } | null;
  /** Pyo's own (base): kept in step with Pyo's release, never deleted. */
  builtin?: boolean;
  /** "unavailable": base, while Pyo has no image for it (`error` says why). */
  state: "pending" | "building" | "ready" | "failed" | "unavailable";
  /** Each sandbox server that may use it, and how far it got there. For
   * base, the servers that have it: they make it on first use. */
  servers: SandboxTemplateServer[];
  error: string | null;
  createdBy: string | null;
  /** Null for base before Pyo adds it. */
  createdAt: number | null;
};

export type SandboxTemplateServer = {
  id: string;
  name: string;
  state: "pending" | "building" | "ready" | "failed";
  phase: "queued" | "pulling" | "unpacking" | "snapshotting" | null;
  pulled: number | null;
  imageBytes: number | null;
  bytes: number | null;
  error: string | null;
};

export type CreateSandbox = {
  teamId: string;
  template: string;
  /** A sandbox server's id: Pyo picks one when left out. */
  server?: string;
  cpus?: number;
  memoryMib?: number;
  diskMib?: number;
  /** Like "30m", "1h" or "7d", or "none" to run until it's deleted. */
  ttl?: string;
  pauseWhenIdleSecs?: number;
  env?: Record<string, string>;
  network?: "internet" | "none";
  isolation?: "auto" | "vm";
  labels?: Record<string, string>;
  projectId?: string;
};

// ── Pyo's API ──────────────────────────────────────────────────────────────

const enc = encodeURIComponent;

export const sandboxApi = {
  list: (team: string, state: "live" | "all" = "live") =>
    request<{ sandboxes: Sandbox[]; limits: SandboxLimits; inUse: SandboxInUse }>(
      `/api/v1/sandboxes?${new URLSearchParams({ team, state })}`,
    ),
  get: (id: string) => request<Sandbox>(`/api/v1/sandboxes/${enc(id)}`),
  create: (body: CreateSandbox) =>
    request<{ sandbox: Sandbox; connection: Connection }>("/api/v1/sandboxes", {
      method: "POST",
      body,
    }),
  remove: (id: string) =>
    request<Sandbox>(`/api/v1/sandboxes/${enc(id)}`, { method: "DELETE" }),
  pause: (id: string) =>
    request<Sandbox>(`/api/v1/sandboxes/${enc(id)}/pause`, { method: "POST" }),
  resume: (id: string) =>
    request<Sandbox>(`/api/v1/sandboxes/${enc(id)}/resume`, { method: "POST" }),
  extend: (id: string, ttl: string) =>
    request<Sandbox>(`/api/v1/sandboxes/${enc(id)}/extend`, {
      method: "POST",
      body: { ttl },
    }),
  openPort: (id: string, port: number, isPublic: boolean) =>
    request<{ url: string; port: number; public: boolean }>(
      `/api/v1/sandboxes/${enc(id)}/ports`,
      { method: "POST", body: { port, public: isPublic } },
    ),
  closePort: (id: string, port: number) =>
    request<{ port: number; closed: boolean }>(
      `/api/v1/sandboxes/${enc(id)}/ports/${port}`,
      { method: "DELETE" },
    ),
  token: (id: string) =>
    request<Connection>(`/api/v1/sandboxes/${enc(id)}/token`, { method: "POST" }),
  servers: (team: string) =>
    request<{ servers: SandboxServer[] }>(
      `/api/v1/sandbox-servers?${new URLSearchParams({ team })}`,
    ),
  addServer: (body: { teamId?: string; name: string; edgeDomain?: string }) =>
    request<{ server: SandboxServer | null; command: string; expiresAt: number }>(
      "/api/v1/sandbox-servers",
      { method: "POST", body },
    ),
  addCloudServer: (body: AddCloudServer) =>
    request<{ server: SandboxServer | null; cloudServer: { id: string; name: string } }>(
      "/api/v1/sandbox-servers/cloud",
      { method: "POST", body },
    ),
  /** `confirm`: also delete it at the cloud provider Pyo made it at. */
  removeServer: (id: string, confirm = false) =>
    request<{ ok: boolean; stopped: number; deleting?: boolean }>(
      `/api/v1/sandbox-servers/${enc(id)}${confirm ? "?confirm=true" : ""}`,
      { method: "DELETE" },
    ),
  issueCert: (id: string, teamId?: string) =>
    request<{ state: string }>(`/api/v1/sandbox-servers/${enc(id)}/certificate`, {
      method: "POST",
      body: teamId ? { teamId } : {},
    }),
  createDnsRecord: (id: string, teamId?: string) =>
    request<{ ok: boolean; status: string; existing: { type: string; content: string }[] }>(
      `/api/v1/sandbox-servers/${enc(id)}/dns-record`,
      { method: "POST", body: teamId ? { teamId } : {} },
    ),
  templates: (team: string) =>
    request<{ templates: SandboxTemplate[] }>(
      `/api/v1/sandbox-templates?${new URLSearchParams({ team })}`,
    ),
  createTemplate: (body: { teamId?: string; name: string; image: string }) =>
    request<SandboxTemplate>("/api/v1/sandbox-templates", {
      method: "POST",
      body,
    }),
  removeTemplate: (id: string) =>
    request<{ ok: boolean }>(`/api/v1/sandbox-templates/${enc(id)}`, {
      method: "DELETE",
    }),
};

// ── Client tokens ──────────────────────────────────────────────────────────

const tokens = new Map<string, Connection>();

/** Keep the connection createSandbox answered, so the first page skips a call. */
export function rememberConnection(c: Connection) {
  tokens.set(c.sandboxId, c);
}

/** A token for the sandbox's server with a minute or more left. */
export async function connectionFor(id: string): Promise<Connection> {
  const c = tokens.get(id);
  if (c && c.expiresAt - Date.now() > 60_000) return c;
  const fresh = await sandboxApi.token(id);
  tokens.set(id, fresh);
  return fresh;
}

// ── The daemon ─────────────────────────────────────────────────────────────

export class DaemonError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

/** The daemon answers only on the tailnet for now. */
export const OFF_TAILNET =
  "Can't reach the sandbox server. It answers only on your tailnet for now.";

async function daemon<T>(
  id: string,
  path: string,
  init: { method?: string; query?: Record<string, string>; body?: BodyInit; json?: unknown; raw?: boolean } = {},
): Promise<T> {
  const c = await connectionFor(id);
  const q = new URLSearchParams(init.query ?? {});
  const url = `${c.url}/v1/sandboxes/${enc(id)}${path}${q.size ? `?${q}` : ""}`;
  let res: Response;
  try {
    res = await fetch(url, {
      method: init.method ?? "GET",
      headers: {
        authorization: `Bearer ${c.token}`,
        ...(init.json !== undefined ? { "content-type": "application/json" } : {}),
      },
      body: init.json !== undefined ? JSON.stringify(init.json) : init.body,
    });
  } catch {
    throw new DaemonError(OFF_TAILNET, 0);
  }
  if (!res.ok) {
    const data = await res.json().catch(() => null);
    throw new DaemonError(
      data?.error?.message ?? `The sandbox server answered ${res.status}.`,
      res.status,
    );
  }
  return (init.raw ? res.blob() : res.json()) as Promise<T>;
}

export type FileEntry = {
  name: string;
  path: string;
  kind: "file" | "dir" | "symlink" | "other";
  size: number;
  mode: number;
  modified: number;
  target?: string;
};

export type ExecResult = {
  exitCode: number | null;
  signal?: number;
  stdout: string;
  stderr: string;
  truncated: boolean;
  timedOut: boolean;
  durationMs: number;
};

export const daemonApi = {
  list: async (id: string, path: string) =>
    (await daemon<{ entries: FileEntry[] }>(id, "/files/list", { query: { path } }))
      .entries,
  read: (id: string, path: string) =>
    daemon<Blob>(id, "/files", { query: { path }, raw: true }),
  write: (id: string, path: string, body: Blob) =>
    daemon<{ bytes: number }>(id, "/files", { method: "PUT", query: { path }, body }),
  remove: (id: string, path: string, recursive = false) =>
    daemon<unknown>(id, "/files", {
      method: "DELETE",
      query: { path, ...(recursive ? { recursive: "true" } : {}) },
    }),
  mkdir: (id: string, path: string) =>
    daemon<unknown>(id, "/files/mkdir", { method: "POST", query: { path } }),
  exec: (id: string, cmd: string, timeoutMs?: number) =>
    daemon<ExecResult>(id, "/exec", { method: "POST", json: { cmd, timeoutMs } }),
};

// ── The daemon's WebSocket: envd frames, one per binary message ───────────
// [len u32 BE][kind u8][stream u32 BE][payload], len = payload + 5.

export const Kind = {
  Hello: 1,
  Request: 2,
  Response: 3,
  Stdin: 4,
  Stdout: 5,
  Stderr: 6,
  Eof: 7,
} as const;

export type Frame = { kind: number; stream: number; payload: Uint8Array };

export function encodeFrame(kind: number, stream: number, payload: Uint8Array = new Uint8Array(0)) {
  const out = new Uint8Array(9 + payload.length);
  const v = new DataView(out.buffer);
  v.setUint32(0, payload.length + 5);
  v.setUint8(4, kind);
  v.setUint32(5, stream);
  out.set(payload, 9);
  return out;
}

export function decodeFrame(bytes: Uint8Array): Frame {
  if (bytes.length < 9) throw new Error("the frame is cut short");
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const len = v.getUint32(0);
  if (len < 5 || bytes.length !== 4 + len) throw new Error("the frame is cut short");
  return { kind: v.getUint8(4), stream: v.getUint32(5), payload: bytes.subarray(9) };
}

const text = new TextEncoder();
export const jsonFrame = (kind: number, stream: number, value: unknown) =>
  encodeFrame(kind, stream, text.encode(JSON.stringify(value)));
export const parseFrame = <T>(f: Frame): T =>
  JSON.parse(new TextDecoder().decode(f.payload)) as T;

/**
 * The daemon's WebSocket for a sandbox. Browsers can't set headers on it, so
 * the token goes as a subprotocol and stays out of the URL.
 */
export async function connectSocket(id: string) {
  const c = await connectionFor(id);
  const base = c.url.replace(/^http/, "ws");
  const ws = new WebSocket(`${base}/v1/sandboxes/${enc(id)}/connect`, [
    "pyo",
    `pyo.token.${c.token}`,
  ]);
  ws.binaryType = "arraybuffer";
  return ws;
}

// ── Display ────────────────────────────────────────────────────────────────

export const SANDBOX_STATE: Record<SandboxState, { label: string; tone: "success" | "warning" | "danger" | "neutral" | "info" }> = {
  creating: { label: "Starting", tone: "info" },
  running: { label: "Running", tone: "success" },
  pausing: { label: "Pausing", tone: "info" },
  paused: { label: "Paused", tone: "neutral" },
  resuming: { label: "Resuming", tone: "info" },
  stopping: { label: "Stopping", tone: "info" },
  stopped: { label: "Stopped", tone: "neutral" },
  failed: { label: "Failed", tone: "danger" },
};

export const ended = (s: Sandbox) => s.state === "stopped" || s.state === "failed";

/** "1 CPU · 1 GB". */
export const sizeLabel = (cpus: number, memoryMib: number) =>
  `${cpus} CPU · ${memoryMib >= 1024 ? `${+(memoryMib / 1024).toFixed(1)} GB` : `${memoryMib} MB`}`;

/** "45m", "2h 10m", "3d": what's left until `at`, or null once it has passed. */
export function timeLeft(at: number | null, now = Date.now()) {
  if (!at) return null;
  const s = Math.floor((at - now) / 1000);
  if (s <= 0) return null;
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/** "7d", "1h", "1h30m", "90s", like Pyo's format_ttl. */
export function formatTtl(secs: number) {
  const d = Math.floor(secs / 86_400);
  const h = Math.floor((secs % 86_400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const out = `${d ? `${d}d` : ""}${h ? `${h}h` : ""}${m ? `${m}m` : ""}`;
  return s || !out ? `${out}${s}s` : out;
}

/** "1 hour", "30 days", "90 minutes": a lifetime in words. */
export function lifetimeLabel(secs: number) {
  const unit = LIFETIME_UNITS.find((u) => secs >= u.secs && secs % u.secs === 0) ?? LIFETIME_UNITS[LIFETIME_UNITS.length - 1];
  const n = +(secs / unit.secs).toFixed(2);
  return `${n} ${n === 1 ? unit.one : unit.many}`;
}

export const RUNTIME_LABEL: Record<string, string> = {
  firecracker: "Firecracker",
  gvisor: "gVisor",
  none: "None",
};

export const SERVER_STATE: Record<SandboxServer["state"], { label: string; tone: "success" | "warning" | "danger" | "neutral" | "info" }> = {
  joining: { label: "Joining", tone: "info" },
  ready: { label: "Ready", tone: "success" },
  unreachable: { label: "Unreachable", tone: "danger" },
  refused: { label: "Can't run sandboxes", tone: "danger" },
};

export const SIZES = [
  { value: "1", cpus: 1, memoryMib: 1024 },
  { value: "2", cpus: 2, memoryMib: 4096 },
  { value: "4", cpus: 4, memoryMib: 8192 },
] as const;

export const LIFETIMES = [
  { value: "5m", label: "5 min", secs: 300 },
  { value: "30m", label: "30 min", secs: 1800 },
  { value: "1h", label: "1 hour", secs: 3600 },
  { value: "6h", label: "6 hours", secs: 21_600 },
  { value: "1d", label: "1 day", secs: 86_400 },
] as const;

/** The `ttl` of a sandbox that runs until it's deleted. */
export const NO_LIFETIME = "none";

/** The longest lifetime Pyo takes: 365 days. */
export const MAX_LIFETIME_SECS = 365 * 86_400;

/** Units for a custom lifetime, largest first. `ttl` is how Pyo reads it. */
export const LIFETIME_UNITS = [
  { value: "d", one: "day", many: "days", secs: 86_400 },
  { value: "h", one: "hour", many: "hours", secs: 3600 },
  { value: "m", one: "minute", many: "minutes", secs: 60 },
] as const;

export type LifetimeUnit = (typeof LIFETIME_UNITS)[number]["value"];

/** Whether a lifetime fits the team's longest. Null `secs`: no lifetime. */
export const lifetimeAllowed = (secs: number | null, limits: SandboxLimits | null) =>
  !limits || limits.maxTtlSecs === null || (secs !== null && secs <= limits.maxTtlSecs);

/** The TypeScript SDK call that makes the same sandbox as the form. */
export function sdkSnippet(b: Omit<CreateSandbox, "teamId">) {
  // base is the default, so it's left out.
  const lines: string[] =
    b.template && b.template !== BASE_TEMPLATE ? [`  template: ${JSON.stringify(b.template)},`] : [];
  if (b.server) lines.push(`  server: ${JSON.stringify(b.server)},`);
  if (b.cpus) lines.push(`  cpus: ${b.cpus},`);
  if (b.memoryMib) lines.push(`  memoryMib: ${b.memoryMib},`);
  if (b.ttl) lines.push(`  ttl: ${JSON.stringify(b.ttl)},`);
  if (b.pauseWhenIdleSecs) lines.push(`  pauseWhenIdleSecs: ${b.pauseWhenIdleSecs},`);
  if (b.network) lines.push(`  network: ${JSON.stringify(b.network)},`);
  if (b.isolation && b.isolation !== "auto")
    lines.push(`  isolation: ${JSON.stringify(b.isolation)},`);
  if (b.env && Object.keys(b.env).length)
    lines.push(`  env: { ${Object.keys(b.env).map((k) => `${/^[A-Za-z_$][\w$]*$/.test(k) ? k : JSON.stringify(k)}: "…"`).join(", ")} },`);
  if (b.labels && Object.keys(b.labels).length)
    lines.push(`  labels: ${JSON.stringify(b.labels)},`);
  return [
    'import { Sandbox } from "@pyodev/sandbox";',
    "",
    "const sbx = await Sandbox.create({",
    ...lines,
    "});",
    'const out = await sbx.exec("uname -a");',
  ].join("\n");
}
