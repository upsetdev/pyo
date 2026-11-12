import { TrashIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { VolumeTable } from "~/components/pyo/tables";
import {
  DeleteVolumeDialog,
  detachedSummary,
} from "~/components/pyo/VolumeDialogs";
import { Button } from "~/components/ui/Button";
import { gib } from "~/lib/pyo/format";
import type { StrayVolume } from "~/lib/pyo/types";
import { Badge } from "~/components/ui/Badge";
import { Panel } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { clusterByRef, usePyo } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/clusters/$clusterId/storage")(
  {
    component: () => {
      const { clusterId } = Route.useParams();
      const s = usePyo();
      const c = clusterByRef(s, clusterId);
      if (!c) return null;
      return (
        <div className="space-y-4">
          <Panel title="Storage classes">
            <Table>
              <thead>
                <tr>
                  <Th>Name</Th>
                  <Th>Provisioner</Th>
                  <Th>Survives a server failure</Th>
                  <Th>Reclaim</Th>
                  <Th>Binding</Th>
                </tr>
              </thead>
              <tbody>
                {c.capabilities.storageClasses.map((sc) => (
                  <Tr key={sc.name}>
                    <Td className="font-mono text-[12px]">
                      {sc.name}{" "}
                      {sc.isDefault ? (
                        <Badge className="ml-1">default</Badge>
                      ) : null}
                    </Td>
                    <Td className="font-mono text-[11.5px] text-muted-foreground">
                      {sc.provisioner}
                    </Td>
                    <Td>
                      {sc.replicated ? (
                        <Badge tone="success">Yes</Badge>
                      ) : (
                        <Badge tone="warning">No, one server</Badge>
                      )}
                    </Td>
                    <Td>{sc.reclaimPolicy}</Td>
                    <Td className="text-muted-foreground">
                      {sc.volumeBindingMode}
                    </Td>
                  </Tr>
                ))}
              </tbody>
            </Table>
          </Panel>
          <ClusterVolumes clusterId={c.id} />
        </div>
      );
    },
  },
);

function ClusterVolumes({ clusterId }: { clusterId: string }) {
  const s = usePyo();
  const c = s.clusters.find((x) => x.id === clusterId);
  const [removing, setRemoving] = useState<StrayVolume | null>(null);
  const volumes = s.volumes.filter((v) => v.clusterId === clusterId);
  const detached = detachedSummary(volumes);
  const strays = c?.strayVolumes ?? [];
  // Where a claim is: its project, when one of the team's has that namespace.
  const where = (ns: string) =>
    s.projects.find((p) => p.teamId === c?.teamId && p.namespace === ns)
      ?.name ?? ns;
  return (
    <>
      <Panel
        title="Volumes"
        actions={
          detached ? (
            <span className="text-[12px] text-muted-foreground">
              {detached}
            </span>
          ) : null
        }
      >
        <VolumeTable s={s} volumes={volumes} showCluster={false} showProject />
      </Panel>
      {strays.length ? (
        <Panel title="Not in Pyo">
          <Table>
            <thead>
              <tr>
                <Th>Volume</Th>
                <Th>Project</Th>
                <Th>Server</Th>
                <Th className="w-10">
                  <span className="sr-only">Actions</span>
                </Th>
              </tr>
            </thead>
            <tbody>
              {strays.map((v) => (
                <Tr key={`${v.namespace}/${v.name}`}>
                  <Td>
                    <p className="font-mono text-[12px] text-foreground">
                      {v.name}
                    </p>
                    {v.sizeGi ? (
                      <p className="text-[11.5px] text-muted-foreground">
                        {gib(v.sizeGi)}
                      </p>
                    ) : null}
                  </Td>
                  <Td className="font-mono text-[11.5px]">
                    {where(v.namespace)}
                  </Td>
                  <Td className="text-muted-foreground">{v.server || "—"}</Td>
                  <Td>
                    {v.deleting ? (
                      <Badge tone="warning">Deleting</Badge>
                    ) : (
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7"
                        aria-label={`Delete ${v.name}`}
                        onClick={() => setRemoving(v)}
                      >
                        <TrashIcon size={14} />
                      </Button>
                    )}
                    {v.error ? (
                      <p className="mt-1 max-w-[220px] text-[11px] leading-4 text-muted-foreground">
                        {v.error}
                      </p>
                    ) : null}
                  </Td>
                </Tr>
              ))}
            </tbody>
          </Table>
        </Panel>
      ) : null}
      <DeleteVolumeDialog
        stray={removing ? { clusterId, volume: removing } : null}
        onClose={() => setRemoving(null)}
      />
    </>
  );
}
