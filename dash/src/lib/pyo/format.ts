// Display helpers shared by every screen.

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

export function timeAgo(at: number, now = Date.now()) {
  const d = Math.max(0, now - at);
  if (d < 45_000) return "just now";
  if (d < HOUR) return `${Math.round(d / MIN)}m ago`;
  if (d < DAY) return `${Math.round(d / HOUR)}h ago`;
  if (d < 30 * DAY) return `${Math.round(d / DAY)}d ago`;
  if (d < 365 * DAY) return `${Math.round(d / (30 * DAY))}mo ago`;
  return new Date(at).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    year: "numeric",
  });
}

export function dateTime(at: number) {
  return new Date(at).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

export function clock(at: number, seconds = true) {
  return new Date(at).toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
    second: seconds ? "2-digit" : undefined,
    hour12: false,
  });
}

export function clockMs(at: number) {
  return `${clock(at)}.${String(new Date(at).getMilliseconds()).padStart(3, "0")}`;
}

export function duration(ms: number) {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** CPU in vCPU as a decimal: 0.1 vCPU, 0.5 vCPU, 2 vCPU. */
export const cpu = (millis: number) => {
  const v = millis / 1000;
  return v > 0 && v < 0.01 ? "<0.01 vCPU" : `${+v.toFixed(2)} vCPU`;
};
export const mem = (mi: number) =>
  mi >= 1024 ? `${+(mi / 1024).toFixed(1)} GiB` : `${Math.round(mi)} MiB`;
export const gib = (g: number) => `${+g.toFixed(1)} GiB`;
export const cores = (c: number) => `${+c.toFixed(1)} cores`;

export function compact(n: number) {
  if (n >= 1_000_000) return `${+(n / 1_000_000).toFixed(1)}M`;
  if (n >= 10_000) return `${Math.round(n / 1000)}k`;
  if (n >= 1000) return `${+(n / 1000).toFixed(1)}k`;
  return String(Math.round(n));
}

export function megabytes(mb: number) {
  if (mb >= 1024 * 1024) return `${+(mb / 1024 / 1024).toFixed(2)} TB`;
  if (mb >= 1024) return `${+(mb / 1024).toFixed(1)} GB`;
  return `${Math.round(mb)} MB`;
}

export const pct = (n: number, digits = 0) => `${(n * 100).toFixed(digits)}%`;

export function slugify(s: string) {
  return s
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40);
}

/** RFC 1123 label, as Kubernetes requires for names and namespaces. */
export const DNS_LABEL = /^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$/;
export const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;
export const HOSTNAME =
  /^(?=.{1,253}$)([a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/;

export function shortSha(sha: string) {
  return sha.startsWith("sha256:")
    ? `sha256:${sha.slice(7, 19)}`
    : sha.slice(0, 7);
}

/** Deterministic PRNG (mulberry32) so mock series are stable across renders. */
export function rng(seed: string | number) {
  let h = typeof seed === "number" ? seed : 0;
  if (typeof seed === "string")
    for (let i = 0; i < seed.length; i++)
      h = Math.imul(31, h) + seed.charCodeAt(i);
  let a = h >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** "prj_k3x9q2mf0a": prefix plus 10 random [a-z0-9]. The API accepts ids made here. */
export function newId(prefix: string) {
  const bytes = new Uint8Array(10);
  crypto.getRandomValues(bytes);
  return `${prefix}_${Array.from(bytes, (b) => "abcdefghijklmnopqrstuvwxyz0123456789"[b % 36]).join("")}`;
}

export function hex(len: number, seed?: string) {
  const r = seed ? rng(seed) : Math.random;
  let s = "";
  for (let i = 0; i < len; i++) s += Math.floor(r() * 16).toString(16);
  return s;
}

export function generatePassword() {
  const chars = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
  let s = "";
  const buf = new Uint32Array(24);
  crypto.getRandomValues(buf);
  for (const n of buf) s += chars[n % chars.length];
  return s;
}

export const plural = (n: number, word: string, many = `${word}s`) =>
  `${n} ${n === 1 ? word : many}`;

/** A domain's address: https once its certificate is issued. */
export const domainUrl = (d: { hostname: string; tls?: string }) =>
  `${d.tls === "issued" ? "https" : "http"}://${d.hostname}`;

/**
 * A server's NVIDIA GPUs: "2 × NVIDIA GeForce RTX 4090", or the model and
 * "starting" while the device plugin hasn't counted them. Null for none.
 */
export function gpuText(n: {
  gpus?: number;
  gpuDetected?: boolean;
  gpuModel?: string;
}) {
  const model = n.gpuModel || "NVIDIA GPU";
  if (n.gpus) return `${n.gpus} × ${model}`;
  if (n.gpuDetected) return `${model} (starting)`;
  return null;
}

/** A cluster's GPU support, in a few words. */
export function clusterGpuText(
  g:
    | { servers: number; total: number; used?: number; devicePlugin: string }
    | null
    | undefined,
) {
  if (!g?.servers) return "None";
  const servers = `${g.servers} server${g.servers === 1 ? "" : "s"}`;
  if (g.devicePlugin !== "ready")
    return `${servers} · NVIDIA device plugin ${g.devicePlugin}`;
  return `${g.total} NVIDIA GPU${g.total === 1 ? "" : "s"} on ${servers}, ${g.total - (g.used ?? 0)} free`;
}

/** "NVIDIA GeForce RTX 4090" -> "RTX 4090", for tight spots. */
export function gpuModelShort(model?: string) {
  return (model || "GPU").replace(/^NVIDIA\s+/i, "").replace(/^GeForce\s+/i, "");
}

/** What's in the way of a server's GPU, in a few words; null when nothing. */
export function gpuProblem(n: { gpuStatus?: string }) {
  switch (n.gpuStatus) {
    case "no-driver":
      return "GPU: no driver";
    case "no-toolkit":
      return "GPU: no toolkit";
    case "no-runtime":
      return "GPU: restart k3s";
    default:
      return null;
  }
}

/** A server's GPUs for a table cell: "1 × RTX 4090 · 1 in use". */
export function gpuCell(n: {
  gpus?: number;
  gpusUsed?: number;
  gpuModel?: string;
  gpuStatus?: string;
}) {
  if (!n.gpuStatus) return null;
  const problem = gpuProblem(n);
  if (problem) return problem.replace(/^GPU: /, "");
  if (!n.gpus) return `${gpuModelShort(n.gpuModel)} · starting`;
  return `${n.gpus} × ${gpuModelShort(n.gpuModel)} · ${n.gpusUsed ?? 0} in use`;
}

/** What's missing for a server's GPU, and how to fix it; null when ready. */
export function gpuReason(n: { gpuStatus?: string }) {
  switch (n.gpuStatus) {
    case "no-driver":
      return "The NVIDIA driver isn't installed (nvidia-smi doesn't work). Install it, reboot, and run the join command again.";
    case "no-toolkit":
      return "The NVIDIA Container Toolkit isn't installed, and the join script couldn't install it on this system. Install it with NVIDIA's guide, then restart k3s: sudo systemctl restart k3s-agent. On a cluster's first server, restart k3s instead.";
    case "no-runtime":
      return "k3s started before the NVIDIA Container Toolkit was installed, so it has no nvidia runtime. Restart it: sudo systemctl restart k3s-agent. On a cluster's first server, restart k3s instead.";
    case "starting":
      return "Waiting for the NVIDIA device plugin to count its GPUs. It takes a minute or two after the server joins.";
    default:
      return null;
  }
}

/** A cluster's GPUs in one line: "2 GPUs · 1 free · RTX 4090, A100". */
export function gpuSummary(
  nodes: { gpus?: number; gpusUsed?: number; gpuModel?: string; gpuStatus?: string }[],
) {
  const withGpu = nodes.filter((n) => n.gpuStatus);
  if (!withGpu.length) return null;
  const total = withGpu.reduce((t, n) => t + (n.gpus ?? 0), 0);
  const used = withGpu.reduce((t, n) => t + (n.gpusUsed ?? 0), 0);
  const models = [
    ...new Set(
      withGpu.filter((n) => n.gpuModel).map((n) => gpuModelShort(n.gpuModel)),
    ),
  ];
  return [
    `${total} GPU${total === 1 ? "" : "s"}`,
    `${Math.max(0, total - used)} free`,
    ...(models.length ? [models.join(", ")] : []),
  ].join(" · ");
}

const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/** A cron expression in words, for the common shapes; "" when it isn't one. */
export function describeCron(expr: string): string {
  const f = expr.trim().split(/\s+/);
  if (f.length !== 5) return "";
  const [mi, h, dom, mon, dow] = f;
  const num = (x: string) => /^\d+$/.test(x);
  const at = () => `${h.padStart(2, "0")}:${mi.padStart(2, "0")} UTC`;
  if (f.every((x) => x === "*")) return "Every minute";
  const step = mi.match(/^\*\/(\d+)$/);
  if (step && h === "*" && dom === "*" && mon === "*" && dow === "*") return `Every ${step[1]} minutes`;
  if (num(mi) && h === "*" && dom === "*" && mon === "*" && dow === "*") return mi === "0" ? "Every hour" : `Every hour at :${mi.padStart(2, "0")}`;
  if (num(mi) && num(h) && dom === "*" && mon === "*" && dow === "*") return `Every day at ${at()}`;
  if (num(mi) && num(h) && dom === "*" && mon === "*" && num(dow) && +dow < 7) return `Every ${DAYS[+dow]} at ${at()}`;
  if (num(mi) && num(h) && num(dom) && mon === "*" && dow === "*") return `On day ${dom} of every month at ${at()}`;
  return "";
}

/** Bytes per second as "1.2 MB/s"; "—" when unknown. */
export function rate(bps: number | null | undefined) {
  if (bps == null || !Number.isFinite(bps)) return "—";
  if (bps < 1000) return `${Math.round(bps)} B/s`;
  if (bps < 1e6) return `${(bps / 1e3).toFixed(bps < 1e4 ? 1 : 0)} kB/s`;
  if (bps < 1e9) return `${(bps / 1e6).toFixed(bps < 1e7 ? 1 : 0)} MB/s`;
  return `${(bps / 1e9).toFixed(1)} GB/s`;
}
