import { GraphicsCardIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { ServerTable } from "~/components/pyo/infra";
import { Panel } from "~/components/ui/Panel";
import { gpuSummary } from "~/lib/pyo/format";
import { clusterByRef, serversOf, usePyo } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/clusters/$clusterId/servers")(
  {
    component: () => {
      const { clusterId } = Route.useParams();
      const s = usePyo();
      const nodes = serversOf(s, clusterByRef(s, clusterId)?.id ?? "");
      const gpus = gpuSummary(nodes);
      return (
        <div className="space-y-2">
          {gpus ? (
            <p className="flex items-center gap-1.5 px-1 text-[12.5px] text-muted-foreground">
              <GraphicsCardIcon size={14} aria-hidden />
              {gpus}
            </p>
          ) : null}
          <Panel>
            <ServerTable s={s} nodes={nodes} />
          </Panel>
        </div>
      );
    },
  },
);
