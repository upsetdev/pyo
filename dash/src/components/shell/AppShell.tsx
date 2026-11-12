import { fetchMeta } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import {
  ArrowLeftIcon,
  TreeStructureIcon,
  BellIcon,
  DesktopIcon,
  DownloadSimpleIcon,
  GearSixIcon,
  GlobeIcon,
  HammerIcon,
  HardDrivesIcon,
  DatabaseIcon,
  KeyIcon,
  ListIcon,
  MoonIcon,
  PlugsConnectedIcon,
  PlusIcon,
  RocketLaunchIcon,
  SidebarSimpleIcon,
  SignOutIcon,
  SquaresFourIcon,
  SunIcon,
  TimerIcon,
  UserCircleIcon,
  UserPlusIcon,
  XIcon,
  type Icon,
} from "@phosphor-icons/react";
import { ProjectIcon } from "~/components/ui/ProjectIcon";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { currentSlug, useTeamPathname, CLUSTER_TABS } from "~/lib/pyo/paths";
import { AppLink } from "~/components/ui/AppLink";
import { UserAvatar } from "~/components/ui/UserAvatar";
import { Fragment, useEffect, useState, type ReactNode } from "react";
import { Logo } from "~/components/site/Logo";
import { useTheme } from "~/components/site/ThemeProvider";
import { Dot } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Menu } from "~/components/ui/Menu";
import { InviteDialog } from "~/components/pyo/InviteDialog";
import { NewDomainDialog } from "~/components/pyo/NewDomainDialog";
import { useUpdateAvailable } from "~/components/pyo/UpdateSettings";
import { Toaster } from "~/components/ui/Toaster";
import { timeAgo } from "~/lib/pyo/format";
import { auth } from "~/lib/pyo/backend";
import {
  activityIn,
  api,
  clusterByRef,
  resetState,
  isMuted,
  topicOf,
  clusterRef,
  type PyoState,
  serverByRef,
  usePyo,
  serviceByRef,
  getState,
  projectByRef,
  projectPath,
  servicePath,
  DB_SECTION_LABEL,
  isDbSection,
} from "~/lib/pyo/store";
import { TeamSwitcher } from "./TeamSwitcher";
import { CrumbSwitcher, type Switcher } from "./CrumbSwitcher";
import { CaretDownIcon } from "@phosphor-icons/react";
import { NAV_GROUPS, NAV_LABEL, currentHref, navChildClass, navIconClass, navItemClass, navLinks, type NavItem } from "./nav";

// Dashboard frame: a flat left sidebar (team, New deploy, sections), a
// slim top bar with the trail and account menus, then the content.

export function AppShell({ children }: { children: ReactNode }) {
  // Team-relative ("/projects"), so nav and trail logic ignore the slug.
  const pathname = useTeamPathname();
  const s = usePyo();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const crumbs = crumbsFor(pathname, s);

  useEffect(() => setDrawerOpen(false), [pathname]);
  // Deploying gets the whole screen: the sidebar steps aside on /deploy pages
  // and comes back elsewhere. The header button overrides it until you move
  // between the two.
  const focus = pathname === "/deploy" || pathname.startsWith("/deploy/");
  const [sidebarPinned, setSidebarPinned] = useState<boolean | null>(null);
  useEffect(() => setSidebarPinned(null), [focus]);
  const sidebarOpen = sidebarPinned ?? !focus;
  useEffect(() => {
    if (!drawerOpen) return;
    const onKey = (e: KeyboardEvent) =>
      e.key === "Escape" && setDrawerOpen(false);
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [drawerOpen]);

  // Onboarding is one job, start to finish: no sidebar and no top bar.
  if (pathname === "/onboarding")
    return (
      <div data-ready className="min-h-dvh w-full bg-canvas text-foreground">
        <main id="main" className="min-w-0">
          {children}
        </main>
        <Toaster />
      </div>
    );

  return (
    <div data-ready className="min-h-dvh w-full bg-sidebar text-foreground">
      <a
        href="#main"
        className="sr-only focus:not-sr-only focus:fixed focus:left-4 focus:top-3 focus:z-[60] focus:rounded-md focus:bg-card focus:px-3 focus:py-2 focus:text-[13px]"
      >
        Skip to content
      </a>

      <aside
        aria-hidden={!sidebarOpen || undefined}
        className={`hidden border-r border-shell-border bg-sidebar transition-transform duration-300 ease-out lg:fixed lg:inset-y-0 lg:left-0 lg:z-40 lg:flex lg:w-60 lg:flex-col ${sidebarOpen ? "" : "lg:pointer-events-none lg:-translate-x-full"}`}
      >
        <Sidebar pathname={pathname} />
      </aside>

      {/* Mobile drawer: always rendered so the slide runs both ways; inert while closed. */}
      <div
        className={`fixed inset-0 z-50 lg:hidden ${drawerOpen ? "" : "pointer-events-none"}`}
        inert={!drawerOpen}
      >
        <div
          aria-hidden
          onClick={() => setDrawerOpen(false)}
          className={`absolute inset-0 bg-black/50 transition-opacity duration-200 ${drawerOpen ? "opacity-100" : "opacity-0"}`}
        />
        <aside
          aria-label="Navigation"
          className={`absolute inset-y-0 left-0 flex w-64 max-w-[85vw] flex-col border-r border-shell-border bg-sidebar transition-transform duration-200 ${drawerOpen ? "translate-x-0 shadow-2xl" : "-translate-x-full"}`}
        >
          <button
            type="button"
            onClick={() => setDrawerOpen(false)}
            aria-label="Close navigation"
            className="absolute right-3 top-3.5 inline-flex h-8 w-8 items-center justify-center rounded-full text-muted-foreground hover:bg-accent hover:text-foreground"
          >
            <XIcon size={14} />
          </button>
          <Sidebar pathname={pathname} />
        </aside>
      </div>

      <div
        className={`flex min-h-dvh min-w-0 flex-col transition-[padding] duration-300 ease-out ${sidebarOpen ? "lg:pl-60" : "lg:pl-0"}`}
      >
        <div className="flex min-h-dvh min-w-0 flex-1 flex-col bg-canvas">
          <header className="sticky top-0 z-30 flex h-14 shrink-0 items-center gap-2 border-b border-shell-border bg-canvas/85 px-4 backdrop-blur md:px-8">
            <button
              type="button"
              onClick={() => setDrawerOpen(true)}
              aria-label="Open navigation"
              aria-expanded={drawerOpen}
              className="-ml-1.5 inline-flex h-8 w-8 items-center justify-center rounded-full text-foreground hover:bg-accent lg:hidden"
            >
              <ListIcon size={16} />
            </button>
            <button
              type="button"
              onClick={() => setSidebarPinned(!sidebarOpen)}
              aria-label={sidebarOpen ? "Hide sidebar" : "Show sidebar"}
              aria-pressed={sidebarOpen}
              title={sidebarOpen ? "Hide sidebar" : "Show sidebar"}
              className="-ml-1.5 mr-2 hidden h-8 w-8 items-center justify-center rounded-full text-muted-foreground hover:bg-accent hover:text-foreground lg:inline-flex"
            >
              <SidebarSimpleIcon size={16} />
            </button>
            <div className="min-w-0 flex-1">
              {crumbs.length ? <Trail crumbs={crumbs} /> : null}
            </div>
            <div className="flex shrink-0 items-center gap-3">
              <NewMenu />
              <Notifications />
              <ProfileMenu />
            </div>
          </header>
          <main id="main" className="min-w-0 flex-1">
            {children}
          </main>
        </div>
      </div>
      <Toaster />
    </div>
  );
}

/** On /profile, /settings, /api-keys and /mcp (outside any team): back to the team, then the account's own pages. */

/** The account's sidebar: back to the team, then profile, settings and developer pages. */
function AccountNav({
  pathname,
  className = "",
}: {
  pathname: string;
  className?: string;
}) {
  const s = usePyo();
  const team = s.teams.find((t) => t.id === s.currentTeamId);
  const updateAvailable = useUpdateAvailable();
  const item = (
    to: "/profile" | "/settings" | "/api-keys" | "/mcp",
    label: string,
    Icon: Icon,
    dot = false,
  ) => {
    const active = pathname === to || pathname.startsWith(`${to}/`);
    return (
      <li>
        <Link
          to={to}
          aria-current={active ? "page" : undefined}
          className={navItemClass(active)}
        >
          <Icon size={16} aria-hidden className={navIconClass(active)} />
          {label}
          {dot ? (
            <span
              className="ml-auto h-1.5 w-1.5 rounded-full bg-info"
              role="img"
              aria-label="Update available"
            />
          ) : null}
        </Link>
      </li>
    );
  };
  return (
    <nav
      className={`mt-4 scrollbar-none flex flex-1 flex-col overflow-y-auto px-3 pb-4 ${className}`}
      aria-label="Account"
    >
      <ul className="flex flex-col gap-1">
        {/* Back to the team: a link like the others, to its Overview. */}
        {team ? (
          <li>
            <AppLink
              href="/"
              aria-label={`Back to ${team.name}`}
              className={navItemClass(false)}
            >
              <ArrowLeftIcon
                size={16}
                aria-hidden
                className={navIconClass(false)}
              />
              Overview
            </AppLink>
          </li>
        ) : null}
        {item("/profile", "Profile", UserCircleIcon)}
        {item("/settings", "Settings", GearSixIcon, updateAvailable)}
      </ul>
      <p className={`mt-8 ${NAV_LABEL}`}>Developers</p>
      <ul className="flex flex-col gap-1">
        {item("/api-keys", "API keys", KeyIcon)}
        {item("/mcp", "MCP", PlugsConnectedIcon)}
      </ul>
    </nav>
  );
}

/**
 * Which menu the sidebar showed last. The shell remounts between team and
 * account pages, so this lives outside React: the menu slides in only when
 * it actually switches, not on every page load.
 */
let lastMenu: "team" | "account" | null = null;

function Sidebar({ pathname }: { pathname: string }) {
  // On pages outside a team (/profile, /settings) no team link is current.
  const inTeam = !!(useParams({ strict: false }) as { team?: string }).team;
  const menu = inTeam ? "team" : "account";
  // Going into settings or the account slides in from the right; back out, from the left.
  const [enter] = useState(() =>
    lastMenu && lastMenu !== menu
      ? menu === "team"
        ? "nav-from-left"
        : "nav-from-right"
      : "",
  );
  useEffect(() => {
    lastMenu = menu;
  }, [menu]);
  // Hosted-only items (billing) show when the API turns them on.
  const meta = useQuery("meta", fetchMeta);
  const features = meta.status === "success" ? meta.data.features : [];
  const s = usePyo();
  // A project's or service's pages (/<project>/...) count as Projects.
  const inProject = !!projectByRef(s, pathname.split("/").filter(Boolean)[0] ?? "");
  const items = navLinks(NAV_GROUPS.flatMap((g) => g.items));
  // A database's pages (/postgres/<id>/...) light up its engine's list.
  const current = inProject ? "/projects" : currentHref(items, pathname);
  const link = (item: NavItem) => {
    if (item.children) return <NavGroup key={item.label} item={item} current={current} />;
    const active = item.href === current;
    const Icon = item.icon;
    return (
      <li key={item.href}>
        <AppLink href={item.href} activeOptions={{ exact: true }} aria-current={active ? "page" : undefined} className={navItemClass(active)}>
          <Icon size={16} aria-hidden className={navIconClass(active)} />
          {item.label}
        </AppLink>
      </li>
    );
  };
  return (
    <>
      {/* Mark, then the team it's showing: "D / Acme". The mark sits
          over the nav icons (both centered 30px in). */}
      <div className="flex h-14 shrink-0 items-center px-3">
        <AppLink
          href="/"
          aria-label="Pyo overview"
          className="flex h-8 shrink-0 items-center rounded-md px-[7px] transition-opacity hover:opacity-80"
        >
          <Logo showName={false} />
        </AppLink>
        <svg
          aria-hidden
          viewBox="0 0 12 20"
          className="h-5 w-3 shrink-0 text-border"
        >
          <path
            d="M9 2 3 18"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
        </svg>
        <div className="ml-1 flex h-8 min-w-0 flex-1 items-center">
          <TeamSwitcher />
        </div>
      </div>
      {!inTeam ? (
        <AccountNav pathname={pathname} className={enter} />
      ) : (
        <nav
          className={`mt-4 scrollbar-none flex flex-1 flex-col overflow-y-auto px-3 pb-4 ${enter}`}
          aria-label="Main"
        >
          <div className="space-y-8">
            {NAV_GROUPS.map((g, i) => (
              <div key={i}>
                {g.label ? <p className={NAV_LABEL}>{g.label}</p> : null}
                <ul className="flex flex-col gap-1">{g.items.filter((it) => !it.feature || features.includes(it.feature)).map(link)}</ul>
              </div>
            ))}
          </div>
        </nav>
      )}
    </>
  );
}

const OPEN_KEY = "pyo:nav-open";
const loadOpen = (): string[] => {
  try {
    return JSON.parse(localStorage.getItem(OPEN_KEY) ?? "[]");
  } catch {
    return [];
  }
};

/** A collapsible menu item: opens by itself on one of its pages, and can be closed. */
function NavGroup({ item, current }: { item: NavItem; current?: string }) {
  const children = item.children ?? [];
  const within = children.some((c) => c.href === current);
  const [open, setOpen] = useState(() => within || (typeof window !== "undefined" && loadOpen().includes(item.label)));
  const toggle = (to: boolean) => {
    setOpen(to);
    try {
      const rest = loadOpen().filter((k) => k !== item.label);
      localStorage.setItem(OPEN_KEY, JSON.stringify(to ? [...rest, item.label] : rest));
    } catch {
      // Private windows: remembered for now only.
    }
  };
  useEffect(() => {
    if (within) toggle(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current]);
  const Icon = item.icon;
  const id = `nav-${item.label.toLowerCase().replace(/\W+/g, "-")}`;
  return (
    <li>
      <button
        type="button"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => toggle(!open)}
        className={`${navItemClass(false)} w-full ${within ? "text-foreground" : ""}`}
      >
        <Icon size={16} aria-hidden className={navIconClass(within)} />
        {item.label}
        <CaretDownIcon
          size={12}
          aria-hidden
          className={`ml-auto text-muted-foreground transition-transform duration-200 ease-out ${open ? "" : "-rotate-90"}`}
        />
      </button>
      <div
        inert={!open}
        className={`grid transition-[grid-template-rows,opacity] duration-200 ease-out ${open ? "grid-rows-[1fr] opacity-100" : "grid-rows-[0fr] opacity-0"}`}
      >
        <div className="min-h-0 overflow-hidden">
          <ul id={id} className="ml-[17px] mt-1 flex flex-col gap-0.5 border-l border-border pl-2.5">
            {children.map((c) => {
              const active = c.href === current;
              return (
                <li key={c.href}>
                  <AppLink href={c.href} activeOptions={{ exact: true }} aria-current={active ? "page" : undefined} className={navChildClass(active)}>
                    {c.label}
                  </AppLink>
                </li>
              );
            })}
          </ul>
        </div>
      </div>
    </li>
  );
}

type Crumb = { label: string; href: string; switcher?: Switcher };

function Trail({ crumbs }: { crumbs: Crumb[] }) {
  const lastIdx = crumbs.length - 1;
  // Narrow screens keep the last crumb, plus the nearest one with a switcher
  // so switching stays one tap away.
  const pinned = crumbs.findLastIndex((c) => c.switcher);
  const narrow = (i: number) => i === lastIdx || i === pinned;
  return (
    <nav aria-label="Breadcrumb">
      <ol className="flex min-w-0 flex-nowrap items-center gap-1.5 whitespace-nowrap text-[13px]">
        {crumbs.map((c, i) => {
          const last = i === lastIdx;
          return (
            <Fragment key={c.href + i}>
              {i > 0 ? (
                <li
                  aria-hidden
                  className={`shrink-0 select-none px-0.5 text-muted-foreground/45 ${narrow(i) && narrow(i - 1) ? "block" : "hidden md:block"}`}
                >
                  /
                </li>
              ) : null}
              <li
                className={`flex min-w-0 items-center ${last ? "" : `max-w-[14rem] ${narrow(i) ? "flex" : "hidden md:flex"}`}`}
              >
                {last && c.switcher ? (
                  // The current page: the whole crumb opens its menu.
                  <CrumbSwitcher switcher={c.switcher} name={c.label} whole />
                ) : last ? (
                  <span
                    aria-current="page"
                    className="block truncate px-1.5 py-1 text-foreground"
                  >
                    {c.label}
                  </span>
                ) : (
                  <AppLink
                    href={c.href}
                    className="block truncate rounded-md px-1.5 py-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                  >
                    {c.label}
                  </AppLink>
                )}
                {c.switcher && !last ? (
                  <CrumbSwitcher switcher={c.switcher} name={c.label} />
                ) : null}
              </li>
            </Fragment>
          );
        })}
      </ol>
    </nav>
  );
}

const TAB_LABELS: Record<string, string> = {
  mtls: "Mutual TLS",
  firewall: "Firewall",
  connect: "Connect",
  extensions: "Extensions",
  terminal: "Terminal",
  "cron-jobs": "Cron Jobs",
  security: "Security",
  backups: "Backups",
  billing: "Billing",
  email: "Email",
  github: "GitHub",
  updates: "Updates",
  clusters: "Clusters",
  servers: "Servers",
  services: "Services",
  deployments: "Deployments",
  analytics: "Analytics",
  observability: "Observability",
  networking: "Networking",
  storage: "Volumes",
  volumes: "Volumes",
  settings: "Settings",
  logs: "Logs",
  traffic: "Traffic",
  domains: "Domains",
  env: "Environment",
  resources: "Resources",
  workloads: "Workloads",
  members: "Members",
  integrations: "Integrations",
  notifications: "Notifications",
  "api-keys": "API keys",
  mcp: "MCP",
  profile: "Profile",
};

/** Trail for the current path, resolving ids to names from the store. */
export function crumbsFor(pathname: string, s: PyoState): Crumb[] {
  const [a, b, c, d] = pathname.split("/").filter(Boolean);
  const out: Crumb[] = [];
  if (!a) return [{ label: "Overview", href: "/" }];
  if (a === "projects") {
    out.push({ label: "Projects", href: "/projects" });
    if (b === "new") out.push({ label: "New project", href: pathname });
  } else if (a === "deployments") {
    // A deployment sits under its service: Projects / project / service / id.
    out.push({ label: "Projects", href: "/projects" });
    if (b) {
      const dep = s.deployments.find((x) => x.id === b);
      const app = dep && s.services.find((x) => x.id === dep.serviceId);
      const p = app && s.projects.find((x) => x.id === app.projectId);
      if (p)
        out.push({
          label: p.name,
          href: projectPath(p.id, "", s),
          switcher: { kind: "project", id: p.id, tab: "" },
        });
      if (app)
        out.push({
          label: app.name,
          href: servicePath(app.id, "deployments", s),
          switcher: { kind: "service", id: app.id, tab: "deployments" },
        });
      out.push({ label: b, href: pathname });
    }
  } else if (a === "builders") {
    out.push({ label: "Builders", href: "/builders" });
    if (b)
      out.push({
        label: s.builders.find((x) => x.id === b)?.name ?? b,
        href: pathname,
      });
  } else if (isDbSection(a)) {
    // Postgres / main / Connect.
    out.push({ label: DB_SECTION_LABEL[a], href: `/${a}` });
    const db = s.databases?.find((d) => d.id === b);
    if (b === "new") out.push({ label: "New database", href: pathname });
    else if (b) {
      out.push({
        label: db?.name ?? b,
        href: `/${a}/${b}`,
        ...(db ? { switcher: { kind: "database" as const, id: db.id, tab: c ?? "" } } : {}),
      });
      if (c) out.push({ label: TAB_LABELS[c] ?? c, href: pathname });
    }
  } else if (a === "deploy") {
    out.push({ label: "New deploy", href: pathname });
  } else if (a === "clusters") {
    // Clusters / prod-fsn1 / fsn1-worker-1 (a server is /clusters/<cluster>/<server>).
    out.push({ label: "Clusters", href: "/clusters" });
    const SECTIONS: Record<string, string> = {
      new: "Add cluster",
      workloads: "Workloads",
      networking: "Networking",
      storage: "Volumes",
    };
    if (b && SECTIONS[b]) out.push({ label: SECTIONS[b], href: pathname });
    else if (b) {
      const cluster = clusterByRef(s, b);
      // Past the cluster: one of its tabs, or a server by name.
      const node =
        c && !CLUSTER_TABS.includes(c)
          ? s.servers.find(
              (n) =>
                n.clusterId === cluster?.id && (n.name === c || n.id === c),
            )
          : undefined;
      if (cluster)
        out.push({ label: cluster.name, href: `/clusters/${cluster.name}` });
      if (node) out.push({ label: node.name, href: pathname });
      else if (c) out.push({ label: TAB_LABELS[c] ?? c, href: pathname });
    }
  } else if (projectByRef(s, a)) {
    // Projects / <project> / <service> / <tab>: /<project>/<service>/<tab>.
    const p = projectByRef(s, a)!;
    out.push({ label: "Projects", href: "/projects" });
    const app = b ? serviceByRef(s, a, b) : undefined;
    out.push({
      label: p.name,
      href: projectPath(p.id, "", s),
      // From a service page, another project opens on its overview.
      switcher: { kind: "project", id: p.id, tab: app ? "" : (b ?? "") },
    });
    if (app) {
      out.push({
        label: app.name,
        href: servicePath(app.id, "", s),
        switcher: { kind: "service", id: app.id, tab: c ?? "" },
      });
      if (c) out.push({ label: TAB_LABELS[c] ?? c, href: pathname });
    } else if (b) out.push({ label: TAB_LABELS[b] ?? b, href: pathname });
  } else {
    out.push({
      label: a === "settings" ? "Team" : (TAB_LABELS[a] ?? a[0].toUpperCase() + a.slice(1)),
      href: `/${a}`,
    });
    if (b) out.push({ label: TAB_LABELS[b] ?? b, href: pathname });
  }
  return out;
}

// ── Header menus ───────────────────────────────────────────────────────────

const ACTIVITY_TONE = {
  success: "success",
  failure: "danger",
  warning: "warning",
  info: "neutral",
} as const;

function Notifications() {
  const s = usePyo();
  // Muted topics (Settings → Notifications) stay in the activity log only.
  const all = activityIn(s).filter((e) => !isMuted(s, topicOf(e.type)));
  const events = all.slice(0, 8);
  const unread = all.filter(
    (e) =>
      e.at > s.notificationsReadAt &&
      (e.status === "failure" || e.status === "warning"),
  ).length;
  return (
    <Menu
      width="w-80"
      label="Notifications"
      hover
      trigger={(p) => (
        <button
          type="button"
          {...p}
          onClick={() => {
            p.onClick();
            if (unread) api.markNotificationsRead();
          }}
          // Hover opens the list too, so seeing it marks them read.
          onMouseEnter={() => {
            if (unread) api.markNotificationsRead();
          }}
          aria-label={
            unread ? `Notifications, ${unread} unread` : "Notifications"
          }
          className="relative inline-flex h-8 w-8 items-center justify-center rounded-full text-muted-foreground transition hover:bg-accent hover:text-foreground"
        >
          <BellIcon size={15} />
          {unread ? (
            <span className="absolute right-1.5 top-1.5 h-2 w-2 rounded-full border-2 border-sidebar bg-destructive" />
          ) : null}
        </button>
      )}
      items={[
        ...events.map((e) => ({
          label: (
            <span className="flex items-start gap-2 whitespace-normal py-0.5">
              <Dot tone={ACTIVITY_TONE[e.status]} className="mt-1.5" />
              <span className="min-w-0">
                <span className="block text-[12.5px] leading-5 text-foreground">
                  {e.title}
                </span>
                <span className="block text-[11px] text-muted-foreground">
                  {e.actor.name} · {timeAgo(e.at)}
                </span>
              </span>
            </span>
          ),
          href:
            e.target.kind === "deployment"
              ? `/deployments/${e.target.id}`
              : e.serviceId
                ? servicePath(e.serviceId, "", s)
                : e.clusterId
                  ? `/clusters/${clusterRef(s, e.clusterId)}`
                  : e.projectId
                    ? projectPath(e.projectId, "", s)
                    : "/activity",
        })),
        ...(events.length
          ? []
          : [{ type: "label" as const, label: "Nothing new in this team." }]),
        { type: "separator" as const },
        { label: "View all activity", href: "/activity" },
        { label: "Notification settings", href: "/notifications" },
      ]}
    />
  );
}

/** The top bar's New button: everything you can create, in one menu. */
function NewMenu() {
  const s = usePyo();
  const navigate = useNavigate();
  const team = currentSlug();
  const clusters = s.clusters.filter((c) => c.teamId === s.currentTeamId);
  // Viewers can't create databases (the API needs developers and up).
  const me = s.members.find((m) => m.teamId === s.currentTeamId && m.email === s.user.email);
  const canCreateDatabase = !me || me.role !== "viewer";
  // Domains need developers and up too; invites need admins and owners.
  const canAddDomain = canCreateDatabase;
  const canInvite = !me || me.role === "owner" || me.role === "admin";
  // Imports need admins and owners (the Import page's own check).
  const canImport = !!me && (me.role === "owner" || me.role === "admin");
  const [dialog, setDialog] = useState<"domain" | "invite" | null>(null);
  // A new database: the engine of the section you're in, else Postgres.
  const section = useTeamPathname().split("/")[1];
  const newDatabase = `/${isDbSection(section) ? section : "postgres"}/new`;
  // Import: Postgres or MySQL, the section's engine when in one.
  const importDatabase = section === "mysql" || section === "postgres" ? `/import?engine=${section}` : "/import";
  return (
    <>
      <Menu
        width="w-56"
        label="New"
        hover
        trigger={(p) => (
          <Button size="sm" {...p}>
            <PlusIcon size={13} weight="bold" />
            <span className="hidden sm:inline">New</span>
          </Button>
        )}
        items={[
          {
            label: "Service",
            icon: <RocketLaunchIcon size={14} />,
            href: "/deploy/new",
          },
          {
            label: "Cron job",
            icon: <TimerIcon size={14} />,
            href: "/cron-jobs/new",
          },
          {
            label: "Project",
            icon: <ProjectIcon size={14} />,
            href: "/projects/new",
          },
          ...(canCreateDatabase
            ? [
                {
                  label: "Database",
                  icon: <DatabaseIcon size={14} />,
                  href: newDatabase,
                },
              ]
            : []),
          ...(canImport
            ? [
                {
                  label: "Import database",
                  icon: <DownloadSimpleIcon size={14} />,
                  href: importDatabase,
                },
              ]
            : []),
          {
            label: "From a template",
            icon: <SquaresFourIcon size={14} />,
            href: "/templates",
          },
          ...(canAddDomain
            ? [
                { type: "separator" as const },
                {
                  label: "Domain",
                  icon: <GlobeIcon size={14} />,
                  onSelect: () => setDialog("domain"),
                },
              ]
            : []),
          { type: "separator" },
          {
            label: "Server",
            icon: <HardDrivesIcon size={14} />,
            // Add server, on a connected cluster to start with (the page can
            // switch). No cluster yet: connect one first.
            onSelect: () => {
              const first =
                clusters.find(
                  (c) => c.status === "connected" || c.status === "degraded",
                ) ?? clusters[0];
              void (first
                ? navigate({
                    to: "/$team/clusters/$clusterId/servers/new",
                    params: { team, clusterId: first.name },
                  })
                : navigate({ to: "/$team/clusters/new", params: { team } }));
            },
          },
          {
            label: "Cluster",
            icon: <TreeStructureIcon size={14} />,
            href: "/clusters/new",
          },
          {
            label: "Builder",
            icon: <HammerIcon size={14} />,
            href: "/builders/new",
          },
          ...(canInvite
            ? [
                { type: "separator" as const },
                {
                  label: "Invite member",
                  icon: <UserPlusIcon size={14} />,
                  onSelect: () => setDialog("invite"),
                },
              ]
            : []),
        ]}
      />
      <NewDomainDialog
        open={dialog === "domain"}
        onClose={() => setDialog(null)}
      />
      <InviteDialog
        open={dialog === "invite"}
        onClose={() => setDialog(null)}
      />
    </>
  );
}

export function ProfileMenu() {
  const s = usePyo();
  const navigate = useNavigate();
  const { theme, setTheme } = useTheme();
  const updateAvailable = useUpdateAvailable();
  return (
    <Menu
      width="w-60"
      label="Account"
      hover
      trigger={(p) => (
        <button
          type="button"
          {...p}
          className="flex rounded-full transition hover:opacity-90"
        >
          <UserAvatar
            name={s.user.name}
            email={s.user.email}
            gravatar={s.user.gravatar}
            size={32}
          />
        </button>
      )}
      items={[
        {
          type: "label",
          label: (
            <span className="block truncate text-[12px] text-foreground">
              {s.user.email}
            </span>
          ),
        },
        { type: "separator" },
        {
          label: "Profile",
          icon: <UserCircleIcon size={14} />,
          href: "/profile",
        },
        // Installation settings (/settings), not the team's; teams have theirs in the sidebar.
        {
          label: "Settings",
          icon: <GearSixIcon size={14} />,
          hint: updateAvailable ? (
            <span className="text-[11px] text-info">Update available</span>
          ) : undefined,
          onSelect: () =>
            void navigate({
              to: updateAvailable ? "/settings/updates" : "/settings",
            }),
        },
        {
          label: "API keys",
          icon: <KeyIcon size={14} />,
          onSelect: () => void navigate({ to: "/api-keys" }),
        },
        {
          label: "MCP",
          icon: <PlugsConnectedIcon size={14} />,
          onSelect: () => void navigate({ to: "/mcp" }),
        },
        { type: "separator" },
        { type: "label", label: "Theme" },
        {
          label: "Light",
          icon: <SunIcon size={14} />,
          hint: theme === "light" ? "✓" : undefined,
          onSelect: () => setTheme("light"),
        },
        {
          label: "Dark",
          icon: <MoonIcon size={14} />,
          hint: theme === "dark" ? "✓" : undefined,
          onSelect: () => setTheme("dark"),
        },
        {
          label: "System",
          icon: <DesktopIcon size={14} />,
          hint: theme === "system" ? "✓" : undefined,
          onSelect: () => setTheme("system"),
        },
        { type: "separator" },
        {
          label: "Sign out",
          icon: <SignOutIcon size={14} />,
          onSelect: () => {
            void auth
              .logout()
              .catch(() => undefined)
              .finally(() => {
                resetState();
                window.location.assign("/login");
              });
          },
        },
      ]}
    />
  );
}
