import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { DeleteServiceDialog } from "~/components/pyo/VolumeDialogs";
import { servicePath } from "~/lib/pyo/store";
import { RESERVED_SERVICE_NAMES } from "~/lib/pyo/paths";
import { TagInput } from "~/components/ui/TagInput";
import { DeployHooksPanel } from "~/components/pyo/DeployHooks";
import { serviceSettingsNav, SettingsFrame } from "~/components/pyo/SettingsFrame";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { DNS_LABEL } from "~/lib/pyo/format";
import { api, usePyo, projectParams, serviceIdOf } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, ServiceSource } from "~/lib/pyo/types";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/settings")(
  {
    component: () => {
      const serviceId = serviceIdOf(Route.useParams());
      const app = usePyo().services.find((a) => a.id === serviceId);
      return app ? (
        <SettingsFrame items={serviceSettingsNav(app.id)}>
          <AppSettings key={app.id} app={app} />
        </SettingsFrame>
      ) : null;
    },
  },
);

/** When a cron job runs, and what. Applied by the next deploy. */
function CronSettings({ app }: { app: Application }) {
  const [schedule, setSchedule] = useState(app.schedule ?? "");
  const [command, setCommand] = useState(app.command ?? "");
  const dirty =
    schedule !== (app.schedule ?? "") || command !== (app.command ?? "");
  const valid = schedule.trim().split(/\s+/).length === 5;
  return (
    <Panel
      title="Schedule"
      actions={
        <Button
          size="xs"
          disabled={!dirty || !valid}
          onClick={() => {
            api.updateService(
              app.id,
              { schedule: schedule.trim(), command: command.trim() },
              { requiresRedeploy: true },
            );
            toast({ title: "Saved", description: "Redeploy to roll it out." });
          }}
        >
          Save
        </Button>
      }
    >
      <div className="grid gap-4 p-4 sm:grid-cols-2">
        <Field
          label="Schedule"
          hint="Cron syntax, in UTC."
          error={dirty && !valid ? "Use five fields, like 0 3 * * *." : undefined}
        >
          {(p) => (
            <Input
              {...p}
              mono
              value={schedule}
              onChange={(e) => setSchedule(e.target.value)}
            />
          )}
        </Field>
        <Field label="Command" optional hint="Runs the image's default if empty.">
          {(p) => (
            <Input
              {...p}
              mono
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              placeholder="npm run cleanup"
            />
          )}
        </Field>
      </div>
    </Panel>
  );
}

function AppSettings({ app }: { app: Application }) {
  const s = usePyo();
  const navigate = useNavigate();
  const [name, setName] = useState(app.name);
  const [serverTags, setServerTags] = useState<string[]>(app.serverTags ?? []);
  const servers = s.servers.filter((n) => n.clusterId === app.clusterId);
  const knownTags = [...new Set(servers.flatMap((n) => n.tags ?? []))].sort();
  const matching = servers.filter((n) =>
    serverTags.every((t) => n.tags?.includes(t)),
  ).length;
  const [source, setSource] = useState<ServiceSource>(app.source);
  const [confirm, setConfirm] = useState<"stop" | "restart" | "delete" | null>(
    null,
  );
  const taken = s.services.some(
    (a) => a.id !== app.id && a.projectId === app.projectId && a.name === name,
  );
  const nameError = !DNS_LABEL.test(name)
    ? "Lowercase letters, digits and dashes."
    : taken
      ? "Another service in this project uses this name."
      : name !== app.name && RESERVED_SERVICE_NAMES.includes(name)
        ? "A project page uses that name. Pick another."
        : "";
  const generalDirty =
    name !== app.name || serverTags.join() !== (app.serverTags ?? []).join();
  const sourceDirty = JSON.stringify(source) !== JSON.stringify(app.source);

  return (
    <div className="space-y-4">
      <Panel title="General">
        <div className="grid items-start gap-4 p-4 sm:grid-cols-2">
          <Field
            label="Name"
            error={nameError || undefined}
            hint="Also changes its internal hostname on the next deploy."
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
            label="Server tags"
            optional
            hint={
              serverTags.length
                ? matching
                  ? `Runs on ${matching} of ${servers.length} servers.`
                  : "No server has these tags yet."
                : "Runs only on servers with these tags."
            }
          >
            {(p) => (
              <TagInput
                id={p.id}
                aria-label="Server tags"
                value={serverTags}
                onChange={setServerTags}
                suggestions={knownTags}
                placeholder="gpu"
              />
            )}
          </Field>
        </div>
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={!generalDirty || !!nameError}
            onClick={() => {
              api.updateService(
                app.id,
                {
                  name,
                  serverTags: serverTags.length ? serverTags : undefined,
                },
                { requiresRedeploy: true, label: "general settings" },
              );
              toast({ title: "Settings saved" });
              // Its address follows its name.
              if (name !== app.name)
                void navigate({
                  href: `/${currentSlug()}${servicePath(app.id, "settings")}`,
                  replace: true,
                });
            }}
          >
            Save
          </Button>
        </div>
      </Panel>

      <Panel title="Deployment source">
        <div className="grid gap-4 p-4 sm:grid-cols-2">
          {source.type === "git" ? (
            <>
              <Field label="Repository" className="sm:col-span-2">
                {(p) => (
                  <Input
                    {...p}
                    mono
                    value={source.repoUrl}
                    onChange={(e) =>
                      setSource({ ...source, repoUrl: e.target.value })
                    }
                  />
                )}
              </Field>
              <Field label="Branch">
                {(p) => (
                  <Input
                    {...p}
                    mono
                    value={source.branch}
                    onChange={(e) =>
                      setSource({ ...source, branch: e.target.value })
                    }
                  />
                )}
              </Field>
              {source.builder === "buildpacks" ? (
                <Field label="Build">
                  {(p) => (
                    <Input
                      {...p}
                      value={`Buildpacks${source.buildpackLanguage ? ` · ${source.buildpackLanguage}` : ""}`}
                      disabled
                    />
                  )}
                </Field>
              ) : (
                <Field
                  label="Dockerfile"
                  hint="The default also finds a Containerfile."
                >
                  {(p) => (
                    <Input
                      {...p}
                      mono
                      value={source.dockerfilePath}
                      onChange={(e) =>
                        setSource({ ...source, dockerfilePath: e.target.value })
                      }
                    />
                  )}
                </Field>
              )}
            </>
          ) : (
            <>
              <Field label="Image" className="sm:col-span-2">
                {(p) => (
                  <Input
                    {...p}
                    mono
                    value={source.image}
                    disabled={source.type === "template"}
                    onChange={(e) =>
                      source.type === "image" &&
                      setSource({ ...source, image: e.target.value })
                    }
                  />
                )}
              </Field>
              <Field
                label="Tag or digest"
                hint={
                  source.type === "template"
                    ? "Upgrading a stateful service can require a data migration."
                    : undefined
                }
              >
                {(p) => (
                  <Input
                    {...p}
                    mono
                    value={source.reference}
                    onChange={(e) =>
                      setSource({ ...source, reference: e.target.value.trim() })
                    }
                  />
                )}
              </Field>
            </>
          )}
        </div>
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={!sourceDirty}
            onClick={() => {
              api.updateService(
                app.id,
                { source },
                { requiresRedeploy: true, label: "deployment source" },
              );
              toast({
                title: "Source saved",
                description: "Redeploy to use it.",
              });
            }}
          >
            Save
          </Button>
        </div>
      </Panel>

      {app.source.type === "git" && app.source.installationId ? (
        <Panel title="Git">
          <div className="p-4">
            <Switch
              checked={app.source.autoDeploy !== false}
              onChange={(on) => {
                if (app.source.type !== "git") return;
                api.updateService(app.id, {
                  source: { ...app.source, autoDeploy: on },
                });
                toast({
                  title: on ? "Deploying on push" : "Not deploying on push",
                });
              }}
              label={`Deploy every push to ${app.source.branch}`}
              description="Paused while rolled back or pinned to a commit."
            />
          </div>
        </Panel>
      ) : null}

      <DeployHooksPanel projectId={app.projectId} serviceId={app.id} />

      {app.workloadKind === "CronJob" ? <CronSettings app={app} /> : null}
      <Panel title="Rollbacks">
        <div className="p-4">
          <Switch
            checked={app.autoRollback !== false}
            onChange={(on) => {
              api.updateService(app.id, { autoRollback: on });
              toast({
                title: on ? "Automatic rollback on" : "Automatic rollback off",
              });
            }}
            label="Roll back automatically when a deploy fails"
            description="Restores the live version if a deploy fails."
          />
        </div>
      </Panel>

      <Panel title="Danger zone" tone="danger">
        <ul className="list-rows">
          {[
            {
              k: "restart" as const,
              title: app.workloadKind === "CronJob" ? "Run now" : "Restart",
              body:
                app.workloadKind === "CronJob"
                  ? "Start a run now, outside its schedule."
                  : "One replica at a time.",
              disabled: app.status === "stopped",
            },
            {
              k: "stop" as const,
              title: app.workloadKind === "CronJob" ? "Pause" : "Stop",
              body:
                app.workloadKind === "CronJob"
                  ? "Stop its scheduled runs. Settings are kept."
                  : "Scale to zero replicas. Data and settings are kept.",
              disabled: app.status === "stopped",
            },
            {
              k: "delete" as const,
              title: "Delete",
              body: "Remove the service and its history from Pyo.",
            },
          ].map((a) => (
            <li
              key={a.k}
              className="flex flex-wrap items-center justify-between gap-3 px-4 py-3"
            >
              <div>
                <p className="text-[13px] font-medium">{a.title}</p>
                <p className="text-[12px] text-muted-foreground">{a.body}</p>
              </div>
              <Button
                variant="secondary"
                size="sm"
                disabled={a.disabled}
                className={a.k === "delete" ? "text-destructive" : ""}
                onClick={() => setConfirm(a.k)}
              >
                {a.title}
              </Button>
            </li>
          ))}
        </ul>
      </Panel>
      <ConfirmDialog
        open={confirm === "restart"}
        onClose={() => setConfirm(null)}
        tone="primary"
        title={`Restart ${app.name}?`}
        description="One replica at a time."
        confirmLabel="Restart"
        onConfirm={() => api.restartService(app.id)}
      />
      <ConfirmDialog
        open={confirm === "stop"}
        onClose={() => setConfirm(null)}
        title={`Stop ${app.name}?`}
        description="Settings and data are kept."
        confirmLabel="Stop"
        onConfirm={() => {
          api.stopService(app.id);
          toast({ title: `Stopping ${app.name}` });
        }}
      />
      <DeleteServiceDialog
        open={confirm === "delete"}
        onClose={() => setConfirm(null)}
        app={app}
        onDeleted={() => {
          void navigate({
            to: "/$team/$projectId",
            params: { team: currentSlug(), ...projectParams(app.projectId) },
          });
        }}
      />
    </div>
  );
}
