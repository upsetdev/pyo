import { NeedsTailscale } from "~/components/pyo/Tailscale";
import { useHosted } from "~/components/pyo/Managed";
import { TailnetLockWait } from "~/components/pyo/TailnetLock";
import { TreeStructureIcon } from "@phosphor-icons/react";
import { site } from "~/config/site";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented, Select } from "~/components/ui/Field";
import { Callout, EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import {
  CloudProgress,
  CloudServerFields,
  hetznerOf,
  priceOf,
  useCloudOptions,
  type CloudChoice,
} from "~/components/pyo/Hetzner";
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
  // Run the command on a machine, or have Pyo make one at Hetzner (not on a
  // hosted Pyo yet).
  const hosted = useHosted();
  const [picked, setHow] = useState<"command" | "hetzner">("command");
  const how = hosted === false ? picked : "command";
  const [builder, setBuilder] = useState(false);
  const [choice, setChoice] = useState<CloudChoice>({
    location: "",
    serverType: "",
    sshKey: "",
  });
  const [confirming, setConfirming] = useState(false);
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState("");
  const hetzner = hetznerOf(s);
  const { options } = useCloudOptions(how === "hetzner" && !!hetzner);
  const value = name || suggested;
  const cloud = s.cloudServers.find(
    (x) => x.clusterId === clusterId && x.name === value,
  );
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
  const price = priceOf(options, choice);
  const create = async () => {
    setConfirming(false);
    setCreating(true);
    setCreateError("");
    const r = await api.createCloudServer({
      clusterId,
      name: value,
      location: choice.location,
      serverType: choice.serverType,
      builder,
      sshKey: choice.sshKey,
      tags,
    });
    setCreating(false);
    if ("error" in r) setCreateError(r.error);
  };
  const live = useLiveCommand(
    `${value}|${tags.join(",")}`,
    how === "command" && connected && !error && !joined,
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
      {hosted === false ? (
        <Segmented
          label="How"
          value={how}
          onChange={setHow}
          options={[
            { value: "command", label: "Run a command" },
            { value: "hetzner", label: "Hetzner" },
          ]}
        />
      ) : null}
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
                  disabled={!!joined || !!cloud}
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
          {how === "hetzner" ? (
            <div className="space-y-4 text-[13px]">
              <NeedsTailscale clusterId={clusterId} />
              {!hetzner ? (
                <Callout tone="warning">
                  Connect Hetzner first.{" "}
                  <ButtonLink href="/integrations#hetzner" size="xs" variant="secondary">
                    Integrations
                  </ButtonLink>
                </Callout>
              ) : cloud ? (
                <CloudProgress server={cloud} />
              ) : (
                <>
                  <Segmented
                    label="Role"
                    value={builder ? "builder" : "services"}
                    onChange={(v) => setBuilder(v === "builder")}
                    options={[
                      { value: "services", label: "Runs services" },
                      { value: "builder", label: "Builds only" },
                    ]}
                  />
                  <CloudServerFields value={choice} onChange={setChoice} />
                  {createError ? <Callout tone="danger">{createError}</Callout> : null}
                  {!connected ? (
                    <Callout tone="warning">The cluster isn't connected.</Callout>
                  ) : null}
                  <Button
                    size="sm"
                    disabled={
                      !!error || !price || !connected || !(c.tailscale || s.tailscale) || creating
                    }
                    onClick={() => setConfirming(true)}
                  >
                    {creating
                      ? "Creating…"
                      : price
                        ? `Create for ${price}/mo`
                        : "Pick a server type"}
                  </Button>
                  <ConfirmDialog
                    open={confirming}
                    onClose={() => setConfirming(false)}
                    onConfirm={() => void create()}
                    tone="primary"
                    title={`Create ${value} at Hetzner?`}
                    description={`${choice.serverType} in ${choice.location}. ${price ?? ""} a month, before VAT.`}
                    confirmLabel="Create server"
                  />
                </>
              )}
            </div>
          ) : (
          <div className="space-y-2 text-[13px]">
            <NeedsTailscale clusterId={clusterId} />
            {made?.tailnetFirst ? (
              <div className="space-y-2">
                <p className="font-medium">First, run this on {made.tailnetFirst.server}</p>
                <p className="text-[12px] text-muted-foreground">It joins your tailnet, so new servers reach it there.</p>
                <LiveCommand command={made.tailnetFirst.command} stale={live.stale} />
              </div>
            ) : null}
            <p className="font-medium">{made?.tailnetFirst ? "Then run this on the new machine" : "Run this on the machine"}</p>
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
          </div>
          )}
        </div>
      </Panel>
      {(how === "command" && made) || joined ? (
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
      {how === "command" && made && !joined ? <TailnetLockWait name={value} /> : null}
      <div className="flex justify-end gap-2">
        {joined ? (
          <ButtonLink href={`${base}/${joined.name}`} size="sm">
            Open {joined.name}
          </ButtonLink>
        ) : (
          <ButtonLink href={`${base}/servers`} variant="ghost" size="sm">
            {made || cloud ? "Finish later" : "Cancel"}
          </ButtonLink>
        )}
      </div>
    </Page>
  );
}
