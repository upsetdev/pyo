import { createFileRoute, Navigate, redirect } from "@tanstack/react-router";
import { DeploymentDetails } from "~/components/pyo/DeploymentDetails";
import { deploymentRedirect } from "~/lib/pyo/deploymentHome";
import { getState, usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

// A deployment's own page, for one whose service was deleted. Older links
// (alerts, emails, commit statuses) open here too: while the service exists
// they go on to its Deployments tab. The snapshot has loaded before this
// runs (the _app layout waits for it), so nothing shows first.
export const Route = createFileRoute("/_app/$team/deployments/$deploymentId")({
  head: () => ({ meta: seo({ title: "Deployment" }) }),
  beforeLoad: ({ params }) => {
    const r = deploymentRedirect(getState(), params.deploymentId, null);
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
  },
  component: PlainDeployment,
});

function PlainDeployment() {
  const { deploymentId } = Route.useParams();
  const r = deploymentRedirect(usePyo(), deploymentId, null);
  // It arrived in a later snapshot, with its service.
  if (r?.to === "service")
    return (
      <Navigate
        to="/$team/$projectId/$serviceId/deployments/$deploymentId"
        params={{
          team: r.team,
          projectId: r.projectId,
          serviceId: r.serviceId,
          deploymentId,
        }}
        replace
      />
    );
  return <DeploymentDetails deploymentId={deploymentId} />;
}
