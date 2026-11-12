import { CaretRightIcon } from "@phosphor-icons/react";
import { createFileRoute, Link } from "@tanstack/react-router";
import { PasskeysPanel } from "~/components/pyo/Passkeys";
import { AppShell } from "~/components/shell/AppShell";
import { TeamAvatar } from "~/components/shell/TeamSwitcher";
import { useTheme } from "~/components/site/ThemeProvider";
import { Field, Input, Segmented } from "~/components/ui/Field";
import { Page, PageHeader, Panel } from "~/components/ui/Panel";
import { plural } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { Button } from "~/components/ui/Button";
import { useState } from "react";
import { seo } from "~/lib/seo";

// The person's own page: profile, appearance and the teams they belong to.
// Its URL is outside any team, but it keeps the dashboard's sidebar (for the
// last-used team) so it feels like the rest of the app.
export const Route = createFileRoute("/_app/profile")({
  head: () => ({ meta: seo({ title: "Profile" }) }),
  component: ProfilePage,
});

const ROLE_LABEL = {
  owner: "Owner",
  admin: "Admin",
  developer: "Developer",
  viewer: "Viewer",
} as Record<string, string>;

function ProfilePage() {
  const s = usePyo();
  const { theme, setTheme } = useTheme();
  const memberships = s.teams
    .map((t) => ({
      t,
      m: s.members.find((m) => m.teamId === t.id && m.email === s.user.email),
    }))
    .filter((x) => x.m);

  return (
    <AppShell>
      <Page width="max-w-3xl">
        <PageHeader
          title="Profile"
          description="Your account and preferences."
        />

        <AccountPanel />

        <PasskeysPanel />

        <Panel title="Appearance">
          <div className="flex flex-wrap items-center justify-between gap-3 p-4">
            <div>
              <p className="text-[13px] font-medium">Theme</p>
              <p className="text-[12px] text-muted-foreground">
                System follows your OS setting.
              </p>
            </div>
            <Segmented
              label="Theme"
              value={theme}
              onChange={setTheme}
              options={[
                { value: "light", label: "Light" },
                { value: "dark", label: "Dark" },
                { value: "system", label: "System" },
              ]}
            />
          </div>
        </Panel>

        <Panel title="Teams">
          <ul className="list-rows">
            {memberships.map(({ t, m }) => {
              const projects = s.projects.filter(
                (p) => p.teamId === t.id,
              ).length;
              return (
                <li key={t.id}>
                  <Link
                    to="/$team"
                    params={{ team: t.slug }}
                    className="group flex items-center gap-3 px-4 py-3 transition-colors hover:bg-accent/40"
                  >
                    <TeamAvatar name={t.name} size={28} />
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-[13.5px] font-medium">
                        {t.name}
                      </span>
                      <span className="block truncate text-[12px] text-muted-foreground">
                        /{t.slug} · {plural(projects, "project")}
                      </span>
                    </span>
                    <span className="shrink-0 text-[12.5px] text-muted-foreground">
                      {ROLE_LABEL[m!.role] ?? m!.role}
                    </span>
                    <CaretRightIcon
                      size={13}
                      className="shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100"
                    />
                  </Link>
                </li>
              );
            })}
          </ul>
        </Panel>
      </Page>
    </AppShell>
  );
}

function AccountPanel() {
  const s = usePyo();
  const [name, setName] = useState(s.user.name);
  const [timezone, setTimezone] = useState(s.user.timezone);
  const [saving, setSaving] = useState(false);
  const dirty =
    name.trim() !== s.user.name || timezone.trim() !== s.user.timezone;
  return (
    <Panel title="Account">
      <form
        onSubmit={async (e) => {
          e.preventDefault();
          setSaving(true);
          const res = await api.updateProfile({
            name: name.trim(),
            timezone: timezone.trim(),
          });
          setSaving(false);
          if (res) toast({ title: "Profile saved" });
        }}
      >
        <div className="grid gap-4 p-4 sm:grid-cols-2">
          <Field label="Name">
            {(p) => (
              <Input
                {...p}
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            )}
          </Field>
          <Field label="Email" hint="Can't be changed yet.">
            {(p) => <Input {...p} value={s.user.email} disabled />}
          </Field>
          <Field label="Time zone" hint="e.g. Asia/Jakarta">
            {(p) => (
              <Input
                {...p}
                value={timezone}
                onChange={(e) => setTimezone(e.target.value)}
              />
            )}
          </Field>
        </div>
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            type="submit"
            size="sm"
            disabled={!dirty || saving || !name.trim()}
          >
            {saving ? "Saving…" : "Save"}
          </Button>
        </div>
      </form>
    </Panel>
  );
}
