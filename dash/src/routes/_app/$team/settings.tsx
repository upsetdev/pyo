import { createFileRoute, Outlet } from "@tanstack/react-router";
import { Page, PageHeader } from "~/components/ui/Panel";
import { useTeamPathname } from "~/lib/pyo/paths";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/settings")({
  head: () => ({ meta: seo({ title: "Team" }) }),
  component: SettingsLayout,
});

// The sidebar lists the sections (see SettingsNav in the shell); the page
// is titled with the one you're in.
const TITLES: Record<string, string> = {
  "/settings": "Settings",
};

function SettingsLayout() {
  const pathname = useTeamPathname();
  return (
    <Page width="max-w-4xl">
      <PageHeader title={TITLES[pathname] ?? "Settings"} />
      <Outlet />
    </Page>
  );
}
