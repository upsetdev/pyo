import { useState } from "react";
import { BrandMark, GITHUB_LOGO } from "~/components/pyo/common";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { api, usePyo } from "~/lib/pyo/store";

// This installation's GitHub App. The installation admin creates it once
// with GitHub's manifest flow, in the GitHub card on Integrations. GitHub
// sends them back to /settings/github?github=created, which opens that card.

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
