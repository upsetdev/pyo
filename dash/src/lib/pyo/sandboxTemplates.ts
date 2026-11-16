// Sandbox templates as the dashboard shows them: Pyo's built-in `base`
// (crates/pyo-api/src/sandbox/base.rs) and the rest. No imports, so
// dash/scripts tests them as they are.

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
  if (!image) return "";
  return t.builtin && t.source?.os ? `${t.source.os} · ${imageRepo(image)}` : image;
}

/** The template a new sandbox form starts on: base once it's ready, else
 * the first ready one. */
export function defaultTemplate(list: TemplateLike[]) {
  const ready = list.filter((t) => t.state === "ready");
  return (ready.find((t) => t.builtin && t.name === BASE_TEMPLATE) ?? ready[0])?.name ?? "";
}
