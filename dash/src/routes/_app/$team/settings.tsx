import { createFileRoute, Outlet } from "@tanstack/react-router";
import { Page, PageHeader } from "~/components/ui/Panel";
import { RouteTabs } from "~/components/ui/Table";
import { fetchMeta } from "~/lib/pyo/backend";
import { useTeamPathname } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import { isTeamAdmin, roleIn, usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/settings")({
  head: () => ({ meta: seo({ title: "Team" }) }),
  component: SettingsLayout,
});

// The team's settings, a tab each: General, Observability, and Billing on
// a Pyo that bills (its owners and admins, as in the sidebar). The page is
// titled with the one you're in.
const TITLES: Record<string, string> = {
  "/settings": "Settings",
  "/settings/billing": "Billing",
  "/settings/observability": "Observability",
};

function SettingsLayout() {
  const pathname = useTeamPathname();
  const s = usePyo();
  const meta = useQuery("meta", fetchMeta);
  const billing =
    meta.status === "success" &&
    meta.data.features.includes("billing") &&
    isTeamAdmin(roleIn(s));
  return (
    <Page width="max-w-4xl">
      <div className="space-y-4">
        <PageHeader title={TITLES[pathname] ?? "Settings"} />
        <RouteTabs
          label="Team settings"
          tabs={[
            { label: "General", to: "/settings", exact: true },
            { label: "Observability", to: "/settings/observability" },
            ...(billing ? [{ label: "Billing", to: "/settings/billing" }] : []),
          ]}
        />
      </div>
      <Outlet />
    </Page>
  );
}
