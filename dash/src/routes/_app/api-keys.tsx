import { createFileRoute } from "@tanstack/react-router";
import { ApiTokensPanel } from "~/components/pyo/ApiTokens";
import { CliInstallPanel } from "~/components/pyo/CliInstall";
import { AppShell } from "~/components/shell/AppShell";
import { Page, PageHeader } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";

// Your API keys (pyo_…) for the REST API. Outside any team: a key acts as you everywhere.
export const Route = createFileRoute("/_app/api-keys")({
  head: () => ({ meta: seo({ title: "API keys" }) }),
  component: () => (
    <AppShell>
      <Page width="max-w-3xl">
        <PageHeader
          title="API keys"
          description="For scripts and CI. A key acts as you."
        />
        <CliInstallPanel />
        <ApiTokensPanel />
      </Page>
    </AppShell>
  ),
});
