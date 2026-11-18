import { CheckCircleIcon, WarningCircleIcon, XCircleIcon } from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Badge } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Input, Switch } from "~/components/ui/Field";
import { Callout } from "~/components/ui/Panel";
import { site } from "~/config/site";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { TailscaleCheck, TailscaleSetupOutcome, TailscaleSetupPlan } from "~/lib/pyo/types";

/** Where a flow that needed Tailscale goes back to once it's saved. */
const BACK = "pyo.tailscaleBack";

const DOCS = `${site.docsUrl}/tailscale`;
const KEYS = "https://login.tailscale.com/admin/settings/keys";

const LINK = "font-medium text-primary hover:text-primary/80";

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
 * join over the tailnet. Links to the card and comes back after saving.
 */
export function NeedsTailscale() {
  const s = usePyo();
  if (s.tailscale) return null;
  return (
    <Callout tone="warning">
      Servers join over your tailnet. Set up Tailscale first.{" "}
      {s.user.isAdmin ? (
        <ButtonLink
          href="/integrations#tailscale"
          size="xs"
          variant="secondary"
          onClick={() => {
            try {
              sessionStorage.setItem(BACK, window.location.pathname);
            } catch {
              // Private mode: no way back, the card stays.
            }
          }}
        >
          Set up Tailscale
        </ButtonLink>
      ) : (
        "Ask an installation admin."
      )}
    </Callout>
  );
}

function Checks({ checks }: { checks: TailscaleCheck[] }) {
  return (
    <ul className="space-y-1 text-[12px]">
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

function ClientForm({ onDone, saved }: { onDone: () => void; saved?: string }) {
  const navigate = useNavigate();
  const [id, setId] = useState(saved ?? "");
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [checks, setChecks] = useState<TailscaleCheck[]>([]);
  return (
    <form
      className="space-y-2"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError("");
        const r = await api.saveTailscale(id, secret);
        setBusy(false);
        if ("error" in r) return setError(r.error);
        setChecks(r.data.checks);
        if (!r.data.ok) return;
        toast({ title: "Tailscale saved" });
        if (!goBack(navigate)) onDone();
      }}
    >
      <div className="grid gap-2 sm:grid-cols-2">
        <Input
          value={id}
          onChange={(e) => setId(e.target.value)}
          placeholder="Client ID"
          aria-label="Client ID"
          autoComplete="off"
          className="h-8"
        />
        <Input
          type="password"
          value={secret}
          onChange={(e) => setSecret(e.target.value)}
          placeholder="tskey-client-…"
          aria-label="Client secret"
          autoComplete="off"
          className="h-8"
        />
      </div>
      {error ? <p className="text-[12px] text-destructive">{error}</p> : null}
      {checks.length ? <Checks checks={checks} /> : null}
      <div className="flex gap-2">
        <Button type="submit" size="xs" disabled={busy || !id.trim() || (!saved && !secret.trim())}>
          {busy ? "Checking…" : "Save"}
        </Button>
      </div>
    </form>
  );
}

/** A list of short lines, each with its mark. */
function Lines({ items, tone }: { items: string[]; tone: "done" | "todo" | "plan" }) {
  if (!items.length) return null;
  return (
    <ul className="space-y-1 text-[12px]">
      {items.map((t) => (
        <li key={t} className="flex items-start gap-1.5">
          {tone === "todo" ? (
            <WarningCircleIcon size={14} weight="fill" className="mt-0.5 shrink-0 text-warning" />
          ) : tone === "done" ? (
            <CheckCircleIcon size={14} weight="fill" className="mt-0.5 shrink-0 text-success" />
          ) : (
            <span className="mt-[7px] size-1 shrink-0 rounded-full bg-foreground/60" />
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

/** What setting up with a token did, until the card is closed. */
export function SetupOutcome({ outcome }: { outcome: TailscaleSetupOutcome }) {
  return (
    <div className="space-y-2">
      {outcome.changed.length ? (
        <Lines items={outcome.changed} tone="done" />
      ) : outcome.ok ? (
        <p className="text-[12px] text-muted-foreground">Nothing changed on your tailnet.</p>
      ) : null}
      {outcome.tokenDeleted ? <Lines items={["Deleted the token"]} tone="done" /> : null}
      {outcome.error ? <p className="text-[12px] text-destructive">{outcome.error}</p> : null}
      <Lines items={outcome.todo} tone="todo" />
      {outcome.checks.some((c) => !c.ok) ? <Checks checks={outcome.checks} /> : null}
    </div>
  );
}

/** The main way in: one short-lived API access token, and Pyo does the rest. */
function TokenSetup({ onDone }: { onDone: (outcome: TailscaleSetupOutcome) => void }) {
  const navigate = useNavigate();
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [plan, setPlan] = useState<TailscaleSetupPlan | null>(null);
  const [outcome, setOutcome] = useState<TailscaleSetupOutcome | null>(null);

  const check = async () => {
    setBusy(true);
    setError("");
    setOutcome(null);
    const r = await api.planTailscaleSetup(token);
    setBusy(false);
    if ("error" in r) return setError(r.error);
    setPlan(r.data.plan);
  };
  const apply = async () => {
    setBusy(true);
    setError("");
    const r = await api.setUpTailscale(token);
    setBusy(false);
    if ("error" in r) return setError(r.error);
    if (!r.data.ok) {
      // Stopped partway: what changed stays listed, and the token stays for another try.
      setPlan(null);
      return setOutcome(r.data);
    }
    setToken("");
    setPlan(null);
    toast({ title: "Tailscale set up" });
    if (!goBack(navigate)) onDone(r.data);
  };
  const reset = () => {
    setPlan(null);
    setError("");
  };

  if (plan)
    return (
      <div className="space-y-3">
        {plan.blocked ? (
          <Callout tone="warning">{plan.blocked}</Callout>
        ) : plan.changes.length ? (
          <div className="space-y-1.5">
            <p className="text-[12px] font-medium">Pyo will</p>
            <Lines items={plan.changes} tone="plan" />
          </div>
        ) : (
          <p className="text-[12px]">Your tailnet is already set up.</p>
        )}
        <Diff lines={plan.diff} />
        <Lines items={plan.done} tone="done" />
        {plan.blocked ? null : <p className="text-[12px] text-muted-foreground">Pyo deletes the token when it's done.</p>}
        {error ? <p className="text-[12px] text-destructive">{error}</p> : null}
        <div className="flex gap-2">
          {plan.blocked ? (
            <Button size="xs" disabled={busy} onClick={() => void check()}>
              {busy ? "Checking…" : "Check again"}
            </Button>
          ) : (
            <Button size="xs" disabled={busy} onClick={() => void apply()}>
              {busy ? "Setting up…" : plan.changes.length ? "Set up" : "Finish"}
            </Button>
          )}
          <Button size="xs" variant="ghost" disabled={busy} onClick={reset}>
            Cancel
          </Button>
        </div>
      </div>
    );

  return (
    <form
      className="space-y-2"
      onSubmit={(e) => {
        e.preventDefault();
        void check();
      }}
    >
      <p className="text-[12px] text-muted-foreground">
        Generate an API access token that expires in 1 day, and paste it here.{" "}
        <a href={KEYS} target="_blank" rel="noreferrer" className={LINK}>
          Open Keys in Tailscale
        </a>
      </p>
      {outcome ? <SetupOutcome outcome={outcome} /> : null}
      <div className="flex gap-2">
        <Input
          type="password"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          placeholder="tskey-api-…"
          aria-label="API access token"
          autoComplete="off"
          className="h-8"
        />
        <Button type="submit" size="xs" className="h-8" disabled={busy || !token.trim()}>
          {busy ? "Checking…" : "Check"}
        </Button>
      </div>
      {error ? <p className="text-[12px] text-destructive">{error}</p> : null}
    </form>
  );
}

/** Setting up: with a token, or with an OAuth client made by hand. */
function Setup({ saved, onDone }: { saved?: string; onDone: (outcome?: TailscaleSetupOutcome) => void }) {
  const [byClient, setByClient] = useState(false);
  return (
    <div className="space-y-3">
      {byClient ? (
        <>
          <p className="text-[12px] text-muted-foreground">
            An OAuth client with Devices › Core and Auth Keys (both with tag:pyo) and Services.{" "}
            <a href={`${DOCS}#with-an-oauth-client`} target="_blank" rel="noreferrer" className={LINK}>
              How to make one
            </a>
          </p>
          <ClientForm saved={saved} onDone={() => onDone()} />
        </>
      ) : (
        <TokenSetup onDone={onDone} />
      )}
      <div className="flex gap-3">
        <button type="button" className={`text-[12px] ${LINK}`} onClick={() => setByClient(!byClient)}>
          {byClient ? "Set up with an access token instead" : "Set up with an OAuth client instead"}
        </button>
        {saved ? (
          <button type="button" className="text-[12px] text-muted-foreground hover:text-foreground" onClick={() => onDone()}>
            Cancel
          </button>
        ) : null}
      </div>
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
    <li className="flex flex-wrap items-center gap-3 px-4 py-3">
      <div className="min-w-0 flex-1">
        <p className="text-[13px] font-medium">{label}</p>
        <p className="font-mono text-[12px] text-muted-foreground">
          {current}.{ts.suffix}
        </p>
        {error ? <p className="text-[12px] text-destructive">{error}</p> : null}
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
      {editing ? (
        <form
          className="flex gap-2"
          onSubmit={async (e) => {
            e.preventDefault();
            const r = await api.renameTailscaleName(kind, name);
            if ("error" in r) return setError(r.error);
            setError("");
            setEditing(false);
          }}
        >
          <Input
            value={name}
            onChange={(e) => setName(e.target.value)}
            aria-label={`${label} name`}
            className="h-8 w-40"
          />
          <Button type="submit" size="xs" disabled={!name.trim()}>
            Rename
          </Button>
        </form>
      ) : (
        <Button
          size="xs"
          variant="ghost"
          onClick={() => {
            setName(current);
            setEditing(true);
          }}
        >
          Rename
        </Button>
      )}
    </li>
  );
}

function PublicDashboardRow() {
  const s = usePyo();
  const pd = s.publicDashboard;
  if (!pd) return null;
  const onTailnet = !!s.tailscale?.dashboard;
  const locked = pd.possible && pd.open && !onTailnet;
  return (
    <li className="px-4 py-3">
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
    </li>
  );
}

/** Pyo's own server moving k3s onto the tailnet, while it happens or if it failed. */
function HostMove() {
  const move = usePyo().tailscale?.hostMove;
  if (!move) return null;
  return (
    <li className="px-4 py-3">
      {move.state === "moving" ? (
        <Callout tone="info">Moving this server to your tailnet…</Callout>
      ) : (
        <Callout tone="danger">
          This server couldn't move to your tailnet. {move.error ?? ""} Pyo tries again tomorrow.
        </Callout>
      )}
    </li>
  );
}

/** Integrations › Tailscale: the OAuth client for the whole Pyo (admins). */
export function TailscaleSettings() {
  const s = usePyo();
  const ts = s.tailscale;
  const [replacing, setReplacing] = useState(false);
  const [checks, setChecks] = useState<TailscaleCheck[]>([]);
  const [outcome, setOutcome] = useState<TailscaleSetupOutcome | null>(null);
  const [testing, setTesting] = useState(false);
  const [removing, setRemoving] = useState(false);
  if (!s.user.isAdmin)
    return (
      <ul className="list-rows">
        <li className="flex items-center gap-3 px-4 py-3 text-[12.5px] text-muted-foreground">
          <span className="flex-1">
            {ts ? `On ${ts.suffix ?? "your tailnet"}` : "An installation admin can set it up."}
          </span>
          {ts ? <Badge tone="success">Set up</Badge> : <Badge>Not set up</Badge>}
        </li>
        <HostMove />
      </ul>
    );
  return (
    <>
      <ul className="list-rows">
        <li className="space-y-3 px-4 py-3">
          {!ts || replacing ? (
            <Setup
              saved={ts?.clientId}
              onDone={(o) => {
                setReplacing(false);
                setOutcome(o ?? null);
              }}
            />
          ) : (
            <>
              <div className="flex flex-wrap items-center gap-3">
                <div className="min-w-0 flex-1">
                  <p className="font-mono text-[12.5px]">{ts.clientId}</p>
                  <p className="text-[12px] text-muted-foreground">
                    Secret {ts.hint}
                    {ts.suffix ? ` · ${ts.suffix}` : ""}
                  </p>
                </div>
                <Button
                  size="xs"
                  variant="secondary"
                  disabled={testing}
                  onClick={async () => {
                    setTesting(true);
                    const r = await api.testTailscale();
                    setTesting(false);
                    if ("error" in r) toast({ tone: "error", title: r.error });
                    else setChecks(r.data.checks);
                  }}
                >
                  {testing ? "Testing…" : "Test"}
                </Button>
                <Button size="xs" variant="secondary" onClick={() => setReplacing(true)}>
                  Replace
                </Button>
                <Button size="xs" variant="ghost" className="text-destructive" onClick={() => setRemoving(true)}>
                  Remove
                </Button>
              </div>
              {outcome ? <SetupOutcome outcome={outcome} /> : null}
              {checks.length ? <Checks checks={checks} /> : null}
            </>
          )}
        </li>
        {ts && !replacing ? (
          <>
            <NameRow kind="dashboard" label="Dashboard" />
            <NameRow kind="registry" label="Registry" />
            <NameRow kind="sandboxes" label="Sandboxes" />
          </>
        ) : null}
        <HostMove />
        <PublicDashboardRow />
      </ul>
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        onConfirm={() => {
          setRemoving(false);
          void api.removeTailscale();
        }}
        title="Remove Tailscale?"
        description="Pyo's names on your tailnet are deleted. Servers stay on it. The public dashboard opens again."
        confirmLabel="Remove"
      />
    </>
  );
}
