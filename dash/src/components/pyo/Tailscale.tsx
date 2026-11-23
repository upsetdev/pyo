import { CheckCircleIcon, ClockIcon, WarningCircleIcon, XCircleIcon } from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useState, useSyncExternalStore, type ReactNode } from "react";
import { siTailscale } from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import { useHosted } from "~/components/pyo/Managed";
import {
  Busy,
  ConfirmNote,
  DialogSection,
  ExternalLink,
  IntegrationDialog,
  Steps,
  Summary,
  TextButton,
} from "~/components/pyo/IntegrationDialog";
import { AppLink } from "~/components/ui/AppLink";
import { Badge } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Field, Input, SecretInput, Switch } from "~/components/ui/Field";
import { Command } from "~/components/pyo/AgentInstall";
import { Callout } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { site } from "~/config/site";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import { api, clustersIn, usePyo, type PyoState } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { TailscaleCheck, TailscaleSetupOutcome, TailscaleSetupPlan, TeamTailscale } from "~/lib/pyo/types";
import { plural } from "~/lib/pyo/format";

/** Where a flow that needed Tailscale goes back to once it's saved. */
const BACK = "pyo.tailscaleBack";

/** The current team's Tailscale, when it's set up. */
export function teamTailscale(s: PyoState): TeamTailscale | null {
  return s.teams.find((t) => t.id === s.currentTeamId)?.tailscale ?? null;
}

/** The current team has the cluster Pyo runs on (self-hosted): Pyo's own names follow its tailnet. */
function holdsHost(s: PyoState): boolean {
  return clustersIn(s).some((c) => c.sameCluster);
}

const DOCS = `${site.docsUrl}/tailscale`;
const KEYS = "https://login.tailscale.com/admin/settings/keys";
const CONSOLE = "https://login.tailscale.com/admin/settings/trust-credentials";

/** After saving: back to the flow that needed Tailscale, if one did. */
function goBack(navigate: ReturnType<typeof useNavigate>): boolean {
  let back: string | null = null;
  try {
    back = sessionStorage.getItem(BACK);
    sessionStorage.removeItem(BACK);
  } catch {
    // Nowhere to go back to.
  }
  if (back) void navigate({ to: back as string });
  return !!back;
}

/** Comes back here once Tailscale is saved. */
function comeBack() {
  try {
    sessionStorage.setItem(BACK, window.location.pathname);
  } catch {
    // Private mode: no way back, the card stays.
  }
}

// ── A token pasted before the team has a cluster ────────────────────────────
// Held in this tab's memory only: never sent to Pyo or kept anywhere. Once
// a cluster of the team connects, the dialog reviews the setup with it. A
// closed tab drops it (the token expires within a day anyway).

let held: { teamId: string; token: string } | null = null;
const heldListeners = new Set<() => void>();
function setHeld(v: typeof held) {
  held = v;
  heldListeners.forEach((l) => l());
}
function useHeldToken(teamId: string): string | null {
  const v = useSyncExternalStore(
    (l) => {
      heldListeners.add(l);
      return () => heldListeners.delete(l);
    },
    () => held,
  );
  return v?.teamId === teamId ? v.token : null;
}

/**
 * A notice while a pasted token waits for the team's first cluster: once
 * one connects, it opens the review. Fixed in a corner, so nothing moves.
 */
export function HeldTailscale() {
  const s = usePyo();
  const token = useHeldToken(s.currentTeamId);
  const connected = clustersIn(s).some((c) => c.status === "connected" || c.status === "degraded");
  if (!token || !connected || teamTailscale(s)) return null;
  if (typeof window !== "undefined" && window.location.pathname.endsWith("/integrations")) return null;
  return (
    <div className="fixed right-4 bottom-4 z-40 max-w-xs rounded-xl border border-border bg-background p-3 shadow-lg">
      <p className="text-[12.5px]">Your cluster is connected. Review the Tailscale setup.</p>
      <div className="mt-2 flex justify-end gap-2">
        <Button size="xs" variant="ghost" onClick={() => setHeld(null)}>
          Drop it
        </Button>
        <ButtonLink href="/integrations#tailscale" size="xs">
          Review
        </ButtonLink>
      </div>
    </div>
  );
}

/**
 * Shown in connect and add-server flows until Tailscale is set up: servers
 * join over the tailnet. The team's Tailscale, or Pyo's when self-hosted
 * (a hosted Pyo's new clusters connect over the internet). Links to the
 * card and comes back after saving. `clusterId`: the cluster a server joins.
 */
export function NeedsTailscale({ clusterId }: { clusterId?: string }) {
  const s = usePyo();
  const hosted = useHosted();
  const teamAdmin = useTeamAdmin();
  if (hosted === undefined) return null;
  const own = clusterId ? s.clusters.find((c) => c.id === clusterId)?.tailscale : null;
  if (own || teamTailscale(s) || (!hosted && s.tailscale) || (hosted && !clusterId)) return null;
  const can = hosted ? teamAdmin : teamAdmin || s.user.isAdmin;
  return (
    <Callout tone="warning">
      Servers join over your tailnet. Set up Tailscale first.{" "}
      {can ? (
        <ButtonLink href="/integrations#tailscale" size="xs" variant="secondary" onClick={comeBack}>
          Set up Tailscale
        </ButtonLink>
      ) : hosted ? (
        "Ask a team admin."
      ) : (
        "Ask an installation admin."
      )}
    </Callout>
  );
}

/** A link to Integrations › Tailscale that comes back here once it's saved. */
export function SetUpTailscaleLink(_: { clusterId?: string }) {
  return (
    <AppLink
      href="/integrations#tailscale"
      className="font-medium text-primary hover:text-primary/80"
      onClick={comeBack}
    >
      Set up Tailscale
    </AppLink>
  );
}

/** Where a cluster stands with the team's tailnet, as a badge. */
export function TailnetBadge({ clusterId }: { clusterId: string }) {
  const s = usePyo();
  const ts = teamTailscale(s);
  const row = ts?.clusters.find((c) => c.id === clusterId);
  if (!ts || !row) return <Badge>Not set up</Badge>;
  return stateBadge(ts, row);
}

function stateBadge(ts: TeamTailscale, row: TeamTailscale["clusters"][number]) {
  if (row.state === "elsewhere") return <Badge tone="warning">On another tailnet</Badge>;
  if (row.state === "waiting") return <Badge tone="danger">Set up again</Badge>;
  if (row.state !== "holder" && !ts.holderConnected)
    return <Badge tone="progress">Waiting for {ts.holderName}</Badge>;
  if (row.onTailnet > 0) return <Badge tone="success">On your tailnet</Badge>;
  return <Badge>Not on it yet</Badge>;
}

function Checks({ checks }: { checks: TailscaleCheck[] }) {
  return (
    <ul className="space-y-1 text-[12.5px]" aria-live="polite">
      {checks.map((c) => (
        <li key={c.name} className="flex items-start gap-1.5">
          {c.ok ? (
            <CheckCircleIcon size={14} weight="fill" className="mt-0.5 shrink-0 text-success" />
          ) : (
            <XCircleIcon size={14} weight="fill" className="mt-0.5 shrink-0 text-destructive" />
          )}
          <span className={c.ok ? "text-muted-foreground" : ""}>{c.message}</span>
        </li>
      ))}
    </ul>
  );
}

/** A list of short lines, each with its mark. */
function Lines({ items, tone }: { items: string[]; tone: "done" | "todo" | "plan" }) {
  if (!items.length) return null;
  return (
    <ul className="space-y-1 text-[12.5px]">
      {items.map((t) => (
        <li key={t} className="flex items-start gap-1.5">
          {tone === "todo" ? (
            <WarningCircleIcon size={14} weight="fill" className="mt-0.5 shrink-0 text-warning" />
          ) : tone === "done" ? (
            <CheckCircleIcon size={14} weight="fill" className="mt-0.5 shrink-0 text-success" />
          ) : (
            <span className="mt-[8px] size-1 shrink-0 rounded-full bg-foreground/60" />
          )}
          <span className={tone === "done" ? "text-muted-foreground" : ""}>{t}</span>
        </li>
      ))}
    </ul>
  );
}

function Diff({ lines }: { lines: string[] }) {
  if (!lines.length) return null;
  return (
    <pre className="max-h-56 overflow-auto rounded-lg border border-border bg-muted/40 px-3 py-2 font-mono text-[11.5px] leading-5">
      {lines.map((l, i) => (
        <div key={i} className={l.startsWith("+") ? "text-success" : "text-destructive"}>
          {l}
        </div>
      ))}
    </pre>
  );
}

/** What setting up with a token did. */
export function SetupOutcome({ outcome }: { outcome: TailscaleSetupOutcome }) {
  return (
    <div className="space-y-2">
      {outcome.changed.length ? (
        <Lines items={outcome.changed} tone="done" />
      ) : outcome.ok ? (
        <p className="text-[12.5px] text-muted-foreground">Nothing changed on your tailnet.</p>
      ) : null}
      {outcome.tokenDeleted ? <Lines items={["Deleted the token"]} tone="done" /> : null}
      {outcome.error ? <p className="text-[12.5px] text-destructive">{outcome.error}</p> : null}
      <Lines items={outcome.todo} tone="todo" />
      {outcome.checks.some((c) => !c.ok) ? <Checks checks={outcome.checks} /> : null}
    </div>
  );
}

function NameRow({ kind, label }: { kind: "dashboard" | "registry" | "sandboxes"; label: string }) {
  const ts = usePyo().tailscale;
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState("");
  const [error, setError] = useState("");
  if (!ts?.suffix) return null;
  const current = { dashboard: ts.dashboardName, registry: ts.registryName, sandboxes: ts.sandboxesName }[kind];
  const problem = { dashboard: ts.dashboardError, registry: ts.registryError, sandboxes: ts.sandboxesError }[kind];
  const ready = { dashboard: ts.dashboard, registry: ts.registry, sandboxes: ts.sandboxes }[kind];
  if (!current) return null;
  return (
    <li className="space-y-2 px-3 py-2.5">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <div className="min-w-0 flex-1">
          <p className="text-[12.5px] font-medium">{label}</p>
          <p className="break-all font-mono text-[11.5px] text-muted-foreground">
            {current}.{ts.suffix}
          </p>
        </div>
        {problem ? (
          <Badge tone="danger">{problem}</Badge>
        ) : ready ? (
          <Badge tone="success">Ready</Badge>
        ) : (
          <Badge tone="progress">
            {ts.hostMove?.state === "moving"
              ? "Moving server"
              : ts.onTailnet
                ? "Getting certificate"
                : "Joining tailnet"}
          </Badge>
        )}
        {editing ? null : (
          <Button
            size="xs"
            variant="ghost"
            onClick={() => {
              setName(current);
              setError("");
              setEditing(true);
            }}
          >
            Rename
          </Button>
        )}
      </div>
      {editing ? (
        <form
          className="flex flex-wrap items-start gap-2"
          onSubmit={async (e) => {
            e.preventDefault();
            const r = await api.renameTailscaleName(kind, name);
            if ("error" in r) return setError(r.error);
            setError("");
            setEditing(false);
          }}
        >
          <Field label={`${label} name`} error={error || undefined} className="min-w-40 flex-1">
            {(p) => <Input {...p} mono value={name} onChange={(e) => setName(e.target.value)} className="h-8" />}
          </Field>
          <div className="flex gap-1.5 pt-[22px]">
            <Button size="xs" variant="ghost" onClick={() => setEditing(false)}>
              Cancel
            </Button>
            <Button type="submit" size="xs" disabled={!name.trim()}>
              Rename
            </Button>
          </div>
        </form>
      ) : null}
    </li>
  );
}

function PublicDashboard() {
  const s = usePyo();
  const pd = s.publicDashboard;
  if (!pd) return null;
  const onTailnet = !!s.tailscale?.dashboard;
  const locked = pd.possible && pd.open && !onTailnet;
  return (
    <div className="rounded-lg border border-border px-3 py-2.5">
      <Switch
        checked={pd.possible && pd.open}
        disabled={!pd.possible || locked}
        onChange={(on) => void api.setPublicDashboard(on)}
        label="Public dashboard"
        description={
          !pd.possible
            ? "Off. Ports 80 and 443 don't reach this server."
            : pd.open && !pd.host && pd.blocked
              ? "Allow ports 80 and 443 in your cloud firewall."
              : locked
                ? "Stays on until Pyo is on your tailnet."
                : pd.open
                  ? (pd.host ?? "Starting")
                  : undefined
        }
      />
    </div>
  );
}

/** Pyo's own server moving k3s onto the tailnet, while it happens or if it failed. */
function HostMove() {
  const move = usePyo().tailscale?.hostMove;
  if (!move) return null;
  return move.state === "moving" ? (
    <Callout tone="info">Moving this server to your tailnet…</Callout>
  ) : (
    <Callout tone="danger">
      This server couldn't move to your tailnet. {move.error ?? ""} Pyo tries again tomorrow.
    </Callout>
  );
}

const ICON = <BrandMark icon={siTailscale} size={15} />;
const ABOUT = "Every cluster of this team joins your tailnet.";
const STEPS = ["Token", "Review", "Done"];
/** Tailscale's plans include 50 tagged devices: a hint from 40 on. */
const DEVICES_HINT = 40;

/** A team's cluster on another tailnet: Move for one server, steps for more. */
function MoveRow({ clusterId, name, suffix }: { clusterId: string; name: string; suffix: string | null }) {
  const s = usePyo();
  const servers = s.servers.filter((x) => x.clusterId === clusterId);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [moved, setMoved] = useState<{ server: string; command: string }[] | null>(null);
  const to = teamTailscale(s)?.suffix ?? "your tailnet";
  if (servers.length > 1)
    return (
      <p className="text-[12px] text-muted-foreground">
        Move its servers by hand. <ExternalLink href={`${DOCS}#move-a-cluster-by-hand`}>Steps in the docs</ExternalLink>
      </p>
    );
  const move = async () => {
    setBusy(true);
    setError("");
    const r = await api.moveClusterToTailnet(clusterId);
    setBusy(false);
    setConfirming(false);
    if ("error" in r) return setError(r.error);
    setMoved(r.data.commands);
  };
  if (moved)
    return (
      <div className="space-y-2">
        {moved.map((m) => (
          <div key={m.server} className="space-y-1.5">
            <p className="text-[12.5px] font-medium">First, run this on {m.server}</p>
            <Command value={m.command} />
          </div>
        ))}
      </div>
    );
  return (
    <div className="space-y-2">
      {confirming ? (
        <ConfirmNote
          title={`Move ${name} to your tailnet?`}
          confirmLabel="Move"
          busy={busy}
          onCancel={() => setConfirming(false)}
          onConfirm={() => void move()}
        >
          Its servers join {to}. Its tailnet domains move to {to}.
          {suffix ? ` Pyo's names on ${suffix} are deleted.` : ""}
        </ConfirmNote>
      ) : (
        <Button size="xs" variant="secondary" onClick={() => setConfirming(true)}>
          Move
        </Button>
      )}
      {error ? <p role="alert" className="text-[12px] text-destructive">{error}</p> : null}
    </div>
  );
}

/** The team's clusters, each with where it stands. */
function ClusterList({ ts, admin }: { ts: TeamTailscale; admin: boolean }) {
  const s = usePyo();
  if (!ts.clusters.length) return null;
  return (
    <DialogSection title="Clusters">
      <ul className="list-rows rounded-lg border border-border">
        {ts.clusters.map((row) => {
          const c = s.clusters.find((x) => x.id === row.id);
          if (!c) return null;
          return (
            <li key={row.id} className="space-y-2 px-3 py-2.5">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-[12.5px] font-medium">{c.name}</span>
                {stateBadge(ts, row)}
              </div>
              {row.state === "elsewhere" && admin ? (
                <MoveRow clusterId={row.id} name={c.name} suffix={row.suffix} />
              ) : null}
            </li>
          );
        })}
      </ul>
    </DialogSection>
  );
}

/** Left to do in the Tailscale console: old OAuth clients, servers of removed clusters. */
function ConsoleTodo({ ts }: { ts: TeamTailscale }) {
  const clients = ts.oldClients.map(
    (c) =>
      (c.suffix && c.suffix !== ts.suffix
        ? `Delete the OAuth client ${c.clientId} in the console of ${c.suffix}.`
        : `Delete the OAuth client ${c.clientId}.`) + (c.error ? ` ${c.error}` : ""),
  );
  const servers = ts.leftovers.map(
    (l) => `Remove ${l.servers.join(", ")} of the removed cluster ${l.cluster}${l.suffix && l.suffix !== ts.suffix ? ` from ${l.suffix}` : ""}.`,
  );
  if (!clients.length && !servers.length) return null;
  return (
    <DialogSection title="In the Tailscale console">
      <Lines items={[...clients, ...servers]} tone="todo" />
      <ExternalLink href={CONSOLE}>Open the Tailscale console</ExternalLink>
    </DialogSection>
  );
}

/** What stops when the team's Tailscale is disconnected. */
function disconnectLines(ts: TeamTailscale, host: boolean): string[] {
  return [
    "Every cluster of this team leaves your tailnet in Pyo.",
    "New servers can't join.",
    "Tailnet domains stop working.",
    ...(host ? ["Pyo's tailnet address stops working. The public dashboard opens again."] : []),
    ts.clientId
      ? `Servers stay on your tailnet. Delete the OAuth client ${ts.clientId} in the Tailscale console.`
      : "Servers stay on your tailnet.",
  ];
}

/**
 * Integrations › Tailscale, for the team (admins set it up, everyone sees
 * it). Set up with one short-lived API access token: paste, check, review
 * what Pyo will change, set up. Or with an OAuth client made by hand. One
 * cluster of the team keeps the client and acts for the others. Once set
 * up: the tailnet, where it's kept, the clusters, what's left to do in the
 * console, Pyo's names (self-hosted), Test and Disconnect.
 */
export function TailscaleDialog({ onClose }: { onClose: () => void }) {
  const s = usePyo();
  const teamAdmin = useTeamAdmin();
  const ts = teamTailscale(s);
  const host = holdsHost(s);
  // The team that holds Pyo's own cluster: its installation admins too.
  const admin = host ? teamAdmin && !!s.user.isAdmin : teamAdmin;
  const clusters = clustersIn(s);
  const connected = clusters.some((c) => c.status === "connected" || c.status === "degraded");
  const navigate = useNavigate();
  const [view, setView] = useState<"manage" | "token" | "client">(ts ? "manage" : "token");

  // With a token, or one held in this tab until a cluster connects.
  const heldToken = useHeldToken(s.currentTeamId);
  const [token, setToken] = useState(heldToken ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [plan, setPlan] = useState<TailscaleSetupPlan | null>(null);
  const [stopped, setStopped] = useState<TailscaleSetupOutcome | null>(null);
  const [done, setDone] = useState<TailscaleSetupOutcome | null>(null);

  // With an OAuth client.
  const [clientId, setClientId] = useState(ts?.clientId ?? "");
  const [secret, setSecret] = useState("");
  const [clientChecks, setClientChecks] = useState<TailscaleCheck[]>([]);

  // Set up.
  const [checks, setChecks] = useState<TailscaleCheck[]>([]);
  const [testing, setTesting] = useState(false);
  const [confirming, setConfirming] = useState(false);

  const setup = !ts || view !== "manage";
  const method = view === "client" ? "client" : "token";
  const toManage = () => {
    setView("manage");
    setPlan(null);
    setError("");
    setDone(null);
  };
  const cancel = ts ? toManage : onClose;

  // `note` shows on the review, as when the tailnet changed since the last one.
  const check = async (note = "", pasted = token) => {
    if (!pasted.trim() || busy) return;
    setBusy(true);
    setError("");
    setStopped(null);
    const r = await api.planTailscaleSetup(pasted);
    setBusy(false);
    if ("error" in r) return setError(r.error);
    setPlan(r.data.plan);
    setError(note);
  };
  // A held token, once a cluster is there: straight to the review.
  useEffect(() => {
    if (heldToken && connected && !ts && !plan && !busy) {
      setHeld(null);
      setToken(heldToken);
      void check("", heldToken);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [heldToken, connected]);
  const apply = async () => {
    if (!plan) return;
    setBusy(true);
    setError("");
    const r = await api.setUpTailscale(token, plan.version);
    setBusy(false);
    if ("error" in r) return setError(r.error);
    if (r.data.reviewAgain) return void check(r.data.error ?? "");
    if (!r.data.ok) {
      // Stopped: what changed stays listed. The setup is over, so the token is done with.
      setToken("");
      setPlan(null);
      return setStopped(r.data);
    }
    setToken("");
    setPlan(null);
    toast({ title: "Tailscale set up" });
    if (!goBack(navigate)) setDone(r.data);
  };
  const saveClient = async () => {
    if (busy || !clientId.trim() || !secret.trim()) return;
    setBusy(true);
    setError("");
    const r = await api.saveTailscale(clientId, secret);
    setBusy(false);
    if ("error" in r) return setError(r.error);
    setClientChecks(r.data.checks);
    if (!r.data.ok) return;
    setSecret("");
    toast({ title: "Tailscale saved" });
    if (!goBack(navigate)) toManage();
  };
  // The saved client's secret, pasted again by hand.
  const replaceSecret = () => {
    setClientId(ts?.clientId ?? "");
    setSecret("");
    setClientChecks([]);
    setError("");
    setView("client");
  };
  const test = async () => {
    setTesting(true);
    const r = await api.testTailscale();
    setTesting(false);
    if ("error" in r) toast({ tone: "error", title: r.error });
    else setChecks(r.data.checks);
  };

  const shell = (p: { body: ReactNode; footer?: ReactNode; onSubmit?: () => void }) => (
    <IntegrationDialog
      icon={ICON}
      name="Tailscale"
      description={ABOUT}
      docs="tailscale"
      onClose={onClose}
      onSubmit={p.onSubmit}
      footer={p.footer}
    >
      {p.body}
    </IntegrationDialog>
  );
  const done_ = <Button size="sm" onClick={onClose}>Done</Button>;

  if (!admin)
    return shell({
      body: (
        <div className="space-y-3">
          <Summary
            items={
              ts
                ? [
                    ["Status", <Badge tone="success">Set up</Badge>],
                    ["Tailnet", <span className="font-mono">{ts.suffix ?? "Not seen yet"}</span>],
                    ["Kept on", ts.holderName ?? "None"],
                  ]
                : [["Status", <Badge>Not set up</Badge>]]
            }
          />
          {ts ? <ClusterList ts={ts} admin={false} /> : (
            <p className="text-[12.5px] text-muted-foreground">
              {host ? "An installation admin can set it up." : "A team admin can set it up."}
            </p>
          )}
          {host ? <HostMove /> : null}
        </div>
      ),
      footer: done_,
    });

  // Done: what setting up did.
  if (done)
    return shell({
      body: (
        <div className="space-y-4">
          <Steps steps={STEPS} at={2} />
          <StatusNote tone="success" icon={<CheckCircleIcon size={15} weight="fill" />} title="Tailscale is set up" />
          <SetupOutcome outcome={done} />
        </div>
      ),
      footer: <Button size="sm" onClick={toManage}>Done</Button>,
    });

  // Review: what Pyo will change.
  if (setup && method === "token" && plan)
    return shell({
      body: (
        <div className="space-y-4">
          <Steps steps={STEPS} at={1} />
          {plan.blocked ? (
            <Callout tone="warning">{plan.blocked}</Callout>
          ) : plan.changes.length ? (
            <DialogSection title="Pyo will">
              <Lines items={plan.changes} tone="plan" />
            </DialogSection>
          ) : (
            <p className="text-[12.5px]">Your tailnet is already set up.</p>
          )}
          <Diff lines={plan.diff} />
          <Lines items={plan.done} tone="done" />
          {plan.blocked ? null : (
            <p className="text-[12.5px] text-muted-foreground">Pyo deletes the token when it's done.</p>
          )}
          {error ? <p role="alert" className="text-[12.5px] text-destructive">{error}</p> : null}
        </div>
      ),
      footer: (
        <>
          <Button variant="ghost" size="sm" disabled={busy} onClick={() => { setPlan(null); setError(""); }}>
            Back
          </Button>
          {plan.blocked ? (
            <Button size="sm" disabled={busy} onClick={() => void check()}>
              {busy ? <Busy>Checking…</Busy> : "Check again"}
            </Button>
          ) : (
            <Button size="sm" disabled={busy} onClick={() => void apply()}>
              {busy ? <Busy>Setting up…</Busy> : plan.changes.length ? "Set up" : "Finish"}
            </Button>
          )}
        </>
      ),
    });

  // No cluster to keep it yet: the paste waits in this tab.
  if (setup && !connected && !ts) {
    const waiting = heldToken !== null;
    return shell({
      onSubmit: () => {
        if (token.trim()) setHeld({ teamId: s.currentTeamId, token: token.trim() });
      },
      body: (
        <div className="space-y-4">
          <Steps steps={STEPS} at={0} />
          <p className="text-[12.5px]">Connect a cluster. Tailscale is set up on it as soon as it connects.</p>
          {waiting ? (
            <StatusNote tone="progress" icon={<ClockIcon size={15} weight="fill" />} title="Waiting for a cluster">
              <p className="text-[12.5px] text-muted-foreground">The token stays in this tab only.</p>
            </StatusNote>
          ) : (
            <Field label="API access token" hint="Make one that expires in 1 day. It stays in this tab only.">
              {(p) => (
                <SecretInput
                  {...p}
                  value={token}
                  onChange={(e) => setToken(e.target.value)}
                  placeholder="tskey-api-…"
                  autoComplete="off"
                />
              )}
            </Field>
          )}
          <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-[12px]">
            <ExternalLink href={KEYS}>Open Keys in Tailscale</ExternalLink>
            <AppLink href="/clusters/new" className="font-medium text-primary hover:text-primary/80">
              Connect a cluster
            </AppLink>
          </div>
        </div>
      ),
      footer: waiting ? (
        <>
          <Button variant="ghost" size="sm" onClick={() => { setHeld(null); setToken(""); }}>
            Drop the token
          </Button>
          {done_}
        </>
      ) : (
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!token.trim()}>
            Keep for now
          </Button>
        </>
      ),
    });
  }

  // Token: paste and check.
  if (setup && method === "token")
    return shell({
      onSubmit: () => void check(),
      body: (
        <div className="space-y-4">
          <Steps steps={STEPS} at={0} />
          {stopped ? (
            <StatusNote
              tone="danger"
              icon={<XCircleIcon size={15} weight="fill" />}
              title={stopped.changed.length ? "Setup stopped partway" : "Setup stopped"}
            >
              <SetupOutcome outcome={stopped} />
            </StatusNote>
          ) : null}
          <div className="space-y-2">
            <Field label="API access token" hint="Make one that expires in 1 day." error={error || undefined}>
              {(p) => (
                <SecretInput
                  {...p}
                  value={token}
                  onChange={(e) => setToken(e.target.value)}
                  placeholder="tskey-api-…"
                  autoComplete="off"
                />
              )}
            </Field>
            <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-[12px]">
              <ExternalLink href={KEYS}>Open Keys in Tailscale</ExternalLink>
              <TextButton onClick={() => { setView("client"); setError(""); }}>
                Set up with an OAuth client instead
              </TextButton>
            </div>
          </div>
          {ts || !host ? null : <PublicDashboard />}
        </div>
      ),
      footer: (
        <>
          <Button variant="ghost" size="sm" onClick={cancel}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={busy || !token.trim()}>
            {busy ? <Busy>Checking…</Busy> : "Check"}
          </Button>
        </>
      ),
    });

  // OAuth client, made by hand.
  if (setup)
    return shell({
      onSubmit: () => void saveClient(),
      body: (
        <div className="space-y-4">
          <p className="text-[12.5px] text-muted-foreground">
            An OAuth client with Devices › Core and Auth Keys (both with tag:pyo) and Services.
          </p>
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Client ID">
              {(p) => (
                <Input {...p} mono value={clientId} onChange={(e) => setClientId(e.target.value)} autoComplete="off" />
              )}
            </Field>
            <Field label="Client secret">
              {(p) => (
                <SecretInput
                  {...p}
                  value={secret}
                  onChange={(e) => setSecret(e.target.value)}
                  placeholder="tskey-client-…"
                  autoComplete="off"
                />
              )}
            </Field>
          </div>
          {error ? <p role="alert" className="text-[12.5px] text-destructive">{error}</p> : null}
          {clientChecks.length ? <Checks checks={clientChecks} /> : null}
          <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-[12px]">
            <ExternalLink href={`${DOCS}#with-an-oauth-client`}>How to make one</ExternalLink>
            <TextButton onClick={() => { setView("token"); setError(""); }}>
              Set up with an access token instead
            </TextButton>
          </div>
          {ts || !host ? null : <PublicDashboard />}
        </div>
      ),
      footer: (
        <>
          <Button variant="ghost" size="sm" onClick={cancel}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={busy || !clientId.trim() || !secret.trim()}>
            {busy ? <Busy>Checking…</Busy> : "Save"}
          </Button>
        </>
      ),
    });

  // Set up: the tailnet, where it's kept, the clusters.
  const installation = s.tailscale;
  return shell({
    body: (
      <div className="space-y-4">
        {host ? <HostMove /> : null}
        {ts.problem === "holder_gone" ? (
          <Callout tone="danger">Set up again. The cluster that kept it is gone.</Callout>
        ) : ts.problem === "client_refused" ? (
          <Callout tone="danger">Tailscale doesn't take the client. Set up again.</Callout>
        ) : null}
        <Summary
          items={[
            ["Tailnet", <span className="truncate font-mono">{ts.suffix ?? "Not seen yet"}</span>],
            ["Kept on", ts.holderName ?? "None"],
            ["Servers", plural(ts.devices, "Pyo server")],
            ["OAuth client", <span className="truncate font-mono">{ts.clientId}</span>],
            [
              "Secret",
              <>
                <span className="font-mono">{ts.hint}</span>
                <Button size="xs" variant="ghost" className="-my-1 -mr-2" onClick={replaceSecret}>
                  Replace
                </Button>
              </>,
            ],
          ]}
        />
        {ts.devices >= DEVICES_HINT ? (
          <p className="text-[12px] text-muted-foreground">Tailscale plans include 50 tagged devices.</p>
        ) : null}
        {ts.problem ? (
          <Button size="xs" variant="secondary" onClick={() => setView("token")}>
            Set up again
          </Button>
        ) : null}
        {checks.length ? <Checks checks={checks} /> : null}
        <ClusterList ts={ts} admin />
        <ConsoleTodo ts={ts} />
        {host && installation?.suffix && (installation.dashboardName || installation.registryName || installation.sandboxesName) ? (
          <DialogSection title="Names on your tailnet">
            <ul className="list-rows rounded-lg border border-border">
              <NameRow kind="dashboard" label="Dashboard" />
              <NameRow kind="registry" label="Registry" />
              <NameRow kind="sandboxes" label="Sandboxes" />
            </ul>
          </DialogSection>
        ) : null}
        {host ? <PublicDashboard /> : null}
        {confirming ? (
          <ConfirmNote
            title="Disconnect Tailscale?"
            confirmLabel="Disconnect"
            onCancel={() => setConfirming(false)}
            onConfirm={() => {
              setConfirming(false);
              void api.removeTailscale();
            }}
          >
            <Lines items={disconnectLines(ts, host)} tone="plan" />
          </ConfirmNote>
        ) : null}
      </div>
    ),
    footer: confirming ? null : (
      <>
        <Button variant="ghost" size="sm" className="mr-auto text-destructive! hover:bg-destructive/10" onClick={() => setConfirming(true)}>
          Disconnect
        </Button>
        <Button variant="secondary" size="sm" disabled={testing || !ts.holderId} onClick={() => void test()}>
          {testing ? <Busy>Testing…</Busy> : "Test"}
        </Button>
        {done_}
      </>
    ),
  });
}
