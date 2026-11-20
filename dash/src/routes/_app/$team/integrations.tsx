import { createFileRoute, redirect } from "@tanstack/react-router";
import { siGitlab, siHetzner, siTailscale, siVault } from "simple-icons";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { z } from "zod";
import {
  IntegrationCard,
  type IntegrationCardProps,
} from "~/components/pyo/IntegrationCard";
import { RegistriesDialog } from "~/components/pyo/RegistryLogins";
import { SecretStoresDialog } from "~/components/pyo/SecretStores";
import { CloudflareDialog } from "~/components/pyo/Cloudflare";
import { TailscaleDialog } from "~/components/pyo/Tailscale";
import {
  GithubDialog,
  GitlabDialog,
  githubPermissionsMissing,
} from "~/components/pyo/GitConnections";
import { HetznerDialog, hetznerOf } from "~/components/pyo/Hetzner";
import { useHosted } from "~/components/pyo/Managed";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import type { Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Page, PageHeader, NoMatches } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { Tabs } from "~/components/ui/Tabs";
import { seo } from "~/lib/seo";
import {
  cloudflareIn,
  clustersIn,
  getState,
  roleIn,
  teamBySlug,
  listedServices, usePyo,
  type PyoState,
} from "~/lib/pyo/store";
import { takeGithubReturn } from "~/lib/pyo/github-return";
import { toast } from "~/lib/pyo/toast";
import { BrandMark, CLOUDFLARE_LOGO, DOCKER_LOGO, GITHUB_LOGO } from "~/components/pyo/common";

// Everything Pyo connects to, as cards: a filter by category, a search, and
// each card's dialog to set it up or manage it. "/integrations#<id>" scrolls
// to a card and opens its dialog. Alert channels live on Notifications.
export const Route = createFileRoute("/_app/$team/integrations")({
  // ?github=: back from GitHub (see useGithubReturn).
  validateSearch: z.object({ github: z.string().optional().catch(undefined) }),
  // Connecting GitHub was started from another page (a deploy link): say
  // how it went there instead.
  beforeLoad: ({ search, params, preload }) => {
    if (preload || !search.github) return;
    const team = teamBySlug(getState(), params.team);
    const back = team ? takeGithubReturn(team.id) : null;
    if (!back) return;
    githubResultToast(search.github, true);
    throw redirect({ href: back, replace: true });
  },
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

type Id = "tailscale" | "hetzner" | "github" | "gitlab" | "cloudflare" | "registries" | "secrets";

type Integration = Omit<IntegrationCardProps, "status" | "action" | "highlight"> & {
  id: Id;
  category: Category;
  connected: boolean;
  /** Set up: what needs a look, or a step under way. */
  problem?: { tone: Tone; label: string; detail?: string } | null;
  /** Not set up: whether this user can set it up. */
  canSetUp: boolean;
  dialog: (onClose: () => void) => ReactNode;
};

const plural = (n: number, one: string, many = `${one}s`) =>
  `${n} ${n === 1 ? one : many}`;

/** Repositories the team's services deploy from, through a connection. */
function reposLinked(s: PyoState, linked: (src: { installationId?: number; gitConnectionId?: string }) => boolean) {
  const repos = new Set<string>();
  for (const a of s.services) {
    if (a.teamId !== s.currentTeamId || a.source.type !== "git") continue;
    if (linked(a.source)) repos.add(a.source.repoUrl);
  }
  return repos.size;
}

const ATTENTION = "Needs attention";

function Integrations() {
  const s = usePyo();
  const teamAdmin = useTeamAdmin();
  const cfAdmin = ["owner", "admin"].includes(roleIn(s) ?? "");
  // Hetzner on a hosted Pyo: not yet (new clusters there have no tailnet
  // to join until their own Tailscale is set up).
  const hosted = useHosted();
  const [filter, setFilter] = useState<string>("all");
  const [q, setQ] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const [highlight, setHighlight] = useState<string | null>(null);

  useGithubReturn(() => setOpen("github"));

  const team = s.currentTeamId;
  // Pyo's own (self-hosted), else the first of the team's clusters with it.
  const teamTailnets = clustersIn(s).flatMap((c) => (c.tailscale ? [c.tailscale] : []));
  const ts = s.tailscale;
  const tsSuffix = ts?.suffix ?? teamTailnets.find((t) => t.suffix)?.suffix;
  const hetzner = hetznerOf(s);
  const cloudServers = s.cloudServers.filter((c) => c.teamId === team);
  const installs = s.githubInstallations.filter((g) => g.teamId === team);
  const gitlab = s.gitConnections.filter((c) => c.teamId === team);
  const cf = cloudflareIn(s);
  const logins = s.registryLogins.filter((l) => l.teamId === team);
  const stores = s.secretStores.filter((x) => x.teamId === team);

  const tsProblem = (): Integration["problem"] => {
    if (!ts) return null;
    if (ts.hostMove?.state === "failed")
      return { tone: "danger", label: ATTENTION, detail: "This server couldn't move" };
    if (ts.hostMove?.state === "moving") return { tone: "progress", label: "Moving this server" };
    if (ts.dashboardError || ts.registryError)
      return { tone: "warning", label: ATTENTION, detail: ts.dashboardError ?? ts.registryError ?? undefined };
    if (!ts.onTailnet) return { tone: "progress", label: "Joining tailnet" };
    if (ts.dashboardName && !ts.dashboard) return { tone: "progress", label: "Getting certificate" };
    return null;
  };

  const failedServers = cloudServers.filter((c) => c.state === "failed").length;
  const gitlabProjects = gitlab.reduce((n, c) => n + (c.info.projects ?? 0), 0);
  const gitlabProblems = gitlab.some((c) => c.info.problems?.length);
  const cfLacking = cf?.zones.filter((z) => !z.canEdit).length ?? 0;
  const storeUses = listedServices(s).filter(
    (a) => a.teamId === team && a.secretImports?.some((i) => stores.some((x) => x.id === i.storeId)),
  ).length;
  const githubRepos = reposLinked(s, (src) => installs.some((g) => g.id === src.installationId));
  const gitlabRepos = reposLinked(s, (src) => gitlab.some((c) => c.id === src.gitConnectionId));

  const offered: Integration[] = [
    {
      id: "tailscale",
      name: "Tailscale",
      category: "Networking",
      icon: <BrandMark icon={siTailscale} size={18} />,
      description: "Servers join your tailnet with it.",
      connected: !!ts || teamTailnets.length > 0,
      detail: tsSuffix ? <span className="font-mono text-[11.5px]">{tsSuffix}</span> : null,
      problem: tsProblem(),
      canSetUp: !!s.user.isAdmin || teamAdmin,
      dialog: (onClose) => <TailscaleDialog onClose={onClose} />,
    },
    {
      id: "hetzner",
      name: "Hetzner",
      category: "Servers",
      icon: <BrandMark icon={siHetzner} size={18} tone="brand" />,
      description: "Pyo makes servers at Hetzner.",
      connected: !!hetzner,
      detail: `${plural(cloudServers.length, "server")} made`,
      problem: failedServers
        ? { tone: "danger", label: ATTENTION, detail: `${plural(failedServers, "server")} failed` }
        : null,
      canSetUp: teamAdmin,
      dialog: (onClose) => <HetznerDialog onClose={onClose} />,
    },
    {
      id: "github",
      name: "GitHub",
      category: "Git",
      icon: <BrandMark icon={GITHUB_LOGO} size={18} />,
      description: "Private repos. Deploys on every push.",
      connected: !!s.github && installs.length > 0,
      detail: `${plural(githubRepos, "repo")} linked`,
      problem: githubPermissionsMissing(s.github, installs)
        ? { tone: "warning", label: ATTENTION, detail: "New permissions to approve" }
        : null,
      canSetUp: !!s.github || !!s.user.isAdmin,
      dialog: (onClose) => <GithubDialog onClose={onClose} />,
    },
    {
      id: "gitlab",
      name: "GitLab",
      category: "Git",
      icon: <BrandMark icon={siGitlab} size={18} />,
      description: "gitlab.com or your own GitLab.",
      connected: gitlab.length > 0,
      detail: gitlabProjects ? plural(gitlabProjects, "project") : `${plural(gitlabRepos, "repo")} linked`,
      problem: gitlabProblems ? { tone: "warning", label: ATTENTION } : null,
      canSetUp: true,
      dialog: (onClose) => <GitlabDialog onClose={onClose} />,
    },
    {
      id: "cloudflare",
      name: "Cloudflare",
      category: "DNS",
      icon: <BrandMark icon={CLOUDFLARE_LOGO} size={18} />,
      description: "Creates DNS records in your zones.",
      connected: !!cf,
      detail: cf ? plural(cf.zones.length - cfLacking, "zone") : null,
      problem: cfLacking
        ? { tone: "warning", label: ATTENTION, detail: `${plural(cfLacking, "zone")} ${cfLacking === 1 ? "lacks" : "lack"} access` }
        : null,
      canSetUp: cfAdmin,
      dialog: (onClose) => <CloudflareDialog onClose={onClose} />,
    },
    {
      id: "registries",
      name: "Container registries",
      category: "Registries",
      icon: <BrandMark icon={DOCKER_LOGO} size={18} />,
      description: "Logins for private images.",
      connected: logins.length > 0,
      detail: plural(logins.length, "login"),
      canSetUp: true,
      dialog: (onClose) => <RegistriesDialog onClose={onClose} />,
    },
    {
      id: "secrets",
      name: "Secret stores",
      category: "Secrets",
      icon: <BrandMark icon={siVault} size={18} tone="mono" />,
      description: "Services import secrets on deploy.",
      connected: stores.length > 0,
      detail: storeUses ? `${plural(storeUses, "service")} importing` : "Not used yet",
      canSetUp: true,
      dialog: (onClose) => <SecretStoresDialog onClose={onClose} />,
    },
  ];
  const all = hosted === false ? offered : offered.filter((x) => x.id !== "hetzner");

  // Connected first, each group in the order above.
  const cards = [...all.filter((x) => x.connected), ...all.filter((x) => !x.connected)];
  const connectedN = all.filter((x) => x.connected).length;
  const used = CATEGORIES.filter((c) => all.some((x) => x.category === c));
  const needle = q.trim().toLowerCase();
  const shown = cards.filter(
    (x) =>
      (filter === "all" || (filter === "connected" ? x.connected : x.category === filter)) &&
      (!needle ||
        [x.name, x.category, x.description].some((t) => t.toLowerCase().includes(needle))),
  );

  // "/integrations#tailscale": show the card, ring it, open its dialog.
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
  const close = useCallback(() => {
    setOpen(null);
    if (window.location.hash)
      window.history.replaceState(window.history.state, "", window.location.pathname + window.location.search);
  }, []);

  const current = all.find((x) => x.id === open);

  const card = (x: Integration): IntegrationCardProps => {
    const status: IntegrationCardProps["status"] = !x.connected
      ? { tone: "neutral", label: "Not set up" }
      : x.problem
        ? { tone: x.problem.tone, label: x.problem.label }
        : { tone: "success", label: "Connected" };
    return {
      id: x.id,
      name: x.name,
      icon: x.icon,
      description: x.description,
      status,
      detail: x.connected ? (x.problem?.detail ?? x.detail) : null,
      action:
        x.connected || x.canSetUp ? (
          <Button
            size="xs"
            variant="secondary"
            aria-label={`${x.connected ? "Manage" : "Set up"} ${x.name}`}
            onClick={() => setOpen(x.id)}
          >
            {x.connected ? "Manage" : "Set up"}
          </Button>
        ) : (
          <span className="shrink-0 text-[12px] text-muted-foreground">Ask an admin</span>
        ),
      highlight: highlight === x.id,
    };
  };

  return (
    <Page width="max-w-7xl">
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
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {shown.map((x) => (
              <IntegrationCard key={x.id} {...card(x)} />
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
      {current ? current.dialog(close) : null}
    </Page>
  );
}

const GITHUB_ERRORS: Record<string, string> = {
  taken: "Another team has connected this GitHub account.",
  denied: "Your GitHub account can't access that installation.",
  expired: "That took too long. Try again.",
};

/** How connecting GitHub went. Elsewhere than here: no pointer to New deploy. */
function githubResultToast(q: string, elsewhere = false) {
  if (q === "connected")
    toast({
      title: "GitHub connected",
      description: elsewhere
        ? undefined
        : "Pick a repository in New deploy. Pushes then deploy on their own.",
    });
  else
    toast({
      tone: "error",
      title: GITHUB_ERRORS[q] ?? "GitHub didn't connect. Try again.",
    });
}

/**
 * Back from GitHub after installing the app (?github=connected, or an
 * error): say how it went and open the GitHub card.
 */
function useGithubReturn(open: () => void) {
  useEffect(() => {
    const q = new URLSearchParams(window.location.search).get("github");
    if (!q) return;
    githubResultToast(q);
    window.history.replaceState(null, "", window.location.pathname);
    open();
    // Once, on arrival.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
