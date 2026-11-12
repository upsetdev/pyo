import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { ArrowsClockwiseIcon, GraphicsCardIcon } from "@phosphor-icons/react";
import { ClusterCommandDialog } from "~/components/pyo/AgentInstall";
import { Button } from "~/components/ui/Button";
import { StatusNote } from "~/components/ui/StatusNote";
import { ServerMetrics } from "~/components/pyo/ServerMetrics";
import { ActivityRows } from "~/components/pyo/widgets";
import { PlaceFlags } from "~/components/ui/Flag";
import { Select } from "~/components/ui/Field";
import { Callout, Metric, MetricStrip, Panel } from "~/components/ui/Panel";
import { placesOf } from "~/lib/pyo/location";
import { gpuSummary } from "~/lib/pyo/format";
import { plural } from "~/lib/pyo/format";
import {
  activityIn,
  clusterByRef,
  clusterSummary,
  serversOf,
  usePyo,
} from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/clusters/$clusterId/")({
  component: ClusterOverview,
});

const RANGES = [
  { value: "1h", label: "Last hour" },
  { value: "24h", label: "Last 24 hours" },
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days" },
];

// A cluster at a glance: its health, then CPU, memory and pods over time
// (from the samples Pyo keeps every minute), per server and per service.
function ClusterOverview() {
  const { clusterId } = Route.useParams();
  const s = usePyo();
  const [range, setRange] = useState("24h");
  const [showCommand, setShowCommand] = useState(false);
  const c = clusterByRef(s, clusterId);
  if (!c) return null;
  const sum = clusterSummary(s, c);
  const nodes = serversOf(s, c.id);
  const services = s.services.filter((a) => a.clusterId === c.id);
  const running = services.filter((a) => a.status === "running").length;
  const places = placesOf(c, s.servers);
  const events = activityIn(s, c.teamId)
    .filter((e) => e.clusterId === c.id)
    .slice(0, 8);
  return (
    <div className="space-y-8">
      <ClusterCommandDialog
        cluster={c}
        open={showCommand}
        onClose={() => setShowCommand(false)}
      />
      {c.status === "pending" ? (
        <StatusNote
          tone="progress"
          icon={<ArrowsClockwiseIcon size={15} className="animate-spin" />}
          title="Setting up"
          action={
            <Button
              size="xs"
              variant="secondary"
              onClick={() => setShowCommand(true)}
            >
              Show install command
            </Button>
          }
        >
          Waiting for the agent&apos;s first report. Run the install command on
          the machine to connect it.
        </StatusNote>
      ) : c.status === "unreachable" ? (
        <Callout tone="danger" title="Cluster unreachable">
          The agent stopped reporting. Workloads may still run.
        </Callout>
      ) : c.status === "degraded" ? (
        <Callout tone="warning" title="Cluster degraded">
          {sum.servers - sum.readyServers} of {plural(sum.servers, "server")} offline.
        </Callout>
      ) : null}

      <MetricStrip cols="grid-cols-2 lg:grid-cols-4">
        <Metric
          label="Servers online"
          value={`${sum.readyServers}/${sum.servers}`}
          sub={`${sum.controlPlane} main`}
          tone={sum.readyServers < sum.servers ? "warning" : undefined}
        />
        <Metric
          label="Services"
          value={services.length}
          sub={`${running} running`}
          tone={running < services.length ? "warning" : undefined}
        />
        <Metric
          label="Workloads"
          value={sum.workloads}
          sub={`${sum.unhealthyWorkloads} unhealthy · ${sum.pods} containers`}
          tone={sum.unhealthyWorkloads ? "warning" : undefined}
        />
        <Metric
          label={places.length === 1 ? "Region" : "Regions"}
          value={
            <span className="flex items-center gap-2">
              <PlaceFlags places={places.map((p) => p.location)} size={26} />
              {places.length > 1 ? places.length : null}
            </span>
          }
          sub={plural(nodes.length, "server")}
        />
      </MetricStrip>
      {gpuSummary(nodes) ? (
        <p className="-mt-5 flex items-center gap-1.5 text-[12.5px] text-muted-foreground">
          <GraphicsCardIcon size={14} aria-hidden />
          GPUs: {gpuSummary(nodes)}
          {nodes.reduce((t, n) => t + (n.gpusUsed ?? 0), 0)
            ? ` · ${nodes.reduce((t, n) => t + (n.gpusUsed ?? 0), 0)} in use`
            : ""}
        </p>
      ) : null}

      <section aria-labelledby="usage-h" className="space-y-4">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="usage-h" className="text-[15px] font-semibold">
            Usage
          </h2>
          <div className="w-40 sm:w-44">
            <Select
              value={range}
              onChange={(e) => setRange(e.target.value)}
              aria-label="Time range"
              heading="Time range"
            >
              {RANGES.map((r) => (
                <option key={r.value} value={r.value}>
                  {r.label}
                </option>
              ))}
            </Select>
          </div>
        </div>
        {c.status === "unreachable" ? (
          <p className="text-[12.5px] text-muted-foreground">
            No new samples while unreachable.
          </p>
        ) : null}
        <ServerMetrics team={c.teamId} cluster={c.id} range={range} inCluster />
      </section>

      <Panel title="Recent events">
        <ActivityRows events={events} emptyTitle="Nothing happened here yet" />
      </Panel>
    </div>
  );
}
