import { createFileRoute } from "@tanstack/react-router";
import { DeploymentsTable } from "~/components/pyo/DeploymentsTable";
import { deploymentsWhere, usePyo, projectIdOf } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/$projectId/deployments")({
  component: () => {
    const projectId = projectIdOf(Route.useParams());
    const s = usePyo();
    return (
      <DeploymentsTable
        s={s}
        deployments={deploymentsWhere(s, { projectId })}
      />
    );
  },
});
