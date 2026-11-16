import { CaretDownIcon, CircleNotchIcon, CodeIcon, HardDrivesIcon } from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { z } from "zod";
import { RuntimeBadge, useSandboxes } from "~/components/pyo/sandbox/common";
import { AppLink } from "~/components/ui/AppLink";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Input, Segmented, Select, Switch, Textarea } from "~/components/ui/Field";
import { Highlighted } from "~/components/ui/Highlighted";
import { Callout, EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { Reveal } from "~/components/ui/Reveal";
import { CopyButton } from "~/components/ui/Table";
import { parseEnv } from "~/lib/pyo/dotenv";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import {
  defaultTemplate,
  freeLabel,
  freeMemory,
  gettingReady,
  isBase,
  LIFETIMES,
  RUNTIME_LABEL,
  serverRefusal,
  usable,
  SIZES,
  rememberConnection,
  sandboxApi,
  sdkSnippet,
  sizeLabel,
  templateImage,
  type CreateSandbox,
} from "~/lib/pyo/sandbox";
import { projectsIn, roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/sandboxes/new")({
  head: () => ({ meta: seo({ title: "New sandbox" }) }),
  // From a template's Use button.
  validateSearch: z.object({
    template: z.string().optional().catch(undefined),
  }),
  component: NewSandbox,
});

// The new sandbox form. The side box shows the same choices as SDK code.
// "Internet" is the default when one of the team's servers can give
// sandboxes a network.

/** Pause after this long without use, when "Pause when idle" is on. */
const IDLE_SECS = 300;

function pairs(text: string) {
  return Object.fromEntries(parseEnv(text).entries);
}

function NewSandbox() {
  const s = usePyo();
  const servers = sandboxServersIn(s);
  if (servers.some((x) => x.state === "ready")) return <NewSandboxForm />;
  // No server that runs sandboxes yet: the form waits for one.
  const joining = servers.find((x) => x.state === "joining");
  const canManage = ["owner", "admin"].includes(roleIn(s) ?? "");
  return (
    <Page>
      <PageHeader title="New sandbox" />
      <Panel>
        {joining ? (
          <EmptyState
            icon={CircleNotchIcon}
            title={`${joining.name} is being set up.`}
            description="You can make sandboxes once it's ready."
            action={
              <ButtonLink href={`/clusters#sandbox-server-${joining.id}`} size="sm" variant="secondary">
                See {joining.name}
              </ButtonLink>
            }
          />
        ) : (
          <EmptyState
            icon={HardDrivesIcon}
            title="Add a sandbox server first."
            description={
              canManage
                ? "Sandboxes run on your sandbox servers."
                : "Sandboxes run on your sandbox servers. Ask a team owner or admin to add one."
            }
            action={
              canManage ? (
                <ButtonLink href="/sandboxes/servers/new" size="sm">
                  Add a sandbox server
                </ButtonLink>
              ) : undefined
            }
          />
        )}
      </Panel>
    </Page>
  );
}

function NewSandboxForm() {
  const s = usePyo();
  const navigate = useNavigate();
  const team = s.currentTeamId;
  const templates = useQuery(`sandbox-templates:${team}`, () => sandboxApi.templates(team));
  const list = useSandboxes(team);
  const ready = templates.status === "success" ? templates.data.templates : [];
  const search = Route.useSearch();
  const [template, setTemplate] = useState(search.template ?? "");
  const [name, setName] = useState("");
  const servers = sandboxServersIn(s);
  const [server, setServer] = useState("");
  const [size, setSize] = useState<(typeof SIZES)[number]["value"]>("1");
  const [ttl, setTtl] = useState<(typeof LIFETIMES)[number]["value"]>("1h");
  const [idle, setIdle] = useState(false);
  const networked = sandboxServersIn(s).some(
    (v) => v.enabled && v.state === "ready" && v.capability?.network,
  );
  // Follows the servers until picked.
  const [pickedNetwork, setNetwork] = useState<"none" | "internet" | null>(null);
  const network = networked ? (pickedNetwork ?? "internet") : "none";
  const [more, setMore] = useState(false);
  const [env, setEnv] = useState("");
  const [labels, setLabels] = useState("");
  const [projectId, setProjectId] = useState("");
  const [isolation, setIsolation] = useState<"auto" | "vm">("auto");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const picked = (template && ready.some((t) => t.name === template && usable(t)) ? template : "") || defaultTemplate(ready);
  const sz = SIZES.find((x) => x.value === size)!;
  const refusal = (x: (typeof servers)[number]) => serverRefusal(x, sz.memoryMib, network);
  const pinned = servers.find((x) => x.id === server && !refusal(x));
  const pickedBase = ready.find((t) => t.name === picked && isBase(t));
  // While a create waits for its server to make base there.
  const preparing =
    busy && list.status === "success"
      ? list.data.sandboxes.find((x) => x.preparing && x.state === "creating" && x.template === picked)
      : undefined;
  const limits = list.status === "success" ? list.data.limits : null;
  const inUse = list.status === "success" ? list.data.inUse : null;
  const body: CreateSandbox = {
    teamId: team,
    ...(name.trim() ? { name: name.trim() } : {}),
    template: picked,
    ...(pinned ? { server: pinned.id } : {}),
    cpus: sz.cpus,
    memoryMib: sz.memoryMib,
    ttl,
    ...(idle ? { pauseWhenIdleSecs: IDLE_SECS } : {}),
    network,
    ...(isolation !== "auto" ? { isolation } : {}),
    ...(env.trim() ? { env: pairs(env) } : {}),
    ...(labels.trim() ? { labels: pairs(labels) } : {}),
    ...(projectId ? { projectId } : {}),
  };
  const { teamId: _, ...code } = body;

  const create = async () => {
    if (!picked || busy) return;
    setBusy(true);
    setError("");
    try {
      const r = await sandboxApi.create(body);
      rememberConnection(r.connection);
      void navigate({
        to: "/$team/sandboxes/$sandboxId",
        params: { team: currentSlug(), sandboxId: r.sandbox.id },
        search: { tab: "terminal" },
      });
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  };

  const tooBig = (cpus: number, mib: number) =>
    !!limits && !!inUse && (inUse.cpus + cpus > limits.maxCpus || inUse.memoryMib + mib > limits.maxMemoryMib);

  return (
    <Page width="max-w-5xl">
      <PageHeader title="New sandbox" />
      <div className="grid items-start gap-6 lg:grid-cols-[minmax(0,1fr)_340px]">
        <Panel>
          <form
            className="space-y-5 p-5"
            onSubmit={(e) => {
              e.preventDefault();
              void create();
            }}
          >
            <Field label="Name" optional>
              {(p) => (
                <Input {...p} value={name} onChange={(e) => setName(e.target.value)} maxLength={63} placeholder="Generated when empty" />
              )}
            </Field>
            <Field
              label="Template"
              hint={
                ready.length ? (
                  <AppLink href="/sandboxes/templates" className="font-medium text-primary hover:text-primary/80">
                    Sandbox templates
                  </AppLink>
                ) : undefined
              }
            >
              {(p) =>
                templates.status === "loading" ? (
                  <Select {...p} disabled value="">
                    <option value="">Loading…</option>
                  </Select>
                ) : ready.length ? (
                  <Select {...p} value={picked} onChange={(e) => setTemplate(e.target.value)}>
                    {ready.map((t) => (
                      <option key={t.id} value={t.name} disabled={!usable(t)}>
                        {t.name}
                        {templateImage(t) ? ` · ${templateImage(t)}` : ""}
                        {!usable(t) ? ` (${t.state === "unavailable" ? "not available yet" : t.state})` : ""}
                      </option>
                    ))}
                  </Select>
                ) : (
                  <Callout tone={templates.status === "error" ? "danger" : "neutral"}>
                    {templates.status === "error" ? (
                      <>
                        Couldn't load templates.{" "}
                        <button type="button" onClick={templates.retry} className="font-medium underline underline-offset-2">
                          Try again
                        </button>
                      </>
                    ) : (
                      <>
                        No templates yet.{" "}
                        <AppLink href="/sandboxes/templates" className="font-medium text-primary hover:text-primary/80">
                          Add a template
                        </AppLink>
                      </>
                    )}
                  </Callout>
                )
              }
            </Field>
            <Field
              label="Server"
              hint={
                pinned ? (
                  <RuntimeBadge server={pinned} />
                ) : pickedBase && !pickedBase.servers.length ? (
                  "The first sandbox from base on a server takes a minute longer."
                ) : undefined
              }
            >
              {(p) => (
                <Select {...p} value={pinned?.id ?? ""} onChange={(e) => setServer(e.target.value)}>
                  <option value="">Any server</option>
                  {servers.map((x) => {
                    const why = refusal(x);
                    const runtime = x.capability?.runtime;
                    return (
                      <option key={x.id} value={x.id} disabled={!!why}>
                        {x.name}
                        {runtime && runtime !== "none" ? ` · ${RUNTIME_LABEL[runtime]}` : ""}
                        {` · ${why || freeLabel(freeMemory(x))}`}
                      </option>
                    );
                  })}
                </Select>
              )}
            </Field>
            <Field
              label="Size"
              hint={
                limits && inUse
                  ? `In use: ${inUse.cpus} of ${limits.maxCpus} CPUs, ${+(inUse.memoryMib / 1024).toFixed(1)} of ${+(limits.maxMemoryMib / 1024).toFixed(1)} GB.`
                  : undefined
              }
            >
              {() => (
                <Segmented
                  label="Size"
                  value={size}
                  onChange={setSize}
                  options={SIZES.map((x) => ({
                    value: x.value,
                    label: sizeLabel(x.cpus, x.memoryMib),
                    disabled: tooBig(x.cpus, x.memoryMib),
                  }))}
                />
              )}
            </Field>
            <Field label="Lifetime">
              {() => (
                <div className="space-y-3">
                  <Segmented
                    label="Lifetime"
                    value={ttl}
                    onChange={setTtl}
                    options={LIFETIMES.map((x) => ({
                      value: x.value,
                      label: x.label,
                      disabled: !!limits && x.secs > limits.maxTtlSecs,
                    }))}
                  />
                  <Switch
                    checked={idle}
                    onChange={setIdle}
                    label="Pause when idle"
                    description="After 5 minutes without use."
                  />
                </div>
              )}
            </Field>
            <Field label="Network" hint={networked ? undefined : "No sandbox server here has networking."}>
              {() => (
                <Segmented
                  label="Network"
                  value={network}
                  onChange={setNetwork}
                  options={[
                    { value: "internet", label: "Internet", disabled: !networked },
                    { value: "none", label: "No network" },
                  ]}
                />
              )}
            </Field>
            <div className="border-t border-border pt-4">
              <button
                type="button"
                aria-expanded={more}
                onClick={() => setMore(!more)}
                className="flex items-center gap-1.5 text-[13px] font-medium text-muted-foreground hover:text-foreground"
              >
                <CaretDownIcon size={12} className={`transition-transform ${more ? "" : "-rotate-90"}`} />
                Environment, labels and isolation
              </button>
              <Reveal open={more}>
                <div className="space-y-4 pt-4">
                  <Field label="Environment variables" hint="KEY=value, one per line. Pyo keeps only the names." optional>
                    {(p) => (
                      <Textarea {...p} rows={3} className="font-mono text-[12px]" value={env} onChange={(e) => setEnv(e.target.value)} />
                    )}
                  </Field>
                  <Field label="Labels" hint="key=value, one per line." optional>
                    {(p) => (
                      <Textarea {...p} rows={2} className="font-mono text-[12px]" value={labels} onChange={(e) => setLabels(e.target.value)} />
                    )}
                  </Field>
                  <Field label="Project" optional>
                    {(p) => (
                      <Select {...p} value={projectId} onChange={(e) => setProjectId(e.target.value)}>
                        <option value="">None</option>
                        {projectsIn(s).map((x) => (
                          <option key={x.id} value={x.id}>
                            {x.name}
                          </option>
                        ))}
                      </Select>
                    )}
                  </Field>
                  <Field label="Isolation">
                    {() => (
                      <Segmented
                        label="Isolation"
                        value={isolation}
                        onChange={setIsolation}
                        options={[
                          { value: "auto", label: "Auto" },
                          { value: "vm", label: "VM only" },
                        ]}
                      />
                    )}
                  </Field>
                </div>
              </Reveal>
            </div>
            {error ? <Callout tone="danger">{error}</Callout> : null}
            <div className="flex justify-end gap-2 border-t border-border pt-4">
              <ButtonLink href="/sandboxes" variant="ghost" size="sm">
                Cancel
              </ButtonLink>
              <Button type="submit" size="sm" disabled={!picked || busy}>
                {preparing ? gettingReady(picked, preparing.server) : busy ? "Creating…" : "Create sandbox"}
              </Button>
            </div>
          </form>
        </Panel>
        <FromCode code={sdkSnippet(code)} template={picked} />
      </div>
    </Page>
  );
}

function FromCode({ code, template }: { code: string; template: string }) {
  const s = usePyo();
  const origin = (s.email?.publicUrl || (typeof window === "undefined" ? "" : window.location.origin)).replace(/\/$/, "");
  const mcp = `claude mcp add --transport http pyo ${origin}/api/mcp`;
  return (
    <Panel
      title={
        <span className="flex items-center gap-2">
          <CodeIcon size={14} /> Use it from code
        </span>
      }
    >
      <div className="space-y-4 p-4 text-[12.5px]">
        <div>
          <div className="mb-1.5 flex items-center justify-between">
            <span className="font-medium">TypeScript</span>
            <CopyButton value={code} label="Copy code" />
          </div>
          <pre className="overflow-x-auto rounded-lg border border-border bg-muted/40 p-3 font-mono text-[11.5px] leading-[18px]">
            <Highlighted text={code} lang="js" />
          </pre>
          <p className="mt-1.5 text-muted-foreground">
            <span className="font-mono">npm i @pyodev/sandbox</span>, with an{" "}
            <a href="/api-keys" className="font-medium text-primary hover:text-primary/80">
              API key
            </a>{" "}
            that can use sandboxes.
          </p>
        </div>
        <div>
          <div className="mb-1.5 flex items-center justify-between">
            <span className="font-medium">MCP</span>
            <CopyButton value={mcp} label="Copy command" />
          </div>
          <pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-lg border border-border bg-muted/40 p-3 font-mono text-[11.5px] leading-[18px]">
            {mcp}
          </pre>
          <p className="mt-1.5 text-muted-foreground">
            Then ask for a sandbox{template ? ` from ${template}` : ""}.
          </p>
        </div>
      </div>
    </Panel>
  );
}
