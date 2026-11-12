import { createFileRoute } from "@tanstack/react-router";
import { InfraHeader, WorkloadTable } from "~/components/pyo/infra";
import { Page } from "~/components/ui/Panel";
import { usePyo, workloadsOf } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/clusters/workloads")({
  head: () => ({ meta: seo({ title: "Workloads" }) }),
  component: () => {
    const s = usePyo();
    return (
      <Page>
        <InfraHeader />
        <WorkloadTable s={s} workloads={workloadsOf(s)} />
      </Page>
    );
  },
});
