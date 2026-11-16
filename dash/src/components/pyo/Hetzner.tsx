import {
  ArrowsClockwiseIcon,
  CheckCircleIcon,
  TrashIcon,
  WarningCircleIcon,
} from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Select, Textarea } from "~/components/ui/Field";
import { Callout } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { TailnetLockSign } from "~/components/pyo/TailnetLock";
import { site } from "~/config/site";
import { fetchCloudOptions } from "~/lib/pyo/backend";
import { api, usePyo, type PyoState } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  CloudOptions,
  CloudServer,
  CloudServerState,
} from "~/lib/pyo/types";

const DOCS = `${site.docsUrl}/hetzner`;

/** "€4.29" */
export function money(currency: string, n: number) {
  const sign = currency === "EUR" ? "€" : currency === "USD" ? "$" : "";
  return sign ? `${sign}${n.toFixed(2)}` : `${n.toFixed(2)} ${currency}`;
}

export function hetznerOf(s: PyoState) {
  return s.cloudConnections.find(
    (c) => c.teamId === s.currentTeamId && c.provider === "hetzner",
  );
}

/** The server Pyo made for a cluster's server, while it's there. */
export function cloudServerOf(
  s: PyoState,
  clusterId: string,
  name: string,
): CloudServer | undefined {
  return s.cloudServers.find(
    (c) => c.clusterId === clusterId && c.name === name,
  );
}

const STATE: Record<CloudServerState, { label: string; tone: Tone }> = {
  creating: { label: "Creating", tone: "progress" },
  booting: { label: "Booting", tone: "progress" },
  joining: { label: "Joining", tone: "progress" },
  ready: { label: "Ready", tone: "success" },
  failed: { label: "Failed", tone: "danger" },
  removing: { label: "Deleting", tone: "warning" },
};

export function CloudStateBadge({ state }: { state: CloudServerState }) {
  return <Badge tone={STATE[state].tone}>{STATE[state].label}</Badge>;
}

/** Hetzner's locations, types and prices, read live. */
export function useCloudOptions(enabled: boolean) {
  const s = usePyo();
  const team = s.currentTeamId;
  const [options, setOptions] = useState<CloudOptions | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    setError("");
    fetchCloudOptions(team)
      .then((o) => live && setOptions(o))
      .catch((e: Error) => live && setError(e.message));
    return () => {
      live = false;
    };
  }, [team, enabled]);
  return { options, error };
}

/** Confirm deleting a server Pyo made, with the cost that stops. */
export function DeleteCloudServerDialog({
  server,
  onClose,
}: {
  server: CloudServer | null;
  onClose: () => void;
}) {
  return (
    <ConfirmDialog
      open={!!server}
      onClose={onClose}
      onConfirm={async () => {
        if (!server) return;
        onClose();
        if (await api.deleteCloudServer(server.id))
          toast({ title: `Deleting ${server.name}` });
      }}
      title={server ? `Delete ${server.name} at Hetzner?` : ""}
      description={
        server
          ? server.sandboxServerId
            ? `Its sandboxes end. Then it leaves your tailnet, and Hetzner deletes it. ${money(server.currency, server.monthly)} a month stops.`
            : `Its services move off it first. Then it leaves the cluster and your tailnet, and Hetzner deletes it. ${money(server.currency, server.monthly)} a month stops.`
          : ""
      }
      confirmLabel="Delete server"
    />
  );
}

/**
 * The team's Hetzner project token: paste, check and save. In Integrations,
 * and inline where a server is bought before Hetzner is connected.
 */
export function HetznerConnectForm({
  onSaved,
  onCancel,
}: {
  onSaved?: () => void;
  onCancel?: () => void;
}) {
  const s = usePyo();
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const save = async () => {
    setBusy(true);
    setError("");
    const r = await api.saveHetzner(token);
    setBusy(false);
    if ("error" in r) return setError(r.error);
    setToken("");
    toast({ title: "Hetzner connected" });
    onSaved?.();
  };
  return (
    <form
      className="space-y-2"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <p className="text-[12px] text-muted-foreground">
        A project API token with Read & Write.{" "}
        <a href={DOCS} target="_blank" rel="noreferrer" className="font-medium text-primary hover:text-primary/80">
          How to make one
        </a>
      </p>
      <Input
        type="password"
        value={token}
        onChange={(e) => setToken(e.target.value)}
        placeholder="API token"
        aria-label="API token"
        autoComplete="off"
        className="h-8"
        disabled={!s.tailscale}
      />
      {error ? <p className="text-[12px] text-destructive">{error}</p> : null}
      <div className="flex gap-2">
        <Button type="submit" size="xs" disabled={busy || !token.trim() || !s.tailscale}>
          {busy ? "Checking…" : "Save"}
        </Button>
        {onCancel ? (
          <Button type="button" size="xs" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        ) : null}
      </div>
    </form>
  );
}

/** Integrations › Hetzner: the team's project token, and the servers Pyo made. */
export function HetznerSettings() {
  const s = usePyo();
  const admin = useTeamAdmin();
  const conn = hetznerOf(s);
  const [replacing, setReplacing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [seen, setSeen] = useState("");
  const [removing, setRemoving] = useState(false);
  const [deleting, setDeleting] = useState<CloudServer | null>(null);
  const servers = s.cloudServers.filter((c) => c.teamId === s.currentTeamId);
  const test = async () => {
    setBusy(true);
    const r = await api.testHetzner();
    setBusy(false);
    if ("error" in r) toast({ tone: "error", title: r.error });
    else setSeen(r.data.locations.map((l) => l.city).join(", "));
  };
  return (
    <>
      <ul className="list-rows">
        <li className="space-y-3 px-4 py-3">
          {!s.tailscale ? (
            <Callout tone="warning">
              Servers join over your tailnet. Set up{" "}
              <a href="#tailscale" className="font-medium text-primary hover:text-primary/80">
                Tailscale
              </a>{" "}
              first.
            </Callout>
          ) : null}
          {!admin ? (
            <div className="flex items-center gap-3 text-[12.5px] text-muted-foreground">
              <span className="flex-1">
                {conn ? `Token ${conn.hint}` : "A team admin can connect it."}
              </span>
              {conn ? <Badge tone="success">Connected</Badge> : <Badge>Not connected</Badge>}
            </div>
          ) : !conn || replacing ? (
            <HetznerConnectForm
              onSaved={() => setReplacing(false)}
              onCancel={conn ? () => setReplacing(false) : undefined}
            />
          ) : (
            <>
              <div className="flex flex-wrap items-center gap-3">
                <div className="min-w-0 flex-1">
                  <p className="font-mono text-[12.5px]">Token {conn.hint}</p>
                  {seen ? (
                    <p className="text-[12px] text-muted-foreground">{seen}</p>
                  ) : null}
                </div>
                <Button size="xs" variant="secondary" disabled={busy} onClick={() => void test()}>
                  {busy ? "Testing…" : "Test"}
                </Button>
                <Button size="xs" variant="secondary" onClick={() => setReplacing(true)}>
                  Replace
                </Button>
                <Button size="xs" variant="ghost" className="text-destructive" onClick={() => setRemoving(true)}>
                  Remove
                </Button>
              </div>
            </>
          )}
        </li>
        {servers.map((c) => {
          const cluster = s.clusters.find((x) => x.id === c.clusterId);
          return (
            <li key={c.id} className="flex flex-wrap items-center gap-3 px-4 py-3">
              <div className="min-w-0 flex-1">
                <p className="text-[13px] font-medium">{c.name}</p>
                <p className="text-[12px] text-muted-foreground">
                  {c.sandboxServerId ? "Sandbox server" : (cluster?.name ?? "Cluster removed")} ·{" "}
                  {c.serverType} · {c.location} · {money(c.currency, c.monthly)}/mo
                </p>
                {c.error ? <p className="text-[12px] text-destructive">{c.error}</p> : null}
              </div>
              <CloudStateBadge state={c.state} />
              {admin && c.state !== "removing" ? (
                <Button
                  size="xs"
                  variant="ghost"
                  className="text-destructive"
                  onClick={() => setDeleting(c)}
                >
                  Delete
                </Button>
              ) : null}
            </li>
          );
        })}
      </ul>
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        onConfirm={() => {
          setRemoving(false);
          void api.removeHetzner();
        }}
        title="Remove Hetzner?"
        description="Servers Pyo made there keep running."
        confirmLabel="Remove"
      />
      <DeleteCloudServerDialog server={deleting} onClose={() => setDeleting(null)} />
    </>
  );
}

/** What a new server at Hetzner is: location, type and an SSH key. */
export type CloudChoice = {
  location: string;
  serverType: string;
  sshKey: string;
};

/**
 * Pick a location and a server type, with the price, specs and
 * architecture Hetzner gives now, and optionally an SSH key.
 */
export function CloudServerFields({
  value,
  onChange,
}: {
  value: CloudChoice;
  onChange: (v: CloudChoice) => void;
}) {
  const { options, error } = useCloudOptions(true);
  const set = (p: Partial<CloudChoice>) => onChange({ ...value, ...p });
  useEffect(() => {
    if (options && !value.location && options.locations[0])
      set({ location: options.locations[0].name });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [options]);
  if (error) return <Callout tone="danger">{error}</Callout>;
  if (!options)
    return <p className="text-[12.5px] text-muted-foreground">Reading Hetzner…</p>;
  const types = options.serverTypes.filter(
    (t) => t.prices[value.location] !== undefined,
  );
  return (
    <div className="space-y-4">
      <Field label="Location">
        {(p) => (
          <Select
            {...p}
            value={value.location}
            onChange={(e) => set({ location: e.target.value, serverType: "" })}
          >
            {options.locations.map((l) => (
              <option key={l.name} value={l.name}>
                {l.city}, {l.country} ({l.name})
              </option>
            ))}
          </Select>
        )}
      </Field>
      <div className="space-y-1.5">
        <p className="text-[13px] font-medium">Server type</p>
        <ul className="list-rows max-h-80 overflow-y-auto rounded-lg border border-border">
          {types.map((t) => {
            const usable = t.supported && t.available.includes(value.location);
            const picked = value.serverType === t.name;
            return (
              <li key={t.name}>
                <button
                  type="button"
                  disabled={!usable}
                  onClick={() => set({ serverType: t.name })}
                  aria-pressed={picked}
                  className={`grid w-full grid-cols-[5rem_minmax(0,1fr)_auto] items-center gap-3 px-3 py-2 text-left text-[12.5px] ${picked ? "bg-primary/10" : "hover:bg-muted/50"} disabled:cursor-not-allowed disabled:opacity-50`}
                >
                  <span className="font-mono font-medium">{t.name}</span>
                  <span className="text-muted-foreground">
                    {t.cores} vCPU · {t.memoryGb} GB · {t.diskGb} GB disk · {t.architecture}
                    {t.cpuType === "dedicated" ? " · dedicated" : ""}
                    {!t.supported ? " · Arm isn't supported" : !usable ? " · unavailable here" : ""}
                  </span>
                  <span className="font-medium tabular-nums">
                    {money(options.currency, t.prices[value.location])}/mo
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
        <p className="text-[12px] text-muted-foreground">Before VAT, with the IPv4 address.</p>
      </div>
      <Field label="SSH key" optional hint="To sign in over your tailnet.">
        {(p) => (
          <Textarea
            {...p}
            value={value.sshKey}
            onChange={(e) => set({ sshKey: e.target.value })}
            placeholder="ssh-ed25519 AAAA…"
            rows={2}
            className="font-mono text-[12px]"
          />
        )}
      </Field>
    </div>
  );
}

/** The price of a choice, once a type is picked. */
export function priceOf(options: CloudOptions | null, c: CloudChoice) {
  const t = options?.serverTypes.find((x) => x.name === c.serverType);
  const p = t?.prices[c.location];
  return options && p !== undefined ? money(options.currency, p) : null;
}

/** A new server's way to ready: Creating, Booting, Joining, Ready. */
export function CloudProgress({ server }: { server: CloudServer }) {
  const [deleting, setDeleting] = useState(false);
  const steps: CloudServerState[] = ["creating", "booting", "joining", "ready"];
  const at = steps.indexOf(server.state);
  if (server.state === "failed")
    return (
      <>
        <StatusNote
          tone="danger"
          icon={<WarningCircleIcon size={15} />}
          title={`${server.name} didn't join`}
          action={
            <Button size="xs" variant="secondary" onClick={() => setDeleting(true)}>
              <TrashIcon size={13} /> Delete it
            </Button>
          }
        >
          {server.error ?? "Pyo stopped waiting for it."}
        </StatusNote>
        <DeleteCloudServerDialog
          server={deleting ? server : null}
          onClose={() => setDeleting(false)}
        />
      </>
    );
  if (server.state === "removing")
    return (
      <StatusNote
        tone="progress"
        icon={<ArrowsClockwiseIcon size={15} className="animate-spin" />}
        title={`Deleting ${server.name}`}
      >
        {server.error ??
          (server.sandboxServerId
            ? "It leaves your tailnet, then Hetzner deletes it."
            : "Its services move off it, then Hetzner deletes it.")}
      </StatusNote>
    );
  return (
    <div className="space-y-3">
      <ol className="flex flex-wrap items-center gap-2 text-[12.5px]">
        {steps.map((st, i) => (
          <li key={st} className="flex items-center gap-1.5">
            {i < at || server.state === "ready" ? (
              <CheckCircleIcon size={14} weight="fill" className="text-success" />
            ) : i === at ? (
              <ArrowsClockwiseIcon size={14} className="animate-spin text-info" />
            ) : (
              <span className="h-3.5 w-3.5 rounded-full border border-border" />
            )}
            <span className={i <= at ? "font-medium" : "text-muted-foreground"}>
              {STATE[st].label}
            </span>
          </li>
        ))}
      </ol>
      {server.signCommand ? <TailnetLockSign command={server.signCommand} /> : null}
      {server.error ? <p className="text-[12px] text-destructive">{server.error}</p> : null}
    </div>
  );
}
