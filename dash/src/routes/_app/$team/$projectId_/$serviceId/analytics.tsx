import { createFileRoute, redirect } from "@tanstack/react-router";
import { getState, serviceIdOf } from "~/lib/pyo/store";

// Analytics lives on one page; a service's "View analytics" lands there filtered to its
// project, and to its website when it has exactly one.
export const Route = createFileRoute(
  "/_app/$team/$projectId_/$serviceId/analytics",
)({
  beforeLoad: ({ params }) => {
    const app = getState().services.find((a) => a.id === serviceIdOf(params));
    const domains = app?.domains.filter((d) => d.status === "active") ?? [];
    throw redirect({
      to: "/$team/observability/analytics",
      params: { team: params.team },
      search: {
        project: app?.projectId,
        site: domains.length === 1 ? domains[0].id : undefined,
      },
      replace: true,
    });
  },
});
