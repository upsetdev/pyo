import { PlusIcon } from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { siGitlab } from "simple-icons";
import { BrandMark, GITHUB_LOGO } from "~/components/pyo/common";
import {
  Busy,
  ConfirmNote,
  DialogRow,
  DialogRows,
  ExternalLink,
  IntegrationDialog,
} from "~/components/pyo/IntegrationDialog";
import { Badge } from "~/components/ui/Badge";
import { Button, buttonClass } from "~/components/ui/Button";
import { Field, Input, SecretInput } from "~/components/ui/Field";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { GitConnection, GithubInstallation } from "~/lib/pyo/types";
import { forgetGithubReturn, rememberGithubReturn } from "~/lib/pyo/github-return";

// Git providers on Integrations. GitHub: the team's accounts, through this
// installation's GitHub App. GitLab: a team's owner or admin adds an access
// token, per GitLab (gitlab.com, the team's own GitLab, or both).

const host = (url: string) => {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
};

const GITLAB = <BrandMark icon={siGitlab} size={15} />;
const GITHUB = <BrandMark icon={GITHUB_LOGO} size={15} />;

function ConnectionRow({
  c,
  onReplace,
}: {
  c: GitConnection;
  onReplace: () => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const [testing, setTesting] = useState(false);
  const polled = c.webhooks.filter((w) => !w.active).length;
  const problems = c.info.problems ?? [];
  const expires = c.info.expiresAt ? `Expires ${c.info.expiresAt}` : null;
  return (
    <DialogRow
      icon={GITLAB}
      title={c.account}
      meta={[host(c.url), expires, polled ? `${polled} checked every minute` : null]
        .filter(Boolean)
        .join(" · ")}
      badge={problems.length ? <Badge tone="warning">Needs attention</Badge> : null}
      actions={
        confirming ? null : (
          <>
            <Button
              variant="ghost"
              size="xs"
              disabled={testing}
              onClick={async () => {
                setTesting(true);
                const r = await api.testGitConnection(c.id);
                setTesting(false);
                if (!r) return;
                toast(
                  r.problems.length
                    ? {
                        tone: "error",
                        title: r.working === false ? "Not working" : "Works, with gaps",
                        description: r.problems.join(" "),
                      }
                    : {
                        title: "Works",
                        description: `${r.projects ?? 0} projects as ${r.user}.`,
                      },
                );
              }}
            >
              {testing ? <Busy>Testing…</Busy> : "Test"}
            </Button>
            <Button variant="ghost" size="xs" onClick={onReplace}>
              Replace
            </Button>
            <Button variant="ghost" size="xs" className="text-destructive! hover:bg-destructive/10" onClick={() => setConfirming(true)}>
              Disconnect
            </Button>
          </>
        )
      }
    >
      {problems.length ? (
        <ul className="space-y-0.5 text-[12px] text-warning">
          {problems.map((p) => (
            <li key={p}>{p}</li>
          ))}
        </ul>
      ) : null}
      {confirming ? (
        <ConfirmNote
          title={`Disconnect ${c.account}?`}
          confirmLabel="Disconnect"
          onCancel={() => setConfirming(false)}
          onConfirm={async () => {
            setConfirming(false);
            if (await api.removeGitConnection(c.id)) toast({ title: `Removed ${c.account}` });
          }}
        >
          Pyo removes its webhooks. Revoke the token in GitLab too.
        </ConfirmNote>
      ) : null}
    </DialogRow>
  );
}

/** Add a GitLab, or replace one's token. */
function TokenForm({
  connection,
  url,
  setUrl,
  token,
  setToken,
  error,
}: {
  connection?: GitConnection;
  url: string;
  setUrl: (v: string) => void;
  token: string;
  setToken: (v: string) => void;
  error: string;
}) {
  return (
    <div className="space-y-4">
      {connection ? (
        <p className="text-[12.5px] text-muted-foreground">
          A new token for <span className="font-medium text-foreground">{connection.account}</span> on{" "}
          {host(connection.url)}.
        </p>
      ) : (
        <Field label="GitLab URL" hint="gitlab.com or your own GitLab.">
          {(p) => (
            <Input
              {...p}
              mono
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder="https://gitlab.example.com"
            />
          )}
        </Field>
      )}
      <Field
        label="Access token"
        hint="A group access token with the Maintainer role and the api scope."
        error={error || undefined}
      >
        {(p) => (
          <SecretInput
            {...p}
            autoComplete="off"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="glpat-…"
          />
        )}
      </Field>
    </div>
  );
}

/**
 * Integrations › GitLab: the team's connections, and a form to add one or
 * replace one's token. Opens on the form while there are none.
 */
export function GitlabDialog({ onClose }: { onClose: () => void }) {
  const s = usePyo();
  const list = s.gitConnections.filter((c) => c.teamId === s.currentTeamId);
  const [form, setForm] = useState<"add" | GitConnection | null>(list.length ? null : "add");
  const [url, setUrl] = useState("https://gitlab.com");
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const connection = form && form !== "add" ? form : undefined;
  const ready = !!token.trim() && (!!connection || !!url.trim());
  const open = (f: "add" | GitConnection | null) => {
    setForm(f);
    setUrl("https://gitlab.com");
    setToken("");
    setError("");
  };
  const back = () => (list.length ? open(null) : onClose());

  if (form)
    return (
      <IntegrationDialog
        icon={GITLAB}
        name="GitLab"
        description="Checked before saving."
        docs="gitlab"
        onClose={onClose}
        onSubmit={async () => {
          if (!ready || busy) return;
          setBusy(true);
          setError("");
          const r = connection
            ? await api.replaceGitConnectionToken(connection.id, token.trim())
            : await api.addGitConnection(url.trim(), token.trim());
          setBusy(false);
          if ("error" in r) return setError(r.error);
          toast({
            title: connection ? "Token replaced" : "GitLab connected",
            ...(r.data.warning ? { tone: "error" as const, description: r.data.warning } : {}),
          });
          open(null);
        }}
        footer={
          <>
            <Button variant="ghost" size="sm" onClick={back}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={!ready || busy}>
              {busy ? <Busy>Checking…</Busy> : connection ? "Replace" : "Connect"}
            </Button>
          </>
        }
      >
        <TokenForm
          connection={connection}
          url={url}
          setUrl={setUrl}
          token={token}
          setToken={setToken}
          error={error}
        />
      </IntegrationDialog>
    );

  return (
    <IntegrationDialog
      icon={GITLAB}
      name="GitLab"
      description="gitlab.com or your own GitLab, with an access token."
      docs="gitlab"
      onClose={onClose}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => open("add")}>
            <PlusIcon size={12} weight="bold" /> Connect another
          </Button>
          <Button size="sm" onClick={onClose}>
            Done
          </Button>
        </>
      }
    >
      <DialogRows>
        {list.map((c) => (
          <ConnectionRow key={c.id} c={c} onReplace={() => open(c)} />
        ))}
      </DialogRows>
    </IntegrationDialog>
  );
}

function GithubRow({ g }: { g: GithubInstallation }) {
  const [confirming, setConfirming] = useState(false);
  return (
    <DialogRow
      icon={GITHUB}
      title={g.account}
      meta={`${g.accountType === "Organization" ? "Organization" : "Account"} · ${g.repositorySelection === "all" ? "all repositories" : "selected repositories"}`}
      actions={
        confirming ? null : (
          <>
            <ExternalLink
              href={`https://github.com/settings/installations/${g.id}`}
              className="px-2 text-[12px]"
            >
              Repositories
            </ExternalLink>
            <Button variant="ghost" size="xs" className="text-destructive! hover:bg-destructive/10" onClick={() => setConfirming(true)}>
              Disconnect
            </Button>
          </>
        )
      }
    >
      {confirming ? (
        <ConfirmNote
          title={`Disconnect ${g.account}?`}
          confirmLabel="Disconnect"
          onCancel={() => setConfirming(false)}
          onConfirm={async () => {
            setConfirming(false);
            if (await api.removeGithubInstallation(g.id))
              toast({
                title: `Disconnected ${g.account}`,
                description: "Uninstall the app on GitHub too.",
              });
          }}
        >
          Uninstall the app on GitHub too.
        </ConfirmNote>
      ) : null}
    </DialogRow>
  );
}

/** Integrations › GitHub: the team's GitHub accounts, through this installation's GitHub App. */
export function GithubDialog({
  onClose,
  returnTo,
}: {
  onClose: () => void;
  /** A path on this dashboard to come back to from GitHub, in place of Integrations. */
  returnTo?: string;
}) {
  const s = usePyo();
  const [busy, setBusy] = useState(false);
  const installs = s.githubInstallations.filter((g) => g.teamId === s.currentTeamId);
  const connect = async () => {
    setBusy(true);
    // Started here without one: a way back kept by an earlier try doesn't apply.
    if (returnTo) rememberGithubReturn(returnTo, s.currentTeamId);
    else forgetGithubReturn();
    const r = await api.githubInstallUrl();
    if (r) window.location.assign(r.url);
    else {
      forgetGithubReturn();
      setBusy(false);
    }
  };
  const shell = (body: ReactNode, footer: ReactNode) => (
    <IntegrationDialog
      icon={GITHUB}
      name="GitHub"
      description="Private repos, and a deploy on every push."
      docs="github"
      onClose={onClose}
      footer={footer}
    >
      {body}
    </IntegrationDialog>
  );

  // No GitHub App yet: the installation admin creates it in Settings.
  if (!s.github)
    return shell(
      <p className="text-[12.5px] text-muted-foreground">
        {s.user.isAdmin
          ? "Set up the GitHub App first."
          : "Ask an installation admin to set up GitHub."}
      </p>,
      s.user.isAdmin ? (
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Link to="/github" className={buttonClass("primary", "sm")}>
            Set up
          </Link>
        </>
      ) : (
        <Button size="sm" onClick={onClose}>
          Done
        </Button>
      ),
    );

  const connectLabel = busy ? <Busy>Opening GitHub…</Busy> : installs.length ? "Connect another" : "Connect";
  return shell(
    installs.length ? (
      <DialogRows>
        {installs.map((g) => (
          <GithubRow key={g.id} g={g} />
        ))}
      </DialogRows>
    ) : (
      <p className="text-[12.5px] text-muted-foreground">
        Install {s.github.name} and pick the repositories Pyo can see.
      </p>
    ),
    installs.length ? (
      <>
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => void connect()}>
          {busy ? null : <PlusIcon size={12} weight="bold" />}
          {connectLabel}
        </Button>
        <Button size="sm" onClick={onClose}>
          Done
        </Button>
      </>
    ) : (
      <>
        <Button variant="ghost" size="sm" onClick={onClose}>
          Cancel
        </Button>
        <Button size="sm" disabled={busy} onClick={() => void connect()}>
          {connectLabel}
        </Button>
      </>
    ),
  );
}
