// Where sign-in may send someone next: only this site.
// bun test scripts/next-path.test.ts
import { expect, test } from "bun:test";
import { safeNextPath } from "../src/lib/validation";

test("paths on this site are kept", () => {
  expect(safeNextPath("/")).toBe("/");
  expect(safeNextPath("/acme/services?tab=logs#x")).toBe("/acme/services?tab=logs#x");
  expect(safeNextPath("/invite?invite=abc")).toBe("/invite?invite=abc");
  expect(safeNextPath("/a/../b")).toBe("/b");
});

test("anything that leaves the site falls back", () => {
  for (const bad of [
    "//evil.com",
    "/\\evil.com",
    "/\\/evil.com",
    "\\\\evil.com",
    "/\t/evil.com",
    "/\n/evil.com",
    "https://evil.com",
    "javascript:alert(1)",
    "evil.com",
    "",
    null,
    undefined,
  ])
    expect(safeNextPath(bad as string)).toBe("/");
  expect(safeNextPath("//evil.com", "/home")).toBe("/home");
});
