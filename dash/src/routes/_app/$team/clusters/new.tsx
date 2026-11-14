import { NeedsTailscale } from "~/components/pyo/Tailscale";
import { ClusterChecklist } from "~/components/pyo/ClusterChecklist";
import { TagInput } from "~/components/ui/TagInput";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AgentInstall } from "~/components/pyo/AgentInstall";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Input, Segmented, Switch } from "~/components/ui/Field";
import { Callout, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { currentSlug } from "~/lib/pyo/paths";
import { useClusterDraft } from "~/lib/pyo/useClusterDraft";
import { LiveCommand } from "~/components/pyo/LiveCommand";
import { ConfirmDialog } from "~/components/ui/Dialog";
import {
  CloudProgress,
  CloudServerFields,
  hetznerOf,
  priceOf,
  useCloudOptions,
  type CloudChoice,
} from "~/components/pyo/Hetzner";
import { usePyo, api } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/clusters/new")({
  head: () => ({ meta: seo({ title: "Add cluster" }) }),
  component: AddServers,
});

// One form: name and tags, and right under them the command that sets the
// machine up. The cluster is made as the page opens (that issues its agent
// token); edits after that rename it. The checklist fills in from what the
// agent reports. Leaving without Cancel or Finish later takes it back.

function AddServers() {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [tags, setTags] = useState<string[]>([]);
  // Advanced: over the public internet instead of the tailnet.
  const [publicNet, setPublicNet] = useState(false);
  // Pyo makes the first server at Hetzner.
  const [hetzner, setHetzner] = useState(false);
  const {
    where,
    setWhere,
    thisMachine,
    nameError,
    showForm,
    otherLocal,
    draft,
    cluster: c,
    connected,
    live,
    keep,
    cancel: discard,
  } = useClusterDraft(name, tags, publicNet, hetzner);
  const cancel = async () => {
    await discard();
    void navigate({ to: "/$team/clusters", params: { team: currentSlug() } });
  };

  return (
    <Page width="max-w-2xl">
      <PageHeader
        title="Add cluster"
        description="Connect a server."
      />
      <Segmented
        label="Which machine"
        value={hetzner ? "hetzner" : where}
        onChange={(v) => {
          setHetzner(v === "hetzner");
          if (v !== "hetzner") setWhere(v);
        }}
        options={[
          { value: "local", label: "This machine" },
          { value: "other", label: "Another machine" },
          { value: "hetzner", label: "Hetzner" },
        ]}
      />
      {hetzner ? (
        <HetznerCluster
          name={name}
          setName={setName}
          tags={tags}
          setTags={setTags}
          draftId={draft?.id}
        />
      ) : !showForm && otherLocal ? (
        <Panel>
          <div className="flex flex-wrap items-center justify-between gap-3 p-5">
            <p className="text-[13px] text-muted-foreground">
              This machine is connected as{" "}
              <span className="font-medium text-foreground">
                {otherLocal.name}
              </span>
              .
            </p>
            <ButtonLink
              href={`/clusters/${otherLocal.name}`}
              size="sm"
              variant="secondary"
            >
              Open {otherLocal.name}
            </ButtonLink>
          </div>
        </Panel>
      ) : (
        <>
          <Panel>
            <div className="space-y-5 p-5">
              <div className="grid items-start gap-4 sm:grid-cols-2">
                <Field
                  label="Cluster name"
                  optional
                  error={nameError || undefined}
                >
                  {(p) => (
                    <Input
                      {...p}
                      mono
                      value={name}
                      onChange={(e) => setName(e.target.value.toLowerCase())}
                      placeholder={connected && c ? c.name : "server hostname"}
                      disabled={connected}
                      autoFocus
                    />
                  )}
                </Field>
                <Field
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
                      placeholder="production"
                    />
                  )}
                </Field>
              </div>
              {connected && !live.data ? null : (
                <div className="space-y-2">
                  <p className="text-[13px] font-medium">
                    {thisMachine
                      ? "Run this on the machine Pyo runs on"
                      : "Run this on the machine"}
                  </p>
                  {thisMachine ? null : <NeedsTailscale />}
                  {live.data && !live.error ? (
                    <>
                      <AgentInstall
                        token={live.data.token}
                        command={live.data.command}
                        stale={live.stale}
                        thisMachine={thisMachine}
                      />
                    </>
                  ) : (
                    <LiveCommand
                      error={live.error}
                      waiting={
                        nameError
                          ? "Fix the name to get the command."
                          : undefined
                      }
                    />
                  )}
                </div>
              )}
            </div>
            {!thisMachine && !connected ? (
              <div className="space-y-2 border-t border-border p-5">
                <Switch
                  checked={publicNet}
                  onChange={setPublicNet}
                  label="Connect over the public internet"
                  description="Advanced. Instead of your tailnet."
                />
                {publicNet ? (
                  <Callout tone="warning">
                    {window.location.origin} must be reachable from it, and apps
                    built from Git won't run there.
                  </Callout>
                ) : null}
              </div>
            ) : null}
          </Panel>
          {draft ? <ClusterChecklist clusterId={draft.id} /> : null}
          <div className="flex justify-end gap-2">
            {connected && c ? (
              <>
                <ButtonLink
                  href={`/clusters/${c.name}`}
                  variant="secondary"
                  size="sm"
                >
                  View servers
                </ButtonLink>
                <ButtonLink href="/deploy/new" size="sm">
                  Deploy to it
                </ButtonLink>
              </>
            ) : (
              <>
                <Button variant="ghost" size="sm" onClick={() => void cancel()}>
                  Cancel
                </Button>
                {draft ? (
                  <ButtonLink
                    href={`/clusters/${c?.name ?? name}`}
                    variant="secondary"
                    size="sm"
                    onClick={keep}
                  >
                    Finish later
                  </ButtonLink>
                ) : null}
              </>
            )}
          </div>
        </>
      )}
    </Page>
  );
}

/** Add cluster › Hetzner: Pyo makes the first server and runs connect.sh there. */
function HetznerCluster({
  name,
  setName,
  tags,
  setTags,
  draftId,
}: {
  name: string;
  setName: (n: string) => void;
  tags: string[];
  setTags: (t: string[]) => void;
  /** Another machine's cluster, made as the page opened: it gives way. */
  draftId?: string;
}) {
  const s = usePyo();
  const conn = hetznerOf(s);
  const { options } = useCloudOptions(!!conn);
  const [choice, setChoice] = useState<CloudChoice>({
    location: "",
    serverType: "",
    sshKey: "",
  });
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [made, setMade] = useState<{ clusterId: string; id: string } | null>(null);
  const price = priceOf(options, choice);
  const nameError =
    name && !/^[a-z0-9]([a-z0-9-]{0,59}[a-z0-9])?$/.test(name)
      ? "Lowercase letters, digits and dashes."
      : s.clusters.some(
            (c) => c.teamId === s.currentTeamId && c.name === name && c.id !== draftId,
          )
        ? "A cluster with this name exists."
        : "";
  const server = made ? s.cloudServers.find((x) => x.id === made.id) : undefined;
  const cluster = made ? s.clusters.find((c) => c.id === made.clusterId) : undefined;
  const create = async () => {
    setConfirming(false);
    setBusy(true);
    setError("");
    if (draftId) await api.discardClusterDraft(draftId);
    const r = await api.createCloudCluster({
      name,
      location: choice.location,
      serverType: choice.serverType,
      sshKey: choice.sshKey,
      tags,
    });
    setBusy(false);
    if ("error" in r) return setError(r.error);
    setMade({ clusterId: r.data.clusterId, id: r.data.id });
  };
  return (
    <>
      <Panel>
        <div className="space-y-5 p-5">
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field label="Cluster name" error={nameError || undefined} hint={name ? `Its server is ${name}-1.` : undefined}>
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={name}
                  onChange={(e) => setName(e.target.value.toLowerCase())}
                  disabled={!!made}
                  autoFocus
                />
              )}
            </Field>
            <Field label="Tags" optional hint="For placing services.">
              {(p) => (
                <TagInput id={p.id} aria-label="Tags" value={tags} onChange={setTags} placeholder="production" />
              )}
            </Field>
          </div>
          <NeedsTailscale />
          {!conn ? (
            <Callout tone="warning">
              Connect Hetzner first.{" "}
              <ButtonLink href="/integrations" size="xs" variant="secondary">
                Integrations
              </ButtonLink>
            </Callout>
          ) : server ? (
            <CloudProgress server={server} />
          ) : (
            <>
              <CloudServerFields value={choice} onChange={setChoice} />
              {error ? <Callout tone="danger">{error}</Callout> : null}
              <Button
                size="sm"
                disabled={!name || !!nameError || !price || !s.tailscale || busy || !!made}
                onClick={() => setConfirming(true)}
              >
                {busy ? "Creating…" : price ? `Create for ${price}/mo` : "Pick a server type"}
              </Button>
              <ConfirmDialog
                open={confirming}
                onClose={() => setConfirming(false)}
                onConfirm={() => void create()}
                tone="primary"
                title={`Create ${name}-1 at Hetzner?`}
                description={`${choice.serverType} in ${choice.location}. ${price ?? ""} a month, before VAT.`}
                confirmLabel="Create server"
              />
            </>
          )}
        </div>
      </Panel>
      {made ? <ClusterChecklist clusterId={made.clusterId} /> : null}
      {cluster && cluster.status !== "pending" ? (
        <div className="flex justify-end gap-2">
          <ButtonLink href={`/clusters/${cluster.name}`} variant="secondary" size="sm">
            View servers
          </ButtonLink>
          <ButtonLink href="/deploy/new" size="sm">
            Deploy to it
          </ButtonLink>
        </div>
      ) : null}
    </>
  );
}
