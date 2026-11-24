// Who changes a team's settings (General, Deployment preferences, Logs,
// Analytics, Agents): owners and admins. The API's updateTeam refuses
// everyone else. Developers and viewers see them read only, with no Save.
// bun test dash/scripts/team-admin.test.ts
import { expect, test } from "bun:test";
import { isTeamAdmin, roleIn, type PyoState } from "../src/lib/pyo/store";

test("owners and admins change team settings", () => {
  expect(isTeamAdmin("owner")).toBe(true);
  expect(isTeamAdmin("admin")).toBe(true);
});

test("other members see them read only", () => {
  expect(isTeamAdmin("developer")).toBe(false);
  expect(isTeamAdmin("viewer")).toBe(false);
  expect(isTeamAdmin(undefined)).toBe(false);
});

test("an admin of one team sees another team's settings read only", () => {
  const s = {
    user: { email: "ada@example.com" },
    currentTeamId: "team_one",
    members: [
      { teamId: "team_one", email: "ada@example.com", role: "admin" },
      { teamId: "team_two", email: "ada@example.com", role: "developer" },
      { teamId: "team_two", email: "bob@example.com", role: "owner" },
    ],
  } as unknown as PyoState;
  expect(isTeamAdmin(roleIn(s))).toBe(true);
  expect(isTeamAdmin(roleIn(s, "team_two"))).toBe(false);
  expect(isTeamAdmin(roleIn(s, "team_three"))).toBe(false);
});
