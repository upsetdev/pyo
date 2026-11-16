import {
  ArrowCircleUpIcon,
  ChartLineUpIcon,
  CloudArrowUpIcon,
  EnvelopeSimpleIcon,
  type Icon,
} from "@phosphor-icons/react";
import {
  createFileRoute,
  Link,
  Outlet,
  useRouterState,
} from "@tanstack/react-router";
import { useUpdateAvailable } from "~/components/pyo/UpdateSettings";
import { AppShell } from "~/components/shell/AppShell";
import { Page, PageHeader } from "~/components/ui/Panel";
import type { SETTINGS_PAGES } from "~/lib/pyo/paths";
import { seo } from "~/lib/seo";

// Settings of this Pyo installation, for every team: backups of its own
// data, outgoing email, analytics privacy and updates of Pyo itself, a page
// each. Pathless, so the pages sit at the root like /profile (/backups,
// /email) and /settings opens the first. The old /settings/<page> URLs
// redirect here. The GitHub App is in the GitHub card on Integrations.
// Team settings stay under each team (/:team/settings).
export const Route = createFileRoute("/_app/_settings")({
  head: () => ({ meta: seo({ title: "Settings" }) }),
  component: InstallationSettings,
});

const TABS: {
  to: (typeof SETTINGS_PAGES)[number];
  label: string;
  icon: Icon;
}[] = [
  { to: "/backups", label: "Backups", icon: CloudArrowUpIcon },
  { to: "/email", label: "Email", icon: EnvelopeSimpleIcon },
  { to: "/observability", label: "Observability", icon: ChartLineUpIcon },
  { to: "/updates", label: "Updates", icon: ArrowCircleUpIcon },
];

function InstallationSettings() {
  const pathname = useRouterState({ select: (r) => r.location.pathname });
  const updateAvailable = useUpdateAvailable();
  return (
    <AppShell>
      <Page width="max-w-3xl">
        <div className="space-y-4">
          <PageHeader
            title="Settings"
            description="For this Pyo installation, across all teams."
          />
          <nav
            aria-label="Settings sections"
            className="scrollbar-none -mb-px flex gap-1 overflow-x-auto border-b border-border"
          >
            {TABS.map((t) => {
              const active = pathname === t.to;
              const Ico = t.icon;
              return (
                <Link
                  key={t.to}
                  to={t.to}
                  aria-current={active ? "page" : undefined}
                  className={`relative flex h-9 shrink-0 items-center gap-1.5 px-2.5 text-[13px] font-medium transition-colors ${
                    active
                      ? "text-foreground"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  <Ico size={14} />
                  {t.label}
                  {t.to === "/updates" && updateAvailable ? (
                    <span
                      className="h-1.5 w-1.5 rounded-full bg-info"
                      aria-label="Update available"
                      role="img"
                    />
                  ) : null}
                  {active ? (
                    <span className="absolute inset-x-1.5 bottom-0 h-0.5 rounded-full bg-foreground" />
                  ) : null}
                </Link>
              );
            })}
          </nav>
        </div>
        <Outlet />
      </Page>
    </AppShell>
  );
}

/** A tab's body: its one-line purpose, then its settings. */
export function SettingsSection({
  description,
  children,
}: {
  description: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-3">
      <p className="text-[13px] text-muted-foreground">{description}</p>
      {children}
    </section>
  );
}
