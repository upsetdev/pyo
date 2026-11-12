import { createFileRoute } from "@tanstack/react-router";
import { DeploymentsTable } from "~/components/pyo/DeploymentsTable";
import { deploymentsWhere, usePyo, serviceIdOf } from "~/lib/pyo/store";

export const Route = createFileRoute(
  "/_app/$team/$projectId_/$serviceId/deployments",
)({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const s = usePyo();
    return (
      <DeploymentsTable
        s={s}
        deployments={deploymentsWhere(s, { serviceId })}
        showService={false}
      />
    );
  },
});
