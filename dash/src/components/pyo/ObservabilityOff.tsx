import { ChartLineIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { EmptyState } from "~/components/ui/Panel";
import { api, usePyo } from "~/lib/pyo/store";

// History views while observability (installation-wide) is off: what's live
// keeps working, the history isn't kept. The admin can turn it on here.

/** Whether observability is on: history views fetch only then. */
export function useObservabilityOn() {
  return usePyo().observabilitySettings.enabled;
}

export function ObservabilityOff({ compact }: { compact?: boolean }) {
  const admin = !!usePyo().user.isAdmin;
  const [busy, setBusy] = useState(false);
  return (
    <EmptyState
      compact={compact}
      icon={ChartLineIcon}
      title="Observability is off."
      action={
        admin ? (
          <Button
            size="sm"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              await api.setObservability(true);
              setBusy(false);
            }}
          >
            Turn on
          </Button>
        ) : undefined
      }
    />
  );
}
