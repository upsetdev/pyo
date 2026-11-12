import { RouteTabs } from "~/components/ui/Table";
import { DeleteServiceDialog } from "~/components/pyo/VolumeDialogs";
import { routeTabs, serviceTabs } from "~/lib/pyo/tabs";
import {
  ArrowClockwiseIcon,
  ArrowUpRightIcon,
  ArrowsOutLineHorizontalIcon,
  ChartLineUpIcon,
  DotsThreeIcon,
  MinusIcon,
  PlusIcon,
  RocketLaunchIcon,
  StopIcon,
  TerminalWindowIcon,
  TrashIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import { ReplicaPicker } from "~/components/pyo/ReplicaPicker";
import { copyIdItem } from "~/components/pyo/copyId";
import {
  createFileRoute,
  Link,
  Outlet,
  useNavigate,
} from "@tanstack/react-router";
import { useState } from "react";
import { Button, ButtonLink, buttonClass } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Stepper, Switch } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Page } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import {
  activeDeploymentFor,
  api,
  desiredReplicas,
  usePyo,
  projectParams,
  servicePath,
  serviceIdOf,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application } from "~/lib/pyo/types";
import { cpu, domainUrl, mem } from "~/lib/pyo/format";
import { seo } from "~/lib/seo";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId")({
  head: () => ({ meta: seo({ title: "Service" }) }),
  component: AppLayout,
});

function AppLayout() {
  const serviceId = serviceIdOf(Route.useParams());
  const s = usePyo();
  const navigate = useNavigate();
  const [confirm, setConfirm] = useState<"stop" | "restart" | "delete" | null>(
    null,
  );
  const [scaling, setScaling] = useState(false);
  const app = s.services.find((a) => a.id === serviceId);

  if (!app || app.teamId !== s.currentTeamId)
    return (
      <Page>
        <EmptyState
          icon={RocketLaunchIcon}
          title="Service not found"
          description={
            app ? "It belongs to another team." : "It may have been deleted."
          }
          action={
            app ? (
              <Button size="sm" onClick={() => api.switchTeam(app.teamId)}>
                Switch team
              </Button>
            ) : (
              <ButtonLink href="/projects" size="sm" variant="secondary">
                Projects
              </ButtonLink>
            )
          }
        />
      </Page>
    );

  const project = s.projects.find((p) => p.id === app.projectId);
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  const active = activeDeploymentFor(s, app.id);
  const current = !!app.currentDeploymentId;
  const base = servicePath(app.id);

  const deploy = () => {
    const id = api.deploy(app.id, {
      trigger: app.pendingChanges ? "config_change" : "redeploy",
    });
    void navigate({
      to: "/$team/deployments/$deploymentId",
      params: { team: currentSlug(), deploymentId: id },
    });
  };

  const domain = app.domains.find((d) => d.status === "active");

  return (
    <Page>
      <div className="space-y-6">
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="min-w-0">
            <h1 className="truncate text-[24px] font-semibold tracking-tight">
              {app.name}
            </h1>
            <p className="mt-1 text-[13px] text-muted-foreground">
              Service in{" "}
              {project ? (
                <Link
                  to="/$team/$projectId"
                  params={{ team: currentSlug(), ...projectParams(project.id) }}
                  className="text-foreground hover:text-primary"
                >
                  {project.name}
                </Link>
              ) : (
                "a deleted project"
              )}
              {cluster ? (
                <>
                  {" "}
                  on{" "}
                  <Link
                    to="/$team/clusters/$clusterId"
                    params={{ team: currentSlug(), clusterId: cluster.name }}
                    className="text-foreground hover:text-primary"
                  >
                    {cluster.name}
                  </Link>
                </>
              ) : null}
            </p>
          </div>
          <div className="flex items-center gap-2">
            {domain && app.status !== "stopped" ? (
              <a
                href={domainUrl(domain)}
                target="_blank"
                rel="noreferrer"
                className={buttonClass("secondary", "sm")}
              >
                Visit <ArrowUpRightIcon size={12} weight="bold" />
              </a>
            ) : null}
            {current &&
            app.status !== "stopped" &&
            app.workloadKind !== "CronJob" ? (
              <ButtonLink
                href={`${base}/terminal`}
                size="sm"
                variant="secondary"
                title="Open a shell in a running replica"
              >
                <TerminalWindowIcon size={13} /> Terminal
              </ButtonLink>
            ) : null}
            {current && app.workloadKind !== "CronJob" ? (
              <Button
                size="sm"
                variant="secondary"
                disabled={!!active}
                title={
                  active
                    ? "A deploy is running"
                    : "Change how many replicas run"
                }
                onClick={() => setScaling(true)}
              >
                <ArrowsOutLineHorizontalIcon size={13} /> Scale
              </Button>
            ) : null}
            {active ? (
              <ButtonLink
                href={`/deployments/${active.id}`}
                size="sm"
                variant="secondary"
              >
                View rollout
              </ButtonLink>
            ) : (
              <Button size="sm" onClick={deploy}>
                {app.status === "stopped" ? "Start" : "Deploy"}
              </Button>
            )}
            <Menu
              label="More actions"
              items={[
                {
                  label: app.workloadKind === "CronJob" ? "Run now" : "Restart",
                  icon: <ArrowClockwiseIcon size={14} />,
                  disabled: app.status === "stopped" || !!active,
                  onSelect: () => setConfirm("restart"),
                },
                {
                  label: app.workloadKind === "CronJob" ? "Pause" : "Stop",
                  icon: <StopIcon size={14} />,
                  disabled: app.status === "stopped" || !!active,
                  onSelect: () => setConfirm("stop"),
                },
                {
                  label: "View analytics",
                  icon: <ChartLineUpIcon size={14} />,
                  href: `${base}/analytics`,
                },
                copyIdItem("Service", app.id),
                copyIdItem("Project", app.projectId),
                { type: "separator" },
                {
                  label: "Delete",
                  icon: <TrashIcon size={14} />,
                  danger: true,
                  onSelect: () => setConfirm("delete"),
                },
              ]}
              trigger={(t) => (
                <Button {...t} variant="secondary" size="icon">
                  <DotsThreeIcon size={16} weight="bold" />
                </Button>
              )}
            />
          </div>
        </div>
        {app.pendingChanges && !active ? (
          <StatusNote
            label="Unapplied changes"
            tone="warning"
            icon={<WarningIcon size={14} weight="bold" />}
            title="Changes not live yet"
            action={
              <Button size="sm" onClick={deploy}>
                Redeploy
              </Button>
            }
          >
            You changed the settings. Redeploy to apply them.
          </StatusNote>
        ) : null}
        <RouteTabs
          label="Service sections"
          tabs={routeTabs(serviceTabs(app), base)}
        />
      </div>
      <Outlet />
      <ScaleDialog
        key={String(scaling)}
        app={app}
        open={scaling}
        onClose={() => setScaling(false)}
      />
      <ConfirmDialog
        open={confirm === "restart"}
        onClose={() => setConfirm(null)}
        tone="primary"
        title={
          app.workloadKind === "CronJob"
            ? `Run ${app.name} now?`
            : `Restart ${app.name}?`
        }
        description="One replica at a time."
        confirmLabel={app.workloadKind === "CronJob" ? "Run now" : "Restart"}
        onConfirm={() => api.restartService(app.id)}
      />
      <ConfirmDialog
        open={confirm === "stop"}
        onClose={() => setConfirm(null)}
        title={`Stop ${app.name}?`}
        description="Scales to zero. Data and settings are kept."
        confirmLabel={app.workloadKind === "CronJob" ? "Pause" : "Stop"}
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
    </Page>
  );
}

/**
 * How many replicas run: a number you pick, or a range Pyo moves within as CPU
 * use changes. Applied in place: running pods aren't restarted.
 */
function ScaleDialog({
  app,
  open,
  onClose,
}: {
  app: Application;
  open: boolean;
  onClose: () => void;
}) {
  const s = usePyo();
  const stopped = app.status === "stopped";
  const live = desiredReplicas(app);
  const [n, setN] = useState(
    stopped ? (app.autoscale?.min ?? 1) : app.replicas,
  );
  const [auto, setAuto] = useState(!!app.autoscale);
  const [min, setMin] = useState(app.autoscale?.min ?? Math.max(1, live));
  const [max, setMax] = useState(app.autoscale?.max ?? Math.max(1, live) * 3);
  const [targetCpu, setTargetCpu] = useState(app.autoscale?.targetCpu ?? 70);
  const [byMemory, setByMemory] = useState(!!app.autoscale?.targetMemory);
  const [targetMemory, setTargetMemory] = useState(
    app.autoscale?.targetMemory ?? 80,
  );
  const [saving, setSaving] = useState(false);
  const single =
    app.workloadKind === "StatefulSet" ||
    s.volumes.some((v) => v.serviceId === app.id);
  const gpus = app.resources.gpu ?? 0;
  const autoscaling = auto && !stopped;
  const turningOff = !!app.autoscale && !auto && !stopped;
  const invalid =
    !(min >= 1 && max <= 50 && min < max) ||
    !(targetCpu >= 10 && targetCpu <= 95) ||
    (byMemory && !(targetMemory >= 10 && targetMemory <= 95));
  const same = autoscaling
    ? !!app.autoscale &&
      app.autoscale.min === min &&
      app.autoscale.max === max &&
      app.autoscale.targetCpu === targetCpu &&
      (app.autoscale.targetMemory ?? null) === (byMemory ? targetMemory : null)
    : !stopped && !turningOff && n === app.replicas;
  const blocked = same || saving || (autoscaling && invalid);
  const perPod = `${cpu(app.resources.cpuLimit)} and ${mem(app.resources.memoryLimit)}`;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={`Scale ${app.name}`}
      description={
        stopped
          ? "It's stopped. Pick how many replicas to start."
          : "Running replicas keep serving."
      }
      onSubmit={async () => {
        if (blocked) return;
        setSaving(true);
        const ok = autoscaling
          ? await api.setAutoscale(app.id, {
              min,
              max,
              targetCpu,
              ...(byMemory ? { targetMemory } : {}),
            })
          : turningOff
            ? await api.setAutoscale(app.id, null)
            : await api.scaleService(app.id, n);
        setSaving(false);
        if (!ok) return;
        toast({
          title: autoscaling
            ? `Autoscaling ${app.name} between ${min} and ${max}`
            : turningOff
              ? `Stopped autoscaling ${app.name}`
              : `Scaling ${app.name} to ${n}`,
        });
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={blocked}>
            {saving
              ? "Saving…"
              : autoscaling
                ? "Save autoscaling"
                : turningOff
                  ? "Turn off autoscaling"
                  : stopped
                    ? `Start ${n}`
                    : `Scale to ${n}`}
          </Button>
        </>
      }
    >
      <div className="space-y-5">
        {stopped ? null : (
          <Switch
            checked={auto}
            onChange={setAuto}
            disabled={(single || gpus > 0) && !app.autoscale}
            label="Autoscale"
            description={
              single
                ? "Services with a volume can't autoscale."
                : gpus > 0
                  ? "Services with GPUs can't autoscale."
                  : "Add and remove replicas as CPU use changes."
            }
          />
        )}
        {autoscaling ? (
          <div className="space-y-4">
            <div className="grid grid-cols-3 gap-3">
              <Field label="Minimum">
                {(p) => (
                  <Stepper {...p} min={1} max={49} value={min} onChange={setMin} />
                )}
              </Field>
              <Field label="Maximum">
                {(p) => (
                  <Stepper {...p} min={2} max={50} value={max} onChange={setMax} />
                )}
              </Field>
              <Field label="CPU target (%)">
                {(p) => (
                  <Stepper {...p} min={10} max={95} step={5} suffix="%" value={targetCpu} onChange={setTargetCpu} />
                )}
              </Field>
            </div>
            <div className="space-y-3 border-t border-border pt-4">
              <Switch
                checked={byMemory}
                onChange={setByMemory}
                label="Also scale on memory"
                description="The busier of CPU and memory decides."
              />
              {byMemory ? (
                <Field label="Memory target (%)" className="max-w-[180px]">
                  {(p) => (
                    <Stepper {...p} min={10} max={95} step={5} suffix="%" value={targetMemory} onChange={setTargetMemory} />
                  )}
                </Field>
              ) : null}
            </div>
          </div>
        ) : turningOff ? null : (
          <div className="py-2">
            <ReplicaPicker value={n} onChange={setN} />
          </div>
        )}
        <p className="rounded-lg bg-muted/50 px-3 py-2.5 text-[12.5px] text-muted-foreground">
          {autoscaling ? (
            invalid ? (
              "1 to 50 replicas, max above min, targets 10% to 95%."
            ) : (
              <>
                Replicas are added when average CPU use goes above {targetCpu}%
                of the {cpu(app.resources.cpuRequest)} each one requests, and
                removed a few minutes after it drops. Now{" "}
                {app.runtime.readyReplicas} of {live} ready.
              </>
            )
          ) : turningOff ? (
            `It stays at ${app.runtime.readyReplicas || 1} until you change it.`
          ) : (
            <>
              Now{" "}
              {stopped
                ? "stopped"
                : `${app.runtime.readyReplicas} of ${app.replicas} ready`}
              . Each replica gets up to {perPod}
              {n > 1 && app.domains.length
                ? ", and traffic is shared between them"
                : ""}
              .
              {app.workloadKind === "StatefulSet" || app.kind === "service"
                ? " Databases and other services with a volume usually run as one replica."
                : ""}
            </>
          )}
        </p>
      </div>
    </Dialog>
  );
}
