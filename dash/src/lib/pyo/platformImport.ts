// Importing from another platform (crates/pyo-api/src/platform_import):
// preview what a Coolify or Dokploy installation becomes, then import the
// picked things. The token goes with each request and is never stored.

import { post } from "./imports";
import { refresh } from "./store";

export type Platform = "coolify" | "dokploy";

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
};

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
  state: "created" | "importing" | "exists" | "skipped" | "failed";
  message: string;
  id?: string | null;
  projectId?: string | null;
};

export type Source = { teamId: string; platform: Platform; url: string; token: string };

export const previewImport = (src: Source) => post<ImportPlan>("/api/v1/platform-imports/preview", src);

export async function startImport(
  src: Source & { clusterId?: string; items: string[]; deploy: boolean },
): Promise<ImportResult[]> {
  const r = await post<{ ok: true; results: ImportResult[] }>("/api/v1/platform-imports", src);
  await refresh();
  return r.results;
}

/** Whether a thing can be picked: importable and not imported before. */
export const pickable = (x: ImportService | ImportStack | ImportDatabase) =>
  "importable" in x
    ? x.importable && !("existingId" in x && x.existingId) && !("existingProjectId" in x && x.existingProjectId)
    : x.action !== "manual" && !x.existingId;

/** Every key picked by default. */
export function defaultPicks(plan: ImportPlan): Set<string> {
  const out = new Set<string>();
  for (const p of plan.projects)
    for (const x of [...p.services, ...p.stacks, ...p.databases]) if (pickable(x)) out.add(x.key);
  return out;
}
