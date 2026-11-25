import { Sparkline } from "~/components/charts/Charts";
import { ListSection } from "~/components/ui/Layered";
import { GlobeIcon, PlusIcon } from "@phosphor-icons/react";
import { Link, createFileRoute, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { DomainKind, DomainMark, DomainStatus } from "~/components/pyo/DomainState";
import { TimeAgo } from "~/components/pyo/common";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import { AddDomainDialog } from "~/components/pyo/AddDomainDialog";
import { Select } from "~/components/ui/Field";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import {
  FilterChips,
  SearchInput,
  Table,
  Td,
  Th,
  Tr,
} from "~/components/ui/Table";
import { onlyRedirects, redirectTarget } from "~/components/pyo/DomainRules";
import { fetchDomainTraffic } from "~/lib/pyo/backend";
import { compact } from "~/lib/pyo/format";
import { hostsOf } from "~/lib/pyo/hosts";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import { servicePath, usePyo, type PyoState } from "~/lib/pyo/store";
import type { Application, Domain, Project } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// Every hostname the team serves, a row each, with where it goes and its
// requests over the last day. Opening one shows its routes, redirects and
// headers. Add one here (picking its service) or on the service.
export const Route = createFileRoute("/_app/$team/domains")({
  head: () => ({ meta: seo({ title: "Domains" }) }),
  component: Domains,
});

function Domains() {
  const s = usePyo();
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const [adding, setAdding] = useState(false);
  const [filter, setFilter] = useState<"all" | "waiting">("all");
  const [sort, setSort] = useState<"domain" | "newest" | "service" | "attention">(
    "domain",
  );
  const rows = useMemo(
    () =>
      hostsOf(s).map((h) => ({
        h,
        d: h.primary.d,
        a: h.primary.a,
        project: s.projects.find((p) => p.id === h.primary.a.projectId),
        services: [
          ...new Set(
            h.entries.filter((e) => !onlyRedirects(e.d)).map((e) => e.a.id),
          ),
        ],
      })),
    [s],
  );
  const traffic = useDomainTraffic(s, rows.length > 0);
  const waiting = rows.filter((r) => r.d.status !== "active").length;
  const needle = q.trim().toLowerCase();
  const byHost = (x: (typeof rows)[number], y: (typeof rows)[number]) =>
    x.d.hostname.localeCompare(y.d.hostname);
  const shown = rows
    .filter(
      (r) =>
        (filter === "all" || r.d.status !== "active") &&
        (!needle ||
          [r.d.hostname, ...r.h.entries.map((e) => e.a.name), r.project?.name ?? ""].some((x) =>
            x.toLowerCase().includes(needle),
          )),
    )
    .sort((x, y) =>
      sort === "newest"
        ? y.d.createdAt - x.d.createdAt
        : sort === "service"
          ? x.a.name.localeCompare(y.a.name) || byHost(x, y)
          : sort === "attention"
            ? Number(x.d.status === "active") -
                Number(y.d.status === "active") || byHost(x, y)
            : byHost(x, y),
    );

  return (
    <Page>
      <PageHeader
        title="Domains"
        actions={
          <Button size="sm" variant="secondary" onClick={() => setAdding(true)}>
            <PlusIcon size={13} weight="bold" /> Add domain
          </Button>
        }
      />
      {rows.length ? (
        <ListSection
          toolbar={
            <>
              <SearchInput
                value={q}
                onChange={setQ}
                placeholder="Search by domain, service or project…"
                className="min-w-0 flex-1 basis-full sm:basis-0"
              />
              <div className="w-36 sm:w-44">
                <Select
                  value={sort}
                  onChange={(e) => setSort(e.target.value as typeof sort)}
                  aria-label="Sort domains"
                  heading="Sort by"
                >
                  <option value="domain">Domain</option>
                  <option value="newest">Newest first</option>
                  <option value="service">Service</option>
                  <option value="attention">Needs attention</option>
                </Select>
              </div>
            </>
          }
        >
          {waiting ? (
            <div className="-mt-1">
              <FilterChips
                label="Show"
                value={filter}
                onChange={setFilter}
                options={[
                  { value: "all", label: "All", count: rows.length },
                  {
                    value: "waiting",
                    label: "Waiting for DNS",
                    count: waiting,
                  },
                ]}
              />
            </div>
          ) : null}
          <Panel>
            <Table fluid>
              <thead>
                <tr>
                  <Th>Domain</Th>
                  <Th className="hidden sm:table-cell">Routed to</Th>
                  <Th>Status</Th>
                  <Th className="hidden md:table-cell">Requests, 24h</Th>
                  <Th className="hidden lg:table-cell">Added</Th>
                </tr>
              </thead>
              <tbody>
                {shown.map((r) => (
                  <Tr
                    key={r.h.hostname}
                    onClick={() =>
                      void navigate({
                        to: "/$team/domains/$hostname",
                        params: { team: currentSlug(), hostname: r.h.hostname },
                      })
                    }
                  >
                    <Td className="py-4">
                      <span className="flex min-w-0 items-center gap-2">
                        <Link
                          to="/$team/domains/$hostname"
                          params={{
                            team: currentSlug(),
                            hostname: r.h.hostname,
                          }}
                          onClick={(e) => e.stopPropagation()}
                          className="min-w-0 truncate text-[14px] text-foreground underline decoration-foreground/30 underline-offset-4 transition-colors hover:decoration-foreground"
                        >
                          {r.d.hostname}
                        </Link>
                        <DomainMark d={r.d} clusterId={r.a.clusterId} logoOnly />
                        <DomainKind hostname={r.d.hostname} />
                      </span>
                      <span className="mt-1 block truncate text-[12px] text-muted-foreground sm:hidden">
                        <RoutedTo {...r} />
                      </span>
                    </Td>
                    <Td className="hidden py-4 text-[13px] sm:table-cell">
                      <RoutedTo {...r} />
                    </Td>
                    <Td className="py-4">
                      <DomainStatus
                        d={r.d}
                        certs={s.certificates}
                        notDeployed={!r.a.currentDeploymentId}
                        plain
                      />
                    </Td>
                    <Td className="hidden w-[200px] py-4 md:table-cell">
                      <Traffic
                        hostname={r.h.hostname}
                        loading={traffic.loading}
                        byHost={traffic.byHost}
                      />
                    </Td>
                    <Td className="hidden py-4 text-[13px] text-muted-foreground lg:table-cell">
                      <TimeAgo at={r.d.createdAt} />
                    </Td>
                  </Tr>
                ))}
                {!shown.length ? (
                  <tr>
                    <Td
                      colSpan={5}
                      className="py-8 text-center text-muted-foreground"
                    >
                      No domains match.
                    </Td>
                  </tr>
                ) : null}
              </tbody>
            </Table>
          </Panel>
        </ListSection>
      ) : (
        <Panel>
          <EmptyState
            icon={GlobeIcon}
            title="No domains yet"
            description="Point a hostname at one of your services."
            action={
              <Button size="sm" onClick={() => setAdding(true)}>
                Add domain
              </Button>
            }
          />
        </Panel>
      )}
      <AddDomainDialog open={adding} onClose={() => setAdding(false)} />
    </Page>
  );
}

type Row = {
  d: Domain;
  a: Application;
  project?: Project;
  services: string[];
};

/** Where a hostname goes: its service and project, several services, or a redirect. */
function RoutedTo({ d, a, project, services }: Row) {
  if (services.length > 1) return <>{services.length} services</>;
  if (onlyRedirects(d))
    return (
      <span className="font-mono text-[12.5px] text-muted-foreground">
        →{" "}
        {redirectTarget(
          d.redirects!.find((x) => x.path === "/") ?? d.redirects![0],
        )}
      </span>
    );
  return (
    <>
      <AppLink
        href={servicePath(a.id)}
        onClick={(e) => e.stopPropagation()}
        className="text-foreground hover:text-primary"
      >
        {a.name}
      </AppLink>
      {project ? (
        <span className="text-muted-foreground"> · {project.name}</span>
      ) : null}
    </>
  );
}

const SPARK_W = 112;

/**
 * Requests per hostname over the last day, one request for the whole list.
 * None while analytics is off, and a failed load shows as no data.
 */
function useDomainTraffic(s: PyoState, any: boolean) {
  const team = s.currentTeamId;
  const on = s.observabilitySettings.enabled && any;
  const q = useQuery(
    `domain-traffic:${team}:${on}`,
    () => (on ? fetchDomainTraffic(team, "24h") : Promise.resolve(null)),
    { refreshMs: 60_000 },
  );
  const byHost = useMemo(
    () =>
      new Map(
        (q.status === "success" ? (q.data?.domains ?? []) : []).map((x) => [
          x.host,
          x,
        ]),
      ),
    [q],
  );
  return { loading: on && q.status === "loading", byHost };
}

/**
 * A sparkline and the total, in a box of fixed size: a faint line while
 * loading, a dash without requests. `hostname` in lower case, as the API
 * sends it.
 */
function Traffic({
  hostname,
  loading,
  byHost,
}: {
  hostname: string;
  loading: boolean;
  byHost: Map<string, { requests: number; series: number[] }>;
}) {
  const t = byHost.get(hostname);
  return (
    <span className="flex h-7 items-center gap-3">
      {loading ? (
        <span
          aria-hidden
          className="block h-px bg-divider"
          style={{ width: SPARK_W }}
        />
      ) : t ? (
        <>
          <Sparkline
            values={t.series}
            width={SPARK_W}
            height={28}
            area
            label={`${hostname}, requests over the last 24 hours`}
          />
          <span className="text-[13px] tabular-nums">
            {compact(t.requests, true)}
          </span>
        </>
      ) : (
        <span className="text-muted-foreground">—</span>
      )}
    </span>
  );
}
