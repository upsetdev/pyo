import {
  CheckCircleIcon,
  KeyIcon,
  PlugsConnectedIcon,
  TrashIcon,
  XCircleIcon,
} from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { fetchMcpCalls, type McpCall } from "~/lib/pyo/backend";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented } from "~/components/ui/Field";
import { Select } from "~/components/ui/Select";
import { Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { ApiKeyAccess, ApiToken, OAuthGrant } from "~/lib/pyo/types";

// Two ways in besides the dashboard, both acting as you with your
// permissions in every team: API keys for scripts calling the REST API, and
// AI assistants over MCP, which sign in with OAuth (you approve them in the
// browser; no key to paste).

function Snippet({ value }: { value: string }) {
  return (
    <div className="flex items-start gap-2 rounded-lg border border-border bg-background px-3 py-2">
      <pre className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[11.5px]">
        {value}
      </pre>
      <CopyButton value={value} label="Copy" />
    </div>
  );
}

const ACCESS: { value: ApiKeyAccess; label: string; hint: string }[] = [
  {
    value: "read",
    label: "Read only",
    hint: "See services, deployments, logs and servers.",
  },
  {
    value: "deploy",
    label: "Deploy",
    hint: "Also deploy, roll back, restart, stop and scale services.",
  },
  {
    value: "full",
    label: "Full access",
    hint: "Anything you can do, except manage your account and keys.",
  },
];

const EXPIRY = [
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
  { value: "365", label: "1 year" },
  { value: "never", label: "Never" },
] as const;
type Expiry = (typeof EXPIRY)[number]["value"];

/** "expires in 12 days", "expired" or "never expires"; amber within a week. */
function keyExpiry(t: ApiToken, now = Date.now()) {
  if (!t.expiresAt) return { text: "never expires", tone: "" };
  const left = Math.ceil((t.expiresAt - now) / 86_400_000);
  if (left <= 0) return { text: "expired", tone: "text-destructive" };
  return {
    text: `expires in ${left} day${left === 1 ? "" : "s"}`,
    tone: left <= 7 ? "text-warning" : "",
  };
}

function baseUrl(publicUrl?: string) {
  return (
    publicUrl || (typeof window === "undefined" ? "" : window.location.origin)
  ).replace(/\/$/, "");
}

export function ApiTokensPanel() {
  const s = usePyo();
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [fresh, setFresh] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<ApiToken | null>(null);
  const [access, setAccess] = useState<ApiKeyAccess>("deploy");
  const [team, setTeam] = useState("all");
  const [expiry, setExpiry] = useState<Expiry>("90");
  const teamName = (id: string) =>
    s.teams.find((t) => t.id === id)?.name ?? "a team you left";
  const scope = (t: ApiToken) =>
    `${ACCESS.find((a) => a.value === t.access)?.label ?? t.access} · ${t.teamIds ? t.teamIds.map(teamName).join(", ") : "All teams"}`;
  const example = `curl ${baseUrl(s.email?.publicUrl)}/api/v1/state \\\n  -H "Authorization: Bearer ${fresh ?? "<your key>"}"`;

  return (
    <Panel>
      {s.apiTokens.length ? (
        <ul className="list-rows">
          {s.apiTokens.map((t) => (
            <li key={t.id} className="flex items-center gap-3 px-4 py-3">
              <KeyIcon size={15} className="shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                <p className="truncate text-[13px] font-medium">{t.name}</p>
                <p className="truncate text-[12px] text-muted-foreground">
                  <span className="font-mono">{t.prefix}…</span> · {scope(t)} ·
                  created {timeAgo(t.createdAt)} ·{" "}
                  {t.lastUsedAt
                    ? `used ${timeAgo(t.lastUsedAt)}`
                    : "never used"}{" "}
                  ·{" "}
                  <span className={keyExpiry(t).tone}>{keyExpiry(t).text}</span>
                </p>
              </div>
              <Button
                variant="ghost"
                size="icon"
                aria-label={`Revoke ${t.name}`}
                onClick={() => setRevoking(t)}
              >
                <TrashIcon size={14} />
              </Button>
            </li>
          ))}
        </ul>
      ) : null}

      {fresh ? (
        <div className="space-y-2 border-t border-border bg-muted/30 px-4 py-3 first:border-t-0">
          <p className="text-[12.5px] font-medium">
            Your new key. Copy it now. It isn't shown again.
          </p>
          <Snippet value={fresh} />
        </div>
      ) : null}

      <form
        className="space-y-4 border-t border-border p-4 first:border-t-0"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!name.trim() || creating) return;
          setCreating(true);
          const r = await api.createApiToken({
            name: name.trim(),
            access,
            teamIds: team === "all" ? null : [team],
            expiresInDays:
              expiry === "never" ? null : (Number(expiry) as 30 | 90 | 365),
          });
          setCreating(false);
          if (!r) return;
          setFresh(r.token);
          setName("");
          toast({
            title: "API key created",
            description: "Copy it now. It isn't shown again.",
          });
        }}
      >
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Name">
            {(p) => (
              <Input
                {...p}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. GitHub Actions"
                maxLength={60}
              />
            )}
          </Field>
          <Field label="Teams">
            {(p) => (
              <Select
                {...p}
                value={team}
                onChange={(e) => setTeam(e.target.value)}
              >
                <option value="all">All teams</option>
                {s.teams.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        </div>
        <div className="space-y-1.5">
          <Segmented
            label="Access"
            value={access}
            onChange={setAccess}
            options={ACCESS.map(({ value, label }) => ({ value, label }))}
          />
          <p className="text-[12px] text-muted-foreground">
            {ACCESS.find((a) => a.value === access)!.hint}
          </p>
        </div>
        <Field label="Expires">
          {() => (
            <Segmented
              label="Expires"
              value={expiry}
              onChange={setExpiry}
              options={EXPIRY.map(({ value, label }) => ({ value, label }))}
            />
          )}
        </Field>
        <div className="flex justify-end">
          <Button type="submit" size="sm" disabled={!name.trim() || creating}>
            {creating ? "Creating…" : "Create key"}
          </Button>
        </div>
      </form>

      <div className="space-y-2 border-t border-border px-4 py-4">
        <p className="text-[13px] font-medium">Try it</p>
        <Snippet value={example} />
      </div>

      <ConfirmDialog
        open={!!revoking}
        onClose={() => setRevoking(null)}
        title={`Revoke "${revoking?.name}"?`}
        description="Anything using it loses access right away."
        confirmLabel="Revoke key"
        onConfirm={async () => {
          if (!revoking) return;
          if (await api.revokeApiToken(revoking.id))
            toast({ title: "API key revoked" });
          if (fresh) setFresh(null);
        }}
      />
    </Panel>
  );
}

export function AssistantsPanel() {
  const s = usePyo();
  const [client, setClient] = useState<"claude" | "json">("claude");
  const [revoking, setRevoking] = useState<OAuthGrant | null>(null);
  const url = `${baseUrl(s.email?.publicUrl)}/api/mcp`;
  const snippet =
    client === "claude"
      ? `claude mcp add --transport http pyo ${url}`
      : JSON.stringify({ mcpServers: { pyo: { type: "http", url } } }, null, 2);

  return (
    <>
      <Panel
        title="Connect an assistant"
        actions={
          <Segmented
            label="Client"
            value={client}
            onChange={setClient}
            options={[
              { value: "claude", label: "Claude Code" },
              { value: "json", label: "Other clients" },
            ]}
          />
        }
        footer={
          <>
            Approve it in the browser.
          </>
        }
      >
        <div className="p-4">
          <Snippet value={snippet} />
        </div>
      </Panel>

      <Panel title="Connected assistants">
        {s.oauthGrants.length ? (
          <ul className="list-rows">
            {s.oauthGrants.map((g) => (
              <li key={g.id} className="flex items-center gap-3 px-4 py-3">
                <PlugsConnectedIcon
                  size={15}
                  className="shrink-0 text-muted-foreground"
                />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-[13px] font-medium">{g.client}</p>
                  <p className="truncate text-[12px] text-muted-foreground">
                    {ACCESS.find((a) => a.value === (g.access ?? "full"))
                      ?.label ?? "Full"}{" "}
                    · connected {timeAgo(g.createdAt)} ·{" "}
                    {g.lastUsedAt
                      ? `used ${timeAgo(g.lastUsedAt)}`
                      : "not used yet"}
                  </p>
                </div>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => setRevoking(g)}
                >
                  Disconnect
                </Button>
              </li>
            ))}
          </ul>
        ) : (
          <p className="px-4 py-6 text-center text-[13px] text-muted-foreground">
            None yet. Assistants you approve show up here.
          </p>
        )}

        <ConfirmDialog
          open={!!revoking}
          onClose={() => setRevoking(null)}
          title={`Disconnect ${revoking?.client}?`}
          description="It loses access right away."
          confirmLabel="Disconnect"
          onConfirm={async () => {
            if (!revoking) return;
            if (await api.revokeOauthGrant(revoking.id))
              toast({ title: `${revoking.client} disconnected` });
          }}
        />
      </Panel>
    </>
  );
}

/** "create_service" → "Create service" */
const toolLabel = (t: string) =>
  (t.charAt(0).toUpperCase() + t.slice(1)).replaceAll("_", " ");

/** What a call was about, in words: the service, deployment or name it named. */
function callTarget(
  s: ReturnType<typeof usePyo>,
  args: Record<string, unknown>,
) {
  const str = (k: string) =>
    typeof args[k] === "string" ? (args[k] as string) : "";
  const id = str("service_id");
  const app = id && s.services.find((a) => a.id === id);
  return [
    app ? app.name : id,
    str("deployment_id"),
    str("name"),
    str("image"),
    str("hostname"),
    typeof args.replicas === "number" ? `${args.replicas} replicas` : "",
  ]
    .filter(Boolean)
    .join(" · ");
}

/** What assistants did over MCP, newest first. */
export function McpActivityPanel() {
  const s = usePyo();
  const [calls, setCalls] = useState<McpCall[] | null>(null);
  useEffect(() => {
    let live = true;
    const load = () =>
      fetchMcpCalls()
        .then((r) => live && setCalls(r.calls))
        .catch(() => live && setCalls((c) => c ?? []));
    void load();
    const t = setInterval(load, 10_000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, []);

  return (
    <Panel title="Activity">
      {calls === null ? (
        <p className="px-4 py-6 text-center text-[13px] text-muted-foreground">
          Loading…
        </p>
      ) : calls.length ? (
        <ul className="list-rows">
          {calls.map((c) => {
            const target = callTarget(s, c.args);
            return (
              <li key={c.id} className="flex items-start gap-3 px-4 py-2.5">
                {c.ok ? (
                  <CheckCircleIcon
                    size={15}
                    weight="fill"
                    className="mt-0.5 shrink-0 text-success"
                    aria-label="Succeeded"
                  />
                ) : (
                  <XCircleIcon
                    size={15}
                    weight="fill"
                    className="mt-0.5 shrink-0 text-destructive"
                    aria-label="Failed"
                  />
                )}
                <div className="min-w-0 flex-1">
                  <p className="truncate text-[13px]">
                    <span className="font-medium">{toolLabel(c.tool)}</span>
                    {target ? (
                      <span className="text-muted-foreground"> · {target}</span>
                    ) : null}
                  </p>
                  {c.error ? (
                    <p className="mt-0.5 text-[12px] text-destructive">
                      {c.error}
                    </p>
                  ) : null}
                </div>
                <span className="shrink-0 text-right text-[12px] text-muted-foreground">
                  {c.client} · {timeAgo(c.createdAt)}
                  <span className="block font-mono text-[11px] text-muted-foreground/70">
                    {c.durationMs < 1000
                      ? `${c.durationMs} ms`
                      : `${(c.durationMs / 1000).toFixed(1)} s`}
                  </span>
                </span>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="px-4 py-6 text-center text-[13px] text-muted-foreground">
          No calls yet. What your assistants do shows up here.
        </p>
      )}
    </Panel>
  );
}
