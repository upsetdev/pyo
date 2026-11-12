import { createFileRoute } from "@tanstack/react-router";
import { WorkloadTable } from "~/components/pyo/infra";
import { clusterByRef, usePyo, workloadsOf } from "~/lib/pyo/store";

export const Route = createFileRoute(
  "/_app/$team/clusters/$clusterId/workloads",
)({
  component: () => {
    const { clusterId } = Route.useParams();
    const s = usePyo();
    return (
      <WorkloadTable
        s={s}
        workloads={workloadsOf(s, clusterByRef(s, clusterId)?.id ?? "")}
        showCluster={false}
      />
    );
  },
});
