import { createFileRoute } from "@tanstack/react-router";
import { OriginPullCertificates } from "~/components/pyo/Mtls";
import { Page, PageHeader } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";

// mTLS certificates for the team's CDN, and the clusters that require them.
export const Route = createFileRoute("/_app/$team/mtls")({
  head: () => ({ meta: seo({ title: "Mutual TLS (mTLS)" }) }),
  component: () => (
    <Page>
      <div className="space-y-4">
        <PageHeader title="Mutual TLS" />
      </div>
      <OriginPullCertificates />
    </Page>
  ),
});
