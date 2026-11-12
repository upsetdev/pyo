import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useCronJobForm } from "~/components/pyo/CronJobForm";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Layered } from "~/components/ui/Layered";
import { EmptyState, Page, PageHeader } from "~/components/ui/Panel";
import { currentSlug } from "~/lib/pyo/paths";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/cron-jobs_/new")({
  head: () => ({ meta: seo({ title: "New cron job" }) }),
  component: NewCronJobPage,
});

function NewCronJobPage() {
  const navigate = useNavigate();
  const f = useCronJobForm();
  const back = () =>
    void navigate({ to: "/$team/cron-jobs", params: { team: currentSlug() } });
  return (
    <Page width="max-w-2xl">
      <PageHeader
        title="New cron job"
        description="Runs a command on a schedule."
      />
      {f.canRun ? (
        <form
          noValidate
          onSubmit={async (e) => {
            e.preventDefault();
            if (await f.save()) back();
          }}
        >
          <Layered
            footer={
              <div className="-mx-1.5 flex items-center justify-between gap-3">
                <span>Runs on its schedule once added. You can also run it now.</span>
                <span className="flex shrink-0 gap-2">
                  <ButtonLink href="/cron-jobs" variant="ghost" size="sm">
                    Cancel
                  </ButtonLink>
                  <Button type="submit" size="sm" disabled={!f.ready}>
                    {f.busy ? "Adding…" : "Add cron job"}
                  </Button>
                </span>
              </div>
            }
          >
            <div className="p-5">{f.fields}</div>
          </Layered>
        </form>
      ) : (
        <Layered>
          <EmptyState
            title="Deploy a service first"
            description="Needs a running service."
            action={
              <ButtonLink href="/deploy/new" size="sm">
                Deploy a service
              </ButtonLink>
            }
          />
        </Layered>
      )}
    </Page>
  );
}
