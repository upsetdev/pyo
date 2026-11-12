import { ListSection } from "~/components/ui/Layered";
import {
  CaretRightIcon,
  DotsThreeIcon,
  TerminalWindowIcon,
  GearSixIcon,
  HardDrivesIcon,
  PlusIcon,
  GraphicsCardIcon,
} from "@phosphor-icons/react";
import { Tags } from "~/components/ui/TagInput";
import { copyIdItem } from "~/components/pyo/copyId";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { locationLabel, parseLocation, placesOf } from "~/lib/pyo/location";
import { Flag, PlaceFlags } from "~/components/ui/Flag";
import { ClusterCommandDialog } from "~/components/pyo/AgentInstall";
import { CARD_HOVER, useStoredView, ViewToggle } from "~/components/pyo/ProjectCard";
import { useState } from "react";
import {
  InfraHeader,
  ServerRole,
  serverRole,
  serverState,
  useNodeActions,
} from "~/components/pyo/infra";
import {
  clusterLabel,
  clusterTone,
  Dot,
} from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Menu } from "~/components/ui/Menu";
import { Select } from "~/components/ui/Field";
import { EmptyState, NoMatches, Page, Panel } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { gpuCell, gpuText, plural } from "~/lib/pyo/format";
import { clustersIn, clusterSummary, serversOf, usePyo } from "~/lib/pyo/store";
import type { Cluster, ClusterServer } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/clusters/")({
  head: () => ({ meta: seo({ title: "Clusters" }) }),
  component: ServersPage,
});

// Servers grouped by the k3s cluster they belong to. Services deploy to a
// cluster and spread across its servers, so each cluster is the group
// header, and its CPU and Memory columns line up with the servers below.

const COLS =
  "grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 md:grid-cols-[minmax(0,1fr)_128px_128px_32px]";
// With a GPU column, when any server has a GPU.
const COLS_GPU =
  "grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 md:grid-cols-[minmax(0,1fr)_128px_128px_150px_32px]";

type Filter = "all" | "attention" | "gpu";

/** A cluster's GPUs for its header: "2 GPUs · 1 free". */
function clusterGpus(servers: ClusterServer[]) {
  const total = servers.reduce((t, n) => t + (n.gpus ?? 0), 0);
  const used = servers.reduce((t, n) => t + (n.gpusUsed ?? 0), 0);
  if (!total) return servers.some((n) => n.gpuStatus) ? "Not ready" : null;
  return `${total} GPU${total === 1 ? "" : "s"} · ${Math.max(0, total - used)} free`;
}

function Usage({
  used,
  total,
  off,
}: {
  used: number;
  total: number;
  off?: boolean;
}) {
  if (off || !total)
    return <span className="text-[12px] text-muted-foreground">—</span>;
  const r = Math.min(1, used / total);
  const color =
    r > 0.9 ? "bg-destructive" : r > 0.75 ? "bg-warning" : "bg-primary";
  return (
    <span
      className="flex items-center gap-2"
      role="meter"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(r * 100)}
    >
      <span className="h-1.5 flex-1 overflow-hidden rounded-full bg-foreground/10">
        <span
          className={`block h-full rounded-full ${color}`}
          style={{ width: `${Math.max(r * 100, 2)}%` }}
        />
      </span>
      <span className="w-8 text-right text-[12px] tabular-nums text-muted-foreground">
        {Math.round(r * 100)}%
      </span>
    </span>
  );
}

function ServersPage() {
  const s = usePyo();
  const [q, setQ] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const [view, setView] = useStoredView("pyo:clusters-view");
  const [sort, setSort] = useState<"name" | "servers" | "workloads">("name");
  const navigate = useNavigate();
  const [commandFor, setCommandFor] = useState<Cluster | null>(null);
  const { items, dialog } = useNodeActions();
  const clusters = clustersIn(s);
  const allServers = clusters.flatMap((c) => serversOf(s, c.id));
  const attention = allServers.filter((n) => serverState(n).label).length;
  const gpuServers = allServers.filter((n) => n.gpuStatus).length;
  const cols = gpuServers ? COLS_GPU : COLS;
  const offline = allServers.filter((n) => n.status !== "ready").length;

  const ql = q.trim().toLowerCase();
  const groups = clusters
    .map((c) => {
      const clusterHit =
        !ql ||
        c.name.includes(ql) ||
        (c.tags ?? []).some((t) => t.includes(ql)) ||
        c.location.toLowerCase().includes(ql) ||
        locationLabel(c.location).toLowerCase().includes(ql);
      const servers = serversOf(s, c.id).filter(
        (n) =>
          (clusterHit ||
            n.name.includes(ql) ||
            n.internalIP.includes(ql) ||
            (n.gpuModel ?? "").toLowerCase().includes(ql) ||
            (n.location ?? "").toLowerCase().includes(ql) ||
            (n.tags ?? []).some((t) => t.includes(ql))) &&
          (filter === "all" ||
            (filter === "gpu" ? !!n.gpuStatus : !!serverState(n).label)),
      );
      return { c, servers };
    })
    .filter((g) => g.servers.length || (!ql && filter === "all"))
    .map((g) => ({ ...g, sum: clusterSummary(s, g.c) }))
    .sort((a, b) =>
      sort === "servers"
        ? b.sum.servers - a.sum.servers || a.c.name.localeCompare(b.c.name)
        : sort === "workloads"
          ? b.sum.workloads - a.sum.workloads ||
            a.c.name.localeCompare(b.c.name)
          : a.c.name.localeCompare(b.c.name),
    );

  // Outlined in the header (the top bar's New is the primary); filled when
  // it's the only thing to do, in the empty state.
  const add = (variant: "primary" | "secondary" = "secondary") => (
    <ButtonLink href="/clusters/new" size="sm" variant={variant}>
      <PlusIcon size={13} weight="bold" /> Add cluster
    </ButtonLink>
  );

  return (
    <Page>
      <InfraHeader
        actions={add()}
        description={
          clusters.length
            ? `${plural(allServers.length, "server")} in ${plural(clusters.length, "cluster")} · ${offline ? `${offline} offline` : "all online"}`
            : undefined
        }
      />

      {clusters.length ? (
        <ListSection
          toolbar={
            <>
              <SearchInput
                value={q}
                onChange={setQ}
                placeholder="Search clusters, servers, regions, IPs, tags or GPUs…"
                className="min-w-0 flex-1 basis-full sm:basis-0"
              />
              <div className="w-36 sm:w-44">
                <Select
                  value={sort}
                  onChange={(e) => setSort(e.target.value as typeof sort)}
                  aria-label="Sort clusters"
                  heading="Sort by"
                >
                  <option value="name">Name</option>
                  <option value="servers">Most servers</option>
                  <option value="workloads">Most workloads</option>
                </Select>
              </div>
              <div className="w-36 sm:w-44">
                <Select
                  value={filter}
                  onChange={(e) => setFilter(e.target.value as Filter)}
                  aria-label="Show servers"
                  heading="Show"
                >
                  <option value="all">All servers ({allServers.length})</option>
                  <option
                    value="attention"
                    disabled={!attention && filter !== "attention"}
                  >
                    Needs attention ({attention})
                  </option>
                  <option
                    value="gpu"
                    disabled={!gpuServers && filter !== "gpu"}
                  >
                    GPU servers ({gpuServers})
                  </option>
                </Select>
              </div>
              <ViewToggle value={view} onChange={setView} />
            </>
          }
        >
          {groups.length && view === "grid" ? (
            <div className="grid gap-1.5 sm:grid-cols-2 lg:grid-cols-3">
              {groups.map(({ c }) => (
                <ClusterCard
                  key={c.id}
                  c={c}
                  onJoin={() =>
                    void navigate({
                      to: "/$team/clusters/$clusterId/servers/new",
                      params: { team: currentSlug(), clusterId: c.name },
                    })
                  }
                  onCommand={() => setCommandFor(c)}
                />
              ))}
            </div>
          ) : groups.length ? (
            <div className="space-y-1.5">
              {groups.map(({ c, servers }) => {
                const sum = clusterSummary(s, c);
                return (
                  <section
                    key={c.id}
                    aria-labelledby={`cl-${c.id}`}
                    className="overflow-hidden rounded-[14px] border border-card-border bg-card"
                  >
                    <Link
                      to="/$team/clusters/$clusterId"
                      params={{ team: currentSlug(), clusterId: c.name }}
                      className={`${cols} group border-b border-border bg-muted/30 px-5 py-3.5 transition-colors hover:bg-accent/40`}
                    >
                      <div className="min-w-0">
                        <h2
                          id={`cl-${c.id}`}
                          className="flex items-center gap-2 text-[14px] font-semibold"
                        >
                          {c.name}
                          {c.status !== "connected" ? (
                            <span className="inline-flex items-center gap-1.5 text-[12.5px] font-normal">
                              <Dot tone={clusterTone[c.status]} />
                              {clusterLabel[c.status]}
                            </span>
                          ) : null}
                        </h2>
                        <p className="truncate text-[12.5px] text-muted-foreground">
                          {placesOf(c, s.servers).length > 1
                            ? `${placesOf(c, s.servers).length} regions`
                            : locationLabel(
                                placesOf(c, s.servers)[0].location,
                              )}{" "}
                          · k3s {c.version.replace(/\+k3s\d+$/, "")} ·{" "}
                          {plural(sum.workloads, "workload")}
                        </p>
                      </div>
                      <div className="hidden md:block">
                        <p className="mb-1 text-[11px] font-medium text-muted-foreground">
                          CPU
                        </p>
                        <Usage
                          used={sum.cpu.used}
                          total={sum.cpu.capacity}
                          off={c.status === "unreachable"}
                        />
                      </div>
                      <div className="hidden md:block">
                        <p className="mb-1 text-[11px] font-medium text-muted-foreground">
                          Memory
                        </p>
                        <Usage
                          used={sum.memory.used}
                          total={sum.memory.capacity}
                          off={c.status === "unreachable"}
                        />
                      </div>
                      {gpuServers ? (
                        <div className="hidden md:block">
                          <p className="mb-1 text-[11px] font-medium text-muted-foreground">
                            GPU
                          </p>
                          <p className="truncate text-[12px] tabular-nums text-muted-foreground">
                            {clusterGpus(serversOf(s, c.id)) ?? "—"}
                          </p>
                        </div>
                      ) : null}
                      <CaretRightIcon
                        size={14}
                        className="justify-self-end text-muted-foreground transition-transform group-hover:translate-x-0.5"
                        aria-label="Open cluster"
                      />
                    </Link>
                    <ul className="list-rows">
                      {servers.map((n) => {
                        const st = serverState(n);
                        const off = n.status !== "ready";
                        return (
                          <li
                            key={n.id}
                            className={`${cols} relative px-5 py-3 transition-colors hover:bg-accent/30`}
                          >
                            <div className="min-w-0">
                              <div className="flex items-center gap-2.5">
                                <Dot tone={st.tone} className="h-2 w-2" />
                                <Link
                                  to="/$team/clusters/$clusterId/$serverId"
                                  params={{
                                    team: currentSlug(),
                                    clusterId: c.name,
                                    serverId: n.name,
                                  }}
                                  className="truncate text-[13.5px] font-medium after:absolute after:inset-0 focus:outline-none focus-visible:underline"
                                >
                                  {n.name}
                                </Link>
                                {st.label ? (
                                  <span
                                    className={`shrink-0 text-[12px] ${st.tone === "danger" ? "text-destructive" : "text-warning"}`}
                                  >
                                    {st.label}
                                  </span>
                                ) : null}
                                <Tags tags={n.tags} className="shrink-0" />
                              </div>
                              <p className="flex min-w-0 items-center gap-1.5 truncate pl-[18px] text-[12px] text-muted-foreground">
                                <Flag
                                  country={
                                    parseLocation(n.location ?? c.location)
                                      .country
                                  }
                                  size={13}
                                />
                                {locationLabel(n.location ?? c.location)} ·{" "}
                                {serverRole(n) ? (
                                  <>
                                    <ServerRole node={n} /> ·{" "}
                                  </>
                                ) : null}
                                <span className="font-mono text-[11.5px]">
                                  {n.internalIP}
                                </span>{" "}
                                · {n.capacity.cpu} CPU · {n.capacity.memoryGi}{" "}
                                GB
                                {gpuText(n) ? (
                                  <span className="md:hidden">
                                    {` · ${gpuText(n)}`}
                                  </span>
                                ) : null}
                              </p>
                            </div>
                            <div className="hidden md:block">
                              <Usage
                                used={n.usage.cpu}
                                total={n.capacity.cpu}
                                off={off}
                              />
                            </div>
                            <div className="hidden md:block">
                              <Usage
                                used={n.usage.memoryGi}
                                total={n.capacity.memoryGi}
                                off={off}
                              />
                            </div>
                            {gpuServers ? (
                              <div className="hidden truncate text-[12px] tabular-nums text-muted-foreground md:block">
                                {gpuCell(n) ?? "—"}
                              </div>
                            ) : null}
                            <div className="relative z-10 justify-self-end">
                              <Menu
                                label={`Actions for ${n.name}`}
                                items={items(n)}
                                trigger={(t) => (
                                  <Button
                                    {...t}
                                    variant="ghost"
                                    size="icon"
                                    className="h-7 w-7"
                                  >
                                    <DotsThreeIcon size={16} weight="bold" />
                                  </Button>
                                )}
                              />
                            </div>
                          </li>
                        );
                      })}
                    </ul>
                  </section>
                );
              })}
            </div>
          ) : (
            <NoMatches
              query={q}
              what="servers"
              onClear={() => {
                setQ("");
                setFilter("all");
              }}
            />
          )}
        </ListSection>
      ) : (
        <Panel>
          <EmptyState
            icon={HardDrivesIcon}
            title="No servers yet"
            description="Connect one to start deploying."
            action={add("primary")}
          />
        </Panel>
      )}
      {dialog}
      {commandFor ? (
        <ClusterCommandDialog
          cluster={commandFor}
          open
          onClose={() => setCommandFor(null)}
        />
      ) : null}
    </Page>
  );
}

/** A cluster at a glance: where it is, how many servers, and each server's state. */
function ClusterCard({
  c,
  onJoin,
  onCommand,
}: {
  c: Cluster;
  onJoin: () => void;
  onCommand: () => void;
}) {
  const s = usePyo();
  const sum = clusterSummary(s, c);
  const places = placesOf(c, s.servers);
  return (
    <article className={`relative flex flex-col rounded-[14px] border border-card-border bg-card p-5 ${CARD_HOVER} has-[a:focus-visible]:ring-2 has-[a:focus-visible]:ring-primary/40`}>
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <h3 className="truncate text-[14px] font-semibold">
            <Link
              to="/$team/clusters/$clusterId"
              params={{ team: currentSlug(), clusterId: c.name }}
              className="after:absolute after:inset-0 after:rounded-xl focus:outline-none focus-visible:underline"
            >
              {c.name}
            </Link>
            {c.status !== "connected" ? (
              <span className="ml-2 inline-flex items-center gap-1.5 align-middle text-[12px] font-normal text-muted-foreground">
                <Dot tone={clusterTone[c.status]} />
                {clusterLabel[c.status]}
              </span>
            ) : null}
            <Tags tags={c.tags} className="ml-2 align-middle font-normal" />
          </h3>
          <p className="truncate text-[13px] text-muted-foreground">
            {plural(sum.servers, "server")} · {plural(sum.workloads, "workload")}
          </p>
          {clusterGpus(serversOf(s, c.id)) ? (
            <span className="mt-1.5 inline-flex items-center gap-1 rounded-md bg-secondary px-1.5 py-0.5 text-[11.5px] font-medium text-muted-foreground">
              <GraphicsCardIcon size={12} aria-hidden />
              {clusterGpus(serversOf(s, c.id))}
            </span>
          ) : null}
        </div>
        <div className="relative z-10 -mr-2 -mt-1.5">
          <Menu
            label={`Actions for ${c.name}`}
            items={[
              // Still connecting: servers can only join once its agent is up.
              c.status === "pending"
                ? {
                    label: "Show install command",
                    icon: <TerminalWindowIcon size={14} />,
                    onSelect: onCommand,
                  }
                : {
                    label: "Add server",
                    icon: <PlusIcon size={14} />,
                    onSelect: onJoin,
                  },
              {
                label: "Settings",
                icon: <GearSixIcon size={14} />,
                href: `/clusters/${c.name}/settings`,
              },
              copyIdItem("Cluster", c.id),
            ]}
            trigger={(t) => (
              <Button
                {...t}
                variant="ghost"
                size="icon"
                aria-label={`Actions for ${c.name}`}
              >
                <DotsThreeIcon size={16} weight="bold" />
              </Button>
            )}
          />
        </div>
      </div>
      <div className="mt-auto pt-12">
        <p className="text-[13px] text-muted-foreground">
          {places.length > 1
            ? `${places.length} regions`
            : locationLabel(places[0].location)}
        </p>
        <div className="relative z-10 mt-2.5 flex w-fit">
          <PlaceFlags places={places.map((pl) => pl.location)} size={32} />
        </div>
      </div>
    </article>
  );
}
