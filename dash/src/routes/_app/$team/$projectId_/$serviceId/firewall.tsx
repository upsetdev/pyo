import { createFileRoute } from "@tanstack/react-router";
import { FirewallTab } from "~/components/pyo/FirewallTab";
import { serviceIdOf, usePyo } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/firewall")({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const s = usePyo();
    const app = s.services.find((a) => a.id === serviceId);
    const cluster = app && s.clusters.find((c) => c.id === app.clusterId);
    return app && cluster ? <FirewallTab key={app.id} cluster={cluster} app={app} /> : null;
  },
});
