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
import { seo } from "~/lib/seo";
import { MenuPath } from "~/components/ui/MenuPath";

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
  } = useClusterDraft(name, tags, publicNet);
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
        value={where}
        onChange={setWhere}
        options={[
          { value: "local", label: "This machine" },
          { value: "other", label: "Another machine" },
        ]}
      />
      {!showForm && otherLocal ? (
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
                  {live.data && !live.error ? (
                    <>
                      <AgentInstall
                        token={live.data.token}
                        command={live.data.command}
                        stale={live.stale}
                        thisMachine={thisMachine}
                      />
                      {!thisMachine &&
                      !live.data.publicNet &&
                      live.data.tailnet &&
                      !live.data.tailscale ? (
                        <Callout tone="warning">
                          Not on your tailnet yet? Save a Tailscale auth key.{" "}
                          <ButtonLink href="/integrations" size="xs" variant="secondary">
                            <MenuPath bare steps={["Integrations", "Tailscale"]} />
                          </ButtonLink>
                        </Callout>
                      ) : null}
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
