import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { TagInput } from "~/components/ui/TagInput";
import { type ReactNode, useState } from "react";
import { RESERVED_CLUSTER_NAMES } from "~/lib/pyo/location";
import { AgentInstall } from "~/components/pyo/AgentInstall";
import { ClusterMtls } from "~/components/pyo/Mtls";
import { ClusterPublicTraffic, ClusterTrafficMode } from "~/components/pyo/TrafficMode";
import { Badge, ClusterStatusBadge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { KeyValue, Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { clusterGpuText, dateTime, timeAgo } from "~/lib/pyo/format";
import { DNS_LABEL } from "~/lib/pyo/format";
import { api, clusterByRef, clustersIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster } from "~/lib/pyo/types";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute(
  "/_app/$team/clusters/$clusterId/settings",
)({
  component: () => {
    const { clusterId } = Route.useParams();
    const c = clusterByRef(usePyo(), clusterId);
    return c ? <ClusterSettings key={c.id} c={c} /> : null;
  },
});

function ClusterSettings({ c }: { c: Cluster }) {
  const s = usePyo();
  const navigate = useNavigate();
  const [name, setName] = useState(c.name);
  const [tags, setTags] = useState<string[]>(c.tags ?? []);
  const [confirm, setConfirm] = useState<
    "rotate" | "remove" | "force" | null
  >(null);
  const [newToken, setNewToken] = useState<{
    token: string;
    command: string;
  } | null>(null);
  const apps = s.services.filter((a) => a.clusterId === c.id);
  // Gone for 10 minutes: removable with its services (as the API allows).
  const gone = Date.now() - c.lastSeenAt >= 10 * 60_000;
  const install = c.capabilities.agentInstall;
  const dirty = name !== c.name || tags.join() !== (c.tags ?? []).join();
  // The name is the cluster's URL, so it follows the same rules as on creation.
  const nameError = !DNS_LABEL.test(name)
    ? "Lowercase letters, digits and dashes."
    : RESERVED_CLUSTER_NAMES.includes(name)
      ? "A page uses that name. Pick another."
      : clustersIn(s).some((x) => x.name === name && x.id !== c.id)
        ? "A cluster with this name exists in this team."
        : "";
  return (
    <div className="grid gap-4 lg:grid-cols-[minmax(0,1.4fr)_minmax(0,1fr)]">
      <div className="space-y-4">
        <Panel title="Metadata">
          <div className="grid items-start gap-4 p-4 sm:grid-cols-2">
            <Field
              label="Name"
              error={dirty ? nameError || undefined : undefined}
            >
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={name}
                  onChange={(e) => setName(e.target.value.toLowerCase())}
                />
              )}
            </Field>
            <Field
              label="Tags"
              optional
              hint="Used to place services, e.g. production."
            >
              {(p) => (
                <TagInput
                  id={p.id}
                  aria-label="Tags"
                  value={tags}
                  onChange={setTags}
                  placeholder="production"
                />
              )}
            </Field>
          </div>
          <div className="flex justify-end border-t border-border px-4 py-3">
            <Button
              size="sm"
              disabled={!dirty || !!nameError}
              onClick={() => {
                api.updateCluster(c.id, { name, tags });
                toast({ title: "Cluster saved" });
                if (name !== c.name)
                  void navigate({
                    to: "/$team/clusters/$clusterId/settings",
                    params: { team: currentSlug(), clusterId: name },
                    replace: true,
                  });
              }}
            >
              Save
            </Button>
          </div>
        </Panel>
        <ClusterPublicTraffic cluster={c} />
        <ClusterTrafficMode cluster={c} />
        <ClusterMtls cluster={c} />
        <Panel title="Danger zone" tone="danger">
          <ul className="list-rows">
            <li className="px-4 py-3">
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div>
                  <p className="text-[13px] font-medium">Rotate agent token</p>
                  <p className="text-[12px] text-muted-foreground">
                    The agent loses access until you rerun the install command.
                  </p>
                </div>
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => setConfirm("rotate")}
                >
                  Rotate token
                </Button>
              </div>
              {newToken ? (
                <div className="mt-3">
                  <AgentInstall
                    token={newToken.token}
                    command={newToken.command}
                  />
                </div>
              ) : null}
            </li>
            <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
              <div>
                <p className="text-[13px] font-medium">Remove from Pyo</p>
                <p className="text-[12px] text-muted-foreground">
                  {!apps.length
                    ? "Forget this cluster and its servers."
                    : gone
                      ? `Unreachable, with ${apps.length} services.`
                      : `Move or delete its ${apps.length} services first.`}
                </p>
              </div>
              <Button
                variant="secondary"
                size="sm"
                className="text-destructive"
                disabled={apps.length > 0 && !gone}
                onClick={() => setConfirm(apps.length ? "force" : "remove")}
              >
                {apps.length && gone ? "Remove anyway" : "Remove"}
              </Button>
            </li>
          </ul>
        </Panel>
      </div>
      <Panel title="Connection">
        <KeyValue
          items={[
            ["Status", <ClusterStatusBadge status={c.status} />],
            ["Method", "Pyo agent (outbound)"],
            ["Last seen", c.lastSeenAt ? timeAgo(c.lastSeenAt) : "Never"],
            [
              "Dials out to",
              <span className="font-mono text-[11.5px]">
                {s.agentApiUrl || "—"}
              </span>,
            ],
            [
              "Agent",
              <span className="inline-flex flex-wrap items-center gap-2">
                {c.connection.agentVersion ?? "—"}
                {install?.state === "updating" ? (
                  <Badge tone="progress">Updating</Badge>
                ) : install?.state === "outdated" ? (
                  <Badge tone="warning">
                    <span title={install.message}>Update needed</span>
                  </Badge>
                ) : null}
              </span>,
            ],
            ...(install?.state === "outdated" && install.command
              ? ([
                  [
                    "Update on its server",
                    <span className="inline-flex items-center gap-1">
                      <span className="font-mono text-[11.5px]">
                        {install.command}
                      </span>
                      <CopyButton value={install.command} />
                    </span>,
                  ],
                ] as [ReactNode, ReactNode][])
              : []),
            ["Distribution", `${c.distribution} ${c.version}`],
            [
              "Metrics server",
              c.capabilities.metricsServer ? "Installed" : "Missing",
            ],
            ["GPUs", clusterGpuText(c.capabilities.gpu)],
            ["Registered", dateTime(c.createdAt)],
          ]}
        />
      </Panel>
      <ConfirmDialog
        open={confirm === "rotate"}
        onClose={() => setConfirm(null)}
        tone="primary"
        title={`Rotate the agent token of ${c.name}?`}
        description="Run the new command to reconnect."
        confirmLabel="Rotate token"
        onConfirm={() => {
          void api.rotateAgentToken(c.id).then((t) => t && setNewToken(t));
        }}
      />
      <ConfirmDialog
        open={confirm === "remove"}
        onClose={() => setConfirm(null)}
        title={`Remove ${c.name}?`}
        description="Nothing on the cluster is deleted."
        confirmText={c.name}
        confirmLabel="Remove cluster"
        onConfirm={() => {
          api.removeCluster(c.id);
          toast({ title: `Removed ${c.name}` });
          void navigate({
            to: "/$team/clusters",
            params: { team: currentSlug() },
          });
        }}
      />
      <ConfirmDialog
        open={confirm === "force"}
        onClose={() => setConfirm(null)}
        title={`Remove ${c.name} anyway?`}
        description="Pyo forgets this cluster, its services, databases and volumes. Anything still running there is left alone."
        confirmText={c.name}
        confirmLabel="Remove cluster"
        onConfirm={() => {
          void api.removeCluster(c.id, true).then((r) => {
            if (!r) return;
            toast({ title: `Removed ${c.name}` });
            void navigate({
              to: "/$team/clusters",
              params: { team: currentSlug() },
            });
          });
        }}
      />
    </div>
  );
}
