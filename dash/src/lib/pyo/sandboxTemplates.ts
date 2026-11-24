// Sandbox templates as the dashboard shows them: Pyo's built-in `base` and
// `desktop` (crates/pyo-api/src/sandbox/base.rs) and the rest, and the
// servers a new sandbox can go to. No imports, so dash/scripts tests them as
// they are.

type TemplateLike = {
  name?: string;
  state?: string;
  builtin?: boolean;
  source?: { image?: string; os?: string } | null;
};

/** The template a sandbox starts from when none is given. */
export const BASE_TEMPLATE = "base";
/** Pyo's desktop: the screen in voxl on DESKTOP_PORT, the browser's CDP on 9222. */
export const DESKTOP_TEMPLATE = "desktop";
export const DESKTOP_PORT = 6080;

/** An image without its tag and digest: ghcr.io/a/b:1@sha256:… is ghcr.io/a/b. */
export function imageRepo(image: string) {
  const name = image.split("@")[0];
  const i = name.lastIndexOf(":");
  return i > name.lastIndexOf("/") ? name.slice(0, i) : name;
}

/** What Pyo's built-in templates run (upsetdev/pyo-sandbox-images), when
 * the template doesn't say. */
export const BUILTIN_OS = "Ubuntu 24.04";

/** A template's image as lists show it. Pyo's built-in one says what it
 * runs and leaves out its version: "Ubuntu 24.04 · ghcr.io/upsetdev/pyo-sandbox-images/base". */
export function templateImage(t: TemplateLike) {
  const image = t.source?.image;
  if (t.builtin && t.source?.os) return image ? `${t.source.os} · ${imageRepo(image)}` : t.source.os;
  return image ?? "";
}

/** What a template runs, when known: Pyo's own say, others when they tell. */
export function templateOs(t: TemplateLike) {
  return t.source?.os || (isPyos(t) ? BUILTIN_OS : "");
}

/** An image ref with its digest cut to 12 characters:
 * "ghcr.io/a/b@sha256:ad5c5976…". Tags and short refs stay as they are. */
export function shortImage(image: string) {
  return image.replace(/sha256:([0-9a-f]{12})[0-9a-f]{4,}/, "sha256:$1…");
}

/** An image ref in parts that end at "/", ":" and "@", so a long one
 * wraps there instead of anywhere. */
export function imageParts(image: string) {
  return image.match(/[^/:@]*[/:@]?/g)?.filter(Boolean) ?? [];
}

/** The new template form's name and image the wrong way round: an image in
 * the name (it has a "/", ":" or "@", which names can't) and a bare word in
 * the image. */
export function fieldsSwapped(name: string, image: string) {
  const imageLike = (s: string) => /[/:@]/.test(s.trim());
  return imageLike(name) && !!image.trim() && !imageLike(image);
}

/** Pyo's built-in base. */
export const isBase = (t: TemplateLike) => !!t.builtin && t.name === BASE_TEMPLATE;

/** Pyo's built-in desktop. */
export const isDesktop = (t: TemplateLike) => !!t.builtin && t.name === DESKTOP_TEMPLATE;

/** One of Pyo's own, which servers make the first time it's used there. */
export const isPyos = (t: TemplateLike) => isBase(t) || isDesktop(t);

/** Whether a new sandbox can start from it now: Pyo's own unless Pyo has no
 * image for it yet, others once ready. */
export const usable = (t: TemplateLike) => (isPyos(t) ? t.state !== "unavailable" : t.state === "ready");

/** Pyo's own first (base, then desktop), the rest as they come. */
export function pyosFirst<T extends TemplateLike>(list: T[]) {
  const rank = (t: T) => (isBase(t) ? 0 : isDesktop(t) ? 1 : 2);
  return [...list].sort((a, b) => rank(a) - rank(b));
}

/** The smallest size the desktop runs well in. */
export const DESKTOP_MIN = { cpus: 2, memoryMib: 2048 };

/** The size a new sandbox from `template` should have at least: the
 * desktop's for it, none for the rest. */
export const minSize = (t: TemplateLike | undefined) => (t && isDesktop(t) ? DESKTOP_MIN : null);

/** An app the desktop can bring to the front when its screen opens. */
export type DesktopApp = "chromium";

/** The desktop's screen in a browser, over HTTPS: voxl decodes video with
 * WebCodecs, which browsers offer only on HTTPS pages (and localhost).
 * `web` is the connection's webUrl, sandboxes' pages on their own tailnet
 * name, which Pyo's API passes on to the daemon's `/web/{port}/`
 * (crates/pyo-api sandbox/pages.rs). It takes the token once and keeps it
 * in a cookie for that sandbox's pages. vnc.html, where noVNC's page was,
 * goes on to voxl's page beside it, which finds its WebSocket there too.
 * `name`, the sandbox's, goes in the tab's title: "Desktop | <name>". */
export function desktopUrl(
  web: string,
  sandboxId: string,
  token: string,
  opts: { app?: DesktopApp; name?: string } = {},
) {
  const prefix = `v1/sandboxes/${encodeURIComponent(sandboxId)}/web/${DESKTOP_PORT}/`;
  const q = new URLSearchParams({
    autoconnect: "1",
    resize: "remote",
    reconnect: "1",
    // The desktop brings it to the front (pyo-sandbox-images, images/desktop/desktop.py).
    ...(opts.app ? { app: opts.app } : {}),
    ...(opts.name ? { name: opts.name } : {}),
    pyo_token: token,
  });
  return `${web.replace(/\/$/, "")}/${prefix}vnc.html?${q}`;
}

type TailscaleLike = { suffix?: string | null; sandboxes?: string | null } | null | undefined;

type OutboundLike = { outbound?: boolean; tailnetName?: string | null } | null | undefined;

/** Where a sandbox server that joined over HTTPS (Pyo Cloud's) answers
 * browsers: HTTPS on its own tailnet name. */
export const TAILNET_PORT = 7443;

/** That server's address for browsers, or null for any other server. */
export function tailnetUrl(x: OutboundLike) {
  return x?.outbound && x.tailnetName ? `https://${x.tailnetName}:${TAILNET_PORT}` : null;
}

/** On Pyo Cloud the desktop, terminal and files open only from the team's tailnet. */
export const OFF_TEAM_TAILNET = "Opens from devices on your team's tailnet.";

/** Why the desktop can't open yet, or null when it can: it opens over
 * HTTPS on the tailnet only. Self-hosted, at sandboxes' own name there.
 * On Pyo Cloud, at the sandbox's server's own name (`server`). */
export function desktopBlocked(ts: TailscaleLike, server?: OutboundLike) {
  if (server?.outbound) return server.tailnetName ? null : "Its address on your tailnet isn't ready yet.";
  if (ts?.sandboxes) return null;
  if (!ts?.suffix) return "Opens over your tailnet. Set up Tailscale in Integrations first.";
  return "Its address on your tailnet isn't ready yet.";
}

/** The template a new sandbox form starts on: base, else the first usable one. */
export function defaultTemplate(list: TemplateLike[]) {
  const ok = list.filter(usable);
  return (ok.find(isBase) ?? ok[0])?.name ?? "";
}

/** A template's status as the templates page says it. */
export function templateStatus(t: TemplateLike) {
  if (isPyos(t)) return t.state === "unavailable" ? "Not available yet" : "Ready to use";
  return { pending: "Pending", building: "Building", ready: "Ready", failed: "Failed" }[t.state ?? ""] ?? t.state ?? "";
}

/** What a sandbox waiting for its server to make its template shows, with
 * how far it got when known. */
export function gettingReady(template: string, server: string | null, percent?: number | null) {
  const pct = percent == null ? "" : ` ${percent}%`;
  return (server ? `Getting ${template} ready on ${server}…` : `Getting ${template} ready…`) + pct;
}

type BuildLike = {
  state: string;
  phase?: string | null;
  pulled?: number | null;
  unpacked?: number | null;
  imageBytes?: number | null;
};

/** How far a server got making a template: a short label and the percent
 * of its step, or null when the step can't say (a bar with no end). */
export type Progress = { label: string; percent: number | null };

const STEPS = ["queued", "pulling", "unpacking", "snapshotting"];

/** Part of a whole as a percent, never 100 while the step still runs. */
const part = (done: number | null | undefined, total: number | null | undefined) =>
  total && total > 0 && done ? Math.min(99, Math.floor((done / total) * 100)) : null;

/** One server's progress, or null when it isn't making the template. A
 * server too old to count what it unpacked says 0 there: no percent. */
export function serverProgress(x: BuildLike): Progress | null {
  if (x.state !== "building") return null;
  switch (x.phase) {
    case "queued":
      return { label: "Waiting", percent: null };
    case "pulling": {
      const percent = x.imageBytes ? (part(x.pulled, x.imageBytes) ?? 0) : null;
      return { label: percent == null ? "Pulling" : `Pulling ${percent}%`, percent };
    }
    case "unpacking": {
      const percent = part(x.unpacked, x.imageBytes);
      return { label: percent == null ? "Unpacking" : `Unpacking ${percent}%`, percent };
    }
    default:
      return { label: "Starting", percent: null };
  }
}

/** How far a server got over all its steps, for a single number that
 * never goes back: downloading is the first 60%, unpacking the next 30%,
 * starting the rest. Null when it isn't making the template. */
export function overallPercent(x: BuildLike): number | null {
  const p = serverProgress(x);
  if (!p) return null;
  const at = (from: number, span: number) => Math.floor(from + ((p.percent ?? 0) / 100) * span);
  switch (x.phase) {
    case "queued":
      return 0;
    case "pulling":
      return at(0, 60);
    case "unpacking":
      return at(60, 30);
    default:
      return 90;
  }
}

/** Where a server is, to find the slowest: its step, then its percent. */
const rank = (x: BuildLike, p: Progress) =>
  Math.max(0, STEPS.indexOf(x.phase ?? "snapshotting")) * 100 + (p.percent ?? 0);

/** A template's progress over its servers: the slowest one's, and how many
 * are making it. Null when none is. */
export function templateProgress(t: { servers: BuildLike[] }): (Progress & { servers: number }) | null {
  const all = t.servers.flatMap((x) => {
    const p = serverProgress(x);
    return p ? [{ p, at: rank(x, p) }] : [];
  });
  if (!all.length) return null;
  const slowest = all.reduce((a, b) => (b.at < a.at ? b : a));
  return { ...slowest.p, servers: all.length };
}

type ServerLike = {
  state: string;
  enabled: boolean;
  capability: { runtime: string; network?: boolean } | null;
  capacity: { memoryAvailable: number; memoryReserved: number } | null;
};

/** Bytes a server can still give sandboxes. */
export const freeMemory = (x: ServerLike) =>
  x.capacity ? Math.max(0, x.capacity.memoryAvailable - x.capacity.memoryReserved) : 0;

/** "6.2 GB free", "512 MB free". */
export function freeLabel(bytes: number) {
  const mb = bytes / 1024 ** 2;
  return mb >= 1024 ? `${+(mb / 1024).toFixed(1)} GB free` : `${Math.floor(mb)} MB free`;
}

/** Why a server can't take a sandbox of this size, or "" when it can. */
export function serverRefusal(x: ServerLike, memoryMib: number, network: "internet" | "none") {
  if (x.state !== "ready") return x.state === "joining" ? "Setting up" : "Not reachable";
  if (!x.enabled) return "Turned off";
  if (!x.capability || x.capability.runtime === "none") return "Can't run sandboxes";
  if (network === "internet" && !x.capability.network) return "No networking";
  if (freeMemory(x) < memoryMib * 1024 ** 2) return "No room";
  return "";
}

/**
 * Why a sandbox server can't be added yet, or "" when it can.
 * Self-hosted, a new one joins at Pyo's own door on the tailnet
 * (`pyoTailnet`: Pyo's server is on Tailscale), and a team without its own
 * Tailscale (`teamTailscale`) uses Pyo's. On Pyo Cloud it joins the team's
 * own tailnet, then Pyo over HTTPS: only the team's Tailscale counts.
 * - "pyo": the team has Tailscale, but Pyo's own server isn't on Tailscale.
 * - "tailscale": no Tailscale to join. Self-hosted, NeedsTailscale says so.
 */
export function addServerBlock(x: { hosted: boolean; pyoTailnet: boolean; teamTailscale: boolean }) {
  if (x.hosted) return x.teamTailscale ? "" : "tailscale";
  if (x.pyoTailnet) return "";
  return x.teamTailscale ? "pyo" : "tailscale";
}
