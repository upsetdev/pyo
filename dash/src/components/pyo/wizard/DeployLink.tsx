import { HammerIcon, LinkBreakIcon, PlugsIcon } from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { GithubDialog, GitlabDialog } from "~/components/pyo/GitConnections";
import { Button } from "~/components/ui/Button";
import { Callout } from "~/components/ui/Panel";
import { fetchGitRepos, fetchGithubRepos } from "~/lib/pyo/backend";
import type { DeployLink } from "~/lib/pyo/deploy-link";
import type { DeployDraft } from "~/lib/pyo/draft";
import { currentSlug } from "~/lib/pyo/paths";
import { buildersIn, getState, usePyo } from "~/lib/pyo/store";

// What a "Deploy with Pyo" link needs besides the form: what's wrong with an
// invalid one, and for a repository the Git connection and the builder.

const NAME = { github: "GitHub", gitlab: "GitLab" } as const;

/** An invalid link, above the start screen. */
export function InvalidLink({ title, message }: { title?: string; message: string }) {
  return (
    <Callout tone="danger" icon={LinkBreakIcon} title={title ?? "This deploy link isn't valid"}>
      {message}
    </Callout>
  );
}

/** A linked repository: connect its Git host or add a builder when missing. */
export function LinkedRepoNotes({
  provider,
}: {
  provider: "github" | "gitlab";
}) {
  const s = usePyo();
  const [connecting, setConnecting] = useState(false);
  const connected =
    provider === "github"
      ? s.githubInstallations.some((x) => x.teamId === s.currentTeamId)
      : s.gitConnections.some((x) => x.teamId === s.currentTeamId);
  const builder = buildersIn(s).some((b) => b.status === "ready");
  if (connected && builder) return null;
  return (
    <div className="space-y-2">
      {builder ? null : (
        <Callout tone="warning" icon={HammerIcon} title="No builder is ready">
          Git deploys need one.{" "}
          <Link
            to="/$team/builders"
            params={{ team: currentSlug() }}
            className="font-medium text-foreground hover:text-primary"
          >
            Add a builder
          </Link>
        </Callout>
      )}
      {connected ? null : (
        <Callout tone="neutral" icon={PlugsIcon} title={`${NAME[provider]} isn't connected`}>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span>Public repositories deploy without it. Connect it for private ones.</span>
            <Button variant="secondary" size="xs" onClick={() => setConnecting(true)}>
              Connect {NAME[provider]}
            </Button>
          </div>
        </Callout>
      )}
      {connecting ? (
        provider === "github" ? (
          <GithubDialog
            onClose={() => setConnecting(false)}
            // Back to this deploy link, with its query, after GitHub.
            returnTo={`${window.location.pathname}${window.location.search}`}
          />
        ) : (
          <GitlabDialog onClose={() => setConnecting(false)} />
        )
      ) : null}
    </div>
  );
}

/**
 * The linked repository through the team's own connection, when it has one
 * that sees it: then private repositories build and pushes deploy. Matched by
 * URL only; a repository it doesn't see stays a public URL.
 */
export function useConnectedRepo(
  link: DeployLink | null | undefined,
  apply: (git: Partial<DeployDraft["git"]>, repoUrl: string) => void,
) {
  useEffect(() => {
    if (link?.kind !== "git") return;
    const s = getState();
    const team = s.currentTeamId;
    const want = link.repo.toLowerCase();
    let stopped = false;
    const done = (git: Partial<DeployDraft["git"]>) => {
      if (!stopped) apply(git, link.repo);
    };
    if (
      link.provider === "github" &&
      s.githubInstallations.some((x) => x.teamId === team)
    )
      void fetchGithubRepos(team).then(
        ({ repositories }) => {
          const r = repositories.find((x) => x.htmlUrl.toLowerCase() === want);
          if (r)
            done({
              repoUrl: r.htmlUrl,
              branch: r.defaultBranch || "main",
              installationId: r.installationId,
              gitConnectionId: undefined,
              private: r.private,
              commit: "",
            });
        },
        () => {},
      );
    if (
      link.provider === "gitlab" &&
      s.gitConnections.some((x) => x.teamId === team)
    )
      void fetchGitRepos(team).then(
        ({ repositories }) => {
          const r = repositories.find((x) => x.htmlUrl.toLowerCase() === want);
          if (r)
            done({
              repoUrl: r.htmlUrl,
              branch: r.defaultBranch || "main",
              gitConnectionId: r.connectionId,
              installationId: undefined,
              private: r.private,
              commit: "",
            });
        },
        () => {},
      );
    return () => {
      stopped = true;
    };
    // Once, for the link the page opened with.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
