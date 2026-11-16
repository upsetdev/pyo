import { createFileRoute, redirect } from "@tanstack/react-router";

// /settings opens on Email.
export const Route = createFileRoute("/_app/settings/")({
  beforeLoad: () => {
    throw redirect({ to: "/settings/email", replace: true });
  },
});
