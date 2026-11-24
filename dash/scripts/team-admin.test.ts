// Who changes a team's settings (Logs, Analytics): owners and admins.
// Developers and viewers see them read only, with no Save.
// bun test dash/scripts/team-admin.test.ts
import { expect, test } from "bun:test";
import { isTeamAdmin } from "../src/lib/pyo/store";

test("owners and admins change team settings", () => {
  expect(isTeamAdmin("owner")).toBe(true);
  expect(isTeamAdmin("admin")).toBe(true);
});

test("other members see them read only", () => {
  expect(isTeamAdmin("developer")).toBe(false);
  expect(isTeamAdmin("viewer")).toBe(false);
  expect(isTeamAdmin(undefined)).toBe(false);
});
