import {
  ArrowSquareOutIcon,
  CheckCircleIcon,
  MinusCircleIcon,
} from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { BrandMark, GITHUB_LOGO } from "~/components/pyo/common";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";

// This installation's GitHub App. An admin creates it once with GitHub's
// manifest flow; teams then connect their GitHub accounts in Integrations.

/** GitHub can only deliver webhooks to a public https address. */
const publicHttps = (url: string) => {
  const m = /^https:\/\/([^/:]+)/.exec(url.trim());
  return (
    !!m &&
    !/^(localhost|127\.|10\.|192\.168\.|100\.)|\.local(host)?$/.test(m[1])
  );
};

export function GithubSettingsSection() {
  const s = usePyo();
  const admin = !!s.user.isAdmin;
  const app = s.github;
  const [publicUrl, setPublicUrl] = useState(
    s.email?.publicUrl ??
      (typeof window === "undefined" ? "" : window.location.origin),
  );
  const [org, setOrg] = useState("");
  const [busy, setBusy] = useState(false);
  const [removing, setRemoving] = useState(false);

  // GitHub sends the admin back with ?github=created (or an error).
  useEffect(() => {
    const q = new URLSearchParams(window.location.search).get("github");
    if (!q) return;
    if (q === "created")
      toast({
        title: "GitHub App created",
        description:
          "Teams connect GitHub in Integrations.",
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

  if (app)
    return (
      <div className="space-y-4">
        <StatusNote
          tone="success"
          icon={<CheckCircleIcon size={15} />}
          title={`${app.name} is set up`}
        >
          Teams connect GitHub in Integrations.
        </StatusNote>
        <Panel>
          <dl className="grid gap-x-8 gap-y-4 p-4 text-[13px] sm:grid-cols-2">
            <Item label="GitHub App">
              <a
                href={app.htmlUrl}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1.5 hover:text-primary"
              >
                <BrandMark icon={GITHUB_LOGO} size={13} /> {app.slug}{" "}
                <ArrowSquareOutIcon
                  size={12}
                  className="text-muted-foreground"
                />
              </a>
            </Item>
            <Item label="Owner">{app.owner}</Item>
            <Item label="Pushes">
              {app.webhookActive
                ? "Instant, by webhook"
                : "Checked every minute"}
            </Item>
            <Item label="Dashboard URL">
              <span className="font-mono text-[12px]">{app.publicUrl}</span>
            </Item>
          </dl>
          {admin ? (
            <div className="flex justify-end border-t border-border px-4 py-3">
              <Button
                variant="secondary"
                size="sm"
                className="text-destructive"
                onClick={() => setRemoving(true)}
              >
                Remove
              </Button>
            </div>
          ) : null}
        </Panel>
        <ConfirmDialog
          open={removing}
          onClose={() => setRemoving(false)}
          title="Remove the GitHub App?"
          description="Teams lose private repos and deploy on push."
          confirmLabel="Remove"
          onConfirm={async () => {
            if (await api.removeGithubApp())
              toast({ title: "GitHub App removed" });
          }}
        />
      </div>
    );

  const webhook = publicHttps(publicUrl);
  return (
    <div className="space-y-4">
      <StatusNote
        tone="neutral"
        icon={<MinusCircleIcon size={15} />}
        title="GitHub isn't set up"
      >
        Adds private repos, deploy on push and commit statuses.
        {admin ? "" : " The installation's admin can set it up."}
      </StatusNote>
      {admin ? (
        <Panel title="Create a GitHub App">
          <div className="space-y-4 p-4">
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
            <Field
              label="Organization"
              optional
              hint="Instead of your account."
            >
              {(p) => (
                <Input
                  {...p}
                  value={org}
                  onChange={(e) => setOrg(e.target.value.trim())}
                  placeholder="acme"
                />
              )}
            </Field>
          </div>
          <div className="flex justify-end border-t border-border px-4 py-3">
            <Button
              size="sm"
              disabled={busy || !/^https?:\/\/\S+$/.test(publicUrl.trim())}
              onClick={async () => {
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
              <BrandMark icon={GITHUB_LOGO} size={13} />{" "}
              {busy ? "Opening GitHub…" : "Create GitHub App"}
            </Button>
          </div>
        </Panel>
      ) : null}
    </div>
  );
}

function Item({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="min-w-0">
      <dt className="mb-1 text-[12px] text-muted-foreground">{label}</dt>
      <dd className="truncate">{children}</dd>
    </div>
  );
}
