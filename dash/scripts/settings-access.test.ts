// Who opens the installation's settings: its admins only, self-hosted and
// on Pyo Cloud. Team owners and Cloud customers don't.
// bun test dash/scripts/settings-access.test.ts
import { expect, test } from "bun:test";
import { opensInstallationSettings } from "../src/lib/pyo/paths";

test("installation admins open the installation's settings", () => {
  expect(opensInstallationSettings({ isAdmin: true })).toBe(true);
});

test("everyone else doesn't", () => {
  expect(opensInstallationSettings({ isAdmin: false })).toBe(false);
  expect(opensInstallationSettings({})).toBe(false);
});
