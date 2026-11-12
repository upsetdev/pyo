import { PlusIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { SearchableServiceTable } from "~/components/pyo/tables";
import { ButtonLink } from "~/components/ui/Button";
import { EmptyState, Panel } from "~/components/ui/Panel";
import { usePyo, projectIdOf } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/$projectId/services")({
  component: ProjectServices,
});

function ProjectServices() {
  const projectId = projectIdOf(Route.useParams());
  const s = usePyo();
  const apps = s.services.filter((a) => a.projectId === projectId);
  return (
    <Panel
      title="Services"

      actions={
        <ButtonLink
          href={`/deploy/new?project=${projectId}`}
          size="xs"
          variant="secondary"
        >
          <PlusIcon size={12} weight="bold" /> Deploy service
        </ButtonLink>
      }
    >
      {apps.length ? (
        <SearchableServiceTable apps={apps} s={s} />
      ) : (
        <EmptyState
          title="No services yet"
          action={
            <ButtonLink href={`/deploy/new?project=${projectId}`} size="sm">
              New deploy
            </ButtonLink>
          }
        />
      )}
    </Panel>
  );
}
