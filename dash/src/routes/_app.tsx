import { createFileRoute, Outlet, redirect } from "@tanstack/react-router";
import { ApiError } from "~/lib/pyo/backend";
import { getState, refresh, startSync } from "~/lib/pyo/store";

// Pathless layout for everything signed-in: team pages (/:team/...) and the
// person's own pages (/profile). The first snapshot loads before any page
// renders (the static page's skeleton shows meanwhile) and doubles as the
// sign-in check: a 401 sends the visitor to the login page. Then it stays
// fresh by polling.
export const Route = createFileRoute("/_app")({
  ssr: false,
  beforeLoad: async ({ location }) => {
    if (!getState().loaded) {
      try {
        await refresh();
      } catch (e) {
        if (e instanceof ApiError && e.status === 401)
          throw redirect({ to: "/login", search: { next: location.href } });
        // Still on the installer's printed sign-in.
        if (e instanceof ApiError && e.status === 428)
          throw redirect({ to: "/setup" });
        throw e;
      }
    }
    startSync();
  },
  component: Outlet,
});
