import { createFileRoute } from "@tanstack/react-router";
import { UpdatesSection } from "~/components/pyo/UpdateSettings";
import { ManagedOr } from "~/components/pyo/Managed";
import { SettingsSection } from "../_settings";

export const Route = createFileRoute("/_app/_settings/updates")({
  component: () => (
    <SettingsSection description="Version and updates.">
      <ManagedOr
        title="Updates are managed for you"
        description="Always up to date."
      >
        <UpdatesSection />
      </ManagedOr>
    </SettingsSection>
  ),
});
