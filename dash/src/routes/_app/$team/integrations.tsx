import { createFileRoute } from "@tanstack/react-router";
import { siGitlab, siHetzner, siTailscale, siVault } from "simple-icons";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { z } from "zod";
import {
  IntegrationCard,
  docsUrl,
  type IntegrationCardProps,
} from "~/components/pyo/IntegrationCard";
import {
  AddLoginDialog,
  RegistryLoginsSettings,
} from "~/components/pyo/RegistryLogins";
import { SecretStoresSettings, StoreDialog } from "~/components/pyo/SecretStores";
import { CloudflareSettings } from "~/components/pyo/Cloudflare";
import { TailscaleSettings } from "~/components/pyo/Tailscale";
import { GitlabRows, TokenDialog } from "~/components/pyo/GitConnections";
import { HetznerSettings, hetznerOf } from "~/components/pyo/Hetzner";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { GithubAppSetup } from "~/components/pyo/GithubSettings";
import { ManagedOr } from "~/components/pyo/Managed";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import type { MenuItem } from "~/components/ui/Menu";
import { Page, PageHeader, NoMatches } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { Tabs } from "~/components/ui/Tabs";
import { timeAgo } from "~/lib/pyo/format";
import { seo } from "~/lib/seo";
import {
  api,
  cloudflareIn,
  roleIn,
  usePyo,
  type PyoState,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { BrandMark, CLOUDFLARE_LOGO, DOCKER_LOGO, GITHUB_LOGO } from "~/components/pyo/common";

// Everything Pyo connects to, as cards: a filter by category, a search, and
// each card's settings in a dialog. "/integrations#<id>" scrolls to a card
// and opens its settings. Notification channels live in Settings.
export const Route = createFileRoute("/_app/$team/integrations")({
  // ?github=: back from GitHub (see useGithubReturn).
  validateSearch: z.object({ github: z.string().optional().catch(undefined) }),
  head: () => ({ meta: seo({ title: "Integrations" }) }),
  component: Integrations,
});

const CATEGORIES = [
  "Git",
  "DNS",
  "Networking",
  "Servers",
  "Registries",
  "Secrets",
] as const;
type Category = (typeof CATEGORIES)[number];

type Integration = Omit<IntegrationCardProps, "category" | "highlight"> & {
  category: Category;
  /** Its settings, in a dialog from the menu or a link to the card. */
  settings?: { title: string; description?: string; body: ReactNode };
};

type Removing = "tailscale" | "hetzner" | "cloudflare" | null;

const plural = (n: number, one: string, many = `${one}s`) =>
  `${n} ${n === 1 ? one : many}`;

const oldest = (xs: { createdAt: number }[]) =>
  Math.min(...xs.map((x) => x.createdAt));

/** Repositories the team's services deploy from, through a connection. */
function reposLinked(s: PyoState, linked: (src: { installationId?: number; gitConnectionId?: string }) => boolean) {
  const repos = new Set<string>();
  for (const a of s.services) {
    if (a.teamId !== s.currentTeamId || a.source.type !== "git") continue;
    if (linked(a.source)) repos.add(a.source.repoUrl);
  }
  return repos.size;
}

function Integrations() {
  const s = usePyo();
  const teamAdmin = useTeamAdmin();
  const cfAdmin = ["owner", "admin"].includes(roleIn(s) ?? "");
  const [filter, setFilter] = useState<string>("all");
  const [q, setQ] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const [adding, setAdding] = useState<"gitlab" | "registries" | "secrets" | null>(null);
  const [removing, setRemoving] = useState<Removing>(null);
  const [highlight, setHighlight] = useState<string | null>(null);

  useGithubReturn(() => setOpen("github"));

  const team = s.currentTeamId;
  const ts = s.tailscale;
  const hetzner = hetznerOf(s);
  const cloudServers = s.cloudServers.filter((c) => c.teamId === team);
  const installs = s.githubInstallations.filter((g) => g.teamId === team);
  const gitlab = s.gitConnections.filter((c) => c.teamId === team);
  const cf = cloudflareIn(s);
  const logins = s.registryLogins.filter((l) => l.teamId === team);
  const stores = s.secretStores.filter((x) => x.teamId === team);

  const connect = (label = "Connect", onClick?: () => void, disabled = false) => (
    <Button size="xs" variant="secondary" disabled={disabled} onClick={onClick}>
      {label}
    </Button>
  );
  const askAdmin = <Badge>Ask an admin</Badge>;
  const settingsItem = (id: string, label = "Settings"): MenuItem => ({
    label,
    onSelect: () => setOpen(id),
  });
  const guideItem = (page: string): MenuItem => ({
    label: "Setup guide",
    href: docsUrl(page),
  });

  const test = {
    tailscale: async () => {
      const r = await api.testTailscale();
      if ("error" in r) return toast({ tone: "error", title: r.error });
      const bad = r.data.checks.filter((c) => !c.ok);
      toast(
        bad.length
          ? { tone: "error", title: "Tailscale has problems", description: bad.map((c) => c.message).join(" ") }
          : { title: "Tailscale works" },
      );
    },
    hetzner: async () => {
      const r = await api.testHetzner();
      if ("error" in r) return toast({ tone: "error", title: r.error });
      toast({ title: "Hetzner works", description: r.data.locations.map((l) => l.city).join(", ") });
    },
    cloudflare: async () => {
      const r = await api.testCloudflare();
      if (!r) return;
      toast(
        r.problems?.length
          ? { tone: "error", title: "Cloudflare has problems", description: r.problems.join(" ") }
          : { title: "Cloudflare token works" },
      );
    },
  };

  const tsStatus = (): { tone: Tone; label: string } | null => {
    if (!ts) return null;
    if (ts.hostMove?.state === "failed") return { tone: "danger", label: "This server couldn't move" };
    if (ts.hostMove?.state === "moving") return { tone: "progress", label: "Moving this server" };
    if (ts.dashboardError || ts.registryError) return { tone: "warning", label: "Needs attention" };
    if (!ts.onTailnet) return { tone: "progress", label: "Joining tailnet" };
    if (ts.dashboardName && !ts.dashboard) return { tone: "progress", label: "Getting certificate" };
    return { tone: "success", label: "On your tailnet" };
  };

  const failedServers = cloudServers.filter((c) => c.state === "failed").length;
  const gitlabProjects = gitlab.reduce((n, c) => n + (c.info.projects ?? 0), 0);
  const gitlabProblems = gitlab.some((c) => c.info.problems?.length);
  const cfLacking = cf?.zones.filter((z) => !z.canEdit).length ?? 0;
  const lastUsed = Math.max(0, ...logins.map((l) => l.lastUsedAt ?? 0));
  const storeUses = s.services.filter(
    (a) => a.teamId === team && a.secretImports?.some((i) => stores.some((x) => x.id === i.storeId)),
  ).length;

  const all: Integration[] = [
    {
      id: "tailscale",
      name: "Tailscale",
      category: "Networking",
      icon: <BrandMark icon={siTailscale} size={17} />,
      description: "Servers join your tailnet with it.",
      connected: !!ts,
      stat: ts?.suffix ? { label: "Tailnet", value: <span className="font-mono text-[12px]">{ts.suffix}</span> } : null,
      tags: ["Join servers", "Private dashboard"],
      status: tsStatus(),
      docs: "tailscale",
      action: s.user.isAdmin ? connect("Connect", () => setOpen("tailscale")) : askAdmin,
      menu: [
        settingsItem("tailscale"),
        ...(s.user.isAdmin
          ? [
              { label: "Test", onSelect: () => void test.tailscale() },
              guideItem("tailscale"),
              { type: "separator" as const },
              { label: "Remove", danger: true, onSelect: () => setRemoving("tailscale") },
            ]
          : [guideItem("tailscale")]),
      ],
      settings: { title: "Tailscale", description: "For the whole Pyo. Servers join your tailnet with it.", body: <TailscaleSettings /> },
    },
    {
      id: "hetzner",
      name: "Hetzner",
      category: "Servers",
      icon: <BrandMark icon={siHetzner} size={17} tone="brand" />,
      description: "Pyo makes servers in your Hetzner project.",
      connected: !!hetzner,
      stat: hetzner ? { label: "Servers made", value: cloudServers.length } : null,
      tags: ["New servers", "Clusters"],
      status: hetzner
        ? failedServers
          ? { tone: "danger", label: `${plural(failedServers, "server")} failed` }
          : { tone: "success", label: `Connected ${timeAgo(hetzner.createdAt)}` }
        : null,
      docs: "hetzner",
      action: teamAdmin ? connect("Connect", () => setOpen("hetzner")) : askAdmin,
      menu: [
        settingsItem("hetzner"),
        ...(teamAdmin
          ? [
              { label: "Test", onSelect: () => void test.hetzner() },
              guideItem("hetzner"),
              { type: "separator" as const },
              { label: "Remove", danger: true, onSelect: () => setRemoving("hetzner") },
            ]
          : [guideItem("hetzner")]),
      ],
      settings: { title: "Hetzner", description: "For this team. Pyo makes servers in your Hetzner project.", body: <HetznerSettings /> },
    },
    {
      id: "github",
      name: "GitHub",
      category: "Git",
      icon: <BrandMark icon={GITHUB_LOGO} size={17} />,
      description: "Private repos, and a deploy on every push.",
      connected: !!s.github && installs.length > 0,
      stat: { label: "Repos linked", value: reposLinked(s, (src) => installs.some((g) => g.id === src.installationId)) },
      tags: ["Private repos", "Deploy on push"],
      status: installs.length
        ? { tone: "success", label: `Connected ${timeAgo(oldest(installs))}` }
        : null,
      docs: "github",
      action: s.github
        ? connect("Connect", () => setOpen("github"))
        : s.user.isAdmin
          ? connect("Set up", () => setOpen("github"))
          : askAdmin,
      menu: [settingsItem("github", "Accounts"), guideItem("github")],
      settings: { title: "GitHub", body: <ul className="list-rows"><GithubRows /></ul> },
    },
    {
      id: "gitlab",
      name: "GitLab",
      category: "Git",
      icon: <BrandMark icon={siGitlab} size={17} />,
      description: "gitlab.com or your own GitLab, with an access token.",
      connected: gitlab.length > 0,
      stat: gitlabProjects
        ? { label: "Projects", value: gitlabProjects }
        : { label: "Repos linked", value: reposLinked(s, (src) => gitlab.some((c) => c.id === src.gitConnectionId)) },
      tags: ["Private repos", "Deploy on push", "Self-hosted"],
      status: gitlab.length
        ? gitlabProblems
          ? { tone: "warning", label: "Needs attention" }
          : { tone: "success", label: `Connected ${timeAgo(oldest(gitlab))}` }
        : null,
      docs: "gitlab",
      action: connect("Connect", () => setAdding("gitlab")),
      menu: [
        settingsItem("gitlab", "Connections"),
        { label: "Connect another GitLab", onSelect: () => setAdding("gitlab") },
        guideItem("gitlab"),
      ],
      settings: { title: "GitLab", body: <ul className="list-rows"><GitlabRows /></ul> },
    },
    {
      id: "cloudflare",
      name: "Cloudflare",
      category: "DNS",
      icon: <BrandMark icon={CLOUDFLARE_LOGO} size={17} />,
      description: "Creates DNS records for domains in your zones.",
      connected: !!cf,
      stat: cf ? { label: "Zones", value: cf.zones.length - cfLacking } : null,
      tags: ["DNS records", "Proxy"],
      status: cf
        ? cfLacking
          ? { tone: "warning", label: `${plural(cfLacking, "zone")} lack access` }
          : { tone: "success", label: `Tested ${timeAgo(cf.testedAt)}` }
        : null,
      docs: "cloudflare",
      action: cfAdmin ? connect("Connect", () => setOpen("cloudflare")) : askAdmin,
      menu: [
        settingsItem("cloudflare"),
        ...(cfAdmin
          ? [
              { label: "Test", onSelect: () => void test.cloudflare() },
              guideItem("cloudflare"),
              { type: "separator" as const },
              { label: "Remove", danger: true, onSelect: () => setRemoving("cloudflare") },
            ]
          : [guideItem("cloudflare")]),
      ],
      settings: { title: "Cloudflare", description: "Creates DNS records for domains in your zones.", body: <CloudflareSettings /> },
    },
    {
      id: "registries",
      name: "Container registries",
      category: "Registries",
      icon: <BrandMark icon={DOCKER_LOGO} size={17} />,
      description: "Logins for private images. Services pull with them.",
      connected: logins.length > 0,
      stat: { label: "Logins", value: logins.length },
      tags: ["Private images", "Docker Hub", "ghcr.io"],
      status: logins.length
        ? lastUsed
          ? { tone: "success", label: `Used ${timeAgo(lastUsed)}` }
          : { tone: "neutral", label: "Not used yet" }
        : null,
      docs: "registries",
      action: connect("Add login", () => setAdding("registries")),
      menu: [
        settingsItem("registries", "Logins"),
        { label: "Add login", onSelect: () => setAdding("registries") },
        guideItem("registries"),
      ],
      settings: { title: "Container registries", body: <RegistryLoginsSettings /> },
    },
    {
      id: "secrets",
      name: "Secret stores",
      category: "Secrets",
      icon: <BrandMark icon={siVault} size={17} tone="mono" />,
      description: "Services import secrets from your store on each deploy.",
      connected: stores.length > 0,
      stat: { label: "Services importing", value: storeUses },
      tags: ["Vault", "AWS", "Doppler"],
      status: stores.length
        ? { tone: "success", label: `Connected ${timeAgo(oldest(stores))}` }
        : null,
      docs: "external-secrets",
      action: connect("Connect", () => setAdding("secrets")),
      menu: [
        settingsItem("secrets", "Stores"),
        { label: "Connect another store", onSelect: () => setAdding("secrets") },
        guideItem("external-secrets"),
      ],
      settings: { title: "Secret stores", body: <SecretStoresSettings /> },
    },
  ];

  // Connected first, each group in the order above.
  const cards = [...all.filter((x) => x.connected), ...all.filter((x) => !x.connected)];
  const connectedN = all.filter((x) => x.connected).length;
  const used = CATEGORIES.filter((c) => all.some((x) => x.category === c));
  const needle = q.trim().toLowerCase();
  const shown = cards.filter(
    (x) =>
      (filter === "all" || (filter === "connected" ? x.connected : x.category === filter)) &&
      (!needle ||
        [x.name, x.category, x.description, ...(x.tags ?? [])].some((t) => t.toLowerCase().includes(needle))),
  );

  // "/integrations#tailscale": show the card, ring it, open its settings.
  const ids = all.map((x) => x.id).join(",");
  const reveal = useCallback(() => {
    const id = window.location.hash.slice(1);
    if (!id || !ids.split(",").includes(id)) return;
    setFilter("all");
    setQ("");
    setOpen(id);
    setHighlight(id);
    requestAnimationFrame(() => document.getElementById(id)?.scrollIntoView({ block: "center" }));
    setTimeout(() => setHighlight((h) => (h === id ? null : h)), 2000);
  }, [ids]);
  useEffect(() => {
    reveal();
    window.addEventListener("hashchange", reveal);
    return () => window.removeEventListener("hashchange", reveal);
  }, [reveal]);
  const close = () => {
    setOpen(null);
    if (window.location.hash)
      window.history.replaceState(window.history.state, "", window.location.pathname + window.location.search);
  };

  const current = all.find((x) => x.id === open);

  return (
    <Page>
      <PageHeader title="Integrations" />
      <div className="space-y-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="min-w-0 max-w-full">
            <Tabs
              label="Show"
              value={filter}
              onChange={setFilter}
              items={[
                { value: "all", label: "All", count: all.length },
                { value: "connected", label: "Connected", count: connectedN },
                ...used.map((c) => ({ value: c, label: c })),
              ]}
            />
          </div>
          <div className="flex w-full flex-wrap items-center gap-3 sm:w-auto">
            <p className="whitespace-nowrap text-[12.5px] text-muted-foreground">
              {connectedN} connected · {all.length - connectedN} available
            </p>
            <SearchInput
              value={q}
              onChange={setQ}
              placeholder="Search integrations"
              label="Search integrations"
              className="min-w-0 flex-1 sm:w-60 sm:flex-none"
            />
          </div>
        </div>
        {shown.length ? (
          <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
            {shown.map(({ settings: _settings, ...x }) => (
              <IntegrationCard key={x.id} {...x} highlight={highlight === x.id} />
            ))}
          </div>
        ) : (
          <NoMatches
            query={q}
            what="integrations"
            onClear={() => {
              setQ("");
              setFilter("all");
            }}
          />
        )}
      </div>
      <Dialog
        open={!!current?.settings}
        onClose={close}
        title={current?.settings?.title ?? ""}
        description={current?.settings?.description}
        width="max-w-xl"
      >
        <div className="-mx-5 -my-4">{current?.settings?.body}</div>
      </Dialog>
      {adding === "gitlab" ? <TokenDialog onClose={() => setAdding(null)} /> : null}
      <AddLoginDialog open={adding === "registries"} onClose={() => setAdding(null)} />
      {adding === "secrets" ? <StoreDialog onClose={() => setAdding(null)} /> : null}
      <ConfirmDialog
        open={removing === "tailscale"}
        onClose={() => setRemoving(null)}
        onConfirm={() => {
          setRemoving(null);
          void api.removeTailscale();
        }}
        title="Remove Tailscale?"
        description="Pyo's names on your tailnet are deleted. Servers stay on it. The public dashboard opens again."
        confirmLabel="Remove"
      />
      <ConfirmDialog
        open={removing === "hetzner"}
        onClose={() => setRemoving(null)}
        onConfirm={() => {
          setRemoving(null);
          void api.removeHetzner();
        }}
        title="Remove Hetzner?"
        description="Servers Pyo made there keep running."
        confirmLabel="Remove"
      />
      <ConfirmDialog
        open={removing === "cloudflare"}
        onClose={() => setRemoving(null)}
        onConfirm={() => {
          setRemoving(null);
          void api.removeCloudflare();
          toast({ title: "Cloudflare token removed" });
        }}
        title="Remove the Cloudflare token?"
        description="Records Pyo made stay in Cloudflare."
        confirmLabel="Remove"
      />
    </Page>
  );
}

const GITHUB_ERRORS: Record<string, string> = {
  taken: "Another team has connected this GitHub account.",
  denied: "Your GitHub account can't access that installation.",
  expired: "That took too long. Try again.",
  "app-failed": "GitHub didn't create the app.",
};

/**
 * Back from GitHub (?github=created after creating the app, ?github=connected
 * after installing it): say how it went and open the GitHub card.
 */
function useGithubReturn(open: () => void) {
  useEffect(() => {
    const q = new URLSearchParams(window.location.search).get("github");
    if (!q) return;
    if (q === "connected")
      toast({
        title: "GitHub connected",
        description:
          "Pick a repository in New deploy. Pushes then deploy on their own.",
      });
    else if (q === "created")
      toast({ title: "GitHub App created", description: "Connect an account next." });
    else
      toast({
        tone: "error",
        title: GITHUB_ERRORS[q] ?? "GitHub didn't connect. Try again.",
      });
    window.history.replaceState(null, "", window.location.pathname);
    open();
    // Once, on arrival.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}

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

/** The team's GitHub accounts, through this installation's GitHub App. */
function GithubRows() {
  const s = usePyo();
  const [busy, setBusy] = useState(false);
  const installs = s.githubInstallations.filter(
    (g) => g.teamId === s.currentTeamId,
  );
  const connect = async () => {
    setBusy(true);
    const r = await api.githubInstallUrl();
    if (r) window.location.assign(r.url);
    else setBusy(false);
  };
  // No GitHub App yet: the installation admin creates it right here.
  if (!s.github)
    return s.user.isAdmin ? (
      <li className="px-5 py-4">
        <ManagedOr
          title="GitHub is managed for you"
          description="Connect repositories when you deploy."
        >
          <GithubAppSetup />
        </ManagedOr>
      </li>
    ) : (
      <Row
        icon={<BrandMark icon={GITHUB_LOGO} size={15} />}
        title="GitHub"
        body="An installation admin can set up GitHub."
        right={<Badge>Not set up</Badge>}
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
      {s.user.isAdmin ? <GithubAppRow /> : null}
    </>
  );
}

/** The installation's GitHub App, for its admin: where it is, and removing it. */
function GithubAppRow() {
  const s = usePyo();
  const [removing, setRemoving] = useState(false);
  const app = s.github;
  if (!app) return null;
  return (
    <>
      <Row
        icon={<BrandMark icon={GITHUB_LOGO} size={15} />}
        title={
          <a
            href={app.htmlUrl}
            target="_blank"
            rel="noreferrer"
            className="hover:text-primary"
          >
            GitHub App · {app.slug}
          </a>
        }
        body={`For every team · owned by ${app.owner} · ${app.webhookActive ? "pushes by webhook" : "pushes checked every minute"}`}
        right={
          <Button
            variant="ghost"
            size="xs"
            className="text-destructive"
            onClick={() => setRemoving(true)}
          >
            Remove
          </Button>
        }
      />
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title="Remove the GitHub App?"
        description="Every team loses private repos and deploy on push."
        confirmLabel="Remove"
        onConfirm={async () => {
          if (await api.removeGithubApp()) toast({ title: "GitHub App removed" });
        }}
      />
    </>
  );
}
