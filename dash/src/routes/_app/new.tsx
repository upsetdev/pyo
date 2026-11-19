import { createFileRoute, redirect } from "@tanstack/react-router";
import { currentTeam, getState } from "~/lib/pyo/store";
import { textParam } from "~/lib/validation";

const text = textParam.catch(undefined);

// "Deploy with Pyo" links land here: /new?repo=<https Git URL> or
// ?image=<image>. They can't know the team, so this opens the deploy page
// of the last-used one with the same query. Signed out, /_app sends the
// visitor to log in first and back here after. The deploy page checks the
// query: this only passes it on.
export const Route = createFileRoute("/_app/new")({
  validateSearch: (raw: Record<string, unknown>) => ({
    repo: text.parse(raw.repo),
    image: text.parse(raw.image),
    // Any query but tracking makes it a link, checked as one.
    link: Object.keys(raw).some((k) => !k.startsWith("utm_")) || undefined,
  }),
  beforeLoad: ({ search }) => {
    const team = currentTeam(getState());
    if (!team) throw redirect({ to: "/profile", replace: true });
    throw redirect({
      to: "/$team/deploy/new",
      params: { team: team.slug },
      search: search.link ? { repo: search.repo, image: search.image, link: true } : {},
      replace: true,
    });
  },
});
