import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

// release-manifest.ts reads the manifests with Bun's YAML parser, stricter
// than Rust's and Python's: a manifest it can't read only fails the
// release workflow, after the tag.
test.each(["deploy/k8s/platform.yaml", "deploy/k8s/pyo.yaml"])("%s parses as the release reads it", (file) => {
  const docs = Bun.YAML.parse(readFileSync(new URL(`../${file}`, import.meta.url), "utf8"));
  const objects = (Array.isArray(docs) ? docs : [docs]).filter((d) => d && typeof d === "object");
  expect(objects.length).toBeGreaterThan(1);
  for (const o of objects) expect(o.kind && o.metadata?.name).toBeTruthy();
});
