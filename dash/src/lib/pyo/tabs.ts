import type { Application as App, Database } from "./types";

// The tab bars of the service and database pages, shared with the crumb
// switcher so it opens the same tab in the sibling you pick. One order for
// both: Overview, the main task, watching (Analytics, Logs), features,
// Settings last. Tabs that don't apply are left out.

export type PageTab = {
  label: string;
  /** The path under the entity ("logs"), or "" for its overview. */
  tab: string;
  /** Sub-pages that also count as this tab. */
  also?: string[];
};

export function serviceTabs(app: App): PageTab[] {
  const cron = app.workloadKind === "CronJob";
  return [
    { label: "Overview", tab: "" },
    { label: "Deployments", tab: "deployments" },
    ...(!cron && app.domains.length ? [{ label: "Analytics", tab: "analytics" }] : []),
    { label: "Logs", tab: "logs" },
    ...(cron
      ? []
      : [
          { label: "Terminal", tab: "terminal" },
          { label: "Domains", tab: "domains" },
          { label: "Firewall", tab: "firewall" },
        ]),
    { label: "Settings", tab: "settings", also: ["env", "networking", "resources", "volumes"] },
  ];
}

export function databaseTabs(d: Database): PageTab[] {
  return [
    { label: "Overview", tab: "" },
    { label: "Connect", tab: "connect" },
    { label: "Analytics", tab: "analytics" },
    { label: "Logs", tab: "logs" },
    { label: "Backups", tab: "backups" },
    ...(d.engine === "postgres" || d.engine === "valkey" ? [{ label: "Extensions", tab: "extensions" }] : []),
    { label: "Settings", tab: "settings" },
  ];
}

/** Every path segment under the entity that opens one of these tabs. */
export const tabPaths = (tabs: PageTab[]): string[] =>
  tabs.flatMap((t) => [t.tab, ...(t.also ?? [])]);

/** The tabs as RouteTabs takes them, under `base`. */
export const routeTabs = (tabs: PageTab[], base: string) =>
  tabs.map((t) => ({
    label: t.label,
    to: t.tab ? `${base}/${t.tab}` : base,
    exact: !t.tab,
    also: t.also?.map((x) => `${base}/${x}`),
  }));
