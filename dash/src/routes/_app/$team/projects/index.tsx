import {
  DotsThreeIcon,
  GearSixIcon,
  PencilSimpleIcon,
  PlusIcon,
  RocketLaunchIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { ProjectIcon } from "~/components/ui/ProjectIcon";
import { DeleteProjectDialog } from "~/components/pyo/VolumeDialogs";
import { copyIdItem } from "~/components/pyo/copyId";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import {
  LastDeploy,
  ProjectCard,
  ServiceStack,
  ViewToggle,
  projectSubtitle,
  ProjectTraffic,
  useStoredView,
} from "~/components/pyo/ProjectCard";
import { ProjectDialog } from "~/components/pyo/ProjectDialog";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Select } from "~/components/ui/Field";
import { Menu, type MenuItem } from "~/components/ui/Menu";
import {
  EmptyState,
  NoMatches,
  Page,
  PageHeader,
  Panel,
} from "~/components/ui/Panel";
import { ListSection } from "~/components/ui/Layered";
import { SearchInput } from "~/components/ui/Table";
import { plural } from "~/lib/pyo/format";
import { projectsIn, projectStats, usePyo, projectParams, projectPath } from "~/lib/pyo/store";
import type { Project } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/projects/")({
  head: () => ({ meta: seo({ title: "Projects" }) }),
  component: ProjectsPage,
});

type SortKey = "activity" | "name" | "services";

// Vercel-style projects page: one toolbar (search, sort, view, new), then
// cards or rows that show each project's URL, source and last deploy.

function ProjectsPage() {
  const s = usePyo();
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<SortKey>("activity");
  const [view, setView] = useStoredView("pyo:projects-view");
  const [editing, setEditing] = useState<Project | null>(null);
  const [deleting, setDeleting] = useState<Project | null>(null);

  const all = projectsIn(s);
  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return all
      .map((p) => ({ p, st: projectStats(s, p) }))
      .filter(
        ({ p, st }) =>
          !q ||
          p.name.toLowerCase().includes(q) ||
          p.namespace.includes(q) ||
          projectSubtitle(st).toLowerCase().includes(q),
      )
      .sort((a, b) =>
        sort === "name"
          ? a.p.name.localeCompare(b.p.name)
          : sort === "services"
            ? b.st.apps.length - a.st.apps.length
            : b.st.lastActivity - a.st.lastActivity,
      );
  }, [all, query, s, sort]);

  const actions = (p: Project): MenuItem[] => [
    {
      label: "Deploy service",
      icon: <RocketLaunchIcon size={14} />,
      href: `/deploy/new?project=${p.id}`,
    },
    {
      label: "Edit",
      icon: <PencilSimpleIcon size={14} />,
      onSelect: () => setEditing(p),
    },
    {
      label: "Settings",
      icon: <GearSixIcon size={14} />,
      href: projectPath(p.id, "settings"),
    },
    copyIdItem("Project", p.id),
    { type: "separator" },
    {
      label: "Delete project",
      icon: <TrashIcon size={14} />,
      danger: true,
      onSelect: () => setDeleting(p),
    },
  ];
  const menu = (p: Project) => (
    <Menu
      label={`Actions for ${p.name}`}
      items={actions(p)}
      trigger={(t) => (
        <Button {...t} variant="ghost" size="icon" className="h-7 w-7">
          <DotsThreeIcon size={16} weight="bold" />
        </Button>
      )}
    />
  );
  const deletingServices = deleting ? projectStats(s, deleting).apps.length : 0;

  return (
    <Page>
      <PageHeader
        title="Projects"
        actions={
          <ButtonLink href="/projects/new" size="sm" variant="secondary">
            <PlusIcon size={13} weight="bold" /> New project
          </ButtonLink>
        }
      />

      {all.length ? (
        <ListSection
          toolbar={
            <>
              <SearchInput
                value={query}
                onChange={setQuery}
                shortcut="/"
                placeholder={`Search ${plural(all.length, "project")}…`}
                className="min-w-0 flex-1 basis-full sm:basis-0"
              />
              <div className="w-36 sm:w-44">
                <Select
                  value={sort}
                  onChange={(e) => setSort(e.target.value as SortKey)}
                  aria-label="Sort projects"
                  heading="Sort by"
                >
                  <option value="activity">Recent activity</option>
                  <option value="name">Name</option>
                  <option value="services">Most services</option>
                </Select>
              </div>
              <ViewToggle value={view} onChange={setView} />
            </>
          }
          footer={
            query.trim() && rows.length ? (
              <>
                {plural(rows.length, "project")} match “{query.trim()}”
              </>
            ) : undefined
          }
        >
          {!rows.length ? (
            <NoMatches
              query={query}
              what="projects"
              hint="Try another name or domain."
              onClear={() => setQuery("")}
            />
          ) : view === "grid" ? (
            <div className="grid gap-1.5 sm:grid-cols-2 xl:grid-cols-3">
              {rows.map(({ p, st }) => (
                <ProjectCard key={p.id} p={p} st={st} menu={menu(p)} />
              ))}
            </div>
          ) : (
            <ul className="list-rows overflow-hidden rounded-[14px] border border-card-border bg-card">
              {rows.map(({ p, st }) => (
                <li
                  key={p.id}
                  className="relative grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 px-5 py-3.5 transition-colors hover:bg-accent/40 md:grid-cols-[minmax(0,1fr)_minmax(0,220px)_160px_140px_32px]"
                >
                  <div className="min-w-0">
                    <Link
                      to="/$team/$projectId"
                      params={{ team: currentSlug(), ...projectParams(p.id) }}
                      className="block truncate text-[14px] font-semibold after:absolute after:inset-0 focus:outline-none focus-visible:underline"
                    >
                      {p.name}
                    </Link>
                    <p className="truncate text-[13px] text-muted-foreground">
                      {projectSubtitle(st)}
                    </p>
                  </div>
                  <div className="hidden min-w-0 items-center gap-3 md:flex">
                    <span className="shrink-0 text-[12.5px] text-muted-foreground">
                      {st.apps.length
                        ? plural(st.apps.length, "service")
                        : "No services"}
                    </span>
                    <ServiceStack apps={st.apps} projectId={p.id} />
                  </div>
                  <p className="hidden truncate text-right text-[12.5px] md:block">
                    <LastDeploy st={st} />
                  </p>
                  <div className="hidden justify-self-end md:block">
                    <ProjectTraffic p={p} st={st} />
                  </div>
                  <div className="relative z-10 justify-self-end">
                    {menu(p)}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </ListSection>
      ) : (
        <Panel>
          <EmptyState
            icon={ProjectIcon}
            title="No projects yet"
            description="Group the services you deploy."
            action={
              <ButtonLink href="/projects/new" size="sm">
                New project
              </ButtonLink>
            }
          />
        </Panel>
      )}

      <ProjectDialog
        open={!!editing}
        onClose={() => setEditing(null)}
        project={editing ?? undefined}
      />
      <DeleteProjectDialog
        open={!!deleting}
        onClose={() => setDeleting(null)}
        project={deleting}
        services={deletingServices}
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
