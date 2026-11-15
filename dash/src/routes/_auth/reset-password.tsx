import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";
import { ResetForm } from "~/components/auth/RecoveryForms";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_auth/reset-password")({
  validateSearch: z.object({ token: z.string().optional().catch(undefined) }),
  head: () => ({ meta: seo({ title: "Choose a new password" }) }),
  component: function Page() {
    const { token } = Route.useSearch();
    return <ResetForm token={token ?? ""} />;
  },
});
