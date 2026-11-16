import { createFileRoute, redirect } from "@tanstack/react-router";
import { SETTINGS_PAGES } from "~/lib/pyo/paths";

// /settings opens on the first settings page.
export const Route = createFileRoute("/_app/settings/")({
  beforeLoad: () => {
    throw redirect({ to: SETTINGS_PAGES[0], replace: true });
  },
});
