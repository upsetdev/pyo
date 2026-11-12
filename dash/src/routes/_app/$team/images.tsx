import { ListSection } from "~/components/ui/Layered";
import { CaretRightIcon, PackageIcon, StackIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { Fragment, useEffect, useMemo, useState } from "react";
import { BrandMark, TimeAgo } from "~/components/pyo/common";
import {
  imageHost,
  imageLabel,
  registryIcon,
  SourceMark,
} from "~/components/pyo/ProjectCard";
import { AppLink } from "~/components/ui/AppLink";
import { Select } from "~/components/ui/Field";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { SearchInput, Table, Td, Th, Tr } from "~/components/ui/Table";
import { fetchImages, type TeamImage } from "~/lib/pyo/backend";
import { mem, plural } from "~/lib/pyo/format";
import { usePyo, servicePath } from "~/lib/pyo/store";
import type { Application } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// Every container image the team's services have run: the ones Pyo built from
// Git (kept in its registry, pruned by deployment retention) and the ones
// pulled from public or private registries.
export const Route = createFileRoute("/_app/$team/images")({
  head: () => ({ meta: seo({ title: "Images" }) }),
  component: Images,
});

const bytes = (n: number) => mem(n / 1048576);
const shortDigest = (d: string) => d.replace(/^sha256:/, "").slice(0, 12);

function Images() {
  const s = usePyo();
  const [images, setImages] = useState<TeamImage[] | null>(null);
  const [q, setQ] = useState("");
  const [filter, setFilter] = useState<"all" | "built" | "pulled">("all");
  const [sort, setSort] = useState<"recent" | "name" | "versions" | "size">(
    "recent",
  );
  const [open, setOpen] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setImages(null);
    fetchImages(s.currentTeamId)
      .then((r) => live && setImages(r.images))
      .catch(() => live && setImages([]));
    return () => {
      live = false;
    };
  }, [s.currentTeamId, s.deployments.length]);

  const serviceById = useMemo(
    () => new Map(s.services.map((a) => [a.id, a])),
    [s.services],
  );
  const rows = useMemo(
    () =>
      (images ?? []).map((i) => {
        const apps = [...new Set(i.versions.flatMap((v) => v.serviceIds))]
          .map((id) => serviceById.get(id))
          .filter((a): a is Application => !!a);
        // A built image is named after the service it was built for.
        const name = i.built
          ? (apps[0]?.name ?? i.repository.split("/").pop() ?? i.repository)
          : imageLabel(i.repository);
        const size = i.versions.reduce(
          (n, v) => n + (v.expired ? 0 : (v.size ?? 0)),
          0,
        );
        return {
          i,
          apps,
          name,
          size,
          last: Math.max(...i.versions.map((v) => v.lastDeployed)),
          live: i.versions.find((v) => v.live),
        };
      }),
    [images, serviceById],
  );
  const built = rows.filter((r) => r.i.built).length;
  const needle = q.trim().toLowerCase();
  const shown = rows
    .filter(
      (r) =>
        (filter === "all" || (filter === "built") === r.i.built) &&
        (!needle ||
          [r.name, r.i.repository, ...r.apps.map((a) => a.name)].some((x) =>
            x.toLowerCase().includes(needle),
          )),
    )
    .sort((x, y) =>
      sort === "name"
        ? x.name.localeCompare(y.name)
        : sort === "versions"
          ? y.i.versions.length - x.i.versions.length ||
            x.name.localeCompare(y.name)
          : sort === "size"
            ? y.size - x.size || x.name.localeCompare(y.name)
            : y.last - x.last,
    );

  return (
    <Page>
      <PageHeader
        title="Images"
        description={
          images?.length
            ? `${plural(rows.length, "image")} · ${built} built, ${rows.length - built} pulled`
            : "Images your services have run."
        }
      />
      {images === null ? (
        <Panel>
          <p className="px-4 py-8 text-center text-[13px] text-muted-foreground">
            Loading images…
          </p>
        </Panel>
      ) : rows.length ? (
        <ListSection
          toolbar={
            <>
              <SearchInput
                value={q}
                onChange={setQ}
                placeholder="Search by image or service…"
                className="min-w-0 flex-1 basis-full sm:basis-0"
              />
              <div className="w-36 sm:w-44">
                <Select
                  value={sort}
                  onChange={(e) => setSort(e.target.value as typeof sort)}
                  aria-label="Sort images"
                  heading="Sort by"
                >
                  <option value="recent">Recently deployed</option>
                  <option value="name">Name</option>
                  <option value="versions">Most versions</option>
                  <option value="size">Size</option>
                </Select>
              </div>
              <div className="w-36 sm:w-44">
                <Select
                  value={filter}
                  onChange={(e) => setFilter(e.target.value as typeof filter)}
                  aria-label="Show images"
                  heading="Show"
                >
                  <option value="all">All images ({rows.length})</option>
                  <option value="built">Built by Pyo ({built})</option>
                  <option value="pulled">
                    From registries ({rows.length - built})
                  </option>
                </Select>
              </div>
            </>
          }
        >
          <Panel>
            <Table>
              <thead>
                <tr>
                  <Th>Image</Th>
                  <Th>Live version</Th>
                  <Th>Used by</Th>
                  <Th className="text-right">Size</Th>
                  <Th>Last deployed</Th>
                </tr>
              </thead>
              <tbody>
                {shown.map((r) => {
                  const expanded = open === r.i.repository;
                  return (
                    <Fragment key={r.i.repository}>
                      <Tr
                        onClick={() =>
                          setOpen(expanded ? null : r.i.repository)
                        }
                        aria-expanded={expanded}
                      >
                        <Td>
                          <span className="flex min-w-0 items-center gap-2.5">
                            <CaretRightIcon
                              size={12}
                              className={`shrink-0 text-muted-foreground transition-transform ${expanded ? "rotate-90" : ""}`}
                            />
                            <ImageMark r={r} />
                            <span className="min-w-0">
                              <span className="block truncate font-medium">
                                {r.name}
                              </span>
                              <span className="block truncate text-[11.5px] text-muted-foreground">
                                {r.i.built || r.i.pushed ? (
                                  <AppLink
                                    href="/registry"
                                    className="hover:text-foreground"
                                    onClick={(e) => e.stopPropagation()}
                                  >
                                    {r.i.built ? "Built by Pyo" : "Pushed to Pyo"}
                                  </AppLink>
                                ) : (
                                  r.i.repository
                                )}{" "}
                                ·{" "}
                                {plural(r.i.versions.length, "version")}
                              </span>
                            </span>
                          </span>
                        </Td>
                        <Td>
                          {r.live ? (
                            <span className="font-mono text-[12px]">
                              {r.live.label}
                            </span>
                          ) : (
                            <span className="text-muted-foreground">
                              Not running
                            </span>
                          )}
                        </Td>
                        <Td>
                          <ServiceLinks apps={r.apps} />
                        </Td>
                        <Td className="text-right tabular-nums text-muted-foreground">
                          {r.size ? bytes(r.size) : "—"}
                        </Td>
                        <Td className="text-muted-foreground">
                          {r.last > 0 ? <TimeAgo at={r.last} /> : "Not deployed"}
                        </Td>
                      </Tr>
                      {expanded ? (
                        <tr>
                          <td
                            colSpan={5}
                            className="border-b border-border bg-muted/25 px-4 py-3"
                          >
                            <Versions r={r} serviceById={serviceById} />
                          </td>
                        </tr>
                      ) : null}
                    </Fragment>
                  );
                })}
                {!shown.length ? (
                  <tr>
                    <Td
                      colSpan={5}
                      className="py-8 text-center text-muted-foreground"
                    >
                      No images match.
                    </Td>
                  </tr>
                ) : null}
              </tbody>
            </Table>
          </Panel>
          {built ? (
            <p className="text-[12.5px] text-muted-foreground">
              Recent images are kept for rollbacks.
            </p>
          ) : null}
        </ListSection>
      ) : (
        <Panel>
          <EmptyState
            icon={StackIcon}
            title="No images yet"
            description="Shown once a service deploys."
          />
        </Panel>
      )}
    </Page>
  );
}

type Row = {
  i: TeamImage;
  apps: Application[];
  name: string;
  size: number;
  last: number;
};

/** A built image by its service's Git host, a pulled one by its registry. */
function ImageMark({ r }: { r: Row }) {
  const src = r.i.built ? r.apps[0]?.source : undefined;
  const icon = r.i.built ? undefined : registryIcon(imageHost(r.i.repository));
  return (
    <span
      aria-hidden
      className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md border border-border bg-muted/60 text-foreground/85"
    >
      {src ? (
        <SourceMark source={src} size={14} />
      ) : icon ? (
        <BrandMark icon={icon} size={14} />
      ) : (
        <PackageIcon size={14} />
      )}
    </span>
  );
}

function ServiceLinks({ apps }: { apps: Application[] }) {
  if (!apps.length)
    return <span className="text-muted-foreground">Deleted services</span>;
  return (
    <span className="flex flex-wrap gap-x-2 gap-y-0.5">
      {apps.slice(0, 3).map((a) => (
        <AppLink
          key={a.id}
          href={servicePath(a.id)}
          onClick={(e) => e.stopPropagation()}
          className="font-medium hover:text-primary"
        >
          {a.name}
        </AppLink>
      ))}
      {apps.length > 3 ? (
        <span className="text-muted-foreground">+{apps.length - 3}</span>
      ) : null}
    </span>
  );
}

function Versions({
  r,
  serviceById,
}: {
  r: Row;
  serviceById: Map<string, Application>;
}) {
  return (
    <ul className="list-rows overflow-hidden rounded-lg border border-card-border bg-card text-[12.5px]">
      {r.i.versions.map((v) => (
        <li
          key={v.digest ?? v.image}
          className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-1 px-3 py-2 sm:grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)_5rem_7rem]"
        >
          <span className="flex min-w-0 items-center gap-2">
            <span className="truncate font-mono">{v.label}</span>
            {v.digest && v.label !== shortDigest(v.digest) ? (
              <span className="hidden truncate font-mono text-[11px] text-muted-foreground md:inline">
                {shortDigest(v.digest)}
              </span>
            ) : null}
            {v.live ? (
              <span className="shrink-0 rounded-md bg-success/15 px-1.5 text-[11px] font-medium text-success">
                Live
              </span>
            ) : null}
            {v.expired ? (
              <span className="shrink-0 rounded-md bg-muted px-1.5 text-[11px] font-medium text-muted-foreground">
                Removed
              </span>
            ) : null}
          </span>
          <span className="hidden truncate text-muted-foreground sm:block">
            {v.serviceIds
              .map((id) => serviceById.get(id)?.name ?? "deleted service")
              .join(", ")}{" "}
            · {plural(v.deployments, "deploy")}
          </span>
          <span className="hidden text-right tabular-nums text-muted-foreground sm:block">
            {v.size ? bytes(v.size) : "—"}
          </span>
          <span className="text-right text-muted-foreground">
            {v.lastDeployed > 0 ? <TimeAgo at={v.lastDeployed} /> : "—"}
          </span>
        </li>
      ))}
    </ul>
  );
}
