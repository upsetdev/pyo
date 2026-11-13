import {
  ClockCounterClockwiseIcon,
  DotsThreeIcon,
  PlusIcon,
  RocketLaunchIcon,
  GearSixIcon,
  ScrollIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { DeleteServiceDialog } from "~/components/pyo/VolumeDialogs";
import { copyIdItem } from "~/components/pyo/copyId";
import { createFileRoute, Link } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { ServiceIcon } from "~/components/pyo/common";
import { IssueRows } from "~/components/pyo/lists";
import { Layered } from "~/components/ui/Layered";
import { NoMatches } from "~/components/ui/Panel";
import { plural } from "~/lib/pyo/format";
import {
  AppCard,
  serviceSubtitle,
  LastDeploy,
  SourcePill,
  useStoredView,
  ViewToggle,
} from "~/components/pyo/ProjectCard";
import { collectIssues } from "~/components/pyo/widgets";
import { ServiceStatusBadge } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Select } from "~/components/ui/Field";
import { Menu, type MenuItem } from "~/components/ui/Menu";
import { SearchInput } from "~/components/ui/Table";
import { deploymentsWhere, usePyo, serviceParams, servicePath, projectIdOf } from "~/lib/pyo/store";
import type { Application } from "~/lib/pyo/types";
import { currentSlug } from "~/lib/pyo/paths";

export const Route = createFileRoute("/_app/$team/$projectId/")({
  component: ProjectOverview,
});

type SortKey = "activity" | "name" | "status";
const STATUS_ORDER: Application["status"][] = [
  "failed",
  "degraded",
  "deploying",
  "running",
  "stopped",
];

// Same shape as the Projects page, one level down: a toolbar, then a card
// or row per app or service in the project. Open issues follow the list.

function ProjectOverview() {
  const projectId = projectIdOf(Route.useParams());
  const s = usePyo();
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<SortKey>("activity");
  const [view, setView] = useStoredView("pyo:project-services-view");
  const [deleting, setDeleting] = useState<Application | null>(null);
  const project = s.projects.find((p) => p.id === projectId);

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return s.services
      .filter((a) => a.projectId === projectId)
      .map((a) => ({ a, latest: deploymentsWhere(s, { serviceId: a.id })[0] }))
      .filter(
        ({ a }) =>
          !q ||
          a.name.includes(q) ||
          a.domains.some((d) => d.hostname.includes(q)),
      )
      .sort((x, y) =>
        sort === "name"
          ? x.a.name.localeCompare(y.a.name)
          : sort === "status"
            ? STATUS_ORDER.indexOf(x.a.status) -
                STATUS_ORDER.indexOf(y.a.status) ||
              x.a.name.localeCompare(y.a.name)
            : (y.latest?.createdAt ?? y.a.updatedAt) -
              (x.latest?.createdAt ?? x.a.updatedAt),
      );
  }, [projectId, query, s, sort]);

  if (!project) return null;
  const total = s.services.filter((a) => a.projectId === projectId).length;
  const issues = collectIssues(s, project.teamId, project.id);

  if (!total)
    return (
      <Layered>
        <div className="flex flex-col items-center px-6 py-16 text-center">
          <span className="flex h-12 w-12 items-center justify-center rounded-full border border-border bg-muted/60 text-muted-foreground">
            <RocketLaunchIcon size={20} />
          </span>
          <p className="mt-5 text-[15px] font-semibold">Nothing deployed yet</p>
          <p className="mt-1.5 max-w-sm text-[13px] text-muted-foreground">
            From a template, Git or an image.
          </p>
          <div className="mt-6 flex gap-2">
            <ButtonLink href="/templates" variant="secondary" size="sm">
              Browse templates
            </ButtonLink>
            <ButtonLink href={`/deploy/new?project=${project.id}`} size="sm">
              <PlusIcon size={13} weight="bold" /> Deploy service
            </ButtonLink>
          </div>
        </div>
      </Layered>
    );

  const actions = (a: Application): MenuItem[] => [
    {
      label: "Logs",
      icon: <ScrollIcon size={14} />,
      href: servicePath(a.id, "logs"),
    },
    {
      label: "Deployments",
      icon: <ClockCounterClockwiseIcon size={14} />,
      href: servicePath(a.id, "deployments"),
    },
    {
      label: "Settings",
      icon: <GearSixIcon size={14} />,
      href: servicePath(a.id, "settings"),
    },
    copyIdItem("Service", a.id),
    { type: "separator" },
    {
      label: "Delete service",
      icon: <TrashIcon size={14} />,
      danger: true,
      onSelect: () => setDeleting(a),
    },
  ];
  const menu = (a: Application) => (
    <Menu
      label={`Actions for ${a.name}`}
      items={actions(a)}
      trigger={(t) => (
        <Button {...t} variant="ghost" size="icon" className="h-7 w-7">
          <DotsThreeIcon size={16} weight="bold" />
        </Button>
      )}
    />
  );

  const q = query.trim();
  return (
    <div className="space-y-5">
      <Layered
        id="services-h"
        title={
          <span className="inline-flex items-center gap-2">
            Services
            <span className="rounded-full bg-foreground/[0.06] px-1.5 text-[11.5px] font-medium tabular-nums text-muted-foreground">
              {total}
            </span>
          </span>
        }
        action={
          <div className="flex items-center gap-2">
            <div className="w-36">
              <Select
                value={sort}
                onChange={(e) => setSort(e.target.value as SortKey)}
                aria-label="Sort services"
                heading="Sort by"
                className="h-8 rounded-full text-[12.5px]"
              >
                <option value="activity">Recent activity</option>
                <option value="name">Name</option>
                <option value="status">Status</option>
              </Select>
            </div>
            <ViewToggle value={view} onChange={setView} />
          </div>
        }
        footer={
          q && rows.length ? (
            <>
              {plural(rows.length, "service")} match “{q}”
            </>
          ) : undefined
        }
        bare
      >
        {total > 3 ? (
          <SearchInput
            value={query}
            onChange={setQuery}
            shortcut="/"
            placeholder="Search services…"
            className="mb-1.5 [&_input]:rounded-[10px] [&_input]:pl-9 [&_svg]:left-3.5"
          />
        ) : null}

        {!rows.length ? (
          <NoMatches
            query={q}
            what="services"
            hint="Try another name or domain."
            onClear={() => setQuery("")}
          />
        ) : view === "grid" ? (
          <div className="grid gap-1.5 sm:grid-cols-2 xl:grid-cols-3">
            {rows.map(({ a, latest }) => (
              <AppCard
                key={a.id}
                a={a}
                namespace={project.namespace}
                latest={latest}
                menu={menu(a)}
              />
            ))}
          </div>
        ) : (
          <ul className="list-rows overflow-hidden rounded-[14px] border border-card-border bg-card">
            {rows.map(({ a, latest }) => (
              <li
                key={a.id}
                className="relative grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 px-5 py-3.5 transition-colors hover:bg-accent/40 md:grid-cols-[minmax(0,1fr)_110px_minmax(0,220px)_160px_32px]"
              >
                <div className="flex min-w-0 items-center gap-3">
                  <ServiceIcon app={a} size="sm" />
                  <div className="min-w-0">
                    <Link
                      to="/$team/$projectId/$serviceId"
                      params={{ team: currentSlug(), ...serviceParams(a.id) }}
                      className="block truncate text-[14px] font-semibold after:absolute after:inset-0 focus:outline-none focus-visible:underline"
                    >
                      {a.name}
                    </Link>
                    <p className="truncate text-[13px] text-muted-foreground">
                      {serviceSubtitle(a, project.namespace)}
                    </p>
                  </div>
                </div>
                <div className="hidden md:block">
                  <ServiceStatusBadge status={a.status} waiting={a.runtime.waiting} />
                </div>
                <div className="hidden min-w-0 md:block">
                  <SourcePill source={a.source} />
                </div>
                <p className="hidden truncate text-right text-[12.5px] md:block">
                  <LastDeploy st={{ latest }} settled />
                </p>
                <div className="relative z-10 justify-self-end">{menu(a)}</div>
              </li>
            ))}
          </ul>
        )}
      </Layered>

      {issues.length ? (
        <Layered
          id="issues-h"
          title={
            <span className="inline-flex items-center gap-2">
              Issues
              <span className="rounded-full bg-destructive/10 px-1.5 text-[11.5px] font-medium tabular-nums text-destructive">
                {issues.length}
              </span>
            </span>
          }
          bare
        >
          <IssueRows issues={issues} />
        </Layered>
      ) : null}

      <DeleteServiceDialog
        open={!!deleting}
        onClose={() => setDeleting(null)}
        app={deleting}
      />
    </div>
  );
}
