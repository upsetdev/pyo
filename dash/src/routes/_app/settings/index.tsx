import { createFileRoute, redirect } from "@tanstack/react-router";

// /settings opens on Backups.
export const Route = createFileRoute("/_app/settings/")({
  beforeLoad: () => {
    throw redirect({ to: "/settings/backups", replace: true });
  },
});
