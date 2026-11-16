import { createFileRoute, redirect } from "@tanstack/react-router";
import { currentTeam, getState } from "~/lib/pyo/store";

// Backups of Pyo's own data moved to the Pyo tab of Backups.
export const Route = createFileRoute("/_app/settings/backups")({
  beforeLoad: () => {
    const team = currentTeam(getState());
    if (team)
      throw redirect({
        to: "/$team/backups",
        params: { team: team.slug },
        search: { tab: "pyo" },
        replace: true,
      });
    throw redirect({ to: "/settings/email", replace: true });
  },
});
