import { createFileRoute, useNavigate } from "@tanstack/react-router";
import {
  ProjectFormFields,
  useProjectForm,
} from "~/components/pyo/ProjectDialog";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Layered } from "~/components/ui/Layered";
import { Page, PageHeader } from "~/components/ui/Panel";
import { api, projectParams } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { seo } from "~/lib/seo";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/projects/new")({
  head: () => ({ meta: seo({ title: "New project" }) }),
  component: NewProjectPage,
});

function NewProjectPage() {
  const navigate = useNavigate();
  const f = useProjectForm();
  return (
    <Page width="max-w-2xl">
      <PageHeader title="New project" description="Group related services." />
      <form
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          f.setTouched(true);
          if (!f.valid) return;
          const id = api.createProject({
            name: f.name,
            description: f.description,
            namespace: f.namespace,
          });
          toast({ title: `Project ${f.name.trim()} created` });
          void navigate({
            to: "/$team/$projectId",
            params: { team: currentSlug(), ...projectParams(id) },
          });
        }}
      >
        <Layered
          footer={
            <div className="-mx-1.5 flex items-center justify-between gap-3">
              <span>You can deploy services into it next.</span>
              <span className="flex shrink-0 gap-2">
                <ButtonLink href="/projects" variant="ghost" size="sm">
                  Cancel
                </ButtonLink>
                <Button type="submit" size="sm">
                  Create project
                </Button>
              </span>
            </div>
          }
        >
          <div className="p-5">
            <ProjectFormFields f={f} />
          </div>
        </Layered>
      </form>
    </Page>
  );
}
