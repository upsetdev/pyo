// The Gateway section's form: what it saves, the words it checks with
// (the API's, gateway_settings.rs), and the confirm before a save that
// drops connections.
// bun test dash/scripts/gateway-settings.test.ts
import { expect, test } from "bun:test";
import {
  GATEWAY_DEFAULTS,
  IDLE_RANGE,
  IDLE_SHORT,
  REQUEST_RANGE,
  agentBlock,
  changes,
  checkForm,
  dropLines,
  formOf,
} from "../src/lib/pyo/gateway";
import type { Cluster } from "../src/lib/pyo/types";

const form = formOf(GATEWAY_DEFAULTS);

test("the form shows the saved settings, the idle timeout in minutes", () => {
  expect(form).toEqual({ http3: false, compression: false, tlsMin: "1.2", requestTimeout: "300", idleMinutes: "60" });
  expect(checkForm(form)).toEqual({ settings: GATEWAY_DEFAULTS });
  expect(changes(GATEWAY_DEFAULTS, GATEWAY_DEFAULTS)).toEqual({});
});

test("only what changed is sent", () => {
  const r = checkForm({ ...form, compression: true, idleMinutes: "120" });
  if (!("settings" in r)) throw new Error(r.error);
  expect(changes(GATEWAY_DEFAULTS, r.settings)).toEqual({ compression: true, idleTimeout: 7200 });
});

test("the timeouts are checked in the API's words", () => {
  const err = (f: Partial<typeof form>) => {
    const r = checkForm({ ...form, ...f });
    return "error" in r ? r.error : null;
  };
  expect(err({ requestTimeout: "3601" })).toBe(REQUEST_RANGE);
  expect(err({ requestTimeout: "-1" })).toBe(REQUEST_RANGE);
  expect(err({ requestTimeout: "1.5" })).toBe(REQUEST_RANGE);
  expect(err({ requestTimeout: "" })).toBe(REQUEST_RANGE);
  expect(err({ idleMinutes: "0" })).toBe(IDLE_RANGE);
  expect(err({ idleMinutes: "1441" })).toBe(IDLE_RANGE);
  expect(err({ requestTimeout: "600", idleMinutes: "5" })).toBe(IDLE_SHORT);
  // No limit has nothing to compare.
  expect(err({ requestTimeout: "0", idleMinutes: "1" })).toBeNull();
  expect(err({ requestTimeout: " 60 " })).toBeNull();
});

test("a save that drops connections says so first, and only those", () => {
  expect(dropLines("eu", GATEWAY_DEFAULTS, { ...GATEWAY_DEFAULTS, compression: true, requestTimeout: 60 })).toEqual([]);
  expect(dropLines("eu", GATEWAY_DEFAULTS, { ...GATEWAY_DEFAULTS, tlsMin: "1.3" })).toEqual([
    "Open HTTPS connections to every website on eu drop once.",
    "Visitors reconnect. WebSockets and uploads in progress are cut.",
  ]);
  const on = dropLines("eu", GATEWAY_DEFAULTS, { ...GATEWAY_DEFAULTS, http3: true });
  expect(on[0]).toBe("Open connections drop once, one server at a time.");
  expect(on.at(-1)).toBe("Open UDP port 443 to every server in your provider's firewall.");
  const off = dropLines("eu", { ...GATEWAY_DEFAULTS, http3: true }, GATEWAY_DEFAULTS);
  expect(off).not.toContain("Open UDP port 443 to every server in your provider's firewall.");
  for (const l of [...on, ...off]) expect(l).not.toMatch(/;| - /);
});

test("the agent and its install come first", () => {
  const c = (caps: Partial<Cluster["capabilities"]>) => ({ capabilities: caps }) as Cluster;
  expect(agentBlock(c({}))).toBe("Update this cluster's agent first.");
  expect(agentBlock(c({ gatewaySettings: true }))).toBeNull();
  expect(agentBlock(c({ gatewaySettings: true, agentInstall: { state: "current" } }))).toBeNull();
  expect(agentBlock(c({ gatewaySettings: true, agentInstall: { state: "outdated" } }))).toBe(
    "Run the command under Refresh this cluster first.",
  );
  expect(agentBlock(c({ gatewaySettings: true, agentInstall: { state: "updating" } }))).toBe(
    "This cluster is updating. Try again in a few minutes.",
  );
});
