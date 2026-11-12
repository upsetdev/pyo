import type { Database } from "./types";

// The names of the objects a database makes in its team's namespace, as
// pyo-proto's database_names (keep the two the same): `#` is one part of
// digits, a last `*` one or more parts of anything.

const RESERVED = ["default", "pyo-isolation", "pyo-backup-*", "cnpg-default-monitoring"];

const CNPG_SUFFIXES = ["primary", "rw", "ro", "r", "app", "superuser", "ca", "server", "replication", "#", "#-wal"];
const CNPG_JOBS = ["initdb", "join", "pgbasebackup", "full-recovery", "snapshot-recovery", "import", "major-upgrade"];

function cnpgCluster(c: string): string[] {
  return [c, ...CNPG_SUFFIXES.map((s) => `${c}-${s}`), ...CNPG_JOBS.map((j) => `${c}-#-${j}`)];
}

function derived(engine: Database["engine"], n: string): string[] {
  const own = [n, `${n}-credentials`, `${n}-backup`];
  switch (engine) {
    case "postgres":
      return [
        ...own,
        ...cnpgCluster(n),
        ...cnpgCluster(`${n}-read`),
        ...["pooler", "read-pooler", "monitoring", "read-monitoring", "db", "backup-#", "backup-region-*"].map((s) => `${n}-${s}`),
      ];
    case "mysql":
      return [
        ...own,
        ...["#", "primary", "replica", "grpc", "mysql"].map((s) => `${n}-${s}`),
        `my-cnf-${n}`,
        `mysql-data-${n}-#`,
        `backup-${n}`,
        `backup-${n}-#`,
        `backup-${n}-first`,
        `restore-${n}`,
      ];
    case "valkey":
      return [
        ...own,
        ...["#", "headless", "primary", "replicas", "scripts", "sentinel", "sentinel-#", "read", "read-#", "rest", "kv"].map((s) => `${n}-${s}`),
        `data-${n}-#`,
        `data-${n}-read-#`,
      ];
    default:
      return own;
  }
}

const digits = (s: string) => /^[0-9]+$/.test(s);
const partMeets = (a: string, b: string) => a === b || (a === "#" && (b === "#" || digits(b))) || (b === "#" && digits(a));

function partsMeet(a: string[], b: string[]): boolean {
  if (a.length === 1 && a[0] === "*") return b.length > 0;
  if (b.length === 1 && b[0] === "*") return a.length > 0;
  if (!a.length || !b.length) return a.length === b.length;
  return partMeets(a[0], b[0]) && partsMeet(a.slice(1), b.slice(1));
}

const meet = (a: string[], b: string[]) => a.some((x) => b.some((y) => partsMeet(x.split("-"), y.split("-"))));

/** Why a new database can't have `name`: "" when it's free. */
export function nameConflict(engine: Database["engine"], name: string, others: Pick<Database, "engine" | "name">[]): string {
  const mine = derived(engine, name);
  if (meet(mine, RESERVED)) return "This name is reserved.";
  const other = others.find((o) => meet(mine, derived(o.engine, o.name)));
  return other ? `Conflicts with the database ${other.name}.` : "";
}
