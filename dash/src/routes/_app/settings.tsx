import {
  ArrowCircleUpIcon,
  ChartLineUpIcon,
  CloudArrowUpIcon,
  EnvelopeSimpleIcon,
  type Icon,
} from "@phosphor-icons/react";
import { siGithub } from "simple-icons";
import type { ComponentType } from "react";
import {
  createFileRoute,
  Link,
  Outlet,
  useRouterState,
} from "@tanstack/react-router";
import { useUpdateAvailable } from "~/components/pyo/UpdateSettings";
import { AppShell } from "~/components/shell/AppShell";
import { Page, PageHeader } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";

// Settings of this Pyo installation, for every team: backups of its own
// data, outgoing email, analytics privacy, the GitHub App and updates of Pyo
// itself, a page each.
// Team settings stay under each team (/:team/settings).
export const Route = createFileRoute("/_app/settings")({
  head: () => ({ meta: seo({ title: "Settings" }) }),
  component: InstallationSettings,
});

/** GitHub's current mark (Phosphor's is the older Octocat), in the text color. */
function GithubIcon({ size = 14 }: { size?: number }) {
  return (
    <svg
      viewBox="0 0 24 24"
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden
    >
      <path d={siGithub.path} />
    </svg>
  );
}

const TABS: {
  to: string;
  label: string;
  icon: Icon | ComponentType<{ size?: number }>;
}[] = [
  { to: "/settings/backups", label: "Backups", icon: CloudArrowUpIcon },
  { to: "/settings/email", label: "Email", icon: EnvelopeSimpleIcon },
  {
    to: "/settings/observability",
    label: "Observability",
    icon: ChartLineUpIcon,
  },
  { to: "/settings/github", label: "GitHub", icon: GithubIcon },
  { to: "/settings/updates", label: "Updates", icon: ArrowCircleUpIcon },
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
                  {t.to === "/settings/updates" && updateAvailable ? (
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
