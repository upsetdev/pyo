import { createFileRoute } from "@tanstack/react-router";
import { ClusterNetworking } from "~/components/pyo/ClusterNetworking";
import { InfraHeader } from "~/components/pyo/infra";
import { EmptyState, Page, Panel } from "~/components/ui/Panel";
import { clustersIn, usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/clusters/networking")({
  head: () => ({ meta: seo({ title: "Networking" }) }),
  component: () => {
    const s = usePyo();
    const clusters = clustersIn(s);
    return (
      <Page>
        <InfraHeader />
        {clusters.length ? (
          clusters.map((c) => (
            <ClusterNetworking key={c.id} cluster={c} titled />
          ))
        ) : (
          <Panel>
            <EmptyState compact title="No servers yet" />
          </Panel>
        )}
      </Page>
    );
  },
});
