import { createFileRoute } from "@tanstack/react-router";
import { ForgotForm } from "~/components/auth/RecoveryForms";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_auth/forgot-password")({
  head: () => ({ meta: seo({ title: "Forgot password" }) }),
  component: ForgotForm,
});
