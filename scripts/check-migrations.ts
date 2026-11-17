// Migrations run while the previous release is still serving, and that release
// has to keep working on the new schema: it's what makes rolling updates
// seamless and rollbacks possible. So a change that would break it (removing
// or renaming something, tightening a column) has to wait for a later release
// than the one that stopped using it.
//
//   bun run scripts/check-migrations.ts --base <git ref>
//
// A migration that really does remove something declares which released
// version stopped depending on it, on a line of its own:
//
//   -- pyo:contract since=v0.1.1 nothing reads users.nickname since v0.1.1
//
// The tag has to exist: the release has to have shipped first.
//
// A migration that can't carry that line any more, because an installation
// already ran it and sqlx checksums the file, gets the same contract in
// scripts/migration-contracts.json instead, keyed by its file name, with the
// file's SHA-256 so it covers only the SQL that was reviewed:
//
//   { "0018_x.sql": { "since": "v0.4.2", "reason": "...", "sha256": "..." } }
//
// The line in the migration stays the way for new ones.
//
// Until the first release is tagged no installation depends on the
// migrations, so they may still be edited, merged or removed.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";

const DIR = "crates/pyo-api/migrations";
const CONTRACTS = "scripts/migration-contracts.json";

/** What would break the previous release, and why. */
const RULES: [RegExp, string][] = [
  [/\bDROP\s+(TABLE|SCHEMA|TYPE|VIEW|FUNCTION)\b/, "drops something the previous release may use"],
  [/\bDROP\s+COLUMN\b/, "drops a column the previous release may read"],
  [/\bRENAME\b/, "renames something the previous release uses by its old name"],
  [/\bALTER\s+(COLUMN\s+)?\S+\s+(SET\s+DATA\s+)?TYPE\b/, "changes a column's type"],
  [/\bSET\s+NOT\s+NULL\b/, "makes a column required, which the previous release's writes may not satisfy"],
  [/\bTRUNCATE\b/, "deletes every row of a table"],
  [/\bCREATE\s+UNIQUE\s+INDEX\b/, "adds a uniqueness rule the previous release's writes may break"],
  [/\bADD\s+(CONSTRAINT\s+\S+\s+)?(UNIQUE|CHECK|FOREIGN\s+KEY)\b/, "adds a constraint the previous release's writes may break"],
];

/** Comments and string contents can't trigger a rule. */
function code(sql: string) {
  return sql
    .replace(/--.*$/gm, " ")
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .replace(/'(?:[^']|'')*'/g, "''");
}

/** The statements of a migration, in upper case with spaces normalized. */
function statements(sql: string) {
  return code(sql)
    .split(";")
    .map((s) => s.replace(/\s+/g, " ").trim().toUpperCase())
    .filter(Boolean);
}

/** A new required column: old code doesn't know to fill it in. */
function addsRequiredColumn(statement: string) {
  return statement
    .split(/,(?![^(]*\))/)
    .some(
      (part) =>
        /\bADD\s+(COLUMN\s+)?/.test(part) &&
        !/\bADD\s+(CONSTRAINT|PRIMARY|UNIQUE|CHECK|FOREIGN)\b/.test(part) &&
        /\bNOT\s+NULL\b/.test(part) &&
        !/\bDEFAULT\b/.test(part),
    );
}

export type Contract = { since: string; reason: string };

/** A contract kept outside its migration, for the SQL whose SHA-256 it names. */
export type ListedContract = Contract & { sha256: string };

const sha256 = (sql: string) => createHash("sha256").update(sql).digest("hex");

/** What's wrong with scripts/migration-contracts.json's entries. */
export function listProblems(list: Record<string, ListedContract>, files: string[]) {
  const problems: string[] = [];
  for (const [file, c] of Object.entries(list)) {
    if (!files.includes(file)) problems.push(`${CONTRACTS}: ${file} isn't a migration.`);
    if (!/^v\S+$/.test(c?.since ?? "") || !c?.reason?.trim() || !/^[0-9a-f]{64}$/.test(c?.sha256 ?? ""))
      problems.push(`${CONTRACTS}: ${file} needs "since" (a version tag), a "reason" and the file's "sha256".`);
  }
  return problems;
}

export function contractOf(sql: string): Contract | null {
  const m = sql.match(/^[ \t]*--[ \t]*pyo:contract[ \t]+since=(v\S+)[ \t]+(\S.*)$/m);
  return m ? { since: m[1], reason: m[2].trim() } : null;
}

/** Why this SQL would break the release before it; empty when it wouldn't. */
export function unsafeChanges(sql: string) {
  const found = new Set<string>();
  for (const s of statements(sql)) {
    for (const [rule, why] of RULES) if (rule.test(s)) found.add(why);
    if (/^ALTER\s+TABLE\b/.test(s) && addsRequiredColumn(s))
      found.add("adds a required column with no default, which the previous release can't fill in");
  }
  return [...found];
}

/** A migration file's change. */
export type Change = { status: string; file: string; sql: string };

export type Repo = {
  /** The migrations on the base branch. */
  existing: string[];
  tags: string[];
  /** scripts/migration-contracts.json, by file name. */
  listed?: Record<string, ListedContract>;
};

/** The problems with a set of migration changes. */
export function check(changes: Change[], { existing, tags, listed = {} }: Repo) {
  const problems: string[] = [];
  const number = (f: string) => Number(f.split("_")[0]);
  // Nothing released yet: no installation has run them.
  const released = tags.length > 0;
  const gone = new Set(released ? [] : changes.filter((c) => c.status === "D").map((c) => c.file));
  const kept = existing.filter((f) => !gone.has(f));
  const highest = Math.max(0, ...kept.map(number));
  const seen = new Set(kept.map(number));

  for (const c of changes) {
    const { status, file, sql } = c;
    if (status !== "A") {
      if (!released) continue;
      problems.push(`${file}: already-applied migrations can't be changed or removed; sqlx checksums them, and Pyo wouldn't start. Add a new one.`);
      continue;
    }
    if (!/^\d{4}_[a-z0-9_]+\.sql$/.test(file)) {
      problems.push(`${file}: name it like 0034_what_it_does.sql.`);
      continue;
    }
    if (number(file) <= highest) problems.push(`${file}: number it above ${String(highest).padStart(4, "0")}, the latest on the base branch.`);
    if (seen.has(number(file))) problems.push(`${file}: another migration has this number.`);
    seen.add(number(file));

    const unsafe = unsafeChanges(sql);
    if (!unsafe.length) continue;
    const entry = Object.hasOwn(listed, file) ? listed[file] : undefined;
    const changed = !!entry && entry.sha256 !== sha256(sql);
    const contract = contractOf(sql) ?? (entry && !changed ? entry : null);
    if (contract && tags.includes(contract.since)) continue;
    const fix = contract
      ? `${contract.since} isn't a released tag, so the code that stopped depending on this hasn't shipped.`
      : changed
        ? `It changed since its entry in ${CONTRACTS} was reviewed (its sha256 differs). Review it again.`
        : "Ship the code that stops depending on this first, then add `-- pyo:contract since=<that tag> <why it's safe>`.";
    for (const why of unsafe) problems.push(`${file}: ${why}. ${fix}`);
  }
  return problems;
}

function git(...args: string[]) {
  return execFileSync("git", args, { encoding: "utf8" }).trim();
}

function lines(s: string) {
  return s.split("\n").filter(Boolean);
}

function main() {
  const at = process.argv.indexOf("--base");
  const base = at > 0 ? process.argv[at + 1] : "";
  if (!base) {
    console.error("usage: bun run scripts/check-migrations.ts --base <git ref>");
    process.exit(2);
  }
  const name = (path: string) => path.slice(DIR.length + 1);
  const tracked = lines(git("diff", "--no-renames", "--name-status", base, "--", DIR)).map((l) => {
    const [status, path] = l.split("\t");
    return { status: status[0], path };
  });
  const untracked = lines(git("ls-files", "--others", "--exclude-standard", "--", DIR)).map((path) => ({ status: "A", path }));
  const changes = [...tracked, ...untracked].map(({ status, path }) => ({
    status,
    file: name(path),
    sql: status === "D" ? "" : readFileSync(path, "utf8"),
  }));
  const existing = lines(git("ls-tree", "--name-only", base, `${DIR}/`)).map(name);
  const listed: Record<string, ListedContract> = existsSync(CONTRACTS) ? JSON.parse(readFileSync(CONTRACTS, "utf8")) : {};
  const files = lines(git("ls-files", "--cached", "--others", "--exclude-standard", "--", DIR)).map(name);
  const problems = [
    ...listProblems(listed, files),
    ...check(changes, { existing, tags: lines(git("tag", "--list", "v*")), listed }),
  ];
  for (const c of changes) {
    const inFile = contractOf(c.sql);
    const contract = inFile ?? (Object.hasOwn(listed, c.file) ? listed[c.file] : null);
    if (contract) console.log(`${c.file}: contract since ${contract.since}${inFile ? "" : ` (${CONTRACTS})`}: ${contract.reason}`);
  }
  if (problems.length) {
    console.error(problems.map((p) => `✗ ${p}`).join("\n"));
    process.exit(1);
  }
  console.log(`${changes.length} migration change(s) keep the previous release working.`);
}

if (import.meta.main) main();
