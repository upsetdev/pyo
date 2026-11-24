import type { Application, Deployment, Project, Team } from "./types";

// Where a deployment's details open. Inside its service, under the
// Deployments tab, while the service exists. On a plain team page
// (/deployments/<id>) once it's deleted. Older links use the plain page,
// which forwards to the service when it can.

type Known = {
  deployments: Pick<Deployment, "id" | "serviceId">[];
  services: Pick<Application, "id" | "name" | "projectId" | "teamId">[];
  projects: Pick<Project, "id" | "namespace">[];
  teams: Pick<Team, "id" | "slug">[];
};

export type DeploymentHome =
  /** Its service exists: the route params of its page there. */
  | {
      kind: "service";
      service: string;
      team: string;
      projectId: string;
      serviceId: string;
    }
  /** Known, but its service is gone: the plain page shows it. */
  | { kind: "deleted" }
  /** Not in the snapshot. */
  | { kind: "unknown" };

export function deploymentHome(s: Known, id: string): DeploymentHome {
  const d = s.deployments.find((x) => x.id === id);
  if (!d) return { kind: "unknown" };
  const app = s.services.find((a) => a.id === d.serviceId);
  const team = app && s.teams.find((t) => t.id === app.teamId);
  if (!app || !team) return { kind: "deleted" };
  const p = s.projects.find((x) => x.id === app.projectId);
  return {
    kind: "service",
    service: app.id,
    team: team.slug,
    projectId: p?.namespace ?? app.projectId,
    serviceId: app.name,
  };
}

/** A deployment's page, team-relative: "/shop/web/deployments/dpl_1", or "/deployments/dpl_1". */
export function deploymentHref(s: Known, id: string): string {
  const h = deploymentHome(s, id);
  return h.kind === "service"
    ? `/${h.projectId}/${h.serviceId}/deployments/${id}`
    : `/deployments/${id}`;
}

export type DeploymentRedirect =
  | { to: "service"; team: string; projectId: string; serviceId: string }
  | { to: "plain" };

/**
 * Where a deployment's page sends someone, or null to show it where they
 * are. `at`: the id of the service whose tab they opened, or null on the
 * plain page. An unknown deployment stays put and says so.
 */
export function deploymentRedirect(
  s: Known,
  id: string,
  at: string | null,
): DeploymentRedirect | null {
  const h = deploymentHome(s, id);
  if (h.kind === "unknown") return null;
  if (h.kind === "deleted") return at === null ? null : { to: "plain" };
  if (h.service === at) return null;
  return {
    to: "service",
    team: h.team,
    projectId: h.projectId,
    serviceId: h.serviceId,
  };
}
