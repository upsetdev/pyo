// IMAGES.md is what scripts/images-md.ts writes from the repo's lists.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { render, thirdParty } from "./images-md";

const root = join(import.meta.dir, "..");
const read = (f: string) => readFileSync(join(root, f), "utf8");

test("IMAGES.md is up to date", () => {
  expect(read("IMAGES.md")).toBe(render(JSON.parse(read("deploy/pyo-images.json")), read("deploy/mirrors.txt")));
});

test("each third-party image takes the comment above its group, without file names", () => {
  const list = [
    "# The header, then a blank line.",
    "",
    "# Builds (platform.yaml, pyo-agent builder.rs).",
    `docker.io/moby/buildkit:v1@sha256:${"a".repeat(64)}`,
    "# Client tools (pyo-proto).",
    `docker.io/library/mysql:8.4@sha256:${"b".repeat(64)}`,
    `docker.io/library/mongo:8.0@sha256:${"c".repeat(64)}`,
  ].join("\n");
  expect(thirdParty(list).map((t) => [t.source, t.version, t.usedFor])).toEqual([
    ["docker.io/moby/buildkit", "v1", "Builds"],
    ["docker.io/library/mysql", "8.4", "Client tools"],
    ["docker.io/library/mongo", "8.0", "Client tools"],
  ]);
  expect(() => thirdParty("registry:3.1.2")).toThrow();
});

test("every image is in it, with its links", () => {
  const md = read("IMAGES.md");
  for (const i of JSON.parse(read("deploy/pyo-images.json")).images) {
    expect(md).toContain(`\`upsetdev/${i.name}\``);
    if (!i.builtBy) {
      expect(md).toContain(`https://github.com/orgs/upsetdev/packages/container/package/${i.name})`);
      expect(md).toContain(`https://hub.docker.com/r/upsetdev/${i.name})`);
    }
  }
  for (const t of thirdParty(read("deploy/mirrors.txt"))) expect(md).toContain(`\`${t.source}\``);
  expect(md).toContain("https://hub.docker.com/_/registry)");
});
