import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Select, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import {
  api,
  clustersIn,
  currentTeam,
  getState,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Team } from "~/lib/pyo/types";
import { CopyButton } from "~/components/ui/Table";

/** A read-only id for the API and MCP tools (their `team` takes either). */
function IdField({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <p className="mb-1.5 text-[13px] font-medium">{label}</p>
      <div className="flex h-9 items-center gap-2 rounded-lg border border-border bg-muted/40 px-3">
        <code className="min-w-0 flex-1 truncate font-mono text-[12.5px]">
          {value}
        </code>
        <CopyButton value={value} label={`Copy ${label.toLowerCase()}`} />
      </div>
    </div>
  );
}

export const Route = createFileRoute("/_app/$team/settings/")({
  component: () => {
    const ws = currentTeam(usePyo());
    return ws ? <TeamSettings key={ws.id} ws={ws} /> : null;
  },
});

function TeamSettings({ ws }: { ws: Team }) {
  const s = usePyo();
  const navigate = useNavigate();
  const [name, setName] = useState(ws.name);
  const [description, setDescription] = useState(ws.description);
  const [defaultClusterId, setDefaultCluster] = useState(
    ws.defaultClusterId ?? "",
  );
  const [prefs, setPrefs] = useState(ws.preferences);
  const [deleting, setDeleting] = useState(false);
  const dirty =
    name !== ws.name ||
    description !== ws.description ||
    defaultClusterId !== (ws.defaultClusterId ?? "") ||
    JSON.stringify(prefs) !== JSON.stringify(ws.preferences);
  const nameError = !name.trim() ? "Name is required." : "";

  return (
    <div className="space-y-4">
      <Panel title="General">
        <div className="space-y-4 p-4">
          <Field label="Team name" error={nameError || undefined}>
            {(p) => (
              <Input
                {...p}
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            )}
          </Field>
          <div className="grid gap-4 sm:grid-cols-2">
            <IdField label="Team ID" value={ws.id} />
            <IdField label="Slug" value={ws.slug} />
          </div>
          <Field
            label="Default target cluster"
            hint="Preselected when deploying."
          >
            {(p) => (
              <Select
                {...p}
                value={defaultClusterId}
                onChange={(e) => setDefaultCluster(e.target.value)}
              >
                <option value="">None</option>
                {clustersIn(s).map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        </div>
      </Panel>
      <Panel title="Deployment preferences">
        <div className="space-y-4 p-4">
          <Switch
            checked={prefs.autoDeployOnPush}
            onChange={(v) => setPrefs({ ...prefs, autoDeployOnPush: v })}
            label="Deploy on Git push"
            description="Needs a Git provider."
          />
          <Switch
            checked={prefs.requireDeployApproval}
            onChange={(v) => setPrefs({ ...prefs, requireDeployApproval: v })}
            label="Require approval for production deploys"
            description="An admin approves each rollout."
          />
        </div>
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={!dirty || !!nameError}
            onClick={async () => {
              const res = await api.updateTeam(ws.id, {
                name: name.trim(),
                description,
                defaultClusterId: defaultClusterId || undefined,
                preferences: prefs,
              });
              if (!res) return;
              toast({ title: "Team saved" });
              // A new name can mean a new slug; keep the URL in step.
              const slug =
                res.slug ?? getState().teams.find((t) => t.id === ws.id)?.slug;
              if (slug && slug !== ws.slug)
                void navigate({
                  to: "/$team/settings",
                  params: { team: slug },
                  replace: true,
                });
            }}
          >
            Save changes
          </Button>
        </div>
      </Panel>
      <Panel title="Danger zone" tone="danger">
        <ul className="list-rows">
          <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
            <div>
              <p className="text-[13px] font-medium">Delete team</p>
              <p className="text-[12px] text-muted-foreground">
                Delete its services first.
              </p>
            </div>
            <Button
              variant="secondary"
              size="sm"
              className="text-destructive"
              disabled={s.teams.length < 2}
              title={
                s.teams.length < 2 ? "You need at least one team" : undefined
              }
              onClick={() => setDeleting(true)}
            >
              Delete team
            </Button>
          </li>
        </ul>
      </Panel>
      <ConfirmDialog
        open={deleting}
        onClose={() => setDeleting(false)}
        title={`Delete ${ws.name}?`}
        description="Nothing on the clusters is touched."
        confirmText={ws.slug}
        confirmLabel="Delete team"
        onConfirm={async () => {
          if (!(await api.deleteTeam(ws.id))) return;
          toast({ title: `Deleted ${ws.name}` });
          void navigate({ to: "/" });
        }}
      />
    </div>
  );
}
