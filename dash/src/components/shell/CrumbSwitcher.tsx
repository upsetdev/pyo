import { CaretUpDownIcon, CheckIcon } from "@phosphor-icons/react";
import { ENGINE_LABEL, dbState } from "~/components/pyo/Databases";
import { serviceTone, Dot } from "~/components/ui/Badge";
import { Menu, type MenuItem } from "~/components/ui/Menu";
import {
  databaseHref,
  projectPath,
  servicePath,
  usePyo,
  type PyoState,
} from "~/lib/pyo/store";
import { databaseTabs, serviceTabs, tabPaths } from "~/lib/pyo/tabs";
import type { Database } from "~/lib/pyo/types";

// The chevron beside a project, service or database crumb: a menu of its
// siblings, opening the same tab in the one you pick when it has it.

export type Switcher = {
  kind: "project" | "service" | "database";
  id: string;
  /** The current tab under the entity ("logs"), or "" for its overview. */
  tab: string;
};

/** Show a search box once a menu lists more than this many. */
const SEARCH_FROM = 8;

const ENGINES: Database["engine"][] = ["postgres", "mysql", "valkey"];

function itemsFor(s: PyoState, sw: Switcher): MenuItem[] {
  const check = (id: string) =>
    id === sw.id ? <CheckIcon size={13} aria-label="Current" /> : undefined;
  if (sw.kind === "project") {
    const projects = s.projects.filter((p) => p.teamId === s.currentTeamId);
    return [
      { type: "label", label: "Projects" },
      ...projects.map((p) => ({
        label: p.name,
        keywords: `${p.name} ${p.namespace}`,
        hint: check(p.id),
        // Every project has the same tabs.
        href: projectPath(p.id, sw.tab, s),
      })),
    ];
  }
  if (sw.kind === "service") {
    const projectId = s.services.find((a) => a.id === sw.id)?.projectId;
    const apps = s.services.filter((a) => a.projectId === projectId);
    return [
      { type: "label", label: "Services" },
      ...apps.map((a) => ({
        label: a.name,
        icon: <Dot tone={serviceTone[a.status]} className="h-2 w-2" />,
        hint: check(a.id),
        href: servicePath(a.id, tabPaths(serviceTabs(a)).includes(sw.tab) ? sw.tab : "", s),
      })),
    ];
  }
  // Databases: every engine's, grouped, the current engine's first.
  const dbs = (s.databases ?? []).filter((d) => d.teamId === s.currentTeamId);
  const engine = dbs.find((d) => d.id === sw.id)?.engine;
  const order = [...ENGINES].sort((a, b) => +(b === engine) - +(a === engine));
  return order.flatMap((e) => {
    const group = dbs.filter((d) => d.engine === e);
    if (!group.length) return [];
    return [
      { type: "label" as const, label: ENGINE_LABEL[e] },
      ...group.map((d) => ({
        label: d.name,
        keywords: `${d.name} ${ENGINE_LABEL[e]} ${e}`,
        // The group label names the engine; the dot shows its state.
        icon: <Dot tone={dbState(d).tone} className="h-2 w-2" />,
        hint: check(d.id),
        href: databaseHref(d.engine, d.id, sw.tab && tabPaths(databaseTabs(d)).includes(sw.tab) ? sw.tab : ""),
      })),
    ];
  });
}

const NOUN = { project: "project", service: "service", database: "database" };

/**
 * A crumb's menu. `whole`: the crumb is the current page, so the name and
 * chevron are one button (like the team switcher); else just the chevron,
 * beside the name's link.
 */
export function CrumbSwitcher({
  switcher,
  name,
  whole = false,
}: {
  switcher: Switcher;
  name: string;
  whole?: boolean;
}) {
  const s = usePyo();
  const items = itemsFor(s, switcher);
  const count = items.filter((i) => i.type !== "label").length;
  const noun = NOUN[switcher.kind];
  return (
    <Menu
      align="start"
      width="w-64"
      label={`Switch ${noun}`}
      search={count > SEARCH_FROM ? `Find ${noun}…` : undefined}
      // Lets the whole-crumb trigger shrink so a long name truncates.
      className={whole ? "min-w-0 max-w-full" : "shrink-0"}
      trigger={(p) =>
        whole ? (
          <button
            type="button"
            {...p}
            aria-label={`${name}, switch ${noun}`}
            aria-current="page"
            // The team switcher's trigger: name, 8px, the 13px chevron.
            className="flex min-w-0 max-w-full items-center gap-2 rounded-md px-1.5 py-1 text-left text-foreground transition-colors hover:bg-accent aria-expanded:bg-accent"
          >
            <span title={name} className="min-w-0 truncate">{name}</span>
            <CaretUpDownIcon size={13} aria-hidden className="shrink-0 text-muted-foreground" />
          </button>
        ) : (
          <button
            type="button"
            {...p}
            aria-label={`Switch ${noun} (current: ${name})`}
            title={`Switch ${noun}`}
            // The team switcher's 8px gap before its chevron: the link's 6px
            // padding plus this 24px target's 5.5px inset, less 3.5px.
            className="-ml-[3.5px] inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground aria-expanded:bg-accent aria-expanded:text-foreground"
          >
            <CaretUpDownIcon size={13} aria-hidden />
          </button>
        )
      }
      items={items}
    />
  );
}
