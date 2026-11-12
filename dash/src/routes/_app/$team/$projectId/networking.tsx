import { createFileRoute } from "@tanstack/react-router";
import {
  projectSettingsNav,
  SettingsFrame,
} from "~/components/pyo/SettingsFrame";
import { DomainStatus } from "~/components/pyo/DomainState";
import { EndpointTable } from "~/components/pyo/tables";
import { EmptyState, Panel } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { EntityLink } from "~/components/pyo/common";
import { usePyo, servicePath, projectIdOf } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/$projectId/networking")({
  component: () => {
    const projectId = projectIdOf(Route.useParams());
    return (
      <SettingsFrame items={projectSettingsNav(projectId)}>
        <ProjectNetworking />
      </SettingsFrame>
    );
  },
});

function ProjectNetworking() {
  const projectId = projectIdOf(Route.useParams());
  const s = usePyo();
  const apps = s.services.filter((a) => a.projectId === projectId);
  const domains = apps.flatMap((a) => a.domains.map((d) => ({ a, d })));
  return (
    <div className="space-y-4">
      <Panel title="Domains">
        {domains.length ? (
          <Table>
            <thead>
              <tr>
                <Th>Hostname</Th>
                <Th>Service</Th>
                <Th>Cluster</Th>
                <Th>Status</Th>
              </tr>
            </thead>
            <tbody>
              {domains.map(({ a, d }) => (
                <Tr key={d.id}>
                  <Td className="font-mono text-[12px]">{d.hostname}</Td>
                  <Td>
                    <EntityLink to={servicePath(a.id, "domains")}>
                      {a.name}
                    </EntityLink>
                  </Td>
                  <Td>{s.clusters.find((c) => c.id === a.clusterId)?.name}</Td>
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
          <EmptyState
            compact
            title="No domains"
            description="Add one from a service's Domains."
          />
        )}
      </Panel>
      <Panel title="Ports and exposure">
        <EndpointTable apps={apps} s={s} />
      </Panel>
    </div>
  );
}
