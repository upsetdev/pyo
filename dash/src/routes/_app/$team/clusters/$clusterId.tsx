import { RouteTabs } from "~/components/ui/Table";
import { PlusIcon, TreeStructureIcon } from "@phosphor-icons/react";
import { TagsBeside } from "~/components/ui/TagInput";
import { createFileRoute, Outlet } from "@tanstack/react-router";
import { placesOf } from "~/lib/pyo/location";
import { PlaceFlags } from "~/components/ui/Flag";
import { ClusterStatusBadge } from "~/components/ui/Badge";
import { ButtonLink } from "~/components/ui/Button";
import { EmptyState, Page, PageHeader } from "~/components/ui/Panel";
import { api, clusterByRef, clusterSummary, usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/clusters/$clusterId")({
  head: () => ({ meta: seo({ title: "Cluster" }) }),
  component: ClusterLayout,
});

function ClusterLayout() {
  const { clusterId } = Route.useParams();
  const s = usePyo();
  const c = clusterByRef(s, clusterId);
  if (!c || c.teamId !== s.currentTeamId)
    return (
      <Page>
        <EmptyState
          icon={TreeStructureIcon}
          title="Cluster not found"
          description="It may have been removed."
          action={
            <ButtonLink href="/clusters" size="sm" variant="secondary">
              All clusters
            </ButtonLink>
          }
        />
      </Page>
    );
  const sum = clusterSummary(s, c);
  const base = `/clusters/${c.name}`;
  return (
    <Page>
      <div className="space-y-4">
        <PageHeader
          title={
            <span className="flex items-center gap-2.5">
              {c.name}
              <ClusterStatusBadge status={c.status} />
              <span className="flex flex-wrap items-center gap-1 text-[13px] font-normal">
                <TagsBeside
                  name={c.name}
                  tags={c.tags}
                  suggestions={[
                    ...new Set(s.clusters.flatMap((x) => x.tags ?? [])),
                  ]}
                  hint="Used to place services, e.g. production."
                  onSave={(tags) => api.updateCluster(c.id, { tags })}
                />
              </span>
            </span>
          }
          meta={
            <>
              <span>
                {c.distribution} {c.version}
              </span>
              <PlaceFlags
                places={placesOf(c, s.servers).map((pl) => pl.location)}
                size={26}
              />
              <span className="font-mono">{c.connection.apiEndpoint}</span>
            </>
          }
          actions={
            <>
              <ButtonLink
                href={`${base}/servers/new`}
                size="sm"
                variant="secondary"
              >
                <PlusIcon size={13} weight="bold" /> Add server
              </ButtonLink>
              <ButtonLink href={`/deploy/new`} size="sm" variant="secondary">
                Deploy to this cluster
              </ButtonLink>
            </>
          }
        />
        <RouteTabs
          label="Cluster sections"
          tabs={[
            { label: "Overview", to: base, exact: true },
            { label: "Servers", to: `${base}/servers`, count: sum.servers },
            { label: "Workloads", to: `${base}/workloads`, count: sum.workloads },
            { label: "Networking", to: `${base}/networking` },
            { label: "Storage", to: `${base}/storage` },
            { label: "Settings", to: `${base}/settings` },
          ]}
        />
      </div>
      <Outlet />
    </Page>
  );
}
