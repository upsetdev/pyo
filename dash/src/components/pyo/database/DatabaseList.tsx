import { DatabaseIcon, LightningIcon, PlusIcon } from "@phosphor-icons/react";
import { useMemo, useState, type ReactNode } from "react";
import { TimeAgo } from "~/components/pyo/common";
import {
  ENGINE_ICON,
  ENGINE_LABEL,
  SIZES,
  dbState,
  engineDetail,
  sizeSpec,
} from "~/components/pyo/Databases";
import { CARD_HOVER } from "~/components/pyo/ProjectCard";
import { PostgresLogo } from "~/components/ui/EngineLogo";
import { AppLink } from "~/components/ui/AppLink";
import { Badge, Dot } from "~/components/ui/Badge";
import { ButtonLink } from "~/components/ui/Button";
import { ListSection } from "~/components/ui/Layered";
import {
  EmptyState,
  NoMatches,
  Page,
  PageHeader,
  Panel,
} from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { plural } from "~/lib/pyo/format";
import { DB_SECTION, databaseHref, usePyo } from "~/lib/pyo/store";
import type { Database } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// One engine's databases, as cards: the sidebar's Postgres, MySQL and KV
// (/postgres, /mysql, /kv).
export const databaseListRoute = (engine: Database["engine"]) => ({
  head: () => ({ meta: seo({ title: ENGINE_LABEL[engine] }) }),
  component: () => <DatabaseList engine={engine} />,
});

function DatabaseList({ engine }: { engine: Database["engine"] }) {
  const s = usePyo();
  const [query, setQuery] = useState("");
  const title = ENGINE_LABEL[engine];
  const newHref = `/${DB_SECTION[engine]}/new`;
  const all = (s.databases ?? []).filter(
    (d) => d.teamId === s.currentTeamId && d.engine === engine,
  );
  const cluster = (id: string) =>
    s.clusters.find((c) => c.id === id)?.name ?? "—";
  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return all.filter(
      (d) =>
        !q ||
        d.name.includes(q) ||
        engineDetail(d).toLowerCase().includes(q) ||
        cluster(d.clusterId).toLowerCase().includes(q),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [all, query, s.clusters]);

  return (
    <Page>
      <PageHeader
        title={title}
        actions={
          <ButtonLink href={newHref} size="sm" variant="secondary">
            <PlusIcon size={13} weight="bold" /> New database
          </ButtonLink>
        }
      />
      {all.length ? (
        <ListSection
          toolbar={
            <SearchInput
              value={query}
              onChange={setQuery}
              shortcut="/"
              placeholder={`Search ${plural(all.length, "database")}…`}
              className="min-w-0 flex-1 basis-full sm:basis-0"
            />
          }
        >
          {rows.length ? (
            <div className="grid gap-1.5 sm:grid-cols-2 xl:grid-cols-3">
              {rows.map((d) => (
                <DatabaseCard key={d.id} d={d} cluster={cluster(d.clusterId)} />
              ))}
            </div>
          ) : (
            <NoMatches
              query={query}
              what="databases"
              onClear={() => setQuery("")}
            />
          )}
        </ListSection>
      ) : (
        <Panel>
          <EmptyState
            icon={engine === "valkey" ? LightningIcon : engine === "postgres" ? PostgresLogo : DatabaseIcon}
            title={`No ${title} databases yet`}
            description={
              engine === "valkey"
                ? "Redis-compatible, on your own servers."
                : "With failover and backups, on your own servers."
            }
            action={
              <ButtonLink href={newHref} size="sm">
                New database
              </ButtonLink>
            }
          />
        </Panel>
      )}
    </Page>
  );
}

function DatabaseCard({ d, cluster }: { d: Database; cluster: string }) {
  const st = dbState(d);
  const EngineIcon = ENGINE_ICON[d.engine];
  const ready = d.status?.readyInstances ?? 0;
  return (
    <article
      className={`group/card relative flex flex-col gap-4 rounded-[14px] border border-card-border bg-card p-5 ${CARD_HOVER} has-[a:focus-visible]:ring-2 has-[a:focus-visible]:ring-primary/40`}
    >
      <div className="flex items-start gap-3">
        <span
          aria-hidden
          className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-border bg-muted text-muted-foreground"
        >
          <EngineIcon size={17} />
        </span>
        <div className="min-w-0 flex-1">
          <h3 className="flex min-w-0 items-center gap-2 text-[14px] font-semibold">
            <AppLink
              href={databaseHref(d.engine, d.id)}
              className="truncate after:absolute after:inset-0 after:rounded-[14px] focus:outline-none"
            >
              {d.name}
            </AppLink>
            <span title={st.label} className="inline-flex">
              <Dot tone={st.tone} className="h-2 w-2" />
              <span className="sr-only">{st.label}</span>
            </span>
          </h3>
          <p className="truncate text-[12.5px] text-muted-foreground">
            {engineDetail(d)}
          </p>
        </div>
        <Badge tone={d.instances > 1 ? "info" : "neutral"}>
          {d.instances > 1 ? "HA" : "Single"}
        </Badge>
      </div>
      <dl className="grid grid-cols-3 gap-x-4 gap-y-2 text-[12.5px]">
        <Item label="Size" className="col-span-3">
          {SIZES.find((x) => x.value === d.size)?.label ?? d.size}
          <span className="text-muted-foreground"> · {sizeSpec(d)}</span>
        </Item>
        <Item label="Storage">{d.storageGi} GiB</Item>
        <Item label="Cluster">{cluster}</Item>
        <Item label="Ready">
          {ready}/{d.instances}
        </Item>
      </dl>
      <p className="mt-auto flex items-center justify-between gap-2 border-t border-border pt-3 text-[12px] text-muted-foreground">
        <span>
          {d.connections.length
            ? `${plural(d.connections.length, "service")} connected`
            : "No services connected"}
        </span>
        <span>
          Created <TimeAgo at={d.createdAt} />
        </span>
      </p>
    </article>
  );
}

function Item({
  label,
  className = "",
  children,
}: {
  label: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={`min-w-0 ${className}`}>
      <dt className="text-[11.5px] text-muted-foreground">{label}</dt>
      <dd className="truncate text-foreground">{children}</dd>
    </div>
  );
}
