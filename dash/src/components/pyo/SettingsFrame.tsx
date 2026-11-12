import { projectPath, servicePath } from "~/lib/pyo/store";
import { AppLink } from "~/components/ui/AppLink";
import { useTeamPathname } from "~/lib/pyo/paths";
import type { ReactNode } from "react";

/** Settings page with a quiet left sub-navigation, like Vercel's project settings. */
export function SettingsFrame({
  items,
  children,
}: {
  items: { label: string; to: string }[];
  children: ReactNode;
}) {
  const pathname = useTeamPathname();
  return (
    <div className="grid gap-8 md:grid-cols-[180px_minmax(0,1fr)]">
      <nav aria-label="Settings" className="min-w-0">
        <ul className="scrollbar-none flex gap-1 overflow-x-auto md:flex-col">
          {items.map((it) => {
            const active = pathname === it.to;
            return (
              <li key={it.to} className="shrink-0">
                <AppLink
                  href={it.to}
                  aria-current={active ? "page" : undefined}
                  className={`block rounded-md px-2.5 py-1.5 text-[13.5px] transition-colors ${active ? "bg-accent font-medium text-foreground" : "text-muted-foreground hover:text-foreground"}`}
                >
                  {it.label}
                </AppLink>
              </li>
            );
          })}
        </ul>
      </nav>
      <div className="min-w-0 space-y-6">{children}</div>
    </div>
  );
}

export const serviceSettingsNav = (serviceId: string) => [
  { label: "General", to: servicePath(serviceId, "settings") },
  { label: "Environment", to: servicePath(serviceId, "env") },
  { label: "Networking", to: servicePath(serviceId, "networking") },
  { label: "Resources", to: servicePath(serviceId, "resources") },
  { label: "Volumes", to: servicePath(serviceId, "volumes") },
];

export const projectSettingsNav = (projectId: string) => [
  { label: "General", to: projectPath(projectId, "settings") },
  { label: "Networking", to: projectPath(projectId, "networking") },
  { label: "Volumes", to: projectPath(projectId, "storage") },
];
