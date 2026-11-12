import { ListSection } from "~/components/ui/Layered";
import { GlobeIcon, PlusIcon } from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { DomainStatus } from "~/components/pyo/DomainState";
import { TimeAgo } from "~/components/pyo/common";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import { AddDomainDialog } from "~/components/pyo/AddDomainDialog";
import { Select } from "~/components/ui/Field";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import {
  FilterChips,
  SearchInput,
  Table,
  Td,
  Th,
  Tr,
} from "~/components/ui/Table";
import { currentSlug } from "~/lib/pyo/paths";
import { servicesIn, usePyo, serviceParams, servicePath } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

// Every hostname the team serves, with the service behind it. Add one here
// (picking its service) or on the service; configure it on the service.
export const Route = createFileRoute("/_app/$team/domains")({
  head: () => ({ meta: seo({ title: "Domains" }) }),
  component: Domains,
});

function Domains() {
  const s = usePyo();
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const [adding, setAdding] = useState(false);
  const [filter, setFilter] = useState<"all" | "waiting">("all");
  const [sort, setSort] = useState<"domain" | "newest" | "service" | "attention">(
    "domain",
  );
  const rows = useMemo(
    () =>
      servicesIn(s)
        .flatMap((a) =>
          a.domains.map((d) => ({
            d,
            a,
            project: s.projects.find((p) => p.id === a.projectId),
          })),
        )
        .sort((x, y) => x.d.hostname.localeCompare(y.d.hostname)),
    [s],
  );
  const waiting = rows.filter((r) => r.d.status !== "active").length;
  const needle = q.trim().toLowerCase();
  const byHost = (x: (typeof rows)[number], y: (typeof rows)[number]) =>
    x.d.hostname.localeCompare(y.d.hostname);
  const shown = rows
    .filter(
      (r) =>
        (filter === "all" || r.d.status !== "active") &&
        (!needle ||
          [r.d.hostname, r.a.name, r.project?.name ?? ""].some((x) =>
            x.toLowerCase().includes(needle),
          )),
    )
    .sort((x, y) =>
      sort === "newest"
        ? y.d.createdAt - x.d.createdAt
        : sort === "service"
          ? x.a.name.localeCompare(y.a.name) || byHost(x, y)
          : sort === "attention"
            ? Number(x.d.status === "active") -
                Number(y.d.status === "active") || byHost(x, y)
            : byHost(x, y),
    );

  return (
    <Page>
      <PageHeader
        title="Domains"
        actions={
          <Button size="sm" variant="secondary" onClick={() => setAdding(true)}>
            <PlusIcon size={13} weight="bold" /> Add domain
          </Button>
        }
      />
      {rows.length ? (
        <ListSection
          toolbar={
            <>
              <SearchInput
                value={q}
                onChange={setQ}
                placeholder="Search by domain, service or project…"
                className="min-w-0 flex-1 basis-full sm:basis-0"
              />
              <div className="w-36 sm:w-44">
                <Select
                  value={sort}
                  onChange={(e) => setSort(e.target.value as typeof sort)}
                  aria-label="Sort domains"
                  heading="Sort by"
                >
                  <option value="domain">Domain</option>
                  <option value="newest">Newest first</option>
                  <option value="service">Service</option>
                  <option value="attention">Needs attention</option>
                </Select>
              </div>
            </>
          }
        >
          {waiting ? (
            <div className="-mt-1">
              <FilterChips
                label="Show"
                value={filter}
                onChange={setFilter}
                options={[
                  { value: "all", label: "All", count: rows.length },
                  {
                    value: "waiting",
                    label: "Waiting for DNS",
                    count: waiting,
                  },
                ]}
              />
            </div>
          ) : null}
          <Panel>
            <Table>
              <thead>
                <tr>
                  <Th>Domain</Th>
                  <Th>Service</Th>
                  <Th>Status</Th>
                  <Th>Added</Th>
                </tr>
              </thead>
              <tbody>
                {shown.map(({ d, a, project }) => (
                  <Tr
                    key={`${a.id}:${d.id}`}
                    onClick={() =>
                      void navigate({
                        to: "/$team/$projectId/$serviceId/domains",
                        params: { team: currentSlug(), ...serviceParams(a.id) },
                      })
                    }
                  >
                    <Td>
                      <span className="font-mono text-[12.5px]">
                        {d.hostname}
                      </span>
                    </Td>
                    <Td>
                      <AppLink
                        href={servicePath(a.id)}
                        onClick={(e) => e.stopPropagation()}
                        className="font-medium hover:text-primary"
                      >
                        {a.name}
                      </AppLink>
                      {project ? (
                        <span className="block text-[11.5px] text-muted-foreground">
                          {project.name}
                        </span>
                      ) : null}
                    </Td>
                    <Td>
                      <DomainStatus
                        d={d}
                        certs={s.certificates}
                        notDeployed={!a.currentDeploymentId}
                      />
                    </Td>
                    <Td className="text-muted-foreground">
                      <TimeAgo at={d.createdAt} />
                    </Td>
                  </Tr>
                ))}
                {!shown.length ? (
                  <tr>
                    <Td
                      colSpan={4}
                      className="py-8 text-center text-muted-foreground"
                    >
                      No domains match.
                    </Td>
                  </tr>
                ) : null}
              </tbody>
            </Table>
          </Panel>
        </ListSection>
      ) : (
        <Panel>
          <EmptyState
            icon={GlobeIcon}
            title="No domains yet"
            description="Point a hostname at one of your services."
            action={
              <Button size="sm" onClick={() => setAdding(true)}>
                Add domain
              </Button>
            }
          />
        </Panel>
      )}
      <AddDomainDialog open={adding} onClose={() => setAdding(false)} />
    </Page>
  );
}
