import { createFileRoute } from "@tanstack/react-router";
import { ControlBackupSection } from "~/components/pyo/ControlBackupSettings";
import { ManagedOr } from "~/components/pyo/Managed";
import { SettingsSection } from "../settings";

export const Route = createFileRoute("/_app/settings/backups")({
  component: () => (
    <SettingsSection description="Encrypted copies of Pyo's own data.">
      <ManagedOr
        title="Backups are managed for you"
        description="Pyo's own data. Service backups are set per service."
      >
        <ControlBackupSection />
      </ManagedOr>
    </SettingsSection>
  ),
});
