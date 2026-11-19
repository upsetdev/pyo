import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";
import { DeployWizard } from "~/components/pyo/wizard/DeployWizard";
import { parseDeployLink } from "~/lib/pyo/deploy-link";
import { seo } from "~/lib/seo";
import { usePyo } from "~/lib/pyo/store";
import { textParam } from "~/lib/validation";

export const Route = createFileRoute("/_app/$team/deploy/new")({
  validateSearch: z.object({
    source: z.enum(["template", "git", "image", "compose"]).optional().catch(undefined),
    template: z.string().optional(),
    project: z.string().optional(),
    /** Open Configure for this Git repository ("https://github.com/o/r", or ".../tree/<branch>"). */
    repo: textParam.catch(undefined),
    /** Open Configure for this image ("nginx:1.27", "ghcr.io/o/app:v2"). */
    image: textParam.catch(undefined),
    /** Create a cron job: starts with its schedule on. */
    cron: z.boolean().optional().catch(undefined),
    /** repo or image came from a "Deploy with Pyo" link (through /new). */
    link: z.boolean().optional().catch(undefined),
  }),
  head: () => ({ meta: seo({ title: "New deploy" }) }),
  component: NewDeployPage,
});

function NewDeployPage() {
  const { link: fromLink, ...search } = Route.useSearch();
  const s = usePyo();
  // A link is checked like anything else in the URL, and more strictly than
  // a pasted repository: a valid one fills the form in, an invalid one opens
  // the start screen with what's wrong. The team's own GitLab hosts work too.
  const gitlabs = s.gitConnections
    .filter((c) => c.teamId === s.currentTeamId)
    .map((c) => c.url);
  const link = fromLink ? parseDeployLink(search, gitlabs) : null;
  const none = { repo: undefined, image: undefined, template: undefined };
  const initial =
    link?.kind === "git"
      ? { ...search, ...none, source: "git" as const, repo: link.repo }
      : link?.kind === "image"
        ? { ...search, ...none, source: "image" as const, image: link.image }
        : link
          ? { ...search, ...none }
          : search;
  return (
    // Remount when the entry point changes (e.g. a template link while open).
    <DeployWizard
      key={`${initial.cron}:${initial.source}:${initial.template}:${initial.project}:${initial.repo}:${initial.image}:${JSON.stringify(link)}`}
      initial={initial}
      link={link}
    />
  );
}
