import { createFileRoute, Outlet } from "@tanstack/react-router";
import { Page } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/certificates")({
  head: () => ({ meta: seo({ title: "Certificates" }) }),
  component: CertificatesLayout,
});

// The certificates a team uploads for its domains, and the ones it creates for
// its CDN.
function CertificatesLayout() {
  return (
    <Page>
      <Outlet />
    </Page>
  );
}
