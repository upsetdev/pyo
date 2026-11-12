import { ListSection } from "~/components/ui/Layered";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { activityLink, ActivityRows } from "~/components/pyo/widgets";
import { Badge } from "~/components/ui/Badge";
import { ButtonLink } from "~/components/ui/Button";
import { Sheet } from "~/components/ui/Dialog";
import { Select } from "~/components/ui/Field";
import { KeyValue, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { dateTime } from "~/lib/pyo/format";
import { activityIn, projectsIn, usePyo } from "~/lib/pyo/store";
import type { ActivityEvent } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/activity")({
  head: () => ({ meta: seo({ title: "Activity" }) }),
  component: ActivityPage,
});

const GROUPS: Record<string, string> = {
  deployment: "Deployments",
  service: "Services",
  project: "Projects",
  env: "Environment",
  domain: "Domains",
  network: "Networking",
  cluster: "Clusters",
  server: "Servers",
  volume: "Storage",
  team: "Team",
  member: "Members",
};

function ActivityPage() {
  const s = usePyo();
  const [q, setQ] = useState("");
  const [group, setGroup] = useState("all");
  const [status, setStatus] = useState("all");
  const [project, setProject] = useState("all");
  const [open, setOpen] = useState<ActivityEvent | null>(null);
  const openLink = open ? activityLink(open) : null;
  const events = activityIn(s).filter(
    (e) =>
      (!q ||
        e.title.toLowerCase().includes(q.toLowerCase()) ||
        e.actor.name.toLowerCase().includes(q.toLowerCase())) &&
      (group === "all" || e.type.startsWith(`${group}.`)) &&
      (status === "all" || e.status === status) &&
      (project === "all" || e.projectId === project),
  );
  const shown = events.slice(0, 200);
  const toneOf = {
    success: "success",
    failure: "danger",
    warning: "warning",
    info: "neutral",
  } as const;
  return (
    <Page>
      <PageHeader
        title="Activity"
        description={`${events.length} ${events.length === 1 ? "event" : "events"}`}
      />
      <ListSection
        toolbar={
          <>
            <SearchInput
              value={q}
              onChange={setQ}
              placeholder="Search events or people…"
              className="min-w-0 flex-1 basis-full sm:basis-0"
            />
            <div className="w-36 sm:w-44">
              <Select
                value={group}
                onChange={(e) => setGroup(e.target.value)}
                aria-label="Event type"
                heading="Type"
              >
                <option value="all">All types</option>
                {Object.entries(GROUPS).map(([k, v]) => (
                  <option key={k} value={k}>
                    {v}
                  </option>
                ))}
              </Select>
            </div>
            <div className="w-36 sm:w-44">
              <Select
                value={status}
                onChange={(e) => setStatus(e.target.value)}
                aria-label="Status"
                heading="Status"
              >
                <option value="all">Any status</option>
                <option value="success">Success</option>
                <option value="failure">Failure</option>
                <option value="warning">Warning</option>
                <option value="info">Info</option>
              </Select>
            </div>
            <div className="w-36 sm:w-44">
              <Select
                value={project}
                onChange={(e) => setProject(e.target.value)}
                aria-label="Project"
                heading="Project"
              >
                <option value="all">All projects</option>
                {projectsIn(s).map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </Select>
            </div>
          </>
        }
      >
        <Panel>
          <ActivityRows
            events={shown}
            onSelect={setOpen}
            emptyTitle={activityIn(s).length ? "No events match" : undefined}
          />
        </Panel>
      </ListSection>
      <Sheet
        open={!!open}
        onClose={() => setOpen(null)}
        title={open?.title ?? ""}
        description={open ? dateTime(open.at) : undefined}
        footer={
          openLink?.label ? (
            <ButtonLink href={openLink.href} size="sm">
              {openLink.label}
            </ButtonLink>
          ) : null
        }
      >
        {open ? (
          <div className="-mx-5 -mt-4">
            <KeyValue
              items={[
                [
                  "Type",
                  <span className="font-mono text-[11.5px]">{open.type}</span>,
                ],
                [
                  "Status",
                  <Badge tone={toneOf[open.status]}>{open.status}</Badge>,
                ],
                [
                  "Actor",
                  `${open.actor.name}${open.actor.kind === "system" ? " (system)" : ""}`,
                ],
                ["Target", `${open.target.kind} · ${open.target.label}`],
                ...(open.projectId
                  ? [
                      [
                        "Project",
                        s.projects.find((p) => p.id === open.projectId)?.name ??
                          "Deleted",
                      ] as [string, string],
                    ]
                  : []),
                ...(open.clusterId
                  ? [
                      [
                        "Cluster",
                        s.clusters.find((c) => c.id === open.clusterId)?.name ??
                          "Removed",
                      ] as [string, string],
                    ]
                  : []),
                ...Object.entries(open.details).map(
                  ([k, v]) => [k, v] as [string, string],
                ),
                [
                  "Event ID",
                  <span className="font-mono text-[11.5px]">{open.id}</span>,
                ],
              ]}
            />
          </div>
        ) : null}
      </Sheet>
    </Page>
  );
}
