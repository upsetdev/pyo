import { createFileRoute } from "@tanstack/react-router";
import { AssistantsPanel, McpActivityPanel } from "~/components/pyo/ApiTokens";
import { AppShell } from "~/components/shell/AppShell";
import { Page, PageHeader } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";

// Connecting AI assistants over MCP. They sign in with OAuth, so there is no key to paste.
export const Route = createFileRoute("/_app/mcp")({
  head: () => ({ meta: seo({ title: "MCP" }) }),
  component: () => (
    <AppShell>
      <Page width="max-w-3xl">
        <PageHeader
          title="MCP"
          description="Let AI assistants use Pyo as you."
        />
        <AssistantsPanel />
        <McpActivityPanel />
      </Page>
    </AppShell>
  ),
});
