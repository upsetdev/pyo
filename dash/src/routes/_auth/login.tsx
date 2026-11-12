import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";
import { AuthForm } from "~/components/auth/AuthForm";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_auth/login")({
  validateSearch: z.object({ next: z.string().optional() }),
  head: () => ({ meta: seo({ title: "Log in" }) }),
  component: function Page() {
    const { next } = Route.useSearch();
    return <AuthForm intent="login" next={next} />;
  },
});
