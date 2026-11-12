import { createFileRoute } from "@tanstack/react-router";
import { ServiceLogViewer } from "~/components/pyo/LogViewer";
import { Panel } from "~/components/ui/Panel";
import { usePyo, serviceIdOf } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/logs")({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const app = usePyo().services.find((a) => a.id === serviceId);
    return app ? (
      <Panel>
        <ServiceLogViewer key={`${app.id}:${app.status}`} app={app} />
      </Panel>
    ) : null;
  },
});
