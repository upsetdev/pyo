import catalog from "./catalog.json";
import { selfRefs } from "./refs";
import type {
  HealthCheck,
  Template,
  TemplateCategory,
  TemplateCompanion,
  TemplateDatabase,
  TemplateCompanionRecord,
  TemplateField,
  TemplateFieldRecord,
  TemplatePort,
  TemplateRecord,
} from "./types";

// Templates come from pyo-templates, the one place they're defined: its
// built index.json is bundled as catalog.json (scripts/sync-templates.ts).
// This file only reads that snapshot into what the gallery, the deploy
// wizard and the review use, and holds the logic around it: versions,
// field values in arguments, generated secrets, databases and companions.

export const TEMPLATE_CATEGORIES: { id: TemplateCategory; label: string }[] = [
  { id: "databases", label: "Databases" },
  { id: "caches", label: "Caches" },
  { id: "storage", label: "Storage" },
  { id: "networking", label: "Networking" },
  { id: "monitoring", label: "Monitoring" },
  { id: "messaging", label: "Messaging" },
  { id: "developer-tools", label: "Dev tools" },
  { id: "web", label: "Web apps" },
  { id: "ai", label: "AI" },
  { id: "other", label: "Other" },
];

export const categoryLabel = (id: TemplateCategory) =>
  TEMPLATE_CATEGORIES.find((c) => c.id === id)?.label ?? id;

// ── Reading the catalog ────────────────────────────────────────────────────

/** One entry of index.json, as pyo-templates builds it (template.schema.json). */
type Entry = Record<string, any>;

const ENGINES = ["mysql", "postgres", "kv"];
const isStr = (v: unknown): v is string => typeof v === "string";
const isNum = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

function healthCheck(h: Entry): HealthCheck {
  return {
    type: h.type,
    path: h.path ?? "",
    port: h.port ?? 0,
    command: h.command ?? "",
    initialDelaySeconds: h.initialDelaySeconds,
    periodSeconds: h.periodSeconds,
  };
}

const ports = (list: Entry[]): TemplatePort[] =>
  list.map((p) => ({ name: p.name, port: p.port, protocol: p.protocol, exposure: p.exposure }));

function companion(c: Entry): TemplateCompanion {
  return {
    name: c.name,
    image: c.image,
    version: c.version,
    workloadKind: c.workload,
    stateful: c.stateful,
    replicas: c.replicas,
    resources: { ...c.resources },
    ...(c.args ? { args: c.args } : {}),
    ...(c.entrypoint ? { entrypoint: c.entrypoint } : {}),
    ...(c.storage ? { storage: c.storage } : {}),
    ports: ports(c.ports),
    healthCheck: healthCheck(c.healthCheck),
    env: c.env.map((e: Entry) => ({ key: e.key, value: e.value ?? "" })),
  };
}

/** Why an entry can't be used, if it can't: then it's left out. */
function problem(t: Entry): string | undefined {
  if (!isStr(t.id) || !/^[a-z0-9]+(-[a-z0-9]+)*$/.test(t.id)) return "id";
  if (!isStr(t.name) || !isStr(t.description)) return "name";
  if (!isStr(t.version) || !/^\d+\.\d+\.\d+$/.test(t.version)) return "version";
  if (t.source?.type !== "image" || !isStr(t.source.image) || !t.source.versions?.length) return "source";
  if (!t.source.versions.every((v: Entry) => isStr(v.tag))) return "versions";
  if (!TEMPLATE_CATEGORIES.some((c) => c.id === t.category)) return "category";
  const r = t.resources ?? {};
  if (![r.cpuRequest, r.cpuLimit, r.memoryRequest, r.memoryLimit].every(isNum)) return "resources";
  if (!Array.isArray(t.ports) || !Array.isArray(t.env) || !Array.isArray(t.fields)) return "lists";
  if (!t.healthCheck?.type) return "healthCheck";
  if ((t.databases ?? []).some((d: Entry) => !ENGINES.includes(d.engine) || typeof d.env !== "object")) return "databases";
  return undefined;
}

/** A catalog entry as the dashboard's Template. */
export function fromCatalog(t: Entry, rank: number, logo?: { hex: string; path: string }): Template {
  const versions: { tag: string; digest: string }[] = t.source.versions;
  // The version picker comes from the image's versions, first.
  const version: TemplateField = {
    key: "version",
    label: "Version",
    type: "select",
    default: versions[0].tag,
    options: versions.map((v) => v.tag),
    required: true,
  };
  const fields: TemplateField[] = t.fields.map((f: Entry) => ({
    ...f,
    // A sensitive field has none in the catalog: empty means generated.
    default: f.default ?? (f.type === "boolean" ? false : ""),
  }));
  const gpu = t.gpu as Template["gpu"];
  return {
    id: t.id,
    name: t.name,
    description: t.description,
    category: t.category,
    ...(logo ? { logo } : {}),
    ...(t.homepage ? { homepage: t.homepage } : {}),
    tier: t.tier ?? "community",
    author: t.author ?? "",
    templateVersion: t.version,
    ...(isStr(t.appVersion) ? { appVersion: t.appVersion } : {}),
    image: t.source.image,
    version: versions[0].tag,
    versions,
    kind: t.kind,
    workloadKind: t.workload,
    stateful: !!t.stateful,
    featured: !!t.featured,
    hidden: !!t.hidden,
    rank,
    ports: ports(t.ports),
    env: t.env.map((e: Entry) => ({
      key: e.key,
      value: e.value ?? "",
      secret: !!e.secret,
      ...(e.generate ? { generate: true } : {}),
      ...(e.format ? { format: e.format } : {}),
      ...(e.prefix ? { prefix: e.prefix } : {}),
    })),
    fields: [version, ...fields],
    resources: {
      ...t.resources,
      // A GPU it needs is part of its size; an optional one is left for
      // people to add when their cluster has one.
      ...(gpu?.use === "required" ? { gpu: gpu.count } : {}),
    },
    replicas: t.replicas,
    ...(t.storage ? { storage: t.storage } : {}),
    healthCheck: healthCheck(t.healthCheck),
    hints: t.hints ?? [],
    ...(t.args ? { args: t.args } : {}),
    ...(t.entrypoint ? { entrypoint: t.entrypoint } : {}),
    ...(gpu ? { gpu } : {}),
    databases: (t.databases ?? []).map(
      (d: Entry): TemplateDatabase => ({
        engine: d.engine === "kv" ? "valkey" : d.engine,
        env: d.env,
        ...(d.engine === "postgres" && Array.isArray(d.extensions) && d.extensions.length
          ? { extensions: d.extensions.filter((x: unknown) => typeof x === "string") }
          : {}),
      }),
    ),
    companions: (t.companions ?? []).map(companion),
  };
}

/** The catalog's templates, in its order; entries this Pyo can't read are left out. */
export function readCatalog(raw: {
  schema?: number;
  templates?: Entry[];
  logos?: Record<string, { hex: string; path: string }>;
}): Template[] {
  if (raw.schema !== 1 || !Array.isArray(raw.templates)) return [];
  return raw.templates.flatMap((t, i) => {
    const why = problem(t);
    if (why) {
      console.warn(`Template ${t?.id ?? i} left out: bad ${why}`);
      return [];
    }
    return [fromCatalog(t, i, raw.logos?.[t.id])];
  });
}

/** Every template, hidden ones too (services keep using them). */
export const TEMPLATES: Template[] = readCatalog(catalog);

/** What galleries show. */
export const GALLERY_TEMPLATES = TEMPLATES.filter((t) => !t.hidden);

export function getTemplate(id: string | undefined) {
  return TEMPLATES.find((t) => t.id === id);
}

/** For links: an id, or a forgiving match ("mariadb", "MariaDB"). */
export function findTemplate(q: string | undefined) {
  const v = q?.trim().toLowerCase();
  if (!v) return undefined;
  return (
    getTemplate(v) ??
    GALLERY_TEMPLATES.find((t) => t.name.toLowerCase() === v) ??
    GALLERY_TEMPLATES.find((t) => t.id.startsWith(v))
  );
}

// ── Deploying one ──────────────────────────────────────────────────────────

/** An image as a template pins it: repository:tag@digest. */
const pinned = (image: string, v: { tag: string; digest: string }) =>
  `${image}:${v.tag}${v.digest ? `@${v.digest}` : ""}`;

/** A field whose value is a secret: recorded only as set or not. */
export const secretField = (f: TemplateField) => !!f.sensitive || f.type === "password";

/**
 * What a service deployed from `t` records in its source, for upgrades
 * later: the template's version, each Compose service's image (its own
 * under the template's id, which is its name there), and the deploy form's
 * values, never a secret one's.
 */
export function templateRecord(
  t: Template,
  values: Record<string, string | number | boolean>,
): TemplateRecord {
  const tag = String(values.version ?? t.version);
  const version = t.versions.find((v) => v.tag === tag) ?? { tag, digest: "" };
  const images: Record<string, string> = { [t.id]: pinned(t.image, version) };
  for (const c of t.companions) images[c.name] = pinned(c.image, c.version);
  const fields: Record<string, TemplateFieldRecord> = {};
  for (const f of t.fields) {
    if (f.key === "version") continue;
    const v = values[f.key] ?? f.default;
    fields[f.key] = secretField(f)
      ? { secret: true, set: v !== "" && v !== undefined, ...(f.envKey ? { env: f.envKey } : {}) }
      : v;
  }
  return {
    templateVersion: t.templateVersion,
    // The app's version is the default tag's.
    ...(t.appVersion && tag === t.version ? { appVersion: t.appVersion } : {}),
    images,
    fields,
  };
}

/** What a companion records: the template and the image it gave it. */
export function companionRecord(t: Template, c: TemplateCompanion): TemplateCompanionRecord {
  return { templateId: t.id, templateVersion: t.templateVersion, service: c.name, image: pinned(c.image, c.version) };
}

/** Its arguments, with `{{key}}` replaced by the deploy form's values. */
export function templateArgs(
  args: string[] | undefined,
  values: Record<string, string | number | boolean>,
) {
  return args?.map((a) =>
    a.replace(/\{\{\s*([^}\s]+)\s*\}\}/g, (_, key: string) => String(values[key] ?? "")),
  );
}

/**
 * A companion's service name: the template's prefix swapped for the
 * service's ("plausible-events" for a service named "stats" is
 * "stats-events"), or the companion's name after the service's.
 */
export function companionService(t: Template, c: TemplateCompanion, service: string) {
  const suffix = c.name.startsWith(`${t.id}-`) ? c.name.slice(t.id.length) : `-${c.name}`;
  return `${service.slice(0, 40 - suffix.length)}${suffix}`;
}

/** `${{self.…}}` and `${{<companion>.…}}` as the services they become. */
export function templateRefs(value: string, service: string, t?: Template) {
  let v = selfRefs(value, service);
  for (const c of t?.companions ?? [])
    v = v.replaceAll(`\${{${c.name}.`, `\${{${companionService(t!, c, service)}.`);
  return v;
}

const LETTERS = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

/** A value Pyo makes for a template's `generate` variable. */
export function generateValue(format?: "long" | "hex-32" | "base64-32") {
  if (format === "base64-32" || format === "hex-32") {
    const b = new Uint8Array(32);
    crypto.getRandomValues(b);
    return format === "hex-32"
      ? [...b].map((x) => x.toString(16).padStart(2, "0")).join("")
      : btoa(String.fromCharCode(...b));
  }
  const n = format === "long" ? 64 : 24;
  const buf = new Uint32Array(n);
  crypto.getRandomValues(buf);
  let s = "";
  for (const x of buf) s += LETTERS[x % LETTERS.length];
  return s;
}
