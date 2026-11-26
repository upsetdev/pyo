// A team picks its IP address and query string settings up to the
// installation's limit. Options above it show as not available.
// bun test dash/scripts/team-observability.test.ts
import { expect, test } from "bun:test";
import { withinLimit } from "../src/lib/pyo/teamObservability";

test("IP addresses up to the limit", () => {
  expect(withinLimit("none", "anonymized")).toBe(true);
  expect(withinLimit("anonymized", "anonymized")).toBe(true);
  expect(withinLimit("full", "anonymized")).toBe(false);
  expect(withinLimit("full", "full")).toBe(true);
  expect(withinLimit("anonymized", "none")).toBe(false);
});

test("query strings up to the limit", () => {
  expect(withinLimit("none", "names")).toBe(true);
  expect(withinLimit("names", "names")).toBe(true);
  expect(withinLimit("keep", "names")).toBe(false);
  expect(withinLimit("keep", "keep")).toBe(true);
  expect(withinLimit("names", "none")).toBe(false);
});
