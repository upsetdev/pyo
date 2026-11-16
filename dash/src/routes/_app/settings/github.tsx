import { createFileRoute, redirect } from "@tanstack/react-router";
import { z } from "zod";
import { currentTeam, getState } from "~/lib/pyo/store";

// The GitHub App lives in the GitHub card on Integrations. GitHub sends the
// admin back here after creating it (?github=created), so this opens that
// card with the result.
export const Route = createFileRoute("/_app/settings/github")({
  validateSearch: z.object({ github: z.string().optional().catch(undefined) }),
  beforeLoad: ({ search }) => {
    const team = currentTeam(getState());
    if (team)
      throw redirect({
        to: "/$team/integrations",
        params: { team: team.slug },
        search: { github: search.github },
        hash: "github",
        replace: true,
      });
    throw redirect({ to: "/profile", replace: true });
  },
});
