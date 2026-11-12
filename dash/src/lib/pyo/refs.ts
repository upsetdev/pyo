// Variable references between a project's services: `${{postgres.DATABASE_URL}}`
// in one service's variable is filled in, on the server, when it deploys
// (crates/pyo-api/src/refs.rs; keep the two in step). The browser only ever
// sees the reference, never the value.

import type { Application } from "./types";

export const REF = /\$\{\{\s*([a-z0-9-]+)\.([A-Za-z_][A-Za-z0-9_]*)\s*\}\}/g;

export const refString = (service: string, variable: string) => `\${{${service}.${variable}}}`;

/**
 * `${{self.VAR}}`, in a template's variables: the service itself, whatever
 * it's named. Becomes `${{<name>.VAR}}` when it's created.
 */
export const SELF = "self";
export const selfRefs = (value: string, name: string) =>
  value.replace(/\$\{\{\s*self\./g, `\${{${name}.`);

/** Whether a value uses the service's own domain (`${{self.PUBLIC_URL}}`). */
export const needsOwnDomain = (values: string[]) =>
  values.some((v) => refsIn(v).some((r) => r.service === SELF && r.variable.startsWith("PUBLIC_")));

export function refsIn(value: string | undefined): { service: string; variable: string }[] {
  return [...(value ?? "").matchAll(REF)].map((m) => ({ service: m[1], variable: m[2] }));
}

/** What another service needs to know of one to list what it offers. */
export type Provider = {
  name: string;
  templateId?: string;
  envKeys: string[];
  port?: number;
  domain?: string;
};

export type Offered = { name: string; hint: string; secret?: boolean };

const CONN: Record<string, { user?: string; password: string; database?: string; url: string; scheme: string }> = {
  postgresql: { user: "POSTGRES_USER", password: "POSTGRES_PASSWORD", database: "POSTGRES_DB", url: "DATABASE_URL", scheme: "postgresql" },
  mysql: { user: "MYSQL_USER", password: "MYSQL_PASSWORD", database: "MYSQL_DATABASE", url: "DATABASE_URL", scheme: "mysql" },
  mariadb: { user: "MARIADB_USER", password: "MARIADB_PASSWORD", database: "MARIADB_DATABASE", url: "DATABASE_URL", scheme: "mysql" },
  mongodb: { user: "MONGO_INITDB_ROOT_USERNAME", password: "MONGO_INITDB_ROOT_PASSWORD", url: "DATABASE_URL", scheme: "mongodb" },
  valkey: { password: "VALKEY_PASSWORD", url: "REDIS_URL", scheme: "redis" },
};

/** Everything a service offers, the most useful first. */
export function offered(p: Provider): Offered[] {
  const out: Offered[] = [];
  const c = p.templateId ? CONN[p.templateId] : undefined;
  if (c) {
    out.push({ name: c.url, hint: `${c.scheme}:// connection string, password included`, secret: true });
    if (c.user) out.push({ name: "USER", hint: "User name" });
    out.push({ name: "PASSWORD", hint: "Password", secret: true });
    if (c.database) out.push({ name: "DATABASE", hint: "Database name" });
  }
  out.push({ name: "HOST", hint: "Address inside the cluster" });
  if (p.port) {
    out.push({ name: "PORT", hint: String(p.port) });
    out.push({ name: "URL", hint: "http:// address inside the cluster" });
  }
  if (p.domain) {
    out.push({ name: "PUBLIC_URL", hint: `https://${p.domain}` });
    out.push({ name: "PUBLIC_DOMAIN", hint: p.domain });
  }
  const taken = new Set(out.map((o) => o.name));
  for (const k of p.envKeys) if (!taken.has(k)) out.push({ name: k, hint: "Its own variable" });
  return out;
}

/** The env name to suggest: DATABASE_URL as is, HOST as POSTGRES_HOST. */
export function suggestKey(provider: string, variable: string): string {
  if (/_URL$/.test(variable) && variable !== "PUBLIC_URL") return variable;
  const stem = provider.toUpperCase().replace(/[^A-Z0-9]+/g, "_").replace(/^_+|_+$/g, "") || "SERVICE";
  return variable.startsWith(`${stem}_`) ? variable : `${stem}_${variable}`;
}

/** Why a value's references can't be filled in, or undefined. */
export function refProblem(value: string | undefined, providers: Provider[]): string | undefined {
  for (const r of refsIn(value)) {
    const p = providers.find((x) => x.name === r.service);
    if (!p) return `There's no service called ${r.service} in this project.`;
    if (!offered(p).some((o) => o.name === r.variable)) return `${r.service} has no variable ${r.variable}.`;
  }
  return undefined;
}

export function providerOfService(a: Application, env: { key: string }[] = a.env): Provider {
  return {
    name: a.name,
    templateId: a.source.type === "template" ? a.source.templateId : undefined,
    envKeys: env.map((e) => e.key).filter(Boolean),
    port: a.ports[0]?.containerPort,
    domain: a.domains[0]?.hostname,
  };
}
