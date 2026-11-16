import { createFileRoute, redirect } from "@tanstack/react-router";
import { z } from "zod";
import { currentTeam, getState } from "~/lib/pyo/store";
import { flagParam } from "~/lib/validation";

// Bare / (bookmarks, sign-in) opens the last-used team; ?onboarding is sign-up's
// one trip to its onboarding page. ?github= is GitHub sending someone back
// when the API can't tell which team: it opens that team's GitHub card.
export const Route = createFileRoute("/_app/")({
  validateSearch: z.object({
    onboarding: flagParam,
    github: z.string().optional().catch(undefined),
  }),
  beforeLoad: ({ search }) => {
    const team = currentTeam(getState());
    if (team && search.github)
      throw redirect({
        to: "/$team/integrations",
        params: { team: team.slug },
        search: { github: search.github },
        hash: "github",
        replace: true,
      });
    if (team)
      throw redirect({
        to: search.onboarding ? "/$team/onboarding" : "/$team",
        params: { team: team.slug },
        replace: true,
      });
    throw redirect({ to: "/profile", replace: true });
  },
});
