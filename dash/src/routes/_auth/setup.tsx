import { createFileRoute } from "@tanstack/react-router";
import { SetupForm } from "~/components/auth/SetupForm";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_auth/setup")({
  head: () => ({ meta: seo({ title: "Set up" }) }),
  component: SetupForm,
});
