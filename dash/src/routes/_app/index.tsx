import { createFileRoute, redirect } from "@tanstack/react-router";
import { z } from "zod";
import { currentTeam, getState } from "~/lib/pyo/store";
import { flagParam } from "~/lib/validation";

// Bare / (bookmarks, sign-in) opens the last-used team; ?onboarding is sign-up's
// one trip to its onboarding page.
export const Route = createFileRoute("/_app/")({
  validateSearch: z.object({ onboarding: flagParam }),
  beforeLoad: ({ search }) => {
    const team = currentTeam(getState());
    if (team)
      throw redirect({
        to: search.onboarding ? "/$team/onboarding" : "/$team",
        params: { team: team.slug },
        replace: true,
      });
    throw redirect({ to: "/profile", replace: true });
  },
});
