import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";
import { GithubSettingsSection } from "~/components/pyo/GithubSettings";
import { ManagedOr } from "~/components/pyo/Managed";
import { SettingsSection } from "../_settings";

export const Route = createFileRoute("/_app/_settings/github")({
  // ?github=: back from creating the app on GitHub.
  validateSearch: z.object({ github: z.string().optional().catch(undefined) }),
  component: () => (
    <SettingsSection description="The GitHub App for every team. Each team connects its own accounts in Integrations.">
      <ManagedOr
        forAdmin
        title="GitHub is managed for you"
        description="Connect repositories when you deploy."
      >
        <GithubSettingsSection />
      </ManagedOr>
    </SettingsSection>
  ),
});
