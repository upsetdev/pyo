import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";
import { AuthForm } from "~/components/auth/AuthForm";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_auth/signup")({
  validateSearch: z.object({
    next: z.string().optional(),
    invite: z.string().optional(),
  }),
  head: () => ({ meta: seo({ title: "Create account" }) }),
  component: function Page() {
    // Invite links land here with the invite's secret.
    const { next, invite } = Route.useSearch();
    return <AuthForm intent="signup" next={next} invite={invite} />;
  },
});
