import {
  ArrowRightIcon,
  CheckCircleIcon,
  CircleIcon,
  XIcon,
} from "@phosphor-icons/react";
import { useState } from "react";
import { AppLink } from "~/components/ui/AppLink";
import { Layered } from "~/components/ui/Layered";
import { isFreeDomain } from "~/lib/pyo/freeDomain";
import {
  deploymentsWhere,
  roleIn,
  servicesIn,
  type PyoState,
} from "~/lib/pyo/store";

// The first things to do on a new Pyo, on the team's Overview. Each step
// ticks itself off from the snapshot and links to where it's done. The card
// goes once every step is done, or when it's dismissed.

const DISMISSED_KEY = "pyo:checklist-dismissed";

type Step = { label: string; href: string; done: boolean };

function steps(s: PyoState): Step[] {
  const team = s.currentTeamId;
  const role = roleIn(s);
  const teamAdmin = role === "owner" || role === "admin";
  const services = servicesIn(s);
  const backups = s.teamBackups.find((t) => t.teamId === team);
  const out: (Step | null)[] = [
    {
      label: "Connect GitHub or GitLab",
      href: "/integrations",
      done:
        (!!s.github && s.githubInstallations.some((g) => g.teamId === team)) ||
        s.gitConnections.some((c) => c.teamId === team),
    },
    {
      label: "Deploy your first service",
      href: "/deploy/new",
      done:
        services.some((a) => a.currentDeploymentId) ||
        deploymentsWhere(s, { teamId: team }).some(
          (d) => d.status === "succeeded",
        ),
    },
    {
      label: "Add a domain",
      href: "/domains",
      done: services.some((a) =>
        a.domains.some((d) => !isFreeDomain(d.hostname)),
      ),
    },
    // Only where there's a store to back up to, for whoever can turn it on.
    teamAdmin && backups?.settings
      ? {
          label: "Turn on backups",
          href: "/backups",
          done:
            backups.settings.enabled ||
            backups.databases.some((d) => d.enabled),
        }
      : null,
    // Tailscale is the installation's, set up by its admin.
    s.user.isAdmin
      ? {
          label: "Set up Tailscale",
          href: "/integrations#tailscale",
          done: !!s.tailscale,
        }
      : null,
  ];
  return out.filter((x): x is Step => !!x);
}

function loadDismissed() {
  try {
    return localStorage.getItem(DISMISSED_KEY) === "1";
  } catch {
    return false;
  }
}

export function FirstRunChecklist({ s }: { s: PyoState }) {
  const [dismissed, setDismissed] = useState(loadDismissed);
  const list = steps(s);
  const done = list.filter((x) => x.done).length;
  if (dismissed || done === list.length) return null;
  const dismiss = () => {
    setDismissed(true);
    try {
      localStorage.setItem(DISMISSED_KEY, "1");
    } catch {
      // Private windows: hidden until the next load.
    }
  };
  return (
    <Layered
      id="setup-h"
      title="Set up Pyo"
      action={
        <>
          <span className="text-[12px] tabular-nums text-muted-foreground">
            {done} of {list.length}
          </span>
          <button
            type="button"
            onClick={dismiss}
            aria-label="Dismiss setup steps"
            title="Dismiss"
            className="-mr-1 inline-flex h-6 w-6 items-center justify-center rounded text-muted-foreground transition hover:bg-accent hover:text-foreground"
          >
            <XIcon size={12} />
          </button>
        </>
      }
    >
      <ul className="list-rows">
        {list.map((st) =>
          st.done ? (
            <li
              key={st.label}
              className="flex items-center gap-2.5 px-4 py-2.5 text-[13px] text-muted-foreground"
            >
              <CheckCircleIcon
                size={16}
                weight="fill"
                className="shrink-0 text-success"
                aria-label="Done"
              />
              {st.label}
            </li>
          ) : (
            <li key={st.label}>
              <AppLink
                href={st.href}
                className="group flex items-center gap-2.5 px-4 py-2.5 text-[13px] font-medium transition-colors hover:bg-accent/40"
              >
                <CircleIcon
                  size={16}
                  className="shrink-0 text-muted-foreground/60"
                  aria-hidden
                />
                <span className="min-w-0 flex-1">{st.label}</span>
                <ArrowRightIcon
                  size={12}
                  aria-hidden
                  className="shrink-0 text-muted-foreground transition-transform group-hover:translate-x-0.5 group-hover:text-foreground"
                />
              </AppLink>
            </li>
          ),
        )}
      </ul>
    </Layered>
  );
}
