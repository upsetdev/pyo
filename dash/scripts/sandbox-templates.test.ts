// Pyo's built-in sandbox templates in the dashboard: how its image reads,
// that new sandboxes start on it, and the servers a new sandbox can go to.
// bun test scripts/sandbox-templates.test.ts
import { expect, test } from "bun:test";
import {
  defaultTemplate,
  desktopUrl,
  fieldsSwapped,
  freeLabel,
  gettingReady,
  imageRepo,
  minSize,
  pyosFirst,
  serverRefusal,
  templateImage,
  templateStatus,
  usable,
} from "../src/lib/pyo/sandboxTemplates";

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

test("a server is offered with its room, or why it can't take the sandbox", () => {
  const GB = 1024 ** 3;
  const ok = {
    state: "ready",
    enabled: true,
    capability: { runtime: "firecracker", network: true },
    capacity: { memoryAvailable: 7.2 * GB, memoryReserved: GB },
  };
  expect(serverRefusal(ok, 2048, "internet")).toBe("");
  expect(freeLabel(6.2 * GB)).toBe("6.2 GB free");
  expect(freeLabel(512 * 1024 ** 2)).toBe("512 MB free");
  expect(serverRefusal(ok, 8192, "internet")).toBe("No room");
  expect(serverRefusal({ ...ok, state: "joining" }, 512, "none")).toBe("Setting up");
  expect(serverRefusal({ ...ok, state: "unreachable" }, 512, "none")).toBe("Not reachable");
  expect(serverRefusal({ ...ok, enabled: false }, 512, "none")).toBe("Turned off");
  const offline = { ...ok, capability: { runtime: "gvisor", network: false } };
  expect(serverRefusal(offline, 512, "internet")).toBe("No networking");
  expect(serverRefusal(offline, 512, "none")).toBe("");
});

const desktop = {
  name: "desktop",
  state: "ready",
  builtin: true,
  source: { image: "ghcr.io/upsetdev/pyo/sandbox-desktop:0.4.2@sha256:ab", os: "Debian 13" },
};

test("desktop comes right after base, made on first use too", () => {
  const node = { name: "alpine", state: "ready" };
  expect(pyosFirst([node, desktop, base]).map((t) => t.name)).toEqual(["base", "desktop", "alpine"]);
  expect(defaultTemplate([desktop, base])).toBe("base");
  expect(usable({ ...desktop, state: "pending" })).toBe(true);
  expect(usable({ ...desktop, state: "unavailable" })).toBe(false);
  expect(templateStatus({ ...desktop, state: "pending" })).toBe("Ready to use");
  // One named desktop isn't Pyo's.
  expect(usable({ name: "desktop", state: "building" })).toBe(false);
  expect(templateImage(desktop)).toBe("Debian 13 · ghcr.io/upsetdev/pyo/sandbox-desktop");
});

test("the desktop needs room, and opens through the daemon", () => {
  expect(minSize(desktop)).toEqual({ cpus: 2, memoryMib: 2048 });
  expect(minSize(base)).toBe(null);
  expect(minSize(undefined)).toBe(null);
  const url = new URL(desktopUrl("http://100.64.0.2:7070/", "sbx_abc", "pyosbx1.x.y"));
  expect(url.origin + url.pathname).toBe("http://100.64.0.2:7070/v1/sandboxes/sbx_abc/web/6080/vnc.html");
  expect(url.searchParams.get("pyo_token")).toBe("pyosbx1.x.y");
  expect(url.searchParams.get("autoconnect")).toBe("1");
  // noVNC finds its WebSocket beside the page.
  expect(url.searchParams.has("path")).toBe(false);
});
