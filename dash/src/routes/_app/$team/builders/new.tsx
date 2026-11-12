import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { z } from "zod";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Input, Segmented, Select } from "~/components/ui/Field";
import { Callout, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { LiveCommand } from "~/components/pyo/LiveCommand";
import { SetupChecklist } from "~/components/pyo/SetupChecklist";
import { seo } from "~/lib/seo";
import { DNS_LABEL, newId } from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import {
  api,
  buildersIn,
  clustersIn,
  serversOf,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Builder } from "~/lib/pyo/types";
import { useLiveCommand } from "~/lib/pyo/useLiveCommand";
import { MenuPath } from "~/components/ui/MenuPath";

export const Route = createFileRoute("/_app/$team/builders/new")({
  head: () => ({ meta: seo({ title: "Add builder" }) }),
  // ?draft=<id>: the builder on a new server this page made.
  validateSearch: z.object({
    draft: z.string().optional().catch(undefined),
  }),
  component: AddBuilder,
});

// Where a builder runs: this machine, a server of a cluster, a new server
// (it joins a cluster as a build-only server; the command shows under the
// form and the checklist follows it).

function AddBuilder() {
  const s = usePyo();
  const navigate = useNavigate();
  const search = Route.useSearch();
  // The builder on a new server, made as you fill in the form, and kept
  // in the address so a reload picks it up again. Asking again with its id
  // replaces it until its server has joined.
  const [draftId] = useState(() => search.draft ?? newId("bld"));
  const resumed = s.builders.find(
    (b) => b.id === search.draft && b.kind === "server",
  );
  // A machine runs one builder: which servers have one already.
  const usedBy = new Map<string, string>(
    buildersIn(s)
      .filter(
        (b) =>
          b.status !== "removing" && b.clusterId && b.server && b.id !== draftId,
      )
      .map((b) => [`${b.clusterId}|${b.server}`, b.name]),
  );
  const clusters = clustersIn(s).filter((c) => serversOf(s, c.id).length);
  const servers = clusters.flatMap((c) =>
    serversOf(s, c.id).map((n) => ({
      value: `${c.id}|${n.name}`,
      label: `${n.name} · ${c.name}`,
      ready: n.status === "ready",
      builder: usedBy.get(`${c.id}|${n.name}`),
    })),
  );
  // "This machine": the cluster Pyo runs on, on its control-plane server.
  const local = clusters.find((c) => c.sameCluster);
  const localNode = local
    ? (serversOf(s, local.id).find(
        (n) => n.roles.includes("control-plane") && n.status === "ready",
      ) ?? serversOf(s, local.id)[0])
    : undefined;
  const localBuilder =
    local && localNode
      ? usedBy.get(`${local.id}|${localNode.name}`)
      : undefined;
  const [where, setWhere] = useState<"local" | Builder["kind"]>(
    resumed ? "server" : localNode && !localBuilder ? "local" : "cluster",
  );
  const kind: Builder["kind"] = where === "local" ? "cluster" : where;
  const joinable = clustersIn(s).filter(
    (c) => c.status === "connected" || c.status === "degraded",
  );
  const [joinCluster, setJoinCluster] = useState(resumed?.clusterId ?? "");
  const [nodeName, setNodeName] = useState(resumed?.server ?? "");
  const [name, setName] = useState(
    resumed?.name ?? `builder-${buildersIn(s).length + 1}`,
  );
  const [server, setServer] = useState("");
  const [saving, setSaving] = useState(false);
  const [touched, setTouched] = useState(false);
  const nameError = !DNS_LABEL.test(name)
    ? "Lowercase letters, digits and dashes."
    : buildersIn(s).some((b) => b.name === name && b.id !== draftId)
      ? "A builder with this name exists."
      : "";
  const free = servers.filter((x) => !x.builder);
  const target =
    where === "local"
      ? local && localNode && !localBuilder
        ? `${local.id}|${localNode.name}`
        : ""
      : server && !usedBy.has(server)
        ? server
        : (free.find((x) => x.ready) ?? free[0])?.value || "";
  const joinTarget = joinCluster || joinable[0]?.id || "";
  const node = nodeName || name;
  const draft = s.builders.find((b) => b.id === draftId);
  const ours = draft?.clusterId === joinTarget && draft?.server === node;
  const nodeError = !DNS_LABEL.test(node)
    ? "Lowercase letters, digits and dashes."
    : joinTarget && !ours && serversOf(s, joinTarget).some((n) => n.name === node)
      ? "A server with this name exists."
      : "";
  const blocked = kind === "cluster" ? !target : !joinTarget || !!nodeError;

  const connected = draft?.status === "ready";
  // Its server has joined: the form can't change it any more.
  const joined =
    !!draft?.clusterId &&
    serversOf(s, draft.clusterId).some((n) => n.name === draft.server);
  const live = useLiveCommand(
    `${name}|${joinTarget}|${node}`,
    kind === "server" && !nameError && !blocked && !joined,
    async () => {
      const r = await api.draftServerBuilder({
        id: draftId,
        name,
        clusterId: joinTarget,
        server: node,
      });
      if ("data" in r && search.draft !== draftId)
        void navigate({
          to: ".",
          search: { draft: draftId },
          replace: true,
        });
      return r;
    },
  );

  const leave = () =>
    void navigate({ to: "/$team/builders", params: { team: currentSlug() } });
  // Cancel takes back a builder the form made whose server never joined.
  const cancel = () => {
    if (draft && !joined && !connected) void api.removeBuilder(draft.id);
    leave();
  };
  const switchTo = (v: typeof where) => {
    if (v !== "server" && draft && !joined) {
      void api.removeBuilder(draft.id);
      void navigate({ to: ".", search: {}, replace: true });
    }
    setWhere(v);
  };
  const add = async () => {
    setTouched(true);
    if (nameError || blocked) return;
    setSaving(true);
    const [clusterId, targetNode] = target.split("|");
    const r = await api.createBuilder({ name, kind, clusterId, server: targetNode });
    setSaving(false);
    if (!r) return;
    toast({ title: `Adding ${name}` });
    void navigate({
      to: "/$team/builders/$builderId",
      params: { team: currentSlug(), builderId: r.id },
    });
  };
  return (
    <Page width="max-w-2xl">
      <PageHeader
        title="Add builder"
        description="Builds Git deploys."
      />
      <Segmented
        label="Where it runs"
        value={where}
        onChange={switchTo}
        options={[
          { value: "local", label: "This machine" },
          { value: "cluster", label: "In a cluster" },
          { value: "server", label: "New server" },
        ]}
      />
      <Panel>
        <form
          className="space-y-4 p-5"
          onSubmit={(e) => {
            e.preventDefault();
            if (kind !== "server") void add();
          }}
        >
          <Field
            label="Name"
            error={touched ? nameError || undefined : undefined}
          >
            {(p) => (
              <Input
                {...p}
                mono
                value={name}
                disabled={joined}
                onChange={(e) => setName(e.target.value.toLowerCase())}
              />
            )}
          </Field>
          {where === "local" ? (
            localBuilder && localNode ? (
              <Callout tone="neutral">
                This machine already runs{" "}
                <span className="font-medium">{localBuilder}</span>.
              </Callout>
            ) : localNode && local ? (
              <p className="text-[12.5px] text-muted-foreground">
                On{" "}
                <span className="font-medium text-foreground">
                  {localNode.name}
                </span>
                , alongside Pyo.
              </p>
            ) : (
              <Callout tone="warning">
                This machine isn&apos;t connected yet.{" "}
                <ButtonLink href="/clusters/new" size="xs" variant="secondary">
                  Connect it
                </ButtonLink>
              </Callout>
            )
          ) : kind === "cluster" ? (
            servers.length ? (
              <Field
                label="Server"
                hint={free.length ? undefined : "Every server has a builder."}
              >
                {(p) => (
                  <Select
                    {...p}
                    value={target}
                    onChange={(e) => setServer(e.target.value)}
                  >
                    {servers.map((x) => (
                      <option
                        key={x.value}
                        value={x.value}
                        disabled={!!x.builder}
                      >
                        {x.label}
                        {x.builder
                          ? ` (runs ${x.builder})`
                          : x.ready
                            ? ""
                            : " (offline)"}
                      </option>
                    ))}
                  </Select>
                )}
              </Field>
            ) : (
              <Callout tone="warning">
                Connect a cluster first.{" "}
                <ButtonLink href="/clusters/new" size="xs" variant="secondary">
                  Add cluster
                </ButtonLink>
              </Callout>
            )
          ) : kind === "server" ? (
            joinable.length ? (
              <>
                <div className="grid gap-4 sm:grid-cols-2">
                  <Field label="Server name" error={nodeError || undefined}>
                    {(p) => (
                      <Input
                        {...p}
                        mono
                        value={node}
                        disabled={joined}
                        onChange={(e) =>
                          setNodeName(e.target.value.toLowerCase())
                        }
                      />
                    )}
                  </Field>
                  <Field label="Joins cluster">
                    {(p) => (
                      <Select
                        {...p}
                        value={joinTarget}
                        onChange={(e) => setJoinCluster(e.target.value)}
                      >
                        {joinable.map((c) => (
                          <option key={c.id} value={c.id}>
                            {c.name}
                          </option>
                        ))}
                      </Select>
                    )}
                  </Field>
                </div>
                <div className="space-y-2 text-[13px]">
                  <p className="font-medium">Run this on the machine</p>
                  <LiveCommand
                    command={live.data?.command}
                    stale={live.stale}
                    error={live.error}
                    waiting={
                      nameError || nodeError
                        ? "Fix the names to get the command."
                        : undefined
                    }
                  >
                    <p className="text-[12px] text-muted-foreground">
                      Expires in 1 hour
                    </p>
                  </LiveCommand>
                  {live.data &&
                  !live.data.command.includes("--tailscale-key") ? (
                    <Callout tone="warning">
                      Not on your tailnet yet? Save a Tailscale auth key.{" "}
                      <ButtonLink href="/integrations" size="xs" variant="secondary">
                        <MenuPath bare steps={["Integrations", "Tailscale"]} />
                      </ButtonLink>
                    </Callout>
                  ) : null}
                </div>
              </>
            ) : (
              <Callout tone="warning">
                Connect a cluster first.{" "}
                <ButtonLink href="/clusters/new" size="xs" variant="secondary">
                  Add cluster
                </ButtonLink>
              </Callout>
            )
          ) : null}
        </form>
      </Panel>
      {kind === "server" && draft ? (
        <SetupChecklist
          title={`Waiting for ${draft.name}`}
          doneTitle={`${draft.name} is ready`}
          checks={[
            {
              label: "Joined the cluster",
              done: joined,
              detail: joined
                ? "Build-only server"
                : "Waiting for the command…",
            },
            {
              label: "Builder started",
              done: connected,
              detail: connected
                ? [
                    draft.info.hostname,
                    draft.info.cpus ? `${draft.info.cpus} CPU` : "",
                    draft.info.memoryGi ? `${draft.info.memoryGi} GB` : "",
                  ]
                    .filter(Boolean)
                    .join(" · ")
                : draft.statusMessage || undefined,
            },
          ]}
        />
      ) : null}
      <div className="flex justify-end gap-2">
        {kind === "server" ? (
          connected && draft ? (
            <ButtonLink href={`/builders/${draft.id}`} size="sm">
              Open {draft.name}
            </ButtonLink>
          ) : (
            <>
              <Button variant="ghost" size="sm" onClick={cancel}>
                Cancel
              </Button>
              {draft ? (
                <Button size="sm" variant="secondary" onClick={leave}>
                  Finish later
                </Button>
              ) : null}
            </>
          )
        ) : (
          <>
            <Button variant="ghost" size="sm" onClick={cancel}>
              Cancel
            </Button>
            <Button
              size="sm"
              disabled={saving || blocked}
              onClick={() => void add()}
            >
              {saving ? "Adding…" : "Add builder"}
            </Button>
          </>
        )}
      </div>
    </Page>
  );
}
