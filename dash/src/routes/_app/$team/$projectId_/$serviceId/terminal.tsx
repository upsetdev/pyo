import { createFileRoute } from "@tanstack/react-router";
import { ServiceTerminal } from "~/components/pyo/Terminal";
import { Panel } from "~/components/ui/Panel";
import { usePyo, serviceIdOf } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/terminal")(
  {
    component: () => {
      const serviceId = serviceIdOf(Route.useParams());
      const app = usePyo().services.find((a) => a.id === serviceId);
      return app ? (
        <Panel>
          <ServiceTerminal key={app.id} app={app} />
        </Panel>
      ) : null;
    },
  },
);
