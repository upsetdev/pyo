import { CheckCircleIcon, CircleNotchIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { NeedsTailscale } from "~/components/pyo/Tailscale";
import { TailnetLockWait } from "~/components/pyo/TailnetLock";
import { AppLink } from "~/components/ui/AppLink";
import { LiveCommand } from "~/components/pyo/LiveCommand";
import { Badge, Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { Callout } from "~/components/ui/Panel";
import { Tooltip } from "~/components/ui/Tooltip";
import { DNS_LABEL } from "~/lib/pyo/format";
import {
  RUNTIME_LABEL,
  SANDBOX_STATE,
  SERVER_STATE,
  gettingReady,
  sandboxApi,
  type Sandbox,
  type SandboxServer,
  type SandboxState,
} from "~/lib/pyo/sandbox";
import { roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
import { useQuery } from "~/lib/pyo/query";
import { toast } from "~/lib/pyo/toast";

/** The time, again every `ms`: for countdowns. */
export function useNow(ms = 15_000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

export function SandboxStatus({ state, sandbox }: { state: SandboxState; sandbox?: Sandbox }) {
  const st = SANDBOX_STATE[state];
  const label =
    state === "creating" && sandbox?.preparing ? gettingReady(sandbox.template, sandbox.server) : st.label;
  return (
    <span className={`inline-flex items-center gap-1.5 text-[12.5px] ${st.tone === "danger" ? "text-destructive" : ""}`}>
      <Dot tone={st.tone} className="h-2 w-2" />
      {label}
    </span>
  );
}

/** Firecracker or gVisor, with the server's one line as its tooltip. */
export function RuntimeBadge({ server }: { server: SandboxServer }) {
  const runtime = server.capability?.runtime;
  if (!runtime) return null;
  const badge = (
    <Badge tone={runtime === "firecracker" ? "info" : runtime === "gvisor" ? "neutral" : "danger"}>
      {RUNTIME_LABEL[runtime]}
      {runtime === "firecracker" && server.capability?.kvm === "nested" ? " · nested" : ""}
    </Badge>
  );
  return server.summary ? <Tooltip content={server.summary}>{badge}</Tooltip> : badge;
}

export function ServerState({ server }: { server: SandboxServer }) {
  const st = SERVER_STATE[server.state] ?? { label: server.state, tone: "neutral" as const };
  return (
    <span className={`inline-flex items-center gap-1.5 text-[12.5px] ${st.tone === "danger" ? "text-destructive" : "text-muted-foreground"}`}>
      <Dot tone={st.tone} className="h-2 w-2" />
      {st.label}
    </span>
  );
}

/** "4 CPU · 16 GB · 3 sandboxes". */
export function serverCapacity(x: SandboxServer) {
  const c = x.capability;
  const parts = [
    c?.cpus ? `${c.cpus} CPU` : "",
    c?.memory ? `${Math.round(c.memory / 1024 ** 3)} GB` : "",
    x.capacity?.diskFree ? `${Math.round(x.capacity.diskFree / 1024 ** 3)} GB disk free` : "",
    `${x.sandboxes} sandbox${x.sandboxes === 1 ? "" : "es"}`,
  ];
  return parts.filter(Boolean).join(" · ");
}

/** Memory the sandboxes may still take on the server, as a share of what they may take. */
export function serverUse(x: SandboxServer) {
  const c = x.capacity;
  if (!c?.memoryTotal) return null;
  const usable = Math.max(1, c.memoryTotal - c.memoryReserved);
  return { used: Math.min(usable, c.memoryCommitted), total: usable };
}

/** How Pyo's "set up Tailscale" errors start: the word Tailscale links to its card. */
const TAILSCALE = "Set up Tailscale";

/** What sandbox.sh takes for --edge-domain. */
const EDGE_DOMAIN = /^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z][a-z0-9-]{0,62}$/;

/** Add a sandbox server: your own, with a one-time code in one command. */
export function AddSandboxServer({ onDone }: { onDone?: () => void }) {
  const s = usePyo();
  const role = roleIn(s);
  const canManage = role === "owner" || role === "admin";
  const [name, setName] = useState(`sandbox-${sandboxServersIn(s).length + 1}`);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState<{ id: string; name: string; command: string; expiresAt: number } | null>(null);
  const [edge, setEdge] = useState("");
  const nameError = DNS_LABEL.test(name) ? "" : "Lowercase letters, digits and dashes.";
  const edgeDomain = edge.trim().replace(/\.$/, "");
  const edgeError = !edgeDomain || EDGE_DOMAIN.test(edgeDomain) ? "" : "A domain, like sbx.example.com.";
  const joined = result
    ? s.sandboxServers.find((x) => x.id === result.id && x.state !== "joining")
    : undefined;
  // The code works for a while: the countdown ticks, and once it's out the
  // command is dead, so a new one is a click away.
  const now = useNow(result && !joined ? 5_000 : 60_000);
  const expired = !!result && !joined && now >= result.expiresAt;

  const add = async () => {
    if (nameError || edgeError || busy) return;
    setBusy(true);
    setError("");
    try {
      const r = await sandboxApi.addServer({
        teamId: s.currentTeamId,
        name,
        ...(edgeDomain ? { edgeDomain } : {}),
      });
      setResult({ id: r.server?.id ?? "", name: r.server?.name ?? name, command: r.command, expiresAt: r.expiresAt });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  if (!canManage)
    return (
      <p className="text-[13px] text-muted-foreground">
        Ask a team owner or admin to add a sandbox server.
      </p>
    );

  return (
    <div className="space-y-4">
      <NeedsTailscale />
      {result ? (
        <div className="space-y-3 text-[13px]">
          <p className="text-muted-foreground">Run this on the server as root.</p>
          {expired ? null : (
            <LiveCommand command={result.command}>
              <p className="text-[12px] text-muted-foreground">
                Works once. Expires in {Math.max(1, Math.ceil((result.expiresAt - now) / 60_000))} min.
              </p>
            </LiveCommand>
          )}
          <div className="flex items-center gap-2 rounded-lg border border-border px-3 py-2.5">
            {expired ? (
              <>
                <span className="text-muted-foreground">This command expired.</span>
                <Button size="xs" variant="secondary" className="ml-auto" disabled={busy} onClick={() => void add()}>
                  {busy ? "Getting…" : "Get a new command"}
                </Button>
              </>
            ) : joined?.state === "ready" ? (
              <>
                <CheckCircleIcon size={16} weight="fill" className="text-success" />
                <span>
                  <span className="font-medium">{joined.name}</span> is ready. {joined.summary}
                </span>
              </>
            ) : joined ? (
              <span className="text-destructive">{joined.error ?? joined.summary ?? "It can't run sandboxes."}</span>
            ) : (
              <>
                <CircleNotchIcon size={16} className="animate-spin text-muted-foreground" />
                <span className="text-muted-foreground">Waiting for {result.name} to join…</span>
              </>
            )}
          </div>
          {joined || expired ? null : <TailnetLockWait name={result.name} />}
          {onDone ? (
            <div className="flex justify-end">
              <Button size="sm" variant="secondary" onClick={onDone}>
                {joined ? "Done" : "Close"}
              </Button>
            </div>
          ) : null}
        </div>
      ) : (
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            void add();
          }}
        >
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Name" error={nameError || undefined}>
              {(p) => (
                <Input {...p} mono value={name} onChange={(e) => setName(e.target.value.toLowerCase())} />
              )}
            </Field>
            <Field
              label="Edge domain"
              optional
              hint="Serves sandbox ports. Point its wildcard at the server."
              error={edgeError || undefined}
            >
              {(p) => (
                <Input
                  {...p}
                  mono
                  placeholder="sbx.example.com"
                  value={edge}
                  onChange={(e) => setEdge(e.target.value.toLowerCase())}
                />
              )}
            </Field>
          </div>
          {error ? (
            <Callout tone="danger">
              {error.startsWith(TAILSCALE) ? (
                <>
                  Set up{" "}
                  <AppLink href="/integrations#tailscale" className="font-medium underline">
                    Tailscale
                  </AppLink>
                  {error.slice(TAILSCALE.length)}
                </>
              ) : (
                error
              )}
            </Callout>
          ) : null}
          <Button type="submit" size="sm" disabled={busy || !!nameError || !!edgeError || !s.tailscale}>
            {busy ? "Preparing…" : "Get the command"}
          </Button>
        </form>
      )}
    </div>
  );
}

export function useSandboxes(teamId: string) {
  return useQuery(`sandboxes:${teamId}`, () => sandboxApi.list(teamId, "all"), {
    refreshMs: 4000,
  });
}

/** Pause, resume or delete, with a toast either way. */
export async function act(what: "pause" | "resume" | "remove", x: Sandbox) {
  try {
    await sandboxApi[what](x.id);
    toast({
      title:
        what === "pause"
          ? `Paused ${x.name}`
          : what === "resume"
            ? `Resumed ${x.name}`
            : `Deleting ${x.name}`,
    });
    return true;
  } catch (e) {
    toast({ tone: "error", title: (e as Error).message });
    return false;
  }
}
