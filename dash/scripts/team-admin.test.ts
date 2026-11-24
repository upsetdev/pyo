// Who changes a team's settings (General, Logs, Analytics, Agents): owners
// and admins. The API's updateTeam refuses everyone else. Developers and
// viewers see them read only, with no Save.
// Who deletes a team: its owners only. The API's deleteTeam refuses
// everyone else, so only owners see Delete team.
// bun test dash/scripts/team-admin.test.ts
import { expect, test } from "bun:test";
import {
  isTeamAdmin,
  isTeamOwner,
  roleIn,
  type PyoState,
} from "../src/lib/pyo/store";

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

test("only owners delete the team", () => {
  expect(isTeamOwner("owner")).toBe(true);
  expect(isTeamOwner("admin")).toBe(false);
  expect(isTeamOwner("developer")).toBe(false);
  expect(isTeamOwner("viewer")).toBe(false);
  expect(isTeamOwner(undefined)).toBe(false);
});

test("an owner of one team can't delete another team", () => {
  const s = {
    user: { email: "ada@example.com" },
    currentTeamId: "team_one",
    members: [
      { teamId: "team_one", email: "ada@example.com", role: "owner" },
      { teamId: "team_two", email: "ada@example.com", role: "admin" },
    ],
  } as unknown as PyoState;
  expect(isTeamOwner(roleIn(s))).toBe(true);
  expect(isTeamOwner(roleIn(s, "team_two"))).toBe(false);
  expect(isTeamOwner(roleIn(s, "team_three"))).toBe(false);
});
