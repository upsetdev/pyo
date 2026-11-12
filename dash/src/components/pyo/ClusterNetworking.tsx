import type { ReactNode } from "react";
import { Layered } from "~/components/ui/Layered";
import { EmptyState, Panel } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { DomainStatus } from "~/components/pyo/DomainState";
import { usePyo, servicePath } from "~/lib/pyo/store";
import type { Cluster } from "~/lib/pyo/types";
import { CodeText, EntityLink } from "./common";
import { EndpointTable } from "./tables";

/** Exposure setup of one cluster: what it can do, its HTTP routes and its Services. */
export function ClusterNetworking({
  cluster: c,
  titled,
}: {
  cluster: Cluster;
  titled?: boolean;
}) {
  const s = usePyo();
  const apps = s.services.filter((a) => a.clusterId === c.id);
  const routes = apps.flatMap((a) => a.domains.map((d) => ({ a, d })));
  const lb = c.capabilities.loadBalancer;
  return (
    <div className="space-y-4">
      {titled ? (
        <h2 className="pt-2 text-[15px] font-semibold">{c.name}</h2>
      ) : null}
      <Layered bare>
        <div className="grid gap-1.5 sm:grid-cols-2 lg:grid-cols-4">
          {(
            [
              [
                "Gateway",
                c.capabilities.ingressController === "envoy" ? (
                  "Running"
                ) : c.capabilities.gatewayError ? (
                  <CodeText text={c.capabilities.gatewayError} />
                ) : (
                  "Not running yet"
                ),
              ],
              [
                "Public IPs",
                c.publicTraffic === false
                  ? "Public traffic is off"
                  : lb === "none"
                  ? "None"
                  : `${lb === "servicelb" ? "k3s ServiceLB" : "MetalLB"} · ${c.capabilities.loadBalancerIPs.join(", ") || "no IPs"}`,
              ],
              [
                "TLS",
                c.capabilities.certManager
                  ? "cert-manager issues certificates"
                  : "No cert-manager, so HTTP only",
              ],
              ["CIDRs", `containers ${c.podCidr} · services ${c.serviceCidr}`],
            ] as [string, ReactNode][]
          ).map(([k, v]) => (
            <div
              key={k}
              className="rounded-[14px] border border-card-border bg-card px-4 py-3"
            >
              <p className="text-[11px] font-medium text-muted-foreground">
                {k}
              </p>
              <p className="mt-0.5 break-words text-[12.5px]">{v}</p>
            </div>
          ))}
        </div>
      </Layered>
      <Panel title="HTTP routes">
        {routes.length ? (
          <Table>
            <thead>
              <tr>
                <Th>Host</Th>
                <Th>Backend</Th>
                <Th>Status</Th>
              </tr>
            </thead>
            <tbody>
              {routes.map(({ a, d }) => (
                <Tr key={d.id}>
                  <Td className="font-mono text-[12px]">{d.hostname}</Td>
                  <Td>
                    <EntityLink to={servicePath(a.id, "domains")}>
                      {a.name}
                    </EntityLink>
                    <span className="text-muted-foreground">:{d.portName}</span>
                  </Td>
                  <Td>
                    <DomainStatus
                      d={d}
                      certs={s.certificates}
                      notDeployed={!a.currentDeploymentId}
                    />
                  </Td>
                </Tr>
              ))}
            </tbody>
          </Table>
        ) : (
          <EmptyState compact title="No HTTP routes" />
        )}
      </Panel>
      <Panel title="Ports">
        <EndpointTable apps={apps} s={s} />
      </Panel>
    </div>
  );
}
