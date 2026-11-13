import { createFileRoute } from "@tanstack/react-router";
import { ObservabilitySettingsSection } from "~/components/pyo/ObservabilitySettings";
import { SettingsSection } from "../settings";

export const Route = createFileRoute("/_app/settings/observability")({
  component: () => (
    <SettingsSection description="Analytics, request logs and usage history, and what they keep about visitors.">
      <ObservabilitySettingsSection />
    </SettingsSection>
  ),
});
