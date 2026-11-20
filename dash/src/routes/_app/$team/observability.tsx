import { useTeamPathname } from "~/lib/pyo/paths";
import { createFileRoute, Outlet, useSearch } from "@tanstack/react-router";
import { Page, PageHeader } from "~/components/ui/Panel";
import { Tooltip } from "~/components/ui/Tooltip";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/observability")({
  head: () => ({ meta: seo({ title: "Observability" }) }),
  component: ObservabilityLayout,
});

// Traffic and server metrics (Analytics), and every request the web server
// handled (Logs), for the team's services.
function ObservabilityLayout() {
  const pathname = useTeamPathname();
  const range = (useSearch({ strict: false }) as { range?: string }).range;
  const logs = pathname === "/observability/logs";
  // As the pages refresh: logs often for a short range, analytics every 30s.
  const every = !logs
    ? "Updates every 30 seconds."
    : range === "7d" || range === "30d"
      ? "Updates every minute."
      : "Updates every 5 seconds.";
  return (
    <Page>
      <div className="space-y-4">
        <PageHeader
          title={
            <span className="inline-flex items-center gap-3">
              {logs ? "Logs" : "Analytics"}
              <Tooltip content={every}>
                <span className="inline-flex items-center gap-1.5 rounded-full border border-success/25 bg-success/10 px-2 py-0.5 text-[11.5px] font-medium tracking-normal text-success">
                  <span className="relative flex h-1.5 w-1.5">
                    <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-success opacity-60" />
                    <span className="relative inline-flex h-1.5 w-1.5 rounded-full bg-success" />
                  </span>
                  Live
                </span>
              </Tooltip>
            </span>
          }
        />
      </div>
      <Outlet />
    </Page>
  );
}
