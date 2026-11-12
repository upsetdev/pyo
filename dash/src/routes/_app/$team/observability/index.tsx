import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/_app/$team/observability/")({
  beforeLoad: ({ params }) => {
    throw redirect({
      to: "/$team/observability/analytics",
      params,
      replace: true,
    });
  },
});
