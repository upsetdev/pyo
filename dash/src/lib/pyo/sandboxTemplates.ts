// Sandbox templates as the dashboard shows them: Pyo's built-in `base`
// (crates/pyo-api/src/sandbox/base.rs) and the rest, and the servers a new
// sandbox can go to. No imports, so dash/scripts tests them as they are.

type TemplateLike = {
  name?: string;
  state?: string;
  builtin?: boolean;
  source?: { image?: string; os?: string } | null;
};

/** The template a sandbox starts from when none is given. */
export const BASE_TEMPLATE = "base";

/** An image without its tag and digest: ghcr.io/a/b:1@sha256:… is ghcr.io/a/b. */
export function imageRepo(image: string) {
  const name = image.split("@")[0];
  const i = name.lastIndexOf(":");
  return i > name.lastIndexOf("/") ? name.slice(0, i) : name;
}

/** A template's image as lists show it. Pyo's built-in one says what it
 * runs and leaves out its version: "Debian 13 · ghcr.io/upsetdev/pyo/sandbox-base". */
export function templateImage(t: TemplateLike) {
  const image = t.source?.image;
  if (t.builtin && t.source?.os) return image ? `${t.source.os} · ${imageRepo(image)}` : t.source.os;
  return image ?? "";
}

/** The new template form's name and image the wrong way round: an image in
 * the name (it has a "/", ":" or "@", which names can't) and a bare word in
 * the image. */
export function fieldsSwapped(name: string, image: string) {
  const imageLike = (s: string) => /[/:@]/.test(s.trim());
  return imageLike(name) && !!image.trim() && !imageLike(image);
}

/** Pyo's built-in base, which servers make the first time it's used there. */
export const isBase = (t: TemplateLike) => !!t.builtin && t.name === BASE_TEMPLATE;

/** Whether a new sandbox can start from it now: base unless Pyo has no
 * image for it yet, others once ready. */
export const usable = (t: TemplateLike) => (isBase(t) ? t.state !== "unavailable" : t.state === "ready");

/** The template a new sandbox form starts on: base, else the first usable one. */
export function defaultTemplate(list: TemplateLike[]) {
  const ok = list.filter(usable);
  return (ok.find(isBase) ?? ok[0])?.name ?? "";
}

/** A template's status as the templates page says it. */
export function templateStatus(t: TemplateLike) {
  if (isBase(t)) return t.state === "unavailable" ? "Not available yet" : "Ready to use";
  return { pending: "Pending", building: "Building", ready: "Ready", failed: "Failed" }[t.state ?? ""] ?? t.state ?? "";
}

/** What a sandbox waiting for its server to make its template shows. */
export const gettingReady = (template: string, server: string | null) =>
  server ? `Getting ${template} ready on ${server}…` : `Getting ${template} ready…`;

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
