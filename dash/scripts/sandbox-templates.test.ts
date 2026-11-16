// Pyo's built-in sandbox template in the dashboard: how its image reads,
// and that new sandboxes start on it.
// bun test scripts/sandbox-templates.test.ts
import { expect, test } from "bun:test";
import { defaultTemplate, fieldsSwapped, imageRepo, templateImage } from "../src/lib/pyo/sandboxTemplates";

test("the new template form notices a swapped name and image", () => {
  expect(fieldsSwapped("frans/sandbox-base", "sandbox-base")).toBe(true);
  expect(fieldsSwapped("node:22", "node")).toBe(true);
  expect(fieldsSwapped("node-22", "node:22")).toBe(false);
  expect(fieldsSwapped("base", "frans/sandbox-base")).toBe(false);
  expect(fieldsSwapped("node:22", "")).toBe(false);
});

const base = {
  name: "base",
  state: "ready",
  builtin: true,
  source: {
    image: "ghcr.io/upsetdev/pyo/sandbox-base:0.4.1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    os: "Debian 13",
  },
};

test("base reads as what it runs and where it comes from", () => {
  expect(templateImage(base)).toBe("Debian 13 · ghcr.io/upsetdev/pyo/sandbox-base");
  expect(templateImage({ source: { image: "node:22" } })).toBe("node:22");
  expect(templateImage({ source: null })).toBe("");
});

test("images lose their tag and digest, not a registry's port", () => {
  expect(imageRepo("ghcr.io/a/b:1@sha256:ab")).toBe("ghcr.io/a/b");
  expect(imageRepo("127.0.0.1:5005/pyo/x")).toBe("127.0.0.1:5005/pyo/x");
  expect(imageRepo("127.0.0.1:5005/pyo/x:dev")).toBe("127.0.0.1:5005/pyo/x");
});

test("new sandboxes start on base once it's ready", () => {
  const node = { name: "node-22", state: "ready" };
  expect(defaultTemplate([node, base])).toBe("base");
  expect(defaultTemplate([node, { ...base, state: "building" }])).toBe("node-22");
  expect(defaultTemplate([{ ...base, state: "building" }])).toBe("");
  // A team's own template named base isn't Pyo's.
  expect(defaultTemplate([node, { name: "base", state: "ready" }])).toBe("node-22");
});
