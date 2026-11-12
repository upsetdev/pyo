import { plural } from "~/lib/pyo/format";
import { serversOf, usePyo } from "~/lib/pyo/store";
import { CodeText } from "./common";
import { SetupChecklist, type SetupCheck } from "./SetupChecklist";

/** What the agent has reported so far, ticked off as it comes in. */
export function ClusterChecklist({ clusterId }: { clusterId: string }) {
  const s = usePyo();
  const c = s.clusters.find((x) => x.id === clusterId);
  const nodes = serversOf(s, clusterId);
  const ready = nodes.filter((n) => n.status === "ready").length;
  const connected = !!c && c.status !== "pending";
  const defaultClass = c?.capabilities.storageClasses.find((x) => x.isDefault);

  const checks: SetupCheck[] = [
    {
      label: "Agent connected",
      done: connected,
      detail: connected
        ? `${c?.distribution} ${c?.version} · agent ${c?.connection.agentVersion ?? ""}`
        : "Waiting for the command…",
    },
    {
      label: "Servers found",
      done: connected && nodes.length > 0,
      detail: nodes.length
        ? `${plural(nodes.length, "server")}, ${ready} ready`
        : undefined,
    },
    {
      label: "Gateway",
      done: connected && c?.capabilities.ingressController !== "none",
      detail: connected ? (
        c?.capabilities.ingressController === "envoy" ? (
          "Running"
        ) : c?.capabilities.gatewayError ? (
          <CodeText text={c.capabilities.gatewayError} />
        ) : (
          "Not running yet: domains won't answer"
        )
      ) : undefined,
    },
    {
      label: "Default storage class",
      done: connected && !!defaultClass,
      detail: connected
        ? defaultClass
          ? defaultClass.name
          : "None: templates with storage can't deploy"
        : undefined,
    },
    {
      label: "Metrics",
      done: connected && !!c?.capabilities.metricsServer,
      detail: connected
        ? c?.capabilities.metricsServer
          ? "metrics-server"
          : "Not installed: CPU and memory show as 0"
        : undefined,
    },
  ];

  return (
    <SetupChecklist
      title="Waiting for it to connect"
      checks={checks.map((ch, i) =>
        i >= 2 ? { ...ch, missing: connected && !ch.done } : ch,
      )}
    />
  );
}
