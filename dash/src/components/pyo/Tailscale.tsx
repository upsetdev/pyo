import { CheckCircleIcon, WarningCircleIcon, XCircleIcon } from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
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
import { Field, Input, SecretInput, Select, Switch } from "~/components/ui/Field";
import { Callout } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { site } from "~/config/site";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import { api, clustersIn, usePyo, type PyoState } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { TailscaleCheck, TailscaleInfo, TailscaleSetupOutcome, TailscaleSetupPlan } from "~/lib/pyo/types";

/** Where a flow that needed Tailscale goes back to once it's saved. */
const BACK = "pyo.tailscaleBack";
/** The cluster that flow needed it on. */
const FOR = "pyo.tailscaleCluster";

/** The cluster Pyo runs on, when it has one (self-hosted). */
function hostOf(s: PyoState): string | undefined {
  return s.tailscale?.clusterId ?? s.clusters.find((c) => c.sameCluster)?.id;
}

/**
 * A cluster's Tailscale as the dialog shows it: the one Pyo runs on with
 * Pyo's names there, any other with its client only. Null when not set up.
 */
function tailnetOf(s: PyoState, clusterId: string | undefined): TailscaleInfo | null {
  if (!clusterId) return null;
  if (s.tailscale && hostOf(s) === clusterId) return s.tailscale;
  const t = s.clusters.find((c) => c.id === clusterId)?.tailscale;
  return t
    ? { suffix: t.suffix, clientId: t.clientId, hint: t.hint, dashboard: null, registry: null, onTailnet: false, hostMove: null }
    : null;
}

const DOCS = `${site.docsUrl}/tailscale`;
const KEYS = "https://login.tailscale.com/admin/settings/keys";

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

/**
 * Shown in connect and add-server flows until Tailscale is set up: servers
 * join over the tailnet. With `clusterId`, the cluster a server joins: its
 * own Tailscale, or Pyo's when self-hosted. Without, a new cluster: Pyo's
 * (a hosted Pyo's clusters connect over the internet). Links to the card
 * and comes back after saving.
 */
export function NeedsTailscale({ clusterId }: { clusterId?: string }) {
  const s = usePyo();
  const hosted = useHosted();
  const teamAdmin = useTeamAdmin();
  if (hosted === undefined) return null;
  const own = clusterId ? s.clusters.find((c) => c.id === clusterId)?.tailscale : null;
  if (own || (!hosted && s.tailscale) || (hosted && !clusterId)) return null;
  const can = clusterId && hosted ? teamAdmin : s.user.isAdmin;
  return (
    <Callout tone="warning">
      Servers join over your tailnet. Set up Tailscale first.{" "}
      {can ? (
        <ButtonLink
          href="/integrations#tailscale"
          size="xs"
          variant="secondary"
          onClick={() => {
            try {
              sessionStorage.setItem(BACK, window.location.pathname);
              if (clusterId && hosted) sessionStorage.setItem(FOR, clusterId);
            } catch {
              // Private mode: no way back, the card stays.
            }
          }}
        >
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

/**
 * A link to Integrations › Tailscale for `clusterId`'s Tailscale, that
 * comes back here once it's saved.
 */
export function SetUpTailscaleLink({ clusterId }: { clusterId: string }) {
  return (
    <AppLink
      href="/integrations#tailscale"
      className="font-medium text-primary hover:text-primary/80"
      onClick={() => {
        try {
          sessionStorage.setItem(BACK, window.location.pathname);
          sessionStorage.setItem(FOR, clusterId);
        } catch {
          // Private mode: no way back, the card stays.
        }
      }}
    >
      Set up Tailscale
    </AppLink>
  );
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
const ABOUT = "For the whole Pyo. Servers join your tailnet with it.";
const STEPS = ["Token", "Review", "Done"];

/**
 * Integrations › Tailscale (admins set it up, everyone sees it). Set up with
 * one short-lived API access token: paste, check, review what Pyo will
 * change, set up. Or with an OAuth client made by hand. Once set up: what's
 * connected, the names on the tailnet, Test and Disconnect.
 */
export function TailscaleDialog({ onClose }: { onClose: () => void }) {
  const s = usePyo();
  const hosted = useHosted();
  const teamAdmin = useTeamAdmin();
  const host = hostOf(s);
  // Each cluster keeps its own client. The ones here: the team's, and the
  // one Pyo runs on for its admins.
  const choices = clustersIn(s).filter((c) => c.id !== host);
  const hostCluster = s.clusters.find((c) => c.id === host);
  if (hostCluster && s.user.isAdmin) choices.unshift(hostCluster);
  const [cid, setCid] = useState<string | undefined>(() => {
    let wanted: string | null = null;
    try {
      wanted = sessionStorage.getItem(FOR);
      sessionStorage.removeItem(FOR);
    } catch {
      // Picked below.
    }
    return choices.find((c) => c.id === wanted)?.id ?? choices[0]?.id ?? host;
  });
  const ts = tailnetOf(s, cid);
  const isHost = !!cid && cid === host;
  const admin = isHost ? !!s.user.isAdmin : teamAdmin;
  const navigate = useNavigate();
  const [view, setView] = useState<"manage" | "token" | "client">(ts ? "manage" : "token");

  // With a token.
  const [token, setToken] = useState("");
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
  const check = async (note = "") => {
    if (!token.trim() || busy) return;
    setBusy(true);
    setError("");
    setStopped(null);
    const r = await api.planTailscaleSetup(token, cid);
    setBusy(false);
    if ("error" in r) return setError(r.error);
    setPlan(r.data.plan);
    setError(note);
  };
  const apply = async () => {
    if (!plan) return;
    setBusy(true);
    setError("");
    const r = await api.setUpTailscale(token, plan.version, cid);
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
    const r = await api.saveTailscale(clientId, secret, cid);
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
    const r = await api.testTailscale(cid);
    setTesting(false);
    if ("error" in r) toast({ tone: "error", title: r.error });
    else setChecks(r.data.checks);
  };

  // The cluster, when there's a choice: each keeps its own client.
  const picker =
    choices.length > 1 || (hosted && choices.length) ? (
      <Field label="Cluster">
        {(p) => (
          <Select
            {...p}
            value={cid ?? ""}
            onChange={(e) => {
              setCid(e.target.value);
              setView(tailnetOf(s, e.target.value) ? "manage" : "token");
              setPlan(null);
              setError("");
              setDone(null);
              setChecks([]);
              setClientId(tailnetOf(s, e.target.value)?.clientId ?? "");
            }}
          >
            {choices.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </Select>
        )}
      </Field>
    ) : null;
  const shell = (p: {
    body: ReactNode;
    footer?: ReactNode;
    onSubmit?: () => void;
  }) => (
    <IntegrationDialog
      icon={ICON}
      name="Tailscale"
      description={ABOUT}
      docs="tailscale"
      onClose={onClose}
      onSubmit={p.onSubmit}
      footer={p.footer}
    >
      <div className="space-y-4">
        {done || plan ? null : picker}
        {p.body}
      </div>
    </IntegrationDialog>
  );

  if (!cid)
    return shell({
      body: <p className="text-[12.5px] text-muted-foreground">Connect a cluster first.</p>,
      footer: <Button size="sm" onClick={onClose}>Done</Button>,
    });

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
                  ]
                : [["Status", <Badge>Not set up</Badge>]]
            }
          />
          {ts ? null : (
            <p className="text-[12.5px] text-muted-foreground">
              {isHost ? "An installation admin can set it up." : "A team admin can set it up."}
            </p>
          )}
          {isHost ? <HostMove /> : null}
        </div>
      ),
      footer: <Button size="sm" onClick={onClose}>Done</Button>,
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
            <Field
              label="API access token"
              hint="Make one that expires in 1 day."
              error={error || undefined}
            >
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
          {ts || !isHost ? null : <PublicDashboard />}
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
          {ts || !isHost ? null : <PublicDashboard />}
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

  // Set up: what's connected.
  return shell({
    body: (
      <div className="space-y-4">
        {isHost ? <HostMove /> : null}
        <Summary
          items={[
            ["Tailnet", <span className="truncate font-mono">{ts.suffix ?? "Not seen yet"}</span>],
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
        {checks.length ? <Checks checks={checks} /> : null}
        {isHost && ts.suffix && (ts.dashboardName || ts.registryName || ts.sandboxesName) ? (
          <DialogSection title="Names on your tailnet">
            <ul className="list-rows rounded-lg border border-border">
              <NameRow kind="dashboard" label="Dashboard" />
              <NameRow kind="registry" label="Registry" />
              <NameRow kind="sandboxes" label="Sandboxes" />
            </ul>
          </DialogSection>
        ) : null}
        {isHost ? <PublicDashboard /> : null}
        {confirming ? (
          <ConfirmNote
            title="Disconnect Tailscale?"
            confirmLabel="Disconnect"
            onCancel={() => setConfirming(false)}
            onConfirm={() => {
              setConfirming(false);
              void api.removeTailscale(cid);
            }}
          >
            {isHost
              ? "Pyo's names on your tailnet are deleted. Servers stay on it. The public dashboard opens again."
              : "Pyo's names on your tailnet are deleted. Servers stay on it."}
          </ConfirmNote>
        ) : null}
      </div>
    ),
    footer: confirming ? null : (
      <>
        <Button variant="ghost" size="sm" className="mr-auto text-destructive! hover:bg-destructive/10" onClick={() => setConfirming(true)}>
          Disconnect
        </Button>
        <Button variant="secondary" size="sm" disabled={testing} onClick={() => void test()}>
          {testing ? <Busy>Testing…</Busy> : "Test"}
        </Button>
        <Button size="sm" onClick={onClose}>
          Done
        </Button>
      </>
    ),
  });
}
