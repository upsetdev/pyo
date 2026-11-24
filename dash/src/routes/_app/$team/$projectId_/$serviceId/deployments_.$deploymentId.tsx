import { createFileRoute, redirect } from "@tanstack/react-router";
import { DeploymentDetails } from "~/components/pyo/DeploymentDetails";
import { deploymentRedirect } from "~/lib/pyo/deploymentHome";
import { getState, serviceIdOf } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

// A deployment's details in its service's Deployments tab, under the
// service's header and tabs.
export const Route = createFileRoute(
  "/_app/$team/$projectId_/$serviceId/deployments_/$deploymentId",
)({
  head: () => ({ meta: seo({ title: "Deployment" }) }),
  // A link that names the wrong service opens the deployment where it
  // belongs: its own service, or its own page once that was deleted.
  beforeLoad: ({ params }) => {
    const r = deploymentRedirect(
      getState(),
      params.deploymentId,
      serviceIdOf(params),
    );
    if (r?.to === "service")
      throw redirect({
        to: "/$team/$projectId/$serviceId/deployments/$deploymentId",
        params: {
          team: r.team,
          projectId: r.projectId,
          serviceId: r.serviceId,
          deploymentId: params.deploymentId,
        },
        replace: true,
      });
    if (r?.to === "plain")
      throw redirect({
        to: "/$team/deployments/$deploymentId",
        params: { team: params.team, deploymentId: params.deploymentId },
        replace: true,
      });
  },
  component: () => {
    const { deploymentId } = Route.useParams();
    return <DeploymentDetails deploymentId={deploymentId} inService />;
  },
});
