import { createFileRoute } from "@tanstack/react-router";
import { AddSandboxServerFlow } from "~/components/pyo/sandbox/AddServerFlow";
import { Page, PageHeader } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/sandboxes/servers/new")({
  head: () => ({ meta: seo({ title: "Add sandbox server" }) }),
  component: () => (
    <Page width="max-w-2xl">
      <PageHeader title="Add sandbox server" />
      <AddSandboxServerFlow />
    </Page>
  ),
});
