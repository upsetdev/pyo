// Bundles the template catalog into the dashboard: pyo-templates' built
// index.json, with each logo's path and colour, as src/lib/pyo/catalog.json.
// The dashboard reads only that file, so templates work offline and on fresh
// installs, and pyo-templates stays their one source.
//
//   cd ../pyo-templates && bun run build
//   cd ../pyo/dash && bun scripts/sync-templates.ts       # write the snapshot
//   bun scripts/sync-templates.ts --check                 # exit 1 if it's stale
//   bun scripts/sync-templates.ts /path/to/dist/v1        # another build
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const dist = resolve(process.argv.slice(2).find((a) => !a.startsWith("--")) ?? join(root, "../../pyo-templates/dist/v1"));
const check = process.argv.includes("--check");
const out = join(root, "src/lib/pyo/catalog.json");

const indexFile = join(dist, "index.json");
if (!existsSync(indexFile)) {
  console.error(`No ${indexFile}: run \`bun run build\` in pyo-templates first.`);
  process.exit(1);
}

type Entry = { id: string; name: string; version?: string; logo: string | null; source?: { type?: string; versions?: unknown[] } };
const index = JSON.parse(readFileSync(indexFile, "utf8")) as { schema: number; templates: Entry[] };

// The same checks the dashboard makes, so a bad build fails here instead.
const problems: string[] = [];
if (index.schema !== 1) problems.push(`schema ${index.schema}: this Pyo reads schema 1`);
if (!Array.isArray(index.templates) || !index.templates.length) problems.push("no templates");
const ids = new Set<string>();
for (const t of index.templates ?? []) {
  if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(t.id ?? "")) problems.push(`bad id ${JSON.stringify(t.id)}`);
  if (ids.has(t.id)) problems.push(`${t.id}: repeated`);
  ids.add(t.id);
  if (t.source?.type !== "image" || !t.source.versions?.length) problems.push(`${t.id}: no image versions`);
  if (!/^\d+\.\d+\.\d+$/.test(t.version ?? "")) problems.push(`${t.id}: no template version`);
}

/** A logo is one path in one colour (pyo-templates checks it's plain). */
const logos: Record<string, { hex: string; path: string }> = {};
for (const t of index.templates ?? []) {
  if (!t.logo) continue;
  const svg = readFileSync(join(dist, t.logo), "utf8");
  const paths = svg.match(/<path\b/g)?.length ?? 0;
  const hex = /fill="#([0-9A-Fa-f]{6})"/.exec(svg)?.[1];
  const d = /\sd="([^"]+)"/.exec(svg)?.[1];
  if (paths !== 1 || !hex || !d) problems.push(`${t.id}: its logo isn't one path with one fill`);
  else logos[t.id] = { hex: hex.toUpperCase(), path: d };
}

if (problems.length) {
  for (const p of problems) console.error(`- ${p}`);
  process.exit(1);
}

const text = `${JSON.stringify({ ...index, logos }, null, 2)}\n`;
if (check) {
  const current = existsSync(out) ? readFileSync(out, "utf8") : "";
  if (current !== text) {
    console.error("src/lib/pyo/catalog.json is out of date: run bun scripts/sync-templates.ts");
    process.exit(1);
  }
  console.log(`catalog.json is up to date (${index.templates.length} templates)`);
} else {
  writeFileSync(out, text);
  console.log(`src/lib/pyo/catalog.json: ${index.templates.length} templates, ${Object.keys(logos).length} logos`);
}
