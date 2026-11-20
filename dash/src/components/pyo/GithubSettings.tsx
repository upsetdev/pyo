import { ArrowSquareOutIcon, WarningIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { BrandMark, GITHUB_LOGO } from "~/components/pyo/common";
import { Button, buttonClass } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { GithubInstallation } from "~/lib/pyo/types";

// This installation's GitHub App, on Settings (/github). The installation
// admin creates it once with GitHub's manifest flow. GitHub sends them back
// to /github?github=created. Teams then connect their own accounts in the
// GitHub card on Integrations.

/** Create the app, or see and remove it. */
export function GithubSettingsSection() {
  const s = usePyo();
  // GitHub sends the admin back with ?github=created (or an error).
  useEffect(() => {
    const q = new URLSearchParams(window.location.search).get("github");
    if (!q) return;
    if (q === "created")
      toast({
        title: "GitHub App created",
        description: "Teams connect GitHub in Integrations.",
      });
    else
      toast({
        tone: "error",
        title:
          q === "expired"
            ? "That took too long. Try again."
            : "GitHub didn't create the app.",
      });
    window.history.replaceState(null, "", window.location.pathname);
  }, []);
  if (!s.user.isAdmin)
    return (
      <Panel>
        <p className="px-4 py-4 text-[13px] text-muted-foreground">
          Only the installation's admin manages the GitHub App.
        </p>
      </Panel>
    );
  return (
    <Panel>
      {s.github ? (
        <ul className="list-rows">
          <GithubAppRow />
        </ul>
      ) : (
        <div className="p-4">
          <GithubAppSetup />
        </div>
      )}
    </Panel>
  );
}

/** A button-styled link to a page on GitHub. */
function GithubLink({ href, children }: { href: string; children: string }) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className={buttonClass("secondary", "sm")}
    >
      {children} <ArrowSquareOutIcon size={12} aria-hidden />
    </a>
  );
}

/**
 * GitHub wants new permissions (pull requests, checks, deployments). The
 * app's owner adds them to the app first, then each account approves them
 * once. Nothing when all are there.
 */
export function GithubPermissionsNote({
  installs,
}: {
  installs: GithubInstallation[];
}) {
  const s = usePyo();
  const app = s.github;
  if (!app) return null;
  const icon = <WarningIcon size={14} weight="bold" />;
  if (app.missingPermissions?.length)
    return (
      <StatusNote
        tone="warning"
        icon={icon}
        title="GitHub needs new permissions"
        action={
          s.user.isAdmin && app.permissionsUrl ? (
            <GithubLink href={app.permissionsUrl}>Update app</GithubLink>
          ) : undefined
        }
      >
        {s.user.isAdmin
          ? "Add them to the GitHub App, then approve them once."
          : "An admin adds them to the GitHub App first."}
      </StatusNote>
    );
  const pending = installs.filter(
    (g) => g.missingPermissions?.length && g.approveUrl,
  );
  if (!pending.length) return null;
  return (
    <StatusNote
      tone="warning"
      icon={icon}
      title="GitHub needs new permissions approved once"
      action={
        pending.length === 1 ? (
          <GithubLink href={pending[0].approveUrl!}>Approve</GithubLink>
        ) : undefined
      }
    >
      {pending.length === 1 ? (
        `For ${pending[0].account}. Previews wait until then.`
      ) : (
        <>
          For{" "}
          {pending.map((g, i) => (
            <span key={g.id}>
              {i ? (i === pending.length - 1 ? " and " : ", ") : ""}
              <a
                href={g.approveUrl}
                target="_blank"
                rel="noreferrer"
                className="font-medium text-primary hover:text-primary/80"
              >
                {g.account}
              </a>
            </span>
          ))}
          . Previews wait until then.
        </>
      )}
    </StatusNote>
  );
}

/** The installation's GitHub App, for its admin: where it is, and removing it. */
export function GithubAppRow() {
  const s = usePyo();
  const [removing, setRemoving] = useState(false);
  const app = s.github;
  if (!app) return null;
  return (
    <>
      <li className="flex flex-wrap items-center gap-3 px-4 py-3">
        <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
          <BrandMark icon={GITHUB_LOGO} size={15} />
        </span>
        <div className="min-w-0 flex-1">
          <p className="text-[13px] font-medium">
            <a
              href={app.htmlUrl}
              target="_blank"
              rel="noreferrer"
              className="hover:text-primary"
            >
              GitHub App · {app.slug}
            </a>
          </p>
          <p className="text-[12px] text-muted-foreground">
            For every team · owned by {app.owner} ·{" "}
            {app.webhookActive
              ? "pushes by webhook"
              : "pushes checked every minute"}
          </p>
        </div>
        <Button
          variant="ghost"
          size="xs"
          className="text-destructive"
          onClick={() => setRemoving(true)}
        >
          Remove
        </Button>
      </li>
      {app.missingPermissions?.length ? (
        <li className="px-4 py-3">
          <StatusNote
            tone="warning"
            icon={<WarningIcon size={14} weight="bold" />}
            title="The app needs new permissions"
            action={
              app.permissionsUrl ? (
                <GithubLink href={app.permissionsUrl}>Update app</GithubLink>
              ) : undefined
            }
          >
            {app.missingPermissions.join(", ")}. Each account then approves
            them once.
          </StatusNote>
        </li>
      ) : null}
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

/** GitHub can only deliver webhooks to a public https address. */
const publicHttps = (url: string) => {
  const m = /^https:\/\/([^/:]+)/.exec(url.trim());
  return (
    !!m &&
    !/^(localhost|127\.|10\.|192\.168\.|100\.)|\.local(host)?$/.test(m[1])
  );
};

/** Create the GitHub App: the dashboard's address, and an organization. */
export function GithubAppSetup() {
  const s = usePyo();
  const [publicUrl, setPublicUrl] = useState(
    s.email?.publicUrl ??
      (typeof window === "undefined" ? "" : window.location.origin),
  );
  const [org, setOrg] = useState("");
  const [busy, setBusy] = useState(false);
  const webhook = publicHttps(publicUrl);
  return (
    <form
      className="space-y-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        const r = await api.githubManifest(publicUrl.trim(), org);
        if (!r) return setBusy(false);
        // GitHub's manifest flow: post the manifest from the browser.
        const form = document.createElement("form");
        form.method = "post";
        form.action = r.action;
        const field = document.createElement("input");
        field.type = "hidden";
        field.name = "manifest";
        field.value = r.manifest;
        form.appendChild(field);
        document.body.appendChild(form);
        form.submit();
      }}
    >
      <p className="text-[13px] text-muted-foreground">
        Create the GitHub App once. Then each team connects its accounts.
      </p>
      <Field
        label="Dashboard URL"
        hint={
          webhook
            ? "GitHub sends each push right away."
            : "Not public: Pyo checks for new commits every minute."
        }
      >
        {(p) => (
          <Input
            {...p}
            mono
            value={publicUrl}
            onChange={(e) => setPublicUrl(e.target.value)}
          />
        )}
      </Field>
      <Field label="Organization" optional hint="Instead of your account.">
        {(p) => (
          <Input
            {...p}
            value={org}
            onChange={(e) => setOrg(e.target.value.trim())}
            placeholder="acme"
          />
        )}
      </Field>
      <div className="flex justify-end">
        <Button
          type="submit"
          size="sm"
          disabled={busy || !/^https?:\/\/\S+$/.test(publicUrl.trim())}
        >
          <BrandMark icon={GITHUB_LOGO} size={13} />{" "}
          {busy ? "Opening GitHub…" : "Create GitHub App"}
        </Button>
      </div>
    </form>
  );
}
