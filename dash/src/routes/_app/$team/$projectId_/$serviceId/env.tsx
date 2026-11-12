import { createFileRoute } from "@tanstack/react-router";
import { serviceSettingsNav, SettingsFrame } from "~/components/pyo/SettingsFrame";
import { useState } from "react";
import { EnvEditor } from "~/components/pyo/editors";
import { SecretImports } from "~/components/pyo/SecretStores";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Panel } from "~/components/ui/Panel";
import { type DraftEnv } from "~/lib/pyo/draft";
import { ENV_KEY } from "~/lib/pyo/format";
import { api, usePyo, serviceIdOf } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application } from "~/lib/pyo/types";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/env")({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const app = usePyo().services.find((a) => a.id === serviceId);
    return app ? (
      <SettingsFrame items={serviceSettingsNav(app.id)}>
        <div className="space-y-4">
          <EnvTab key={app.id} app={app} />
          <SecretImports key={`imports-${app.id}`} app={app} />
        </div>
      </SettingsFrame>
    ) : null;
  },
});

function EnvTab({ app }: { app: Application }) {
  const [rows, setRows] = useState<DraftEnv[]>(() =>
    app.env.map(({ id, key, value, secret, hasValue }) => ({
      id,
      key,
      value,
      secret,
      hasValue,
    })),
  );
  const [confirming, setConfirming] = useState(false);
  const s = usePyo();
  const role = s.members.find(
    (m) => m.teamId === app.teamId && m.email === s.user.email,
  )?.role;
  const canReveal = role === "owner" || role === "admin";
  const [showErrors, setShowErrors] = useState(false);

  const errors: Record<string, string> = {};
  const seen = new Set<string>();
  for (const r of rows) {
    if (!r.key) errors[`env.${r.id}`] = "Key is required.";
    else if (!ENV_KEY.test(r.key))
      errors[`env.${r.id}`] =
        "Use letters, digits and underscores. Don't start with a digit.";
    else if (seen.has(r.key)) errors[`env.${r.id}`] = "Duplicate key.";
    seen.add(r.key);
  }
  const before = new Map(app.env.map((e) => [e.key, e]));
  const after = new Map(rows.map((r) => [r.key, r]));
  const changed = [
    ...rows
      .filter(
        (r) =>
          !before.has(r.key) ||
          before.get(r.key)!.value !== r.value ||
          before.get(r.key)!.secret !== r.secret,
      )
      .map((r) => r.key),
    ...app.env.filter((e) => !after.has(e.key)).map((e) => e.key),
  ];
  const dirty = changed.length > 0;

  const save = () => {
    const now = Date.now();
    api.setEnv(
      app.id,
      rows.map((r) => ({
        ...r,
        updatedAt:
          before.get(r.key)?.value === r.value
            ? (before.get(r.key)?.updatedAt ?? now)
            : now,
      })),
      changed,
    );
    toast({
      title: "Environment saved",
      description: "Redeploy to roll it out.",
    });
  };

  return (
    <Panel
      title="Environment variables"

      actions={
        <>
          <Button
            variant="ghost"
            size="xs"
            disabled={!dirty}
            onClick={() =>
              setRows(
                app.env.map(({ id, key, value, secret, hasValue }) => ({
                  id,
                  key,
                  value,
                  secret,
                  hasValue,
                })),
              )
            }
          >
            Discard
          </Button>
          <Button
            size="xs"
            disabled={!dirty}
            onClick={() => {
              setShowErrors(true);
              if (Object.keys(errors).length) return;
              setConfirming(true);
            }}
          >
            Save changes
          </Button>
        </>
      }
    >
      <div className="p-4">
        <EnvEditor
          rows={rows}
          onChange={setRows}
          errors={showErrors ? errors : {}}
          onReveal={
            canReveal ? (r) => api.revealSecret(app.id, r.id) : undefined
          }
        />
      </div>
      <ConfirmDialog
        open={confirming}
        onClose={() => setConfirming(false)}
        tone="primary"
        title="Save environment?"
        description={
          <>
            Changes to{" "}
            <span className="font-mono text-foreground">
              {changed.join(", ")}
            </span>{" "}
            apply on the next deployment.
          </>
        }
        confirmLabel="Save"
        onConfirm={save}
      />
    </Panel>
  );
}
