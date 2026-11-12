import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { InfraHeader } from "~/components/pyo/infra";
import { VolumeTable } from "~/components/pyo/tables";
import { Segmented, Select } from "~/components/ui/Field";
import { EmptyState, Page, Panel } from "~/components/ui/Panel";
import { clustersIn, usePyo, volumesIn } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/clusters/storage")({
  head: () => ({ meta: seo({ title: "Volumes" }) }),
  component: StoragePage,
});

type Attached = "all" | "attached" | "detached";

function StoragePage() {
  const s = usePyo();
  const [cluster, setCluster] = useState("");
  const [attached, setAttached] = useState<Attached>("all");
  const all = volumesIn(s);
  const clusters = clustersIn(s).filter((c) =>
    all.some((v) => v.clusterId === c.id),
  );
  const volumes = all.filter(
    (v) =>
      (!cluster || v.clusterId === cluster) &&
      (attached === "all" || !!v.serviceId === (attached === "attached")),
  );
  return (
    <Page>
      <InfraHeader />
      <Panel
        title="Volumes"
        actions={
          all.length ? (
            <div className="flex flex-wrap items-center gap-2">
              {clusters.length > 1 ? (
                <div className="w-40">
                  <Select
                    value={cluster}
                    onChange={(e) => setCluster(e.target.value)}
                    aria-label="Cluster"
                  >
                    <option value="">All clusters</option>
                    {clusters.map((c) => (
                      <option key={c.id} value={c.id}>
                        {c.name}
                      </option>
                    ))}
                  </Select>
                </div>
              ) : null}
              <Segmented
                label="Attached"
                size="md"
                value={attached}
                onChange={setAttached}
                options={[
                  { value: "all", label: "All" },
                  { value: "attached", label: "Attached" },
                  { value: "detached", label: "Not attached" },
                ]}
              />
            </div>
          ) : null
        }
      >
        {all.length && !volumes.length ? (
          <EmptyState compact title="No volumes match" />
        ) : (
          <VolumeTable s={s} volumes={volumes} showProject />
        )}
      </Panel>
    </Page>
  );
}
