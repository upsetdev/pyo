import { createFileRoute } from "@tanstack/react-router";
import { EmailSettingsSection } from "~/components/pyo/EmailSettings";
import { ManagedOr } from "~/components/pyo/Managed";
import { SettingsSection } from "../_settings";

export const Route = createFileRoute("/_app/_settings/email")({
  component: () => (
    <SettingsSection description="How Pyo sends invites and notifications.">
      <ManagedOr
        forAdmin
        title="Email is managed for you"
        description="Nothing to set up."
      >
        <EmailSettingsSection />
      </ManagedOr>
    </SettingsSection>
  ),
});
