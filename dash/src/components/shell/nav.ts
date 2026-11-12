import {
  BellIcon,
  PulseIcon,
  CloudArrowUpIcon,
  CreditCardIcon,
  DatabaseIcon,
  GaugeIcon,
  GlobeIcon,
  HammerIcon,
  NetworkIcon,
  PlugsConnectedIcon,
  ShieldCheckIcon,
  SlidersHorizontalIcon,
  SquaresFourIcon,
  StackIcon,
  TimerIcon,
  UsersIcon,
  type Icon,
} from "@phosphor-icons/react";
import { ProjectIcon } from "~/components/ui/ProjectIcon";

// The dashboard's navigation, shared by the sidebar and the static frame shown
// before the app loads, so the two can't differ.

export type NavLink = {
  label: string;
  href: string;
  exact?: boolean;
  match?: string[];
  /** Shown only when the API lists this feature (GET /api/v1/meta). */
  feature?: string;
};
export type NavItem = NavLink & {
  icon: Icon;
  /** A collapsible group: the item only opens and closes; these are its pages. */
  children?: NavLink[];
};

/** Every page in the menu: links, and the pages inside groups. */
export const navLinks = (items: NavItem[]): NavLink[] =>
  items.flatMap((t) => (t.children ? t.children : [t]));

// What you ship, what it runs on, then the team's settings. Project, service
// and cluster pages have their own tabs.
export const NAV_GROUPS: { label?: string; items: NavItem[] }[] = [
  {
    items: [
      { label: "Overview", href: "/", icon: GaugeIcon, exact: true },
      // Project and service pages are /<project>/...; see the sidebar.
      // A deployment's page belongs to its service, so it lights up Projects.
      { label: "Projects", href: "/projects", icon: ProjectIcon, match: ["/deployments", "/deploy"] },
      {
        label: "Databases",
        href: "/postgres",
        icon: DatabaseIcon,
        children: [
          { label: "Postgres", href: "/postgres" },
          { label: "MySQL", href: "/mysql" },
          { label: "KV", href: "/kv" },
        ],
      },
      {
        label: "Domains",
        href: "/domains",
        icon: GlobeIcon,
        children: [
          { label: "All domains", href: "/domains" },
          { label: "Certificates", href: "/certificates", exact: true },
          { label: "Mutual TLS", href: "/mtls" },
        ],
      },
      {
        label: "Observability",
        href: "/observability",
        icon: PulseIcon,
        children: [
          { label: "Analytics", href: "/observability/analytics", match: ["/observability"] },
          { label: "Logs", href: "/observability/logs" },
          { label: "Activity", href: "/activity" },
        ],
      },
      { label: "Cron Jobs", href: "/cron-jobs", icon: TimerIcon },
      { label: "Templates", href: "/templates", icon: SquaresFourIcon },
    ],
  },
  {
    label: "Infrastructure",
    items: [
      { label: "Clusters", href: "/clusters", icon: NetworkIcon },
      { label: "Builders", href: "/builders", icon: HammerIcon },
      {
        label: "Images",
        href: "/images",
        icon: StackIcon,
        children: [
          { label: "All images", href: "/images" },
          { label: "Registry", href: "/registry" },
        ],
      },
      { label: "Backups", href: "/backups", icon: CloudArrowUpIcon },
      { label: "Security", href: "/security", icon: ShieldCheckIcon },
    ],
  },
  {
    label: "Team",
    items: [
      { label: "Settings", href: "/settings", icon: SlidersHorizontalIcon, exact: true },
      { label: "Members", href: "/members", icon: UsersIcon },
      { label: "Notifications", href: "/notifications", icon: BellIcon },
      { label: "Integrations", href: "/integrations", icon: PlugsConnectedIcon },
      { label: "Billing", href: "/settings/billing", icon: CreditCardIcon, feature: "billing" },
    ],
  },
];

/** The item is the current page (team-relative path). */
export const navMatches = (t: NavLink, pathname: string) => matchLength(t, pathname) > 0;

/** How much of the path the item claims: its longest matching prefix, or 0. */
const matchLength = (t: NavLink, pathname: string) =>
  t.exact
      ? pathname === t.href ? t.href.length : 0
      : Math.max(
          0,
          ...[t.href, ...(t.match ?? [])]
            .filter((h) => pathname === h || pathname.startsWith(`${h}/`))
            .map((h) => h.length),
        );

/** The most specific current item under these (on /observability/logs, Logs, not Analytics). */
export function currentHref(items: NavLink[], pathname: string): string | undefined {
  // The item claiming the most of the path: on /observability/logs, Logs
  // (its own link) beats Analytics (which also claims /observability).
  const all = items.filter((t) => navMatches(t, pathname));
  return all.sort((x, y) => matchLength(y, pathname) - matchLength(x, pathname))[0]?.href;
}

/** A sidebar link: muted until hovered, a soft pill when it's the current page. */
export const navItemClass = (active: boolean) =>
  `group flex items-center gap-3 rounded-lg px-2.5 py-[7px] text-[13.5px] transition-[background-color,color,box-shadow] duration-200 ease-out ${
    active
      ? "bg-card text-foreground shadow-card"
      : "text-muted-foreground hover:bg-foreground/[0.05] hover:text-foreground"
  }`;
export const navIconClass = (active: boolean) =>
  `shrink-0 transition-colors duration-200 ease-out ${active ? "text-foreground" : "text-muted-foreground group-hover:text-foreground"}`;
/** A page inside an open group: indented along a thin line. */
export const navChildClass = (active: boolean) =>
  `flex items-center rounded-lg px-2.5 py-[6px] text-[13px] transition-[background-color,color,box-shadow] duration-200 ease-out ${
    active
      ? "bg-card text-foreground shadow-card"
      : "text-muted-foreground hover:bg-foreground/[0.05] hover:text-foreground"
  }`;
export const NAV_LABEL =
  "mb-2 px-2.5 text-[12px] font-medium text-muted-foreground/70";
