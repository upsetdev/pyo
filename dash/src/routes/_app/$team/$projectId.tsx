import { RouteTabs } from "~/components/ui/Table";
import { DeleteProjectDialog } from "~/components/pyo/VolumeDialogs";
import {
  ArrowUpRightIcon,
  ChartLineUpIcon,
  DotsThreeIcon,
  CubeIcon,
  GlobeSimpleIcon,
  GearSixIcon,
  PencilSimpleIcon,
  PlusIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { ProjectIcon } from "~/components/ui/ProjectIcon";
import { copyIdItem } from "~/components/pyo/copyId";
import { createFileRoute, Outlet, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { ProjectDialog } from "~/components/pyo/ProjectDialog";
import { Button, ButtonLink, buttonClass } from "~/components/ui/Button";
import { Menu } from "~/components/ui/Menu";
import { Dot } from "~/components/ui/Badge";
import { EmptyState, Page } from "~/components/ui/Panel";
import { plural } from "~/lib/pyo/format";
import {
  api,
  projectStats,
  usePyo,
  projectPath,
  projectIdOf,
} from "~/lib/pyo/store";
import { seo } from "~/lib/seo";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/$projectId")({
  head: () => ({ meta: seo({ title: "Project" }) }),
  component: ProjectLayout,
});

function ProjectLayout() {
  const projectId = projectIdOf(Route.useParams());
  const s = usePyo();
  const navigate = useNavigate();
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const project = s.projects.find((p) => p.id === projectId);

  if (!project || project.teamId !== s.currentTeamId) {
    return (
      <Page>
        <EmptyState
          icon={ProjectIcon}
          title="Project not found"
          description={
            project
              ? "This project belongs to another team."
              : "It may have been deleted."
          }
          action={
            project ? (
              <Button size="sm" onClick={() => api.switchTeam(project.teamId)}>
                Switch to its team
              </Button>
            ) : (
              <ButtonLink href="/projects" size="sm" variant="secondary">
                All projects
              </ButtonLink>
            )
          }
        />
      </Page>
    );
  }

  const st = projectStats(s, project);
  const base = projectPath(project.id);
  const domain = st.apps
    .flatMap((a) => a.domains)
    .find((d) => d.status === "active");

  return (
    <Page>
      <div className="space-y-4">
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="min-w-0">
            <h1 className="truncate text-[24px] font-semibold tracking-tight">
              {project.name}
            </h1>
            <p className="mt-2 flex flex-wrap items-center gap-x-5 gap-y-1 text-[13px] text-muted-foreground">
              {st.apps.length ? (
                <span className="inline-flex items-center gap-2">
                  <Dot
                    tone={
                      st.failing
                        ? "danger"
                        : st.running < st.apps.length
                          ? "warning"
                          : "success"
                    }
                  />
                  <span>
                    <span className="text-foreground">{st.running}</span> of{" "}
                    {plural(st.apps.length, "service")} running
                  </span>
                </span>
              ) : (
                <span>No services yet</span>
              )}
              {domain ? (
                <a
                  href={`https://${domain.hostname}`}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex min-w-0 items-center gap-1.5 transition-colors hover:text-foreground"
                >
                  <GlobeSimpleIcon size={13} className="shrink-0" />
                  <span className="truncate">{domain.hostname}</span>
                </a>
              ) : null}
              <span className="inline-flex items-center gap-1.5">
                <CubeIcon size={13} className="shrink-0" />
                <span className="font-mono text-[12px]">
                  {project.namespace}
                </span>
              </span>
            </p>
          </div>
          <div className="flex items-center gap-2">
            {domain ? (
              <a
                href={`https://${domain.hostname}`}
                target="_blank"
                rel="noreferrer"
                className={buttonClass("secondary", "sm")}
              >
                Visit <ArrowUpRightIcon size={12} />
              </a>
            ) : null}
            <ButtonLink
              href={`/deploy/new?project=${project.id}`}
              size="sm"
              variant="secondary"
            >
              <PlusIcon size={13} weight="bold" /> Deploy service
            </ButtonLink>
            <Menu
              label="More project actions"
              items={[
                {
                  label: "Edit",
                  icon: <PencilSimpleIcon size={14} />,
                  onSelect: () => setEditing(true),
                },
                {
                  label: "View analytics",
                  icon: <ChartLineUpIcon size={14} />,
                  href: `/observability/analytics?project=${project.id}`,
                },
                {
                  label: "Settings",
                  icon: <GearSixIcon size={14} />,
                  href: `${base}/settings`,
                },
                copyIdItem("Project", project.id),
                { type: "separator" },
                {
                  label: "Delete project",
                  icon: <TrashIcon size={14} />,
                  danger: true,
                  onSelect: () => setDeleting(true),
                },
              ]}
              trigger={(t) => (
                <Button {...t} variant="secondary" size="icon">
                  <DotsThreeIcon size={16} weight="bold" />
                </Button>
              )}
            />
          </div>
        </div>
        <RouteTabs
          label="Project sections"
          tabs={[
            { label: "Overview", to: base, exact: true, also: [`${base}/services`] },
            { label: "Deployments", to: `${base}/deployments` },
            { label: "Settings", to: `${base}/settings`, also: [`${base}/networking`, `${base}/storage`] },
          ]}
        />
      </div>
      <Outlet />
      <ProjectDialog
        open={editing}
        onClose={() => setEditing(false)}
        project={project}
      />
      <DeleteProjectDialog
        open={deleting}
        onClose={() => setDeleting(false)}
        project={project}
        services={st.apps.length}
        onDeleted={() => {
          void navigate({
            to: "/$team/projects",
            params: { team: currentSlug() },
          });
        }}
      />
    </Page>
  );
}
