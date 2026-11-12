import { Layered } from "~/components/ui/Layered";
import {
  ArrowsClockwiseIcon,
  CheckCircleIcon,
  CircleNotchIcon,
  DotsThreeIcon,
  HammerIcon,
  KeyIcon,
  MinusCircleIcon,
  TerminalWindowIcon,
  TrashIcon,
  WarningCircleIcon,
} from "@phosphor-icons/react";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { DeploymentStatusBadge, Dot } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Page, Panel } from "~/components/ui/Panel";
import { StatusNote, type NoteTone } from "~/components/ui/StatusNote";
import { CopyButton } from "~/components/ui/Table";
import { BUILDER_STATUS, builderWhere, removeCopy } from "~/lib/pyo/builders";
import { BuilderCommandDialog } from "~/components/pyo/BuilderInstall";
import { duration, timeAgo } from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import { isActive } from "~/lib/pyo/simulation";
import {
  api,
  buildersIn,
  usePyo,
  type PyoState,
  clusterRef,
  getState,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Builder } from "~/lib/pyo/types";
import { buildFileOf } from "~/lib/pyo/detect";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/builders/$builderId")({
  head: () => ({ meta: seo({ title: "Builder" }) }),
  component: BuilderPage,
});

// One builder: is it connected, where it runs, and what it built lately.

function BuilderPage() {
  const { builderId } = Route.useParams();
  const s = usePyo();
  const navigate = useNavigate();
  const [showCommand, setShowCommand] = useState(false);
  const [removing, setRemoving] = useState(false);
  const b = buildersIn(s).find((x) => x.id === builderId);

  if (!b)
    return (
      <Page>
        <EmptyState
          icon={HammerIcon}
          title="Builder not found"
          description="It may have been removed."
          action={
            <ButtonLink href="/builders" size="sm" variant="secondary">
              All builders
            </ButtonLink>
          }
        />
      </Page>
    );

  const cluster = s.clusters.find((c) => c.id === b.clusterId);
  const st = BUILDER_STATUS[b.status];
  const builds = s.deployments
    .filter((d) => d.builderId === b.id)
    .sort((x, y) => y.createdAt - x.createdAt);
  const building = builds.find((d) => isActive(d.status));

  return (
    <Page width="max-w-5xl">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="flex items-center gap-3 truncate text-[24px] font-semibold tracking-tight">
            {b.name}
            <span className="inline-flex items-center gap-1.5 text-[13px] font-normal text-muted-foreground">
              <Dot tone={st.tone} className="h-2 w-2" />
              {b.info.busyWith && b.status === "ready" ? "Building" : st.label}
            </span>
          </h1>
          <p className="mt-1 text-[13px] text-muted-foreground">
            {b.kind === "cluster"
              ? "Builder in a cluster"
              : "Builder on a build-only server"}{" "}
            · {builderWhere(b, cluster?.name)}
          </p>
        </div>
        <Menu
          label="More actions"
          items={[
            ...(b.kind === "server"
              ? [
                  {
                    label: "Show install command",
                    icon: <TerminalWindowIcon size={14} />,
                    onSelect: () => setShowCommand(true),
                  },
                ]
              : []),
            {
              label: "Remove builder",
              icon: <TrashIcon size={14} />,
              danger: true,
              disabled: b.status === "removing",
              onSelect: () => setRemoving(true),
            },
          ]}
          trigger={(t) => (
            <Button {...t} variant="secondary" size="icon">
              <DotsThreeIcon size={16} weight="bold" />
            </Button>
          )}
        />
      </div>

      <BuilderStatus
        b={b}
        buildingId={building?.id}
        onShowCommand={() => setShowCommand(true)}
      />

      <Layered bare>
        <section
          aria-label="Connection"
          className="grid gap-x-8 gap-y-5 rounded-[14px] border border-card-border bg-card p-5 sm:grid-cols-2 lg:grid-cols-3"
        >
          <Field label="Status">
            <span className="inline-flex items-center gap-1.5">
              <Dot tone={st.tone} /> {st.label}
            </span>
          </Field>
          <Field label="Last heartbeat">
            {b.lastSeenAt ? (
              timeAgo(b.lastSeenAt)
            ) : (
              <span className="text-muted-foreground">Never</span>
            )}
          </Field>
          <Field label="BuildKit">
            {b.status === "ready" ? (
              "Answering"
            ) : b.status === "error" ? (
              <span className="text-destructive">Not answering</span>
            ) : (
              <span className="text-muted-foreground">Unknown</span>
            )}
          </Field>
          <Field label="Runs on">
            {b.kind === "cluster" && b.server ? (
              <Link
                to="/$team/clusters/$clusterId/$serverId"
                params={{
                  team: currentSlug(),
                  clusterId: clusterRef(getState(), b.clusterId ?? ""),
                  serverId: b.server,
                }}
                className="hover:text-primary"
              >
                {b.server}
              </Link>
            ) : (
              "Build-only server"
            )}
            {cluster ? (
              <span className="text-muted-foreground">
                {" "}
                in{" "}
                <Link
                  to="/$team/clusters/$clusterId"
                  params={{ team: currentSlug(), clusterId: cluster.name }}
                  className="hover:text-primary"
                >
                  {cluster.name}
                </Link>
              </span>
            ) : null}
          </Field>
          <Field
            label={b.kind === "server" ? "Hostname" : "Address"}
          >
            {b.kind !== "cluster" ? (
              <Mono value={b.info.hostname ?? "—"} />
            ) : (
              <Mono
                value={
                  s.servers.find(
                    (n) => n.name === b.server && n.clusterId === b.clusterId,
                  )?.internalIP ?? "—"
                }
              />
            )}
          </Field>
          <Field label="Capacity">
            {b.info.cpus ? (
              `${b.info.cpus} CPU · ${b.info.memoryGi} GB`
            ) : (
              <span className="text-muted-foreground">Not reported yet</span>
            )}
          </Field>
          <Field label="Builder version">
            {b.info.version ?? <span className="text-muted-foreground">—</span>}
          </Field>
          <Field label="Added">{timeAgo(b.createdAt)}</Field>
          <Field label="Builds">
            {builds.length} {builds.length === 1 ? "build" : "builds"}
            {builds.length ? (
              <span className="text-muted-foreground">
                {" "}
                · {pctOk(builds)} succeeded
              </span>
            ) : null}
          </Field>
        </section>
      </Layered>

      <Panel title="Recent builds">
        {builds.length ? (
          <ul className="list-rows">
            {builds.slice(0, 10).map((d) => {
              const app = s.services.find((a) => a.id === d.serviceId);
              const how =
                d.source.type === "git" && d.source.buildpackLanguage
                  ? d.source.builder === "railpack"
                    ? `${d.source.buildpackLanguage} · zero config`
                    : buildFileOf(d.source.buildpackLanguage)
                  : null;
              return (
                <li key={d.id}>
                  <Link
                    to="/$team/deployments/$deploymentId"
                    params={{ team: currentSlug(), deploymentId: d.id }}
                    className="flex items-center gap-3 px-4 py-3 text-[13px] transition-colors hover:bg-accent/40"
                  >
                    <span className="w-28 shrink-0">
                      <DeploymentStatusBadge status={d.status} />
                    </span>
                    <span className="min-w-0 flex-1 truncate">
                      <span className="font-medium">
                        {app?.name ?? "Deleted service"}
                      </span>
                      {d.revisionLabel ? (
                        <span className="ml-2 font-mono text-[12px] text-muted-foreground">
                          {d.revisionLabel}
                        </span>
                      ) : null}
                      {how ? (
                        <span className="ml-2 rounded-md bg-muted px-1.5 py-px text-[11.5px] text-muted-foreground">
                          {how}
                        </span>
                      ) : null}
                    </span>
                    <span className="hidden w-16 shrink-0 text-right text-[12.5px] tabular-nums text-muted-foreground sm:block">
                      {isActive(d.status)
                        ? "running"
                        : duration((d.finishedAt ?? d.createdAt) - d.createdAt)}
                    </span>
                    <span className="w-20 shrink-0 text-right text-[12.5px] tabular-nums text-muted-foreground">
                      {timeAgo(d.createdAt)}
                    </span>
                  </Link>
                </li>
              );
            })}
          </ul>
        ) : (
          <p className="px-4 py-6 text-[13px] text-muted-foreground">
            No builds yet.
          </p>
        )}
      </Panel>

      <Panel title="How builds run here">
        <ol className="space-y-2.5 px-4 py-4 text-[13px] text-muted-foreground">
          <Step n={1}>Checks out the commit.</Step>
          <Step n={2}>
            Dockerfile, or the framework detected.
          </Step>
          <Step n={3}>Runs each step in a sandboxed container.</Step>
          <Step n={4}>Pushes the image to Pyo's registry.</Step>
        </ol>
      </Panel>

      <BuilderCommandDialog
        builder={b}
        open={showCommand}
        onClose={() => setShowCommand(false)}
      />
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title={`Remove ${b.name}?`}
        description={removeCopy(b)}
        confirmLabel="Remove builder"
        onConfirm={async () => {
          if (!(await api.removeBuilder(b.id))) return;
          toast({ title: `Removing ${b.name}` });
          void navigate({
            to: "/$team/builders",
            params: { team: currentSlug() },
          });
        }}
      />
    </Page>
  );
}

const pctOk = (builds: { status: string }[]) => {
  const done = builds.filter(
    (d) =>
      d.status === "succeeded" ||
      d.status === "failed" ||
      d.status === "cancelled",
  );
  return done.length
    ? `${Math.round((done.filter((d) => d.status === "succeeded").length / done.length) * 100)}%`
    : "–";
};

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <p className="mb-1 text-[12px] text-muted-foreground">{label}</p>
      <div className="truncate text-[13px]">{children}</div>
    </div>
  );
}

function Mono({ value }: { value: string }) {
  return (
    <span className="inline-flex min-w-0 items-center gap-1">
      <span className="truncate font-mono text-[12px]">{value}</span>
      {value !== "—" ? <CopyButton value={value} label="Copy" /> : null}
    </span>
  );
}

function Step({ n, children }: { n: number; children: ReactNode }) {
  return (
    <li className="flex gap-3">
      <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-muted text-[11px] font-medium text-foreground">
        {n}
      </span>
      <span>{children}</span>
    </li>
  );
}

/** The builder's state in a sentence, with the one thing to do about it. */
function BuilderStatus({
  b,
  buildingId,
  onShowCommand,
}: {
  b: Builder;
  buildingId?: string;
  onShowCommand: () => void;
}) {
  const seen = b.lastSeenAt ? timeAgo(b.lastSeenAt) : "never";
  let tone: NoteTone = "neutral";
  let icon: ReactNode = <MinusCircleIcon size={15} />;
  let title = "";
  let body: ReactNode = null;
  let action: ReactNode;
  if (b.status === "ready" && (b.info.busyWith || buildingId)) {
    tone = "progress";
    icon = <CircleNotchIcon size={15} className="animate-spin" />;
    title = "Building";
    body = "A build is running here now.";
    action = buildingId ? (
      <ButtonLink
        href={`/deployments/${buildingId}`}
        size="xs"
        variant="secondary"
      >
        See logs
      </ButtonLink>
    ) : undefined;
  } else if (b.status === "ready") {
    tone = "success";
    icon = <CheckCircleIcon size={15} />;
    title = "Connected";
    body = `Heartbeat ${seen}. It picks up the team's next Git build.`;
  } else if (b.status === "provisioning") {
    tone = "progress";
    icon = <ArrowsClockwiseIcon size={15} className="animate-spin" />;
    title = "Setting up";
    body =
      b.statusMessage ||
      (b.kind === "server"
        ? "Waiting for the install command to run on the new server."
        : "Starting BuildKit and the builder on the server.");
    if (b.kind === "server")
      action = (
        <Button size="xs" variant="secondary" onClick={onShowCommand}>
          Show install command
        </Button>
      );
  } else if (b.status === "error") {
    tone = "danger";
    icon = <WarningCircleIcon size={15} />;
    title = "Not working";
    body = `${b.statusMessage ?? "Something's wrong with this builder."} Last heartbeat ${seen}.`;
  } else if (b.status === "offline") {
    tone = "danger";
    icon = <WarningCircleIcon size={15} />;
    title = "Offline";
    body = `No heartbeat since ${seen}. ${b.kind === "server" ? "The machine or its pyo-builder container may be down." : "Its container may not be running."} Builds go to the team's other ready builders.`;
  } else {
    title = "Removing";
    body = "Its resources are being deleted.";
  }
  return (
    <StatusNote tone={tone} icon={icon} title={title} action={action}>
      {body}
    </StatusNote>
  );
}
