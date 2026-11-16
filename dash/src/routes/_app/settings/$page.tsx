import { createFileRoute, redirect } from "@tanstack/react-router";
import { movedSettingsHref, SETTINGS_PAGES } from "~/lib/pyo/paths";

// The settings pages moved to the root (/settings/email is now /email).
// Old links land on the same page with their query kept. Unknown pages open
// Settings. /settings/github has its own route.
export const Route = createFileRoute("/_app/settings/$page")({
  beforeLoad: ({ params, location }) => {
    const page = SETTINGS_PAGES.find((p) => p === `/${params.page}`);
    throw redirect({
      href: movedSettingsHref(page ?? SETTINGS_PAGES[0], location),
      replace: true,
    });
  },
});
