// Pyo's own images have one list, deploy/pyo-images.json. The release
// workflow's matrix names them too: each built image's flat name and old
// nested name must be the list's.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "..");
const list = JSON.parse(readFileSync(join(root, "deploy/pyo-images.json"), "utf8"));
type Own = { name: string; placeholder?: string; keys: string[]; builtBy?: string };
const own: Own[] = list.images;

test("every name is flat, under ghcr.io/upsetdev and docker.io/upsetdev", () => {
  expect(list.registries).toEqual(["ghcr.io/upsetdev", "docker.io/upsetdev"]);
  expect(list.old.prefix).toBe("ghcr.io/upsetdev/pyo");
  expect(list.old.until).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  for (const i of own) {
    expect(i.name).toMatch(/^pyo(-[a-z]+(-[a-z]+)*)?$/);
    if (i.placeholder) expect(i.name).toBe(i.placeholder === "pyo" ? "pyo" : `pyo-${i.placeholder}`);
    expect(i.keys.length).toBeGreaterThan(0);
  }
  expect(new Set(own.map((i) => i.name)).size).toBe(own.length);
});

test("release.yml builds each image the list has, under its flat name", () => {
  const workflow = Bun.YAML.parse(readFileSync(join(root, ".github/workflows/release.yml"), "utf8")) as {
    jobs: { images: { strategy: { matrix: { include: { image: string; name: string; repo?: string }[] } } } };
  };
  const built = workflow.jobs.images.strategy.matrix.include;
  const fromWorkflow = new Map<string, Set<string>>();
  for (const b of built) {
    const old = b.repo ?? b.image;
    expect(b.name).toBe(old === "pyo" ? "pyo" : `pyo-${old}`);
    fromWorkflow.set(b.name, (fromWorkflow.get(b.name) ?? new Set()).add(old));
  }
  const fromList = own.filter((i) => !i.builtBy);
  expect([...fromWorkflow.keys()].sort()).toEqual(fromList.map((i) => i.name).sort());
  for (const i of fromList) expect([...fromWorkflow.get(i.name)!]).toEqual([i.placeholder!]);
  // One Postgres image per key.
  const postgres = own.find((i) => i.placeholder === "postgres")!;
  expect(built.filter((b) => b.name === "pyo-postgres").length).toBe(postgres.keys.length);
});

test("the sandbox templates' images are built elsewhere", () => {
  for (const i of own.filter((i) => i.builtBy)) {
    expect(i.builtBy).toBe("upsetdev/pyo-sandbox-images");
    expect(i.placeholder).toBeUndefined();
  }
});
