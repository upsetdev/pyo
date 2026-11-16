// Pyo's built-in sandbox template in the dashboard: how its image reads,
// and that new sandboxes start on it.
// bun test scripts/sandbox-templates.test.ts
import { expect, test } from "bun:test";
import {
  defaultTemplate,
  gettingReady,
  imageRepo,
  templateImage,
  templateStatus,
  usable,
} from "../src/lib/pyo/sandboxTemplates";

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
  expect(templateImage({ ...base, source: { os: "Debian 13" } })).toBe("Debian 13");
  expect(templateImage({ source: { image: "node:22" } })).toBe("node:22");
  expect(templateImage({ source: null })).toBe("");
});

test("images lose their tag and digest, not a registry's port", () => {
  expect(imageRepo("ghcr.io/a/b:1@sha256:ab")).toBe("ghcr.io/a/b");
  expect(imageRepo("127.0.0.1:5005/pyo/x")).toBe("127.0.0.1:5005/pyo/x");
  expect(imageRepo("127.0.0.1:5005/pyo/x:dev")).toBe("127.0.0.1:5005/pyo/x");
});

test("new sandboxes start on base, which servers make on first use", () => {
  const node = { name: "node-22", state: "ready" };
  expect(defaultTemplate([node, base])).toBe("base");
  // Not on any server yet, or being made: still the default.
  expect(defaultTemplate([node, { ...base, state: "pending" }])).toBe("base");
  expect(defaultTemplate([{ ...base, state: "building" }])).toBe("base");
  // Pyo has no image for it yet.
  const none = { ...base, state: "unavailable" };
  expect(usable(none)).toBe(false);
  expect(defaultTemplate([node, none])).toBe("node-22");
  expect(defaultTemplate([none])).toBe("");
  // A team's template waits until it's built. One named base isn't Pyo's.
  expect(usable({ name: "node-22", state: "building" })).toBe(false);
  expect(defaultTemplate([node, { name: "base", state: "ready" }])).toBe("node-22");
});

test("base's status says it's ready to use", () => {
  expect(templateStatus(base)).toBe("Ready to use");
  expect(templateStatus({ ...base, state: "pending" })).toBe("Ready to use");
  expect(templateStatus({ ...base, state: "unavailable" })).toBe("Not available yet");
  expect(templateStatus({ name: "node-22", state: "pending" })).toBe("Pending");
  expect(gettingReady("base", "sandbox-1")).toBe("Getting base ready on sandbox-1…");
  expect(gettingReady("base", null)).toBe("Getting base ready…");
});
