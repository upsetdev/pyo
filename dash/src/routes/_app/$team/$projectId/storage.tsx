import { createFileRoute } from "@tanstack/react-router";
import {
  projectSettingsNav,
  SettingsFrame,
} from "~/components/pyo/SettingsFrame";
import { VolumeTable } from "~/components/pyo/tables";
import { detachedSummary } from "~/components/pyo/VolumeDialogs";
import { Panel } from "~/components/ui/Panel";
import { usePyo, projectIdOf } from "~/lib/pyo/store";

export const Route = createFileRoute("/_app/$team/$projectId/storage")({
  component: () => {
    const projectId = projectIdOf(Route.useParams());
    return (
      <SettingsFrame items={projectSettingsNav(projectId)}>
        <ProjectStorage />
      </SettingsFrame>
    );
  },
});

function ProjectStorage() {
  const projectId = projectIdOf(Route.useParams());
  const s = usePyo();
  const volumes = s.volumes.filter((v) => v.projectId === projectId);
  const detached = detachedSummary(volumes);
  return (
    <Panel
      title="Volumes"
      actions={
        detached ? (
          <span className="text-[12px] text-muted-foreground">{detached}</span>
        ) : null
      }
    >
      <VolumeTable s={s} volumes={volumes} />
    </Panel>
  );
}
