import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";
import { DeployWizard } from "~/components/pyo/wizard/DeployWizard";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/deploy/new")({
  validateSearch: z.object({
    source: z.enum(["template", "git", "image", "compose"]).optional().catch(undefined),
    template: z.string().optional(),
    project: z.string().optional(),
    /** Open Configure for this Git repository ("https://github.com/o/r", or ".../tree/<branch>"). */
    repo: z.string().optional(),
    /** Open Configure for this image ("nginx:1.27", "ghcr.io/o/app:v2"). */
    image: z.string().optional(),
    /** Create a cron job: starts with its schedule on. */
    cron: z.boolean().optional().catch(undefined),
  }),
  head: () => ({ meta: seo({ title: "New deploy" }) }),
  component: NewDeployPage,
});

function NewDeployPage() {
  const search = Route.useSearch();
  return (
    // Remount when the entry point changes (e.g. a template link while open).
    <DeployWizard
      key={`${search.cron}:${search.source}:${search.template}:${search.project}:${search.repo}:${search.image}`}
      initial={search}
    />
  );
}
