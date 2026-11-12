import { DatabaseIcon, DotsThreeIcon, DownloadSimpleIcon } from "@phosphor-icons/react";
import { Outlet, redirect, useParams, useRouterState } from "@tanstack/react-router";
import { IconTile } from "~/components/pyo/common";
import { ENGINE_ICON, dbState, engineDetail } from "~/components/pyo/Databases";
import { Status } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Menu } from "~/components/ui/Menu";
import { importing } from "~/lib/pyo/imports";
import { EmptyState, Page, PageHeader } from "~/components/ui/Panel";
import { RouteTabs } from "~/components/ui/Table";
import { databaseTabs, routeTabs, tabPaths } from "~/lib/pyo/tabs";
import { DB_SECTION, databaseHref, getState, usePyo, type DbSection } from "~/lib/pyo/store";
import type { Database } from "~/lib/pyo/types";

// A database's pages live under its engine's section: /postgres/<id>/...,
// /mysql/<id>/... and /kv/<id>/.... Each section has thin route files that
// render these pages.

/** The database a page under /<engine>/<id> is about. */
export function useDatabase(): Database | undefined {
  const { databaseId } = useParams({ strict: false });
  const s = usePyo();
  return s.databases.find((d) => d.id === databaseId && d.teamId === s.currentTeamId);
}

/**
 * Where a database page belongs: its engine's section, the same tab (dropped
 * when the engine has no such tab) and query. Undefined: unknown database.
 */
export function canonicalDatabaseHref(team: string, id: string, rest: string[], searchStr = "", hash = "") {
  const d = getState().databases.find((x) => x.id === id);
  if (!d) return undefined;
  const [tab = "", ...deeper] = rest.filter(Boolean);
  const known = tabPaths(databaseTabs(d)).includes(tab);
  const path = databaseHref(d.engine, d.id, known ? [tab, ...deeper].join("/") : "");
  return `/${team}${path}${known ? searchStr : ""}${hash ? `#${hash}` : ""}`;
}

/** The route options of /<section>/<id>: a database of another engine moves to its own section. */
export const databaseRoute = (section: DbSection) => ({
  beforeLoad: ({
    params,
    location,
  }: {
    params: { team: string; databaseId: string };
    location: { pathname: string; searchStr: string; hash: string };
  }) => {
    const d = getState().databases.find((x) => x.id === params.databaseId);
    if (!d || DB_SECTION[d.engine] === section) return;
    const rest = location.pathname.split("/").filter(Boolean).slice(3);
    const href = canonicalDatabaseHref(params.team, d.id, rest, location.searchStr, location.hash);
    if (href) throw redirect({ href, replace: true });
  },
  component: () => <DatabaseLayout section={section} />,
});

function DatabaseLayout({ section }: { section: DbSection }) {
  const d = useDatabase();
  const s = usePyo();
  const pathname = useRouterState({ select: (r) => r.location.pathname });
  const role = s.members.find((m) => m.teamId === s.currentTeamId && m.email === s.user.email)?.role;
  const admin = role === "owner" || role === "admin";
  if (!d)
    return (
      <Page>
        <EmptyState
          icon={DatabaseIcon}
          title="Database not found"
          description="It may have been deleted."
          action={
            <ButtonLink href={`/${section}`} size="sm" variant="secondary">
              All databases
            </ButtonLink>
          }
        />
      </Page>
    );
  const st = dbState(d);
  const base = databaseHref(d.engine, d.id);
  return (
    <Page>
      <div className="space-y-4">
        <PageHeader
          icon={<IconTile icon={ENGINE_ICON[d.engine]} size="lg" />}
          title={
            <span className="flex items-center gap-2.5">
              {d.name}
              <Status tone={st.tone} label={st.label} className="text-[13px] font-normal" />
            </span>
          }
          description={`${engineDetail(d)} on ${s.clusters.find((c) => c.id === d.clusterId)?.name ?? "a cluster"}`}
          actions={
            <span className="flex items-center gap-2">
              {/* Not on the Connect tab itself. */}
              {pathname.endsWith("/connect") ? null : (
                <ButtonLink href={`${base}/connect`} size="sm" variant="secondary">
                  Connect
                </ButtonLink>
              )}
              {d.engine !== "valkey" && admin ? (
                <Menu
                  label="More"
                  items={[
                    {
                      label: "Import…",
                      icon: <DownloadSimpleIcon size={14} />,
                      disabled: importing(d) || d.state !== "active" || !!d.restoredFrom,
                      hint: "Into an empty database",
                      href: `/import?engine=${d.engine}&into=${d.id}`,
                    },
                  ]}
                  trigger={(t) => (
                    <Button {...t} variant="secondary" size="icon" className="h-8 w-8" aria-label="More">
                      <DotsThreeIcon size={16} weight="bold" />
                    </Button>
                  )}
                />
              ) : null}
            </span>
          }
        />
        <RouteTabs
          label="Database sections"
          tabs={routeTabs(databaseTabs(d), base)}
        />
      </div>
      <Outlet />
    </Page>
  );
}

/**
 * A tab this engine doesn't have (MySQL has no Extensions): the database's
 * overview. Runs after databaseRoute, which moves a database of another
 * engine (a KV database's Extensions) to its own section first.
 */
export const missingTabRoute = {
  beforeLoad: ({ params }: { params: { team: string; databaseId: string } }) => {
    const d = getState().databases.find((x) => x.id === params.databaseId);
    if (d) throw redirect({ href: `/${params.team}${databaseHref(d.engine, d.id)}`, replace: true });
  },
  component: () => null,
};
