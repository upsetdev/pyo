import { MenuPath } from "~/components/ui/MenuPath";
import { KeyIcon, TreeStructureIcon } from "@phosphor-icons/react";
import { createFileRoute, Link } from "@tanstack/react-router";
import { RegistryLoginsPanel } from "~/components/pyo/RegistryLogins";
import { SecretStoresPanel } from "~/components/pyo/SecretStores";
import { useEffect, useState, type ReactNode } from "react";
import { Badge, ClusterStatusBadge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Page, PageHeader, Panel } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";
import { api, clustersIn, integrationIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { Input } from "~/components/ui/Field";
import { BrandMark, GITHUB_LOGO } from "~/components/pyo/common";
import { currentSlug } from "~/lib/pyo/paths";

// Everything Pyo connects to on the team's behalf: Git, registries,
// clusters and secret stores. Notification channels live in Settings.
export const Route = createFileRoute("/_app/$team/integrations")({
  head: () => ({ meta: seo({ title: "Integrations" }) }),
  component: Integrations,
});

function Row({
  icon,
  title,
  body,
  right,
}: {
  icon: ReactNode;
  title: ReactNode;
  body: ReactNode;
  right: ReactNode;
}) {
  return (
    <li className="flex flex-wrap items-center gap-3 px-4 py-3">
      <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
        {icon}
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-[13px] font-medium">{title}</p>
        <p className="text-[12px] text-muted-foreground">{body}</p>
      </div>
      {right}
    </li>
  );
}

function Integrations() {
  const s = usePyo();
  return (
    <Page width="max-w-4xl">
      <PageHeader title="Integrations" />
      <Panel title="Tailscale" description="Joins new servers and clusters to your tailnet.">
        <ul className="list-rows">
          <KeyRow
            kind="tailscale"
            title="Tailscale auth key"
            placeholder="tskey-auth-…"
            help={
              <>
                Use a one-off or pre-approved key: <MenuPath steps={["Settings", "Keys"]} />.
              </>
            }
          />
        </ul>
      </Panel>
      <Panel title="Git providers">
        <ul className="list-rows">
          <GithubRows />
        </ul>
      </Panel>
      <RegistryLoginsPanel />
      <Panel title="Cluster connections">
        <ul className="list-rows">
          {clustersIn(s).map((c) => (
            <Row
              key={c.id}
              icon={<TreeStructureIcon size={15} />}
              title={
                <Link
                  to="/$team/clusters/$clusterId/settings"
                  params={{ team: currentSlug(), clusterId: c.name }}
                  className="hover:text-primary"
                >
                  {c.name}
                </Link>
              }
              body={`${c.connection.method === "agent" ? `Agent ${c.connection.agentVersion}` : "Kubeconfig"}`}
              right={<ClusterStatusBadge status={c.status} />}
            />
          ))}
          {!clustersIn(s).length ? (
            <Row
              icon={<TreeStructureIcon size={15} />}
              title="No clusters"
              body="Add a cluster under Clusters."
              right={null}
            />
          ) : null}
        </ul>
      </Panel>
      <Panel title="Secret management">
        <ul className="list-rows">
          <Row
            icon={<KeyIcon size={15} />}
            title="Cluster Secrets"
            body="Default. Stored in the target cluster."
            right={<Badge tone="success">Active</Badge>}
          />
        </ul>
      </Panel>
      <SecretStoresPanel />
    </Page>
  );
}

/** The team's GitHub accounts, through this installation's GitHub App. */
function GithubRows() {
  const s = usePyo();
  const [busy, setBusy] = useState(false);
  const installs = s.githubInstallations.filter(
    (g) => g.teamId === s.currentTeamId,
  );
  useEffect(() => {
    const q = new URLSearchParams(window.location.search).get("github");
    if (!q) return;
    if (q === "connected")
      toast({
        title: "GitHub connected",
        description:
          "Pick a repository in New deploy. Pushes then deploy on their own.",
      });
    else
      toast({
        tone: "error",
        title:
          q === "taken"
            ? "Another team has connected this GitHub account."
            : q === "denied"
              ? "Your GitHub account can't access that installation."
              : "GitHub didn't connect. Try again.",
      });
    window.history.replaceState(null, "", window.location.pathname);
  }, []);
  const connect = async () => {
    setBusy(true);
    const r = await api.githubInstallUrl();
    if (r) window.location.assign(r.url);
    else setBusy(false);
  };
  if (!s.github)
    return (
      <Row
        icon={<BrandMark icon={GITHUB_LOGO} size={15} />}
        title="GitHub"
        body={
          s.user.isAdmin
            ? "Set up the GitHub App for private repos and deploy on push."
            : "An installation admin can set up GitHub."
        }
        right={
          s.user.isAdmin ? (
            <Link
              to="/settings/github"
              className="text-[12.5px] font-medium text-primary hover:text-primary/80"
            >
              Set up in Settings
            </Link>
          ) : (
            <Badge>Not set up</Badge>
          )
        }
      />
    );
  return (
    <>
      {installs.map((g) => (
        <Row
          key={g.id}
          icon={<BrandMark icon={GITHUB_LOGO} size={15} />}
          title={`GitHub · ${g.account}`}
          body={`${g.accountType === "Organization" ? "Organization" : "Account"} · ${g.repositorySelection === "all" ? "all repositories" : "selected repositories"}`}
          right={
            <span className="flex items-center gap-2">
              <a
                href={`https://github.com/settings/installations/${g.id}`}
                target="_blank"
                rel="noreferrer"
                className="text-[12.5px] text-muted-foreground hover:text-foreground"
              >
                Repositories
              </a>
              <Button
                variant="ghost"
                size="xs"
                className="text-destructive"
                onClick={async () => {
                  if (await api.removeGithubInstallation(g.id))
                    toast({
                      title: `Disconnected ${g.account}`,
                      description: "Uninstall the app on GitHub too.",
                    });
                }}
              >
                Disconnect
              </Button>
            </span>
          }
        />
      ))}
      <Row
        icon={<BrandMark icon={GITHUB_LOGO} size={15} />}
        title={installs.length ? "Another GitHub account" : "GitHub"}
        body={`Install ${s.github.name} and pick the repositories Pyo can see.`}
        right={
          <Button size="xs" disabled={busy} onClick={connect}>
            {busy ? "Opening GitHub…" : "Connect"}
          </Button>
        }
      />
    </>
  );
}

function KeyRow({
  kind,
  title,
  placeholder,
  help,
}: {
  kind: "tailscale";
  title: string;
  placeholder: string;
  help: ReactNode;
}) {
  const s = usePyo();
  const saved = integrationIn(s, kind);
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);
  return (
    <li className="flex flex-wrap items-center gap-3 px-4 py-3">
      <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
        <KeyIcon size={15} />
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-[13px] font-medium">{title}</p>
        <p className="text-[12px] text-muted-foreground">
          {saved ? `Saved · ${saved.hint}` : help}
        </p>
      </div>
      {saved ? (
        <Button
          variant="secondary"
          size="xs"
          onClick={() => void api.removeIntegration(kind)}
        >
          Remove
        </Button>
      ) : (
        <form
          className="flex w-full gap-2 sm:w-auto"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!value.trim()) return;
            setSaving(true);
            const r = await api.saveIntegration(kind, value.trim());
            setSaving(false);
            if (r) {
              setValue("");
              toast({ title: `${title} saved` });
            }
          }}
        >
          <Input
            type="password"
            autoComplete="off"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder={placeholder}
            aria-label={title}
            className="h-8 sm:w-56"
          />
          <Button type="submit" size="xs" disabled={saving || !value.trim()}>
            {saving ? "Saving…" : "Save"}
          </Button>
        </form>
      )}
    </li>
  );
}
