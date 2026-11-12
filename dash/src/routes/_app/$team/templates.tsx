import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { TemplateGallery } from "~/components/pyo/TemplateGallery";
import { Page, PageHeader } from "~/components/ui/Panel";
import { usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/templates")({
  head: () => ({ meta: seo({ title: "Templates" }) }),
  component: TemplatesPage,
});

function TemplatesPage() {
  const s = usePyo();
  const navigate = useNavigate();
  return (
    <Page>
      <PageHeader title="Templates" description="Ready-to-run services." />
      <TemplateGallery
        recentIds={s.recentTemplateIds}
        onSelect={(t) =>
          void navigate({
            to: "/$team/deploy/new",
            params: { team: currentSlug() },
            search: { source: "template", template: t.id },
          })
        }
      />
    </Page>
  );
}
