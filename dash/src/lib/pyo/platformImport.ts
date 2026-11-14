// Importing from another platform (crates/pyo-api/src/platform_import):
// preview what a Coolify or Dokploy installation, or a Railway or Vercel
// account, becomes, then import the picked things. The token goes with each
// request and is never stored.

import { fetchPlatformImport, fetchPlatformImports } from "./backend";
import { post } from "./imports";
import { refresh } from "./store";

export type Platform = "coolify" | "dokploy" | "railway" | "vercel";

export type PlanPort = {
  name: string;
  containerPort: number;
  protocol: "HTTP" | "TCP" | "UDP";
  public: boolean;
  publicPort?: number | null;
};

export type PlanService = {
  name: string;
  composeName: string;
  source: {
    type: "git" | "image" | "";
    image: string;
    reference: string;
    repoUrl: string;
    branch: string;
    buildContext: string;
  };
  ports: PlanPort[];
  /** Names only: a preview has no values. */
  env: { key: string; secret: boolean }[];
  volumes: { name: string; mountPath: string; sizeGi: number; fromBind?: boolean }[];
};

export type ImportDomain = { service: string; hostname: string; portName: string };

export type ImportService = {
  key: string;
  sourceName: string;
  builder: string;
  service: PlanService;
  domains: ImportDomain[];
  warnings: string[];
  importable: boolean;
  existingId?: string | null;
  /** Databases of the import its variables point at: connected before its first deploy. */
  connects: ImportConnect[];
  /** Why it isn't deployed. */
  holds: string[];
};

/** `env`: the service's variables Pyo replaces, each with the database's variable it becomes. */
export type ImportConnect = { database: string; env: Record<string, string> };

export type ImportStack = {
  key: string;
  sourceName: string;
  plan: { project: { name: string }; services: PlanService[] };
  domains: ImportDomain[];
  warnings: string[];
  importable: boolean;
  existingProjectId?: string | null;
};

export type ImportDatabase = {
  key: string;
  sourceName: string;
  name: string;
  engine: string;
  image: string;
  target?: "postgres" | "mysql" | "valkey" | null;
  action: "import" | "create" | "manual";
  warnings: string[];
  existingId?: string | null;
};

export type ImportProject = {
  key: string;
  name: string;
  services: ImportService[];
  stacks: ImportStack[];
  databases: ImportDatabase[];
  existingProjectId?: string | null;
};

export type ImportPlan = {
  platform: Platform;
  host: string;
  projects: ImportProject[];
  warnings: string[];
};

export type ImportResult = {
  key: string;
  name: string;
  state: "created" | "importing" | "exists" | "held" | "skipped" | "failed";
  message: string;
  id?: string | null;
  projectId?: string | null;
  /** A service: the databases Pyo connected it to, with the variables each set. */
  connected?: { database: string; databaseId: string; variables: string[] }[];
};

/** `url` is empty for Railway and Vercel (read from their own API). `team` is Vercel's. */
export type Source = { teamId: string; platform: Platform; url: string; token: string; team?: string };

export const previewImport = (src: Source) => post<ImportPlan>("/api/v1/platform-imports/preview", src);

/** One thing an import makes. */
export type ImportItem = { key: string; name: string; kind: "service" | "stack" | "database" };

/** An import as it runs on the server, and once it's done. */
export type ImportRun = {
  id: string;
  teamId: string;
  platform: Platform;
  status: "reading" | "importing" | "done" | "failed" | "interrupted";
  deploy: boolean;
  items: ImportItem[];
  results: ImportResult[];
  error: string;
  createdBy: string;
  createdAt: number;
  finishedAt?: number | null;
};

/** Whether it's still going. */
export const running = (r: ImportRun) => r.status === "reading" || r.status === "importing";

/** Starts an import. It runs on the server whatever happens to this page. */
export const startImport = (src: Source & { clusterId?: string; items: string[]; deploy: boolean }) =>
  post<ImportRun>("/api/v1/platform-imports", src);

export async function getImport(id: string): Promise<ImportRun> {
  const r = await fetchPlatformImport<ImportRun>(id);
  if (!running(r)) await refresh();
  return r;
}

/** The team's imports, newest first. */
export const listImports = async (teamId: string) => (await fetchPlatformImports<ImportRun>(teamId)).imports;

/** Whether a thing can be picked: importable and not imported before. */
export const pickable = (x: ImportService | ImportStack | ImportDatabase) =>
  "importable" in x
    ? x.importable && !("existingId" in x && x.existingId) && !("existingProjectId" in x && x.existingProjectId)
    : x.action !== "manual" && !x.existingId;

/** What Pyo does with a service's databases given the picks: the ones it connects, and why it holds the deploy. */
export function serviceLinks(plan: ImportPlan, svc: ImportService, picks: Set<string>) {
  const dbs = plan.projects.flatMap((p) => p.databases);
  const connects: { vars: string[]; database: string }[] = [];
  const holds = [...(svc.holds ?? [])];
  for (const c of svc.connects ?? []) {
    const db = dbs.find((d) => d.key === c.database);
    const vars = Object.keys(c.env);
    if (db && (db.existingId || picks.has(db.key))) connects.push({ vars, database: db.name });
    else
      holds.push(
        `${vars.join(", ")} ${vars.length === 1 ? "points" : "point"} at ${db?.sourceName ?? "a database"}, which this import doesn't bring.`,
      );
  }
  return { connects, holds };
}

/** Every key picked by default. */
export function defaultPicks(plan: ImportPlan): Set<string> {
  const out = new Set<string>();
  for (const p of plan.projects)
    for (const x of [...p.services, ...p.stacks, ...p.databases]) if (pickable(x)) out.add(x.key);
  return out;
}
