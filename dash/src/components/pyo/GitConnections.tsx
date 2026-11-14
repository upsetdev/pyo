import { useState } from "react";
import { siGitlab } from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { GitConnection } from "~/lib/pyo/types";

// GitLab connections: a team's owner or admin adds an access token, per
// GitLab (gitlab.com, a self-managed one, or both).

const host = (url: string) => {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
};

/** Add a connection, or replace one's token. */
function TokenDialog({
  connection,
  onClose,
}: {
  connection?: GitConnection;
  onClose: () => void;
}) {
  const [url, setUrl] = useState("https://gitlab.com");
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const ready = !!token.trim() && (!!connection || !!url.trim());
  return (
    <Dialog
      open
      onClose={onClose}
      title={
        connection ? `Replace the token for ${connection.account}` : "Connect GitLab"
      }
      description="Checked before saving."
      onSubmit={async () => {
        if (!ready || busy) return;
        setBusy(true);
        setError("");
        const r = connection
          ? await api.replaceGitConnectionToken(connection.id, token.trim())
          : await api.addGitConnection(url.trim(), token.trim());
        setBusy(false);
        if ("error" in r) {
          setError(r.error);
          return;
        }
        toast({
          title: connection ? "Token replaced" : "GitLab connected",
          ...(r.data.warning
            ? { tone: "error" as const, description: r.data.warning }
            : {}),
        });
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!ready || busy}>
            {busy ? "Checking…" : connection ? "Replace" : "Connect"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {connection ? null : (
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
            <Input
              {...p}
              type="password"
              autoComplete="off"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="glpat-…"
            />
          )}
        </Field>
      </div>
    </Dialog>
  );
}

function ConnectionRow({ c }: { c: GitConnection }) {
  const [replacing, setReplacing] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [testing, setTesting] = useState(false);
  const polled = c.webhooks.filter((w) => !w.active).length;
  const problems = c.info.problems ?? [];
  const expires = c.info.expiresAt ? `Expires ${c.info.expiresAt}` : null;
  return (
    <li className="flex flex-wrap items-center gap-3 px-4 py-3">
      <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
        <BrandMark icon={siGitlab} size={15} />
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-[13px] font-medium">GitLab · {c.account}</p>
        <p className="text-[12px] text-muted-foreground">
          {[
            host(c.url),
            expires,
            polled ? `${polled} checked every minute` : null,
          ]
            .filter(Boolean)
            .join(" · ")}
        </p>
      </div>
      {problems.length ? <Badge tone="warning">Needs attention</Badge> : null}
      <span className="flex items-center gap-1">
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
          {testing ? "Testing…" : "Test"}
        </Button>
        <Button variant="ghost" size="xs" onClick={() => setReplacing(true)}>
          Replace
        </Button>
        <Button
          variant="ghost"
          size="xs"
          className="text-destructive"
          onClick={() => setRemoving(true)}
        >
          Remove
        </Button>
      </span>
      {replacing ? (
        <TokenDialog connection={c} onClose={() => setReplacing(false)} />
      ) : null}
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        onConfirm={async () => {
          setRemoving(false);
          if (await api.removeGitConnection(c.id))
            toast({ title: `Removed ${c.account}` });
        }}
        title={`Remove ${c.account}?`}
        description="Pyo removes its webhooks. Revoke the token in GitLab too."
        confirmLabel="Remove"
      />
    </li>
  );
}

/** Integrations › Git providers: the team's GitLab connections. */
export function GitlabRows() {
  const s = usePyo();
  const [adding, setAdding] = useState(false);
  const list = s.gitConnections.filter((c) => c.teamId === s.currentTeamId);
  return (
    <>
      {list.map((c) => (
        <ConnectionRow key={c.id} c={c} />
      ))}
      <li className="flex flex-wrap items-center gap-3 px-4 py-3">
        <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
          <BrandMark icon={siGitlab} size={15} />
        </span>
        <div className="min-w-0 flex-1">
          <p className="text-[13px] font-medium">
            {list.length ? "Another GitLab" : "GitLab"}
          </p>
          <p className="text-[12px] text-muted-foreground">
            gitlab.com or self-managed, with an access token.
          </p>
        </div>
        <Button size="xs" onClick={() => setAdding(true)}>
          Connect
        </Button>
      </li>
      {adding ? <TokenDialog onClose={() => setAdding(false)} /> : null}
    </>
  );
}
