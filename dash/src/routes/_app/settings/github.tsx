import { createFileRoute } from "@tanstack/react-router";
import { GithubSettingsSection } from "~/components/pyo/GithubSettings";
import { ManagedOr } from "~/components/pyo/Managed";
import { SettingsSection } from "../settings";

export const Route = createFileRoute("/_app/settings/github")({
  component: () => (
    <SettingsSection description="Private repositories and deploys on push.">
      <ManagedOr
        title="GitHub is managed for you"
        description="Connect repositories when you deploy."
      >
        <GithubSettingsSection />
      </ManagedOr>
    </SettingsSection>
  ),
});
