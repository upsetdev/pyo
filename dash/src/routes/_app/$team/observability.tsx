import { useTeamPathname } from "~/lib/pyo/paths";
import { createFileRoute, Outlet } from "@tanstack/react-router";
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
  return (
    <Page>
      <div className="space-y-4">
        <PageHeader
          title={
            <span className="inline-flex items-center gap-3">
              {pathname === "/observability/logs" ? "Logs" : "Analytics"}
              <Tooltip content="Updates every 30 seconds.">
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
