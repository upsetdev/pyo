import { ShieldCheckIcon } from "@phosphor-icons/react";
import { createFileRoute, notFound, Outlet } from "@tanstack/react-router";
import { HeldTailscale } from "~/components/pyo/Tailscale";
import { AppShell } from "~/components/shell/AppShell";
import { api, currentTeam, getState, teamBySlug } from "~/lib/pyo/store";

// Every dashboard page belongs to the team in the URL. Opening one makes it
// the current team, so bare links (/, /profile's "Back") return to it.
export const Route = createFileRoute("/_app/$team")({
  beforeLoad: ({ params }) => {
    const team = teamBySlug(getState(), params.team);
    if (!team) throw notFound();
    if (getState().currentTeamId !== team.id) api.switchTeam(team.id);
  },
  notFoundComponent: TeamNotFound,
  component: () => (
    <AppShell>
      <Outlet />
      <HeldTailscale />
    </AppShell>
  ),
});

function TeamNotFound() {
  const { team: slug } = Route.useParams();
  const known = !!teamBySlug(getState(), slug);
  const team = currentTeam(getState());
  // A team that requires two-factor, which this account hasn't set up.
  const locked = getState().user.twoFactor?.lockedTeams.find(
    (t) => t.slug === slug,
  );
  if (locked) return <TwoFactorRequired name={locked.name} />;
  return (
    <main className="flex min-h-dvh flex-col items-center justify-center bg-sidebar px-6 text-center">
      <p className="text-[12px] font-medium uppercase tracking-wider text-muted-foreground">
        Not found
      </p>
      <h1 className="mt-3 text-2xl font-semibold tracking-tight">
        {known ? "Page not found" : "You're not in this team"}
      </h1>
      <p className="mt-2 max-w-md text-[14px] text-muted-foreground">
        {known
          ? "This page doesn't exist, or what it pointed to was removed."
          : "Not found, or you aren't a member."}
      </p>
      {team ? (
        <a
          href={`/${known ? slug : team.slug}`}
          className="mt-6 text-[13px] font-medium underline underline-offset-4"
        >
          Go to {known ? "the overview" : team.name}
        </a>
      ) : null}
    </main>
  );
}

function TwoFactorRequired({ name }: { name: string }) {
  return (
    <main className="flex min-h-dvh flex-col items-center justify-center bg-sidebar px-6 text-center">
      <span className="flex h-11 w-11 items-center justify-center rounded-xl border border-border bg-background text-muted-foreground">
        <ShieldCheckIcon size={20} />
      </span>
      <h1 className="mt-4 text-2xl font-semibold tracking-tight">
        Two-factor required
      </h1>
      <p className="mt-2 max-w-md text-[14px] text-muted-foreground">
        {name} requires two-factor authentication. Set it up to open the team.
      </p>
      <a
        href="/profile#two-factor"
        className="mt-6 inline-flex h-9 items-center rounded-full bg-primary px-4 text-[13px] font-medium text-primary-foreground transition hover:opacity-90"
      >
        Set up two-factor
      </a>
    </main>
  );
}
