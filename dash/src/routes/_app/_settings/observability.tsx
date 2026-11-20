import { createFileRoute } from "@tanstack/react-router";
import { ObservabilitySettingsSection } from "~/components/pyo/ObservabilitySettings";
import { SettingsSection } from "../_settings";

export const Route = createFileRoute("/_app/_settings/observability")({
  component: () => (
    <SettingsSection description="Analytics, logs and usage history: what they keep and for how long.">
      <ObservabilitySettingsSection />
    </SettingsSection>
  ),
});
