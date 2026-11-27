import { CheckCircleIcon, CircleNotchIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { useHosted } from "~/components/pyo/Managed";
import { NeedsTailscale, SetUpTailscaleLink, teamTailscale } from "~/components/pyo/Tailscale";
import { TailnetLockWait } from "~/components/pyo/TailnetLock";
import { AppLink } from "~/components/ui/AppLink";
import { LiveCommand } from "~/components/pyo/LiveCommand";
import { Badge, Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { Callout } from "~/components/ui/Panel";
import { Tooltip } from "~/components/ui/Tooltip";
import { DNS_LABEL } from "~/lib/pyo/format";
import { addServerBlock } from "~/lib/pyo/sandboxTemplates";
import {
  OFF_TEAM_TAILNET,
  RUNTIME_LABEL,
  SANDBOX_STATE,
  SERVER_STATE,
  desktopBlocked,
  gettingReady,
  reachable,
  sandboxApi,
  tailnetUrl,
  type JoinCommand,
  type Sandbox,
  type SandboxServer,
  type SandboxState,
} from "~/lib/pyo/sandbox";
import { refresh, roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
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

/** Whether this browser reaches each Pyo Cloud sandbox server at `urls`
 * (asked again every 30 seconds). Missing while it asks. */
function useReach(urls: string[]) {
  const key = [...new Set(urls)].sort().join(" ");
  const [ok, setOk] = useState<Record<string, boolean>>({});
  useEffect(() => {
    if (!key) return;
    let live = true;
    const ask = () => {
      for (const u of key.split(" "))
        void reachable(u).then((v) => live && setOk((o) => (o[u] === v ? o : { ...o, [u]: v })));
    };
    ask();
    const t = setInterval(ask, 30_000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [key]);
  return ok;
}

/**
 * Why each sandbox's desktop can't open, or null when it can. On Pyo Cloud
 * it opens on its server's tailnet name, so only from devices on the team's
 * tailnet: a browser that doesn't reach the server gets that note. `list`:
 * the sandboxes that show a desktop button.
 */
export function useDesktopNotes(list: Sandbox[]) {
  const s = usePyo();
  const serverOf = (x: Sandbox) => s.sandboxServers.find((v) => v.id === x.serverId);
  const ok = useReach(list.flatMap((x) => tailnetUrl(serverOf(x)) ?? []));
  return (x: Sandbox) => {
    const server = serverOf(x);
    const url = tailnetUrl(server);
    return desktopBlocked(server) ?? (url && ok[url] === false ? OFF_TEAM_TAILNET : null);
  };
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

/** A join command on its way to a server: which server, and when its code runs out. */
export type Joining = {
  id: string;
  name: string;
  command: string;
  expiresAt: number;
  /** When the server joined before, to tell a new join from that one. */
  joinedAt: number | null;
};

export function joiningOf(r: JoinCommand, name: string): Joining {
  return {
    id: r.server?.id ?? "",
    name: r.server?.name ?? name,
    command: r.command,
    expiresAt: r.expiresAt,
    joinedAt: r.server?.joinedAt ?? null,
  };
}

/**
 * The server once it joined with `j`'s command, or undefined while it
 * hasn't. On Pyo Cloud a server can be joined already (Reset access), so a
 * join counts once Pyo saw it waiting, or it joined again since.
 */
export function useJoined(j: Joining | null, again: boolean) {
  const s = usePyo();
  const server = j ? s.sandboxServers.find((x) => x.id === j.id) : undefined;
  const [waited, setWaited] = useState(false);
  useEffect(() => setWaited(false), [j?.command]);
  useEffect(() => {
    if (server?.state === "joining") setWaited(true);
  }, [server?.state]);
  if (!j || !server || server.state === "joining") return undefined;
  if (again && !waited && server.joinedAt === j.joinedAt) return undefined;
  return server;
}

/** The command to run on the server, until it joins: works once, with a
 * countdown, then a new one a click away. */
export function JoinCommandView({
  joining,
  joined,
  hint,
  busy,
  onAgain,
  onDone,
}: {
  joining: Joining;
  joined: SandboxServer | undefined;
  hint: string;
  busy: boolean;
  onAgain: () => void;
  onDone?: () => void;
}) {
  // The code works for a while: the countdown ticks, and once it's out the
  // command is dead, so a new one is a click away.
  const now = useNow(joined ? 60_000 : 5_000);
  const expired = !joined && now >= joining.expiresAt;
  return (
    <div className="space-y-3 text-[13px]">
      <p className="text-muted-foreground">{hint}</p>
      {expired ? null : (
        <LiveCommand command={joining.command}>
          <p className="text-[12px] text-muted-foreground">
            Works once. Expires in {Math.max(1, Math.ceil((joining.expiresAt - now) / 60_000))} min.
          </p>
        </LiveCommand>
      )}
      <div className="flex items-center gap-2 rounded-lg border border-border px-3 py-2.5">
        {expired ? (
          <>
            <span className="text-muted-foreground">This command expired.</span>
            <Button size="xs" variant="secondary" className="ml-auto" disabled={busy} onClick={onAgain}>
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
            <span className="text-muted-foreground">Waiting for {joining.name} to join…</span>
          </>
        )}
      </div>
      {joined || expired ? null : <TailnetLockWait name={joining.name} />}
      {onDone ? (
        <div className="flex justify-end">
          <Button size="sm" variant="secondary" onClick={onDone}>
            {joined ? "Done" : "Close"}
          </Button>
        </div>
      ) : null}
    </div>
  );
}

/** Add a sandbox server: your own, with a one-time code in one command.
 * On Pyo Cloud it joins the team's tailnet, then Pyo over HTTPS. */
export function AddSandboxServer({ onDone }: { onDone?: () => void }) {
  const s = usePyo();
  const role = roleIn(s);
  const canManage = role === "owner" || role === "admin";
  const hosted = useHosted();
  const [name, setName] = useState(`sandbox-${sandboxServersIn(s).length + 1}`);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState<Joining | null>(null);
  const [edge, setEdge] = useState("");
  const nameError = DNS_LABEL.test(name) ? "" : "Lowercase letters, digits and dashes.";
  const edgeDomain = edge.trim().replace(/\.$/, "");
  const edgeError = !edgeDomain || EDGE_DOMAIN.test(edgeDomain) ? "" : "A domain, like sbx.example.com.";
  // Every new server joins over HTTPS, self-hosted too: it's joined once
  // Pyo saw it waiting, or it joined again since.
  const joined = useJoined(result, true);

  const add = async () => {
    if (nameError || edgeError || busy) return;
    setBusy(true);
    setError("");
    try {
      const body = { teamId: s.currentTeamId, name, ...(edgeDomain ? { edgeDomain } : {}) };
      const r = hosted ? await sandboxApi.createJoinCode(body) : await sandboxApi.addServer(body);
      setResult(joiningOf(r, name));
      void refresh().catch(() => undefined);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  // Wait for what decides the form: self-hosted or Pyo Cloud.
  if (hosted === undefined) return null;
  const block = addServerBlock({ hosted, pyoTailnet: !!s.tailscale, teamTailscale: !!teamTailscale(s) });

  if (!canManage)
    return (
      <p className="text-[13px] text-muted-foreground">
        Ask a team owner or admin to add a sandbox server.
      </p>
    );

  if (hosted && block === "tailscale")
    return (
      <p className="text-[13px] text-muted-foreground">
        Sandbox servers join your team's tailnet. <SetUpTailscaleLink /> in Integrations first.
      </p>
    );

  return (
    <div className="space-y-4">
      {hosted ? null : <NeedsTailscale />}
      {result ? (
        <JoinCommandView
          joining={result}
          joined={joined}
          hint={hosted ? "Run this on the server." : "Run this on the server as root."}
          busy={busy}
          onAgain={() => void add()}
          onDone={onDone}
        />
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
          {/* The team has Tailscale, so NeedsTailscale above shows nothing: the reason the button is off goes here. */}
          {block === "pyo" ? (
            <Callout tone="warning">
              {s.user.isAdmin
                ? "Pyo's own server isn't on Tailscale yet. Set up Tailscale in the team that runs Pyo."
                : "Pyo's own server isn't on Tailscale yet. Ask an installation admin."}
            </Callout>
          ) : null}
          <Button type="submit" size="sm" disabled={busy || !!nameError || !!edgeError || !!block}>
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
