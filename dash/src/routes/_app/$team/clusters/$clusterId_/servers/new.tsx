import { TreeStructureIcon } from "@phosphor-icons/react";
import { site } from "~/config/site";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { ButtonLink } from "~/components/ui/Button";
import { Field, Input, Select } from "~/components/ui/Field";
import { Callout, EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { TagInput } from "~/components/ui/TagInput";
import { LiveCommand } from "~/components/pyo/LiveCommand";
import { SetupChecklist } from "~/components/pyo/SetupChecklist";
import { seo } from "~/lib/seo";
import { DNS_LABEL } from "~/lib/pyo/format";
import { locationLabel } from "~/lib/pyo/location";
import { CLUSTER_TABS, currentSlug } from "~/lib/pyo/paths";
import {
  api,
  clusterByRef,
  clustersIn,
  serversOf,
  usePyo,
} from "~/lib/pyo/store";
import { useLiveCommand } from "~/lib/pyo/useLiveCommand";
import { MenuPath } from "~/components/ui/MenuPath";

export const Route = createFileRoute(
  "/_app/$team/clusters/$clusterId_/servers/new",
)({
  head: () => ({ meta: seo({ title: "Add server" }) }),
  component: AddServer,
});

// Add a server to a cluster: name it, and right under that the command to
// run on the machine, which follows the form (a join token lasts an hour).
// The checklist follows the server as it joins.

function AddServer() {
  const { clusterId } = Route.useParams();
  const s = usePyo();
  const c = clusterByRef(s, clusterId);
  if (!c || c.teamId !== s.currentTeamId)
    return (
      <Page>
        <EmptyState
          icon={TreeStructureIcon}
          title="Cluster not found"
          description="It may have been removed."
          action={
            <ButtonLink href="/clusters" size="sm" variant="secondary">
              All clusters
            </ButtonLink>
          }
        />
      </Page>
    );
  // Keyed: picking another cluster starts its form afresh.
  return <JoinForm key={c.id} clusterId={c.id} clusterName={c.name} />;
}

function JoinForm({
  clusterId,
  clusterName,
}: {
  clusterId: string;
  clusterName: string;
}) {
  const s = usePyo();
  const navigate = useNavigate();
  const c = s.clusters.find((x) => x.id === clusterId)!;
  const nodes = serversOf(s, clusterId);
  // Servers already there when the page opened: only a new one counts as joined.
  const [existing] = useState(() => new Set(nodes.map((n) => n.name)));
  // Picked once: the server joining mustn't move the suggestion on.
  const [suggested] = useState(() => {
    for (let i = nodes.length + 1; ; i++)
      if (!nodes.some((n) => n.name === `${clusterName}-${i}`))
        return `${clusterName}-${i}`;
  });
  const [name, setName] = useState("");
  const [tags, setTags] = useState<string[]>([]);
  const value = name || suggested;
  const connected = c.status === "connected" || c.status === "degraded";
  const joined = existing.has(value)
    ? undefined
    : nodes.find((n) => n.name === value);
  const error = !DNS_LABEL.test(value)
    ? "Lowercase letters, digits and dashes."
    : CLUSTER_TABS.includes(value)
      ? "That name is reserved."
      : existing.has(value)
        ? "A server with this name exists."
        : "";
  const live = useLiveCommand(
    `${value}|${tags.join(",")}`,
    connected && !error && !joined,
    () => api.draftJoin(clusterId, value, tags),
  );
  const made = live.data;
  const base = `/clusters/${clusterName}`;

  return (
    <Page width="max-w-2xl">
      <PageHeader
        title="Add server"
        description="Join a server to a cluster."
      />
      <Panel>
        <div className="space-y-5 p-5">
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field
              label="Server name"
              error={joined ? undefined : error || undefined}
              hint="Also its name on your tailnet."
            >
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={value}
                  disabled={!!joined}
                  onChange={(e) => setName(e.target.value.toLowerCase())}
                  autoFocus
                />
              )}
            </Field>
            <Field label="Cluster">
              {(p) => (
                <Select
                  {...p}
                  value={clusterId}
                  disabled={!!joined}
                  onChange={(e) => {
                    const to = clustersIn(s).find(
                      (x) => x.id === e.target.value,
                    );
                    if (to)
                      void navigate({
                        to: "/$team/clusters/$clusterId/servers/new",
                        params: { team: currentSlug(), clusterId: to.name },
                        replace: true,
                      });
                  }}
                >
                  {clustersIn(s).map((x) => (
                    <option key={x.id} value={x.id}>
                      {x.name}
                      {x.status === "connected" || x.status === "degraded"
                        ? ""
                        : " (not connected)"}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
            <Field
              className="sm:col-span-2"
              label="Tags"
              optional
              hint="For placing services."
            >
              {(p) => (
                <TagInput
                  id={p.id}
                  aria-label="Tags"
                  value={tags}
                  onChange={setTags}
                  placeholder="gpu"
                  suggestions={[...new Set(nodes.flatMap((n) => n.tags ?? []))]}
                />
              )}
            </Field>
          </div>
          <div className="space-y-2 text-[13px]">
            <p className="font-medium">Run this on the machine</p>
            <LiveCommand
              command={made?.command}
              stale={live.stale}
              error={
                connected
                  ? live.error
                  : "The cluster isn't connected."
              }
              waiting={
                error ? "Fix the name to get the command." : undefined
              }
            >
              {made ? (
                <p className="text-[12px] text-muted-foreground">
                  Expires at{" "}
                  {new Date(made.expiresAt).toLocaleTimeString([], {
                    hour: "2-digit",
                    minute: "2-digit",
                  })}
                  {" · "}
                  <a
                    href={made.scriptUrl}
                    target="_blank"
                    rel="noreferrer"
                    className="font-medium text-primary hover:text-primary/80"
                  >
                    View the script
                  </a>
                </p>
              ) : null}
              {made ? (
                <p className="mt-1 text-[12px] text-muted-foreground">
                  <a
                    href={`${site.docsUrl}/gpus`}
                    target="_blank"
                    rel="noreferrer"
                    className="font-medium text-primary hover:text-primary/80"
                  >
                    GPU servers
                  </a>{" "}
                  need the NVIDIA driver first.
                </p>
              ) : null}
            </LiveCommand>
            {made && !made.tailscale ? (
              <Callout tone="warning">
                Not on your tailnet yet? Save a Tailscale auth key.{" "}
                <ButtonLink href="/integrations" size="xs" variant="secondary">
                  <MenuPath bare steps={["Integrations", "Tailscale"]} />
                </ButtonLink>
              </Callout>
            ) : null}
          </div>
        </div>
      </Panel>
      {made || joined ? (
        <SetupChecklist
          title={`Waiting for ${value} to join`}
          doneTitle={`${value} joined`}
          checks={[
            {
              label: "Joined the cluster",
              done: !!joined,
              detail: joined
                ? `${joined.roles.join(", ") || "worker"} · ${joined.internalIP}`
                : "Waiting for the command…",
            },
            {
              label: "Ready",
              done: joined?.status === "ready",
              detail: joined
                ? `${joined.osImage} · ${joined.kubeletVersion}`
                : undefined,
            },
            {
              label: "Region",
              done: !!joined?.location,
              missing: joined?.status === "ready" && !joined.location,
              detail: joined?.location
                ? locationLabel(joined.location)
                : joined?.status === "ready"
                  ? "Not detected: set it on the server page."
                  : undefined,
            },
          ]}
        />
      ) : null}
      <div className="flex justify-end gap-2">
        {joined ? (
          <ButtonLink href={`${base}/${joined.name}`} size="sm">
            Open {joined.name}
          </ButtonLink>
        ) : (
          <ButtonLink href={`${base}/servers`} variant="ghost" size="sm">
            {made ? "Finish later" : "Cancel"}
          </ButtonLink>
        )}
      </div>
    </Page>
  );
}
