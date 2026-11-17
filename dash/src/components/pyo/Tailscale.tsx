import { CheckCircleIcon, XCircleIcon } from "@phosphor-icons/react";
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
import type { TailscaleCheck } from "~/lib/pyo/types";

/** Where a flow that needed Tailscale goes back to once it's saved. */
const BACK = "pyo.tailscaleBack";

const DOCS = `${site.docsUrl}/tailscale`;

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
        let back: string | null = null;
        try {
          back = sessionStorage.getItem(BACK);
          sessionStorage.removeItem(BACK);
        } catch {
          // Nowhere to go back to.
        }
        if (back) void navigate({ to: back as string });
        else onDone();
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
        {saved ? (
          <Button type="button" size="xs" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        ) : null}
      </div>
    </form>
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
            <>
              <p className="text-[12px] text-muted-foreground">
                An OAuth client with Devices › Core and Auth Keys (both with tag:pyo) and Services.{" "}
                <a href={DOCS} target="_blank" rel="noreferrer" className="font-medium text-primary hover:text-primary/80">
                  How to make one
                </a>
              </p>
              <ClientForm saved={ts?.clientId} onDone={() => setReplacing(false)} />
            </>
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
