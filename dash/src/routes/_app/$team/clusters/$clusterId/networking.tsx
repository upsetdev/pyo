import { createFileRoute } from "@tanstack/react-router";
import { ClusterNetworking } from "~/components/pyo/ClusterNetworking";
import { clusterByRef, usePyo } from "~/lib/pyo/store";

export const Route = createFileRoute(
  "/_app/$team/clusters/$clusterId/networking",
)({
  component: () => {
    const { clusterId } = Route.useParams();
    const c = clusterByRef(usePyo(), clusterId);
    return c ? <ClusterNetworking cluster={c} /> : null;
  },
});
