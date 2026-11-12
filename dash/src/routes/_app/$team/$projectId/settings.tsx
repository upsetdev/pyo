import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { DeleteProjectDialog } from "~/components/pyo/VolumeDialogs";
import { useState } from "react";
import { DeployHooksPanel } from "~/components/pyo/DeployHooks";
import {
  ProjectFormFields,
  useProjectForm,
} from "~/components/pyo/ProjectDialog";
import {
  projectSettingsNav,
  SettingsFrame,
} from "~/components/pyo/SettingsFrame";
import { Button } from "~/components/ui/Button";
import { KeyValue, Panel } from "~/components/ui/Panel";
import { dateTime, plural } from "~/lib/pyo/format";
import { api, usePyo, projectIdOf } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Project } from "~/lib/pyo/types";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/$projectId/settings")({
  component: function ProjectSettingsRoute() {
    const projectId = projectIdOf(Route.useParams());
    const project = usePyo().projects.find((p) => p.id === projectId);
    return project ? (
      <SettingsFrame items={projectSettingsNav(project.id)}>
        <ProjectSettings key={project.id} project={project} />
      </SettingsFrame>
    ) : null;
  },
});

function ProjectSettings({ project }: { project: Project }) {
  const s = usePyo();
  const navigate = useNavigate();
  const f = useProjectForm(project);
  const [deleting, setDeleting] = useState(false);
  const apps = s.services.filter((a) => a.projectId === project.id);
  const clusters = [
    ...new Set(
      apps
        .map((a) => s.clusters.find((c) => c.id === a.clusterId)?.name)
        .filter(Boolean),
    ),
  ];
  const dirty =
    f.name.trim() !== project.name ||
    f.description.trim() !== project.description;

  return (
    <div className="space-y-4">
      <Panel title="General">
        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            f.setTouched(true);
            if (!f.valid) return;
            api.updateProject(project.id, {
              name: f.name.trim(),
              description: f.description.trim(),
            });
            toast({ title: "Project saved" });
          }}
        >
          <div className="p-4">
            <ProjectFormFields f={f} editing autoFocus={false} />
          </div>
          <div className="flex justify-end border-t border-border px-4 py-3">
            <Button type="submit" size="sm" disabled={!dirty}>
              Save
            </Button>
          </div>
        </form>
      </Panel>

      <Panel title="Details">
        <KeyValue
          items={[
            [
              "Project ID",
              <span className="font-mono text-[11.5px]">{project.id}</span>,
            ],
            [
              "Slug",
              <span className="font-mono text-[11.5px]">
                {project.namespace}
              </span>,
            ],
            ["Clusters", clusters.length ? clusters.join(", ") : "None yet"],
            ["Created", dateTime(project.createdAt)],
          ]}
        />
      </Panel>

      <DeployHooksPanel projectId={project.id} />
      <Panel title="Danger zone" tone="danger">
        <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
          <div>
            <p className="text-[13px] font-medium">Delete project</p>
            <p className="text-[12px] text-muted-foreground">
              Removes {plural(apps.length, "service")} from Pyo.
            </p>
          </div>
          <Button
            variant="secondary"
            size="sm"
            className="text-destructive"
            onClick={() => setDeleting(true)}
          >
            Delete project
          </Button>
        </div>
      </Panel>

      <DeleteProjectDialog
        open={deleting}
        onClose={() => setDeleting(false)}
        project={project}
        services={apps.length}
        onDeleted={() => {
          void navigate({
            to: "/$team/projects",
            params: { team: currentSlug() },
          });
        }}
      />
    </div>
  );
}
