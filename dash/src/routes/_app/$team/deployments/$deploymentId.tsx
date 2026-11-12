import { Layered } from "~/components/ui/Layered";
import {
  AppWindowIcon,
  ArrowClockwiseIcon,
  ArrowCounterClockwiseIcon,
  ArrowUpRightIcon,
  CaretRightIcon,
  CheckCircleIcon,
  CircleNotchIcon,
  CopyIcon,
  DotsThreeIcon,
  RocketLaunchIcon,
  StopCircleIcon,
  StopIcon,
  WarningCircleIcon,
} from "@phosphor-icons/react";
import { Celebration } from "~/components/pyo/Celebration";
import { copyIdItem } from "~/components/pyo/copyId";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import {
  DeployActor,
  TRIGGER_LABEL,
  useDeploymentActions,
} from "~/components/pyo/DeploymentsTable";
import { imageLabel, SourceMark } from "~/components/pyo/ProjectCard";
import {
  LogLines,
  WrapSwitch,
  copyLines,
  useWrapLines,
} from "~/components/pyo/LogViewer";
import { stripAnsi } from "~/lib/pyo/ansi";
import { ZoomButton, Zoomable } from "~/components/ui/Zoom";
import { Button, ButtonLink, buttonClass } from "~/components/ui/Button";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Page } from "~/components/ui/Panel";
import { StatusNote, type NoteTone } from "~/components/ui/StatusNote";
import { CopyButton, SearchInput } from "~/components/ui/Table";
import {
  clock,
  cpu,
  domainUrl,
  duration,
  mem,
  timeAgo,
} from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import { isActive, STAGE_LABEL } from "~/lib/pyo/simulation";
import {
  activeDeploymentFor,
  api,
  usePyo,
  serviceParams,
  servicePath,
} from "~/lib/pyo/store";
import type { Deployment, DeploymentStage } from "~/lib/pyo/types";
import { buildFileOf } from "~/lib/pyo/detect";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/deployments/$deploymentId")({
  head: () => ({ meta: seo({ title: "Deployment" }) }),
  component: DeploymentPage,
});

// One question first: did it work? Then what was deployed, the next thing to
// do, and the logs. Everything else is under "More details".

/** Re-render every second while something is running, for elapsed time. */
function useNow(active: boolean) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!active) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [active]);
  return now;
}

/** The eight stages, as the three steps a person thinks in. */
const PHASES: { label: string; stages: DeploymentStage[] }[] = [
  { label: "Build", stages: ["building", "pushing"] },
  {
    label: "Deploy",
    stages: [
      "queued",
      "waiting_database",
      "applying",
      "scheduling",
      "pulling",
      "starting",
    ],
  },
  { label: "Health check", stages: ["waiting_ready"] },
];

function DeploymentPage() {
  const { deploymentId } = Route.useParams();
  const s = usePyo();
  const navigate = useNavigate();
  const d = s.deployments.find((x) => x.id === deploymentId);
  const actions = useDeploymentActions(s);
  const now = useNow(!!d && isActive(d.status));

  if (!d)
    return (
      <Page>
        <EmptyState
          icon={RocketLaunchIcon}
          title="Deployment not found"
          description="It may belong to a deleted service."
          action={
            <ButtonLink href="/projects" size="sm" variant="secondary">
              Projects
            </ButtonLink>
          }
        />
      </Page>
    );

  const app = s.services.find((a) => a.id === d.serviceId);
  const active = isActive(d.status);
  const busy = app ? !!activeDeploymentFor(s, app.id) : true;
  const current = app?.currentDeploymentId === d.id;
  const domain = app?.domains.find((x) => x.status === "active");
  const live = s.deployments.find((x) => x.id === app?.currentDeploymentId);
  const olderStillLive =
    !!live && live.id !== d.id && live.createdAt < d.createdAt;
  const failed = d.status === "failed";
  const elapsed = (d.finishedAt ?? now) - d.createdAt;

  const go = (id: string) =>
    void navigate({
      to: "/$team/deployments/$deploymentId",
      params: { team: currentSlug(), deploymentId: id },
    });
  // "Try again" uses the app's current settings, so a fix made after the
  // failure (a port, a variable) is picked up.
  const retry = () => app && go(api.deploy(app.id, { trigger: "retry" }));
  const redeploy = () => app && go(api.deploy(app.id, { trigger: "redeploy" }));

  // The one obvious next step.
  const primary = active ? (
    <Button variant="secondary" size="sm" onClick={() => actions.cancel(d)}>
      <StopIcon size={13} /> Cancel
    </Button>
  ) : failed || d.status === "cancelled" ? (
    <Button size="sm" disabled={busy || !app} onClick={retry}>
      <ArrowClockwiseIcon size={13} /> Try again
    </Button>
  ) : current && domain ? (
    <a
      href={domainUrl(domain)}
      target="_blank"
      rel="noreferrer"
      className={buttonClass("primary", "sm")}
    >
      Visit <ArrowUpRightIcon size={12} weight="bold" />
    </a>
  ) : null;

  return (
    <Page width="max-w-5xl">
      {/* Header: what, whether it worked, what next. */}
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="truncate text-[24px] font-semibold tracking-tight">
            {app ? (
              <Link
                to="/$team/$projectId/$serviceId"
                params={{ team: currentSlug(), ...serviceParams(app.id) }}
                className="hover:text-primary"
              >
                {app.name}
              </Link>
            ) : (
              "Deleted service"
            )}
          </h1>
          <p className="mt-1 flex flex-wrap items-center gap-x-2 text-[13px] text-muted-foreground">
            <Source d={d} />
            <span aria-hidden>·</span>
            <span className="inline-flex items-center gap-1.5">
              {TRIGGER_LABEL[d.trigger]} by{" "}
              <DeployActor name={d.actor} size={18} /> {timeAgo(d.createdAt)}
            </span>
          </p>
        </div>
        <div className="flex items-center gap-2">
          {primary}
          <Menu
            label="More actions"
            items={[
              ...(app && !active && !failed && d.status !== "cancelled"
                ? [
                    {
                      label: "Redeploy",
                      icon: <ArrowClockwiseIcon size={14} />,
                      disabled: busy,
                      onSelect: redeploy,
                    },
                  ]
                : []),
              ...(d.status === "succeeded" && !current && app
                ? [
                    {
                      label: "Roll back to this version",
                      icon: <ArrowCounterClockwiseIcon size={14} />,
                      disabled: busy,
                      onSelect: () => actions.rollback(d),
                    },
                  ]
                : []),
              ...(app
                ? [
                    {
                      label: "Open service",
                      icon: <AppWindowIcon size={14} />,
                      href: servicePath(app.id),
                    },
                  ]
                : []),
              {
                label: "Copy logs",
                icon: <CopyIcon size={14} />,
                disabled: !d.logs.length,
                onSelect: () => copyLines(d.logs),
              },
              copyIdItem("Deployment", d.id),
              ...(app ? [copyIdItem("Service", app.id)] : []),
            ]}
            trigger={(t) => (
              <Button
                {...t}
                variant="secondary"
                size="icon"
                aria-label="More actions"
              >
                <DotsThreeIcon size={16} weight="bold" />
              </Button>
            )}
          />
        </div>
      </div>

      {app &&
      current &&
      d.status === "succeeded" &&
      d.trigger !== "rollback" &&
      d.finishedAt &&
      Date.now() - d.finishedAt < 30 * 60_000 &&
      !s.deployments.some(
        (x) =>
          x.serviceId === d.serviceId &&
          x.id !== d.id &&
          x.status === "succeeded" &&
          x.createdAt < d.createdAt,
      ) ? (
        <Celebration
          name={app.name}
          took={duration(elapsed)}
          url={domain ? domainUrl(domain) : undefined}
          servicePath={servicePath(app.id)}
        />
      ) : null}

      <StatusCard
        d={d}
        serviceName={app?.name ?? "The service"}
        current={current}
        domain={domain?.hostname}
        olderStillLive={olderStillLive}
        elapsed={elapsed}
      />

      <Logs d={d} />

      <MoreDetails d={d} />
      {actions.dialogs}
    </Page>
  );
}

function Source({ d }: { d: Deployment }) {
  const src = d.source;
  if (src.type === "git") {
    const repo = src.repoUrl
      .replace(/^https?:\/\/(www\.)?/, "")
      .replace(/\.git$/, "");
    return (
      <span className="inline-flex min-w-0 items-center gap-1.5">
        <SourceMark source={src} />
        <span className="truncate">{repo}</span>
        <span className="text-foreground">{src.branch}</span>
        {d.revisionLabel ? (
          <span className="font-mono text-[12px]">{d.revisionLabel}</span>
        ) : null}
        {/* How the builder built it: the repo's Dockerfile or Containerfile, or what Railpack detected. */}
        {src.buildpackLanguage ? (
          <span
            className="rounded-md bg-muted px-1.5 py-px text-[11.5px]"
            title={
              src.builder === "railpack"
                ? "No Dockerfile or Containerfile, so Railpack detected how to build it"
                : `Built with the repository's ${buildFileOf(src.buildpackLanguage)}`
            }
          >
            {src.builder === "railpack"
              ? `${src.buildpackLanguage} · zero config`
              : buildFileOf(src.buildpackLanguage)}
          </span>
        ) : null}
      </span>
    );
  }
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <SourceMark source={src} />
      <span className="truncate font-mono text-[12.5px] text-foreground">
        {imageLabel(src.image)}
        {src.reference.startsWith("sha256:") ? "@" : ":"}
        {src.reference}
      </span>
    </span>
  );
}

/**
 * The outcome as one card: a tinted icon, a short title and one sentence.
 * Everything else lives in the logs and "More details".
 */
function StatusCard({
  d,
  serviceName,
  current,
  domain,
  olderStillLive,
  elapsed,
}: {
  d: Deployment;
  serviceName: string;
  current: boolean;
  domain?: string;
  olderStillLive: boolean;
  elapsed: number;
}) {
  const took = duration(elapsed);
  let tone: NoteTone;
  let icon: ReactNode;
  let title: string;
  let body: ReactNode;

  if (d.status === "rolling_back") {
    tone = "progress";
    icon = <CircleNotchIcon size={15} className="animate-spin" />;
    title = "Rolling back…";
    body = `${d.failure?.message ? `${d.failure.message} ` : ""}Putting the previous version back · ${took} so far.`;
  } else if (isActive(d.status)) {
    const phases = PHASES.filter((p) =>
      p.stages.some((st) => d.stages.some((x) => x.stage === st)),
    );
    const at = Math.max(
      0,
      phases.findIndex((p) => p.stages.includes(d.status as DeploymentStage)),
    );
    tone = "progress";
    icon = <CircleNotchIcon size={15} className="animate-spin" />;
    title = `${STAGE_LABEL[d.status as DeploymentStage]}…`;
    body = `Step ${at + 1} of ${phases.length}: ${phases[at]?.label ?? "Deploy"} · ${took} so far.`;
  } else if (d.status === "succeeded" && current) {
    tone = "success";
    icon = <CheckCircleIcon size={15} />;
    title = d.trigger === "rollback" ? "Rolled back" : "Deployed";
    body = domain ? (
      <>
        {serviceName} is live at{" "}
        <a
          href={`http://${domain}`}
          target="_blank"
          rel="noreferrer"
          className="text-foreground hover:text-primary"
        >
          {domain}
        </a>
        . Took {took}.
      </>
    ) : (
      `${serviceName} is live inside the cluster. Took ${took}.`
    );
  } else if (d.status === "succeeded") {
    tone = "neutral";
    icon = <CheckCircleIcon size={15} />;
    title = "Replaced";
    body = "This version deployed fine. A newer deployment is live now.";
  } else if (d.status === "failed") {
    tone = "danger";
    icon = <WarningCircleIcon size={15} />;
    title = d.autoRolledBack
      ? "Deployment failed, rolled back"
      : "Deployment failed";
    const after =
      d.autoRolledBack === true
        ? " The previous version is back."
        : d.autoRolledBack === false
          ? " Putting the previous version back didn't work either. Check the service's logs."
          : olderStillLive
            ? " Your previous version is still live."
            : "";
    body = `${d.failure?.message ?? "The rollout didn't finish."}${after}`;
  } else {
    tone = "neutral";
    icon = <StopCircleIcon size={15} />;
    title = "Cancelled";
    body = `Stopped after ${took}.${d.autoRolledBack ? " The previous version was put back." : olderStillLive ? " Your previous version is still live." : ""}`;
  }

  return (
    <StatusNote tone={tone} icon={icon} title={title}>
      {body}
    </StatusNote>
  );
}

function Logs({ d }: { d: Deployment }) {
  const [query, setQuery] = useState("");
  const [zoomed, setZoomed] = useState(false);
  const [wrap, setWrap] = useWrapLines();
  const shown = useMemo(() => {
    const q = query.toLowerCase();
    return q
      ? d.logs.filter((l) => stripAnsi(l.message).toLowerCase().includes(q))
      : d.logs;
  }, [d.logs, query]);
  return (
    <Layered bare>
      <section
        aria-label="Logs"
        className="overflow-hidden rounded-[14px] border border-card-border bg-card"
      >
        <Zoomable
          zoomed={zoomed}
          onClose={() => setZoomed(false)}
          label="Deployment logs"
        >
          <div className={zoomed ? "flex min-h-0 flex-1 flex-col" : ""}>
            <div className="flex flex-wrap items-center gap-3 border-b border-border px-4 py-2.5">
              <h2 className="text-[14px] font-medium">Logs</h2>
              <SearchInput
                value={query}
                onChange={setQuery}
                placeholder="Search"
                className="ml-auto w-full sm:w-56"
              />
              <Button
                variant="ghost"
                size="xs"
                disabled={!shown.length}
                onClick={() => copyLines(shown)}
              >
                <CopyIcon size={12} /> Copy
              </Button>
              <ZoomButton
                zoomed={zoomed}
                onClick={() => setZoomed((z) => !z)}
              />
            </div>
            <LogLines
              entries={shown}
              query={query}
              wrap={wrap}
              height={zoomed ? "min-h-0 flex-1" : undefined}
              empty={
                query ? "No lines match." : "Waiting for the first log line…"
              }
            />
            <div className="hidden justify-end border-t border-border px-4 py-1.5 sm:flex">
              <WrapSwitch wrap={wrap} onChange={setWrap} />
            </div>
          </div>
        </Zoomable>
      </section>
    </Layered>
  );
}

/** Everything that's useful sometimes: IDs, timing, events, configuration. */
function MoreDetails({ d }: { d: Deployment }) {
  const [open, setOpen] = useState(false);
  const warnings = d.events.filter((e) => e.type === "Warning").length;
  return (
    <Layered bare>
      {" "}
      <section className="rounded-[14px] border border-card-border bg-card">
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          aria-expanded={open}
          className="flex w-full items-center gap-2 px-4 py-3 text-left text-[13.5px] font-medium"
        >
          <CaretRightIcon
            size={12}
            className={`text-muted-foreground transition-transform ${open ? "rotate-90" : ""}`}
          />
          More details
          {warnings ? (
            <span className="ml-auto text-[12px] font-normal text-warning">
              {warnings} warning{warnings === 1 ? "" : "s"}
            </span>
          ) : null}
        </button>
        {open ? (
          <div className="space-y-5 border-t border-border px-4 py-4 text-[13px]">
            <dl className="grid gap-x-6 gap-y-2.5 sm:grid-cols-2">
              <Row k="Deployment ID">
                <span className="inline-flex items-center gap-1 font-mono text-[12px]">
                  {d.id} <CopyButton value={d.id} label="Copy ID" />
                </span>
              </Row>
              <Row k="Started">{clock(d.createdAt)}</Row>
              <Row k="Replicas">{d.config.replicas}</Row>
              <Row k="Resources">
                {cpu(d.config.resources.cpuRequest)} CPU ·{" "}
                {mem(d.config.resources.memoryRequest)}
              </Row>
              <Row k="Environment">
                {d.config.envKeys.length ? (
                  <span className="font-mono text-[12px]">
                    {d.config.envKeys.join(", ")}
                  </span>
                ) : (
                  "No variables"
                )}
              </Row>
              {d.revision && d.source.type === "git" ? (
                <Row k="Commit">
                  <span className="font-mono text-[12px]">{d.revision}</span>
                </Row>
              ) : null}
              {d.rollbackOf ? (
                <Row k="Rolled back to">
                  <DeploymentLink id={d.rollbackOf} />
                </Row>
              ) : null}
              {d.retryOf ? (
                <Row k="Retry of">
                  <DeploymentLink id={d.retryOf} />
                </Row>
              ) : null}
            </dl>
            <div>
              <p className="mb-2 text-[12px] font-medium text-muted-foreground">
                Events
              </p>
              {d.events.length ? (
                <ul className="space-y-1.5">
                  {d.events.map((e, i) => (
                    <li key={i} className="flex gap-3 text-[12.5px]">
                      <span className="w-16 shrink-0 font-mono text-[11.5px] text-muted-foreground">
                        {clock(e.at)}
                      </span>
                      <span className="min-w-0">
                        <span
                          className={`font-medium ${e.type === "Warning" ? "text-warning" : ""}`}
                        >
                          {e.reason}
                        </span>{" "}
                        <span className="text-muted-foreground">
                          {e.message}
                        </span>
                      </span>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-muted-foreground">None yet.</p>
              )}
            </div>
          </div>
        ) : null}
      </section>
    </Layered>
  );
}

function Row({ k, children }: { k: string; children: ReactNode }) {
  return (
    <div className="flex min-w-0 gap-3">
      <dt className="w-28 shrink-0 text-muted-foreground">{k}</dt>
      <dd className="min-w-0 break-words">{children}</dd>
    </div>
  );
}

function DeploymentLink({ id }: { id: string }) {
  return (
    <Link
      to="/$team/deployments/$deploymentId"
      params={{ team: currentSlug(), deploymentId: id }}
      className="font-mono text-[12px] hover:text-primary"
    >
      {id}
    </Link>
  );
}
