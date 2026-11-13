import { CheckIcon, HammerIcon, RocketLaunchIcon } from "@phosphor-icons/react";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState, type ReactNode } from "react";
import { z } from "zod";
import { Logo } from "~/components/site/Logo";
import { AppLink } from "~/components/ui/AppLink";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Input, Segmented } from "~/components/ui/Field";
import { Layered } from "~/components/ui/Layered";
import { Callout, Page } from "~/components/ui/Panel";
import { AgentInstall } from "~/components/pyo/AgentInstall";
import { ClusterChecklist } from "~/components/pyo/ClusterChecklist";
import { LiveCommand } from "~/components/pyo/LiveCommand";
import { SetupChecklist } from "~/components/pyo/SetupChecklist";
import {
  gitImport,
  gitParam,
  imageImport,
  imageParam,
  ImportPanel,
} from "~/components/pyo/wizard/DeployWizard";
import { seo } from "~/lib/seo";
import {
  draftId,
  emptyDraft,
  NEW_PROJECT,
  suggestName,
  validateSection,
  type DeployDraft,
  type Issues,
} from "~/lib/pyo/draft";
import { freeHostname, freeDomainIp } from "~/lib/pyo/freeDomain";
import { DNS_LABEL } from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import {
  api,
  buildersIn,
  clustersIn,
  currentTeam,
  draftProject,
  serversOf,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster } from "~/lib/pyo/types";
import { useClusterDraft } from "~/lib/pyo/useClusterDraft";

export const Route = createFileRoute("/_app/$team/onboarding")({
  // What's being deployed lives in the URL, as on New deploy, so a reload
  // while a server connects keeps it.
  validateSearch: z.object({
    source: z.enum(["git", "image"]).optional().catch(undefined),
    repo: z.string().optional(),
    image: z.string().optional(),
  }),
  head: () => ({ meta: seo({ title: "Get started" }) }),
  component: Onboarding,
});

// A first deploy, one step at a time: what to run, the server it runs on, a
// builder (Git only), then Deploy. Each step is done when the team has what it
// asks for, so anything set up earlier is already ticked off. Sign-up lands
// here once; after that it's only reachable by URL, and nothing links to it.

function Onboarding() {
  const s = usePyo();
  const ws = currentTeam(s);
  const search = Route.useSearch();
  const { team } = Route.useParams();
  const navigate = useNavigate({ from: Route.fullPath });

  const patch = useMemo(
    () =>
      search.repo
        ? gitImport(search.repo).patch
        : search.image
          ? imageImport(search.image, s.registries).patch
          : undefined,
    [search.repo, search.image, s.registries],
  );
  const usable = clustersIn(s).filter(
    (c) => c.status === "connected" || c.status === "degraded",
  );
  const cluster =
    usable.find((c) => c.id === ws?.defaultClusterId) ?? usable[0];
  const needsBuilder = patch
    ? patch.sourceType === "git"
    : search.source !== "image";
  const builderDone = buildersIn(s).some((b) => b.status === "ready");
  // On a new install this server connects, and gets a builder, on its own.
  const local = clustersIn(s).find((c) => c.sameCluster);
  const starting = buildersIn(s).find((b) => b.status === "provisioning");
  const ready = !!patch && !!cluster && (!needsBuilder || builderDone);

  if (!ws) return null;
  const clusterStep = 2;
  const deployStep = needsBuilder ? 4 : 3;

  return (
    <Page width="max-w-2xl">
      <div className="flex items-center justify-between">
        <Logo />
        <AppLink
          href="/"
          className="text-[12.5px] font-medium text-muted-foreground transition-colors hover:text-foreground"
        >
          Skip for now
        </AppLink>
      </div>

      <div className="flex flex-col items-center text-center">
        <span className="flex h-16 w-16 items-center justify-center rounded-full border border-border bg-card text-primary shadow-sm">
          <RocketLaunchIcon size={26} weight="duotone" />
        </span>
        <h1 className="mt-5 text-[26px] font-semibold tracking-tight">
          Deploy your first service
        </h1>
        <p className="mt-1.5 max-w-md text-[14px] text-muted-foreground">
          {cluster
            ? "Pick what to run."
            : "Pick what to run and connect a server."}
        </p>
      </div>

      <div className="space-y-5">
        {patch ? (
          <StepRow
            n={1}
            done
            title={patch.sourceType === "git" ? "Repository" : "Image"}
            detail={
              patch.sourceType === "git"
                ? patch.git?.repoUrl.replace(/^https?:\/\/(www\.)?/, "")
                : search.image
            }
            action={
              <Button
                size="xs"
                variant="ghost"
                onClick={() =>
                  void navigate({
                    search: { source: patch.sourceType as "git" | "image" },
                    replace: true,
                  })
                }
              >
                Change
              </Button>
            }
          />
        ) : (
          <div className="space-y-3">
            <ImportPanel
              title={<StepTitle n={1}>What do you want to deploy?</StepTitle>}
              hint="Or try the image nginx:1.27."
              initialKind={search.source === "image" ? "image" : "git"}
              focusImport
              onKind={(source) =>
                source !== "compose" &&
                void navigate({ search: { source }, replace: true })
              }
              onImport={(p) =>
                void navigate({
                  search:
                    p.sourceType === "git"
                      ? { repo: gitParam(p) }
                      : { image: imageParam(p) },
                  replace: true,
                })
              }
              registries={s.registries}
            />
            <p className="text-center text-[12.5px] text-muted-foreground">
              Or start from a{" "}
              <Link
                to="/$team/deploy/new"
                params={{ team }}
                search={{ source: "template" }}
                className="font-medium text-foreground hover:text-primary"
              >
                template
              </Link>{" "}
              or a{" "}
              <Link
                to="/$team/deploy/new"
                params={{ team }}
                search={{ source: "compose" }}
                className="font-medium text-foreground hover:text-primary"
              >
                Compose file
              </Link>
            </p>
          </div>
        )}

        {cluster ? (
          <StepRow n={clusterStep} done title="Server" detail={cluster.name} />
        ) : local?.status === "pending" ? (
          <StepRow
            n={clusterStep}
            title="Server"
            detail={`Connecting ${local.name}…`}
          />
        ) : patch ? (
          <ConnectServer n={clusterStep} />
        ) : (
          <StepRow
            n={clusterStep}
            title="Connect a server"
            detail="Where your services run"
          />
        )}

        {!needsBuilder ? null : builderDone ? (
          <StepRow
            n={3}
            done
            title="Builder"
            detail={buildersIn(s).find((b) => b.status === "ready")?.name}
          />
        ) : starting && !patch ? (
          <StepRow
            n={3}
            title="Builder"
            detail={`Starting ${starting.name}…`}
          />
        ) : patch && cluster ? (
          <SetUpBuilder n={3} cluster={cluster} />
        ) : (
          <StepRow
            n={3}
            title="Set up a builder"
            detail="Turns your code into an image"
          />
        )}

        {ready && patch && cluster ? (
          <DeployStep
            n={deployStep}
            teamId={ws.id}
            patch={patch}
            cluster={cluster}
          />
        ) : (
          <StepRow
            n={deployStep}
            title="Deploy"
            detail="Live on the web, with HTTPS"
          />
        )}
      </div>
    </Page>
  );
}

function StepNumber({ n, done }: { n: number; done?: boolean }) {
  return (
    <span
      className={`flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-[11.5px] font-semibold ${done ? "bg-success/15 text-success" : "bg-secondary text-muted-foreground"}`}
    >
      {done ? <CheckIcon size={12} weight="bold" aria-label="Done" /> : n}
    </span>
  );
}

function StepTitle({ n, children }: { n: number; children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-2.5">
      <StepNumber n={n} />
      {children}
    </span>
  );
}

/** A step that's done, or still to come: one line. */
function StepRow({
  n,
  title,
  detail,
  done,
  action,
}: {
  n: number;
  title: string;
  detail?: string;
  done?: boolean;
  action?: ReactNode;
}) {
  return (
    <section className="flex items-center gap-3 rounded-[14px] border border-card-border bg-card px-5 py-3.5">
      <StepNumber n={n} done={done} />
      <h2
        className={`shrink-0 text-[14px] font-medium ${done ? "" : "text-muted-foreground"}`}
      >
        {title}
      </h2>
      <span className="ml-auto min-w-0 truncate text-[12.5px] text-muted-foreground">
        {detail}
      </span>
      {action}
    </section>
  );
}

function ConnectServer({ n }: { n: number }) {
  const c = useClusterDraft();
  return (
    <>
      <Layered
        title={<StepTitle n={n}>Connect a server</StepTitle>}
        footer="The agent dials out. Nothing is exposed."
      >
        <div className="space-y-4 p-5">
          <p className="text-[13px] text-muted-foreground">
            Start with the machine Pyo runs on.
          </p>
          <Segmented
            label="Which machine"
            value={c.where}
            onChange={c.setWhere}
            options={[
              { value: "local", label: "This machine" },
              { value: "other", label: "Another machine" },
            ]}
          />
          {!c.showForm && c.otherLocal ? (
            <Callout tone="warning">
              This machine is connected as{" "}
              <span className="font-medium">{c.otherLocal.name}</span>, but it
              isn&apos;t reporting in.{" "}
              <ButtonLink
                href={`/clusters/${c.otherLocal.name}`}
                size="xs"
                variant="secondary"
              >
                Open {c.otherLocal.name}
              </ButtonLink>
            </Callout>
          ) : c.live.data && !c.live.error ? (
            <AgentInstall
              token={c.live.data.token}
              command={c.live.data.command}
              stale={c.live.stale}
              thisMachine={c.thisMachine}
            />
          ) : (
            <LiveCommand error={c.live.error} />
          )}
        </div>
      </Layered>
      {c.draft ? <ClusterChecklist clusterId={c.draft.id} /> : null}
    </>
  );
}

function SetUpBuilder({ n, cluster }: { n: number; cluster: Cluster }) {
  const s = usePyo();
  const [busy, setBusy] = useState(false);
  const nodes = serversOf(s, cluster.id);
  const node =
    nodes.find(
      (x) => x.roles.includes("control-plane") && x.status === "ready",
    ) ??
    nodes.find((x) => x.status === "ready") ??
    nodes[0];
  const builder = buildersIn(s).find((b) => b.status !== "removing");
  const add = async () => {
    if (!node) return;
    setBusy(true);
    await api.createBuilder({
      name: builderName(
        node.name,
        buildersIn(s).map((b) => b.name),
      ),
      kind: "cluster",
      clusterId: cluster.id,
      server: node.name,
    });
    setBusy(false);
  };
  return (
    <>
      <Layered
        title={<StepTitle n={n}>Set up a builder</StepTitle>}
        footer="Move builds to a dedicated machine later, from Builders."
      >
        <div className="space-y-4 p-5">
          <p className="text-[13px] text-muted-foreground">
            Turns your repository into an image.
          </p>
          {builder?.status === "provisioning" ? null : builder ? (
            <Callout tone="warning">
              {builder.name}: {builder.statusMessage || builder.status}{" "}
              <ButtonLink
                href={`/builders/${builder.id}`}
                size="xs"
                variant="secondary"
              >
                Open {builder.name}
              </ButtonLink>
            </Callout>
          ) : (
            <Button
              size="sm"
              disabled={busy || !node}
              onClick={() => void add()}
            >
              <HammerIcon size={14} />
              {busy
                ? "Adding…"
                : node
                  ? `Build on ${node.name}`
                  : "Waiting for the server…"}
            </Button>
          )}
        </div>
      </Layered>
      {builder?.status === "provisioning" ? (
        <SetupChecklist
          title={`Starting ${builder.name}`}
          checks={[
            {
              label: "Builder started",
              done: false,
              detail:
                builder.statusMessage || `Setting up on ${builder.server ?? ""}…`,
            },
          ]}
        />
      ) : null}
    </>
  );
}

/** Named after its server, as the first one Pyo adds. */
function builderName(server: string, taken: string[]) {
  const base = server.split(".")[0];
  return DNS_LABEL.test(base) && !taken.includes(base)
    ? base
    : `builder-${taken.length + 1}`;
}

/** The last step: a name, and the same Deploy button as New deploy. */
function DeployStep({
  n,
  teamId,
  patch,
  cluster,
}: {
  n: number;
  teamId: string;
  patch: Partial<DeployDraft>;
  cluster: Cluster;
}) {
  const s = usePyo();
  const navigate = useNavigate();
  const base = useMemo<DeployDraft>(
    () => ({ ...emptyDraft(teamId, NEW_PROJECT, cluster.id), ...patch }),
    [teamId, cluster.id, patch],
  );
  const [name, setName] = useState(() => suggestName(base));
  const [errors, setErrors] = useState<Issues>({});
  const named = { ...base, name };
  const project = draftProject(s, named);

  // Without a domain only the cluster can reach it, and the point of a first
  // deploy is a URL to open: use the free one, which needs no DNS.
  const ip = freeDomainIp(s, cluster.id);
  const port = base.ports.find((p) => p.protocol === "HTTP");
  const host =
    ip && port
      ? freeHostname({ name: name || "app" }, project?.namespace ?? "app", ip)
      : "";

  const deploy = () => {
    const ctx = { clusters: clustersIn(s), takenNames: [] };
    const issues = (["source", "basics", "target"] as const).reduce<Issues>(
      (acc, sec) => ({ ...acc, ...validateSection(sec, named, ctx) }),
      {},
    );
    setErrors(issues);
    if (Object.keys(issues).length) return;
    const { deploymentId } = api.createService({
      ...named,
      domains:
        host && port
          ? [
              {
                id: draftId(),
                hostname: host,
                portName: port.name,
                redirectHttps: true,
                tlsMode: "auto",
              },
            ]
          : [],
    });
    toast({ tone: "info", title: `Deploying ${name}` });
    void navigate({
      to: "/$team/deployments/$deploymentId",
      params: { team: currentSlug(), deploymentId },
    });
  };

  const problem = Object.entries(errors).find(([k]) => k !== "name")?.[1];
  return (
    <div className="space-y-5">
      <Layered
        title={<StepTitle n={n}>Deploy</StepTitle>}
        footer={[
          project ? `New project ${project.name}` : "",
          host ? `public at ${host}` : "private until you add a domain",
        ]
          .filter(Boolean)
          .join(" · ")}
      >
        <div className="p-5">
          <Field
            label="Service name"
            error={errors.name}
            hint="Change the rest later."
          >
            {(p) => (
              <Input
                {...p}
                mono
                value={name}
                onChange={(e) =>
                  setName(
                    e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, "-"),
                  )
                }
              />
            )}
          </Field>
        </div>
      </Layered>
      <div className="space-y-2.5">
        <Button size="lg" className="h-12 w-full text-[15px]" onClick={deploy}>
          <RocketLaunchIcon size={17} weight="fill" /> Deploy
          {name ? ` ${name}` : ""}
        </Button>
        {problem ? (
          <p
            className="text-center text-[12.5px] text-destructive"
            role="alert"
          >
            {problem}
          </p>
        ) : null}
      </div>
    </div>
  );
}
