import {
  ArrowsClockwiseIcon,
  CertificateIcon,
  CircleNotchIcon,
  DotsThreeIcon,
  GlobeIcon,
  KeyIcon,
  PlusIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { CloudStateBadge, money } from "~/components/pyo/Hetzner";
import { Button, ButtonLink } from "~/components/ui/Button";
import { Dot } from "~/components/ui/Badge";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Menu } from "~/components/ui/Menu";
import { Callout, Meter } from "~/components/ui/Panel";
import { cloudflareZoneFor, refresh, roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
import { NEEDS_UPDATE, sandboxApi, type SandboxServer } from "~/lib/pyo/sandbox";
import { toast } from "~/lib/pyo/toast";
import {
  JoinCommandView,
  RuntimeBadge,
  ServerState,
  joiningOf,
  serverCapacity,
  serverUse,
  useJoined,
  type Joining,
} from "./common";

/**
 * Sandbox servers on the servers page, next to the clusters: a third kind,
 * with the runtime badge, capacity and remove.
 */
export function SandboxServers() {
  const s = usePyo();
  const servers = sandboxServersIn(s);
  const role = roleIn(s);
  const [removing, setRemoving] = useState<SandboxServer | null>(null);
  const [resetting, setResetting] = useState<SandboxServer | null>(null);
  const [rejoin, setRejoin] = useState<(Joining & { update: boolean }) | null>(null);
  const [resetBusy, setResetBusy] = useState(false);
  const issuing = servers.some((x) => x.edge?.cert.state === "issuing");
  useEffect(() => {
    if (!issuing) return;
    const t = setInterval(() => void refresh().catch(() => undefined), 4000);
    return () => clearInterval(t);
  }, [issuing]);
  if (!servers.length) return null;
  // A new key for the server and the command that joins it again. For a
  // server that needs an update, the command that updates it.
  const reset = async (x: SandboxServer) => {
    setResetBusy(true);
    try {
      setRejoin({ ...joiningOf(await sandboxApi.resetAccess(x.id), x.name), update: !!x.needsUpdate });
      void refresh().catch(() => undefined);
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setResetBusy(false);
    }
  };
  const canRemove = (x: SandboxServer) =>
    x.teamId ? role === "owner" || role === "admin" : !!s.user.isAdmin;
  // The installation's servers use this team's Cloudflare, so its admin
  // needs to be the team's too.
  const canEdge = (x: SandboxServer) =>
    canRemove(x) && (role === "owner" || role === "admin");
  const zoneOf = (x: SandboxServer) => {
    const z = x.edge ? cloudflareZoneFor(s, x.edge.domain) : undefined;
    return z?.canEdit ? z : undefined;
  };
  const edgeAct = (x: SandboxServer, what: "cert" | "dns") => {
    const team = x.teamId ? undefined : s.currentTeamId;
    const run =
      what === "cert"
        ? sandboxApi.issueCert(x.id, team).then(() => ({ title: "Getting a certificate", tone: undefined }))
        : sandboxApi.createDnsRecord(x.id, team).then((r) =>
            r.ok
              ? { title: "DNS record created", tone: undefined }
              : { title: `${x.edge?.dns.name} has another record in Cloudflare`, tone: "error" as const },
          );
    void run
      .then((t) => toast(t))
      .catch((e: Error) => toast({ tone: "error", title: e.message }))
      .finally(() => void refresh().catch(() => undefined));
  };
  const edgeItems = (x: SandboxServer) => {
    const e = x.edge;
    if (!e || !canEdge(x) || !zoneOf(x)) return [];
    const items = [
      {
        label: e.cert.state === "ready" ? "Renew certificate" : "Get certificate",
        icon: <CertificateIcon size={14} />,
        disabled: e.cert.state === "issuing",
        onSelect: () => edgeAct(x, "cert"),
      },
    ];
    const made = e.dns.made?.status;
    if (e.dns.records.length && made !== "created" && made !== "exists")
      items.push({
        label: "Create DNS record",
        icon: <GlobeIcon size={14} />,
        disabled: false,
        onSelect: () => edgeAct(x, "dns"),
      });
    return items;
  };
  return (
    <section
      aria-labelledby="sandbox-servers"
      className="overflow-hidden rounded-[14px] border border-card-border bg-card"
    >
      <div className="flex items-center justify-between gap-4 border-b border-border bg-muted/30 px-5 py-3.5">
        <div className="min-w-0">
          <h2 id="sandbox-servers" className="text-[14px] font-semibold">
            Sandbox servers
          </h2>
          <p className="truncate text-[12.5px] text-muted-foreground">
            {servers.length} server{servers.length === 1 ? "" : "s"} · {servers.reduce((n, x) => n + x.sandboxes, 0)} sandboxes
          </p>
        </div>
        {role === "owner" || role === "admin" ? (
          <ButtonLink href="/sandboxes/servers/new" size="xs" variant="secondary">
            <PlusIcon size={12} /> Add server
          </ButtonLink>
        ) : null}
      </div>
      <ul className="list-rows">
        {servers.map((x) => {
          const use = serverUse(x);
          return (
            <li key={x.id} id={`sandbox-server-${x.id}`} className="scroll-mt-4 grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 px-5 py-3 md:grid-cols-[minmax(0,1fr)_128px_32px]">
              <div className="min-w-0">
                <div className="flex items-center gap-2.5">
                  <span className="truncate text-[13.5px] font-medium">{x.name}</span>
                  <RuntimeBadge server={x} />
                  {x.needsUpdate ? (
                    <NeedsUpdateState />
                  ) : x.cloud && x.cloud.state !== "ready" ? (
                    <CloudStateBadge state={x.cloud.state} />
                  ) : (
                    <ServerState server={x} />
                  )}
                  {x.teamId ? null : <span className="text-[12px] text-muted-foreground">Installation</span>}
                </div>
                <p className="truncate text-[12px] text-muted-foreground">
                  {x.outbound && x.tailnetName ? (
                    <span className="font-mono text-[11.5px]">{x.tailnetName} · </span>
                  ) : x.address ? (
                    <span className="font-mono text-[11.5px]">{x.address} · </span>
                  ) : null}
                  {serverCapacity(x)}
                </p>
                {x.cloud ? (
                  <p className="truncate text-[12px] text-muted-foreground">
                    Hetzner · {x.cloud.serverType} · {x.cloud.location} · {money(x.cloud.currency, x.cloud.monthly)}/mo
                  </p>
                ) : null}
                {x.cloud?.error ? <p className="text-[12px] text-destructive">{x.cloud.error}</p> : null}
                {x.edge ? <EdgeLine server={x} covered={!!zoneOf(x)} /> : null}
                {x.needsUpdate ? (
                  <p className="flex items-center gap-2 text-[12px] text-muted-foreground">
                    {NEEDS_UPDATE}
                    {canRemove(x) ? (
                      <Button size="xs" variant="secondary" disabled={resetBusy} onClick={() => void reset(x)}>
                        Update
                      </Button>
                    ) : null}
                  </p>
                ) : null}
                {!x.needsUpdate && x.state !== "ready" && (x.error || x.summary) ? (
                  <p className={`text-[12px] ${x.state === "refused" || x.state === "unreachable" ? "text-destructive" : "text-muted-foreground"}`}>
                    {x.error ?? x.summary}
                  </p>
                ) : null}
              </div>
              <div className="hidden md:block">
                {use ? (
                  <>
                    <p className="mb-1 text-[11px] font-medium text-muted-foreground">Memory</p>
                    <Meter value={use.used} max={use.total} />
                  </>
                ) : (
                  <span className="text-[12px] text-muted-foreground">—</span>
                )}
              </div>
              <div className="justify-self-end">
                {canRemove(x) ? (
                  <Menu
                    label={`Actions for ${x.name}`}
                    items={[
                      ...edgeItems(x),
                      ...(x.outbound
                        ? [
                            {
                              label: "Reset access",
                              icon: <KeyIcon size={14} />,
                              onSelect: () => setResetting(x),
                            },
                          ]
                        : []),
                      ...(x.needsUpdate
                        ? [
                            {
                              label: "Update",
                              icon: <ArrowsClockwiseIcon size={14} />,
                              onSelect: () => void reset(x),
                            },
                          ]
                        : []),
                      {
                        label: "Remove server",
                        icon: <TrashIcon size={14} />,
                        danger: true,
                        onSelect: () => setRemoving(x),
                      },
                    ]}
                    trigger={(t) => (
                      <Button {...t} variant="ghost" size="icon" className="h-7 w-7">
                        <DotsThreeIcon size={16} weight="bold" />
                      </Button>
                    )}
                  />
                ) : null}
              </div>
            </li>
          );
        })}
      </ul>
      <ConfirmDialog
        open={!!resetting}
        onClose={() => setResetting(null)}
        title={`Reset access for ${resetting?.name ?? "server"}?`}
        description="Pyo stops reaching it until you run a new command on it. Its sandboxes keep running."
        confirmLabel="Reset access"
        onConfirm={() => {
          if (resetting) void reset(resetting);
        }}
      />
      {rejoin ? (
        <RejoinDialog
          joining={rejoin}
          busy={resetBusy}
          onAgain={() => {
            const x = servers.find((v) => v.id === rejoin.id);
            if (x) void reset(x);
          }}
          onClose={() => setRejoin(null)}
        />
      ) : null}
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={removing?.cloud ? `Delete ${removing.name} at Hetzner?` : `Remove ${removing?.name ?? "server"}?`}
        description={removeText(removing)}
        confirmLabel={removing?.cloud ? "Delete server" : "Remove server"}
        onConfirm={() => {
          const x = removing;
          if (!x) return;
          void sandboxApi
            .removeServer(x.id, !!x.cloud)
            .then(() => toast({ title: x.cloud ? `Deleting ${x.name}` : `Removed ${x.name}` }))
            .catch((e: Error) => toast({ tone: "error", title: e.message }))
            .finally(() => void refresh().catch(() => undefined));
        }}
      />
    </section>
  );
}

/** A server that joined before 0.52.0: it takes no commands until it's updated. */
function NeedsUpdateState() {
  return (
    <span className="inline-flex items-center gap-1.5 text-[12.5px] text-muted-foreground">
      <Dot tone="warning" className="h-2 w-2" />
      Needs update
    </span>
  );
}

/**
 * On a sandbox whose server joined before 0.52.0: the server takes no
 * commands until it's updated, with Update for those who may run it.
 */
export function NeedsUpdateNotice({ server }: { server: SandboxServer }) {
  const s = usePyo();
  const role = roleIn(s);
  const may = server.teamId ? role === "owner" || role === "admin" : !!s.user.isAdmin;
  const [joining, setJoining] = useState<(Joining & { update: boolean }) | null>(null);
  const [busy, setBusy] = useState(false);
  const update = async () => {
    setBusy(true);
    try {
      setJoining({ ...joiningOf(await sandboxApi.resetAccess(server.id), server.name), update: true });
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <Callout tone="warning">
        <span className="flex flex-wrap items-center gap-2">
          {NEEDS_UPDATE}
          {may ? (
            <Button size="xs" variant="secondary" disabled={busy} onClick={() => void update()}>
              Update
            </Button>
          ) : null}
        </span>
      </Callout>
      {joining ? (
        <RejoinDialog
          joining={joining}
          busy={busy}
          onAgain={() => void update()}
          onClose={() => {
            setJoining(null);
            void refresh().catch(() => undefined);
          }}
        />
      ) : null}
    </>
  );
}

/** After Reset access: the command that joins the server again. After
 * Update: the command that updates it. */
function RejoinDialog({
  joining,
  busy,
  onAgain,
  onClose,
}: {
  joining: Joining & { update: boolean };
  busy: boolean;
  onAgain: () => void;
  onClose: () => void;
}) {
  const joined = useJoined(joining, true);
  return (
    <Dialog
      open
      onClose={onClose}
      width="max-w-2xl"
      title={joining.update ? `Update ${joining.name}` : `Join ${joining.name} again`}
    >
      <JoinCommandView
        joining={joining}
        joined={joined}
        hint={joining.update ? "Run this on the server as root. Its sandboxes keep running." : "Run this on the server."}
        busy={busy}
        onAgain={onAgain}
        onDone={onClose}
      />
    </Dialog>
  );
}

/** What removing a server does: its sandboxes end, and one Pyo made goes at Hetzner. */
function removeText(x: SandboxServer | null) {
  if (!x) return "";
  const n = x.sandboxes;
  const ending = n ? `Its ${n} sandbox${n === 1 ? "" : "es"} end. ` : "";
  return x.cloud
    ? `${ending}It leaves your tailnet, and Hetzner deletes it. ${money(x.cloud.currency, x.cloud.monthly)} a month stops.`
    : `${ending}Run sandbox.sh --uninstall on the server to take Pyo off it.`;
}

const day = (at: number) =>
  new Date(at).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" });

/** The edge's domain, its certificate and, until Pyo made it, the record to point at the server. */
function EdgeLine({ server, covered }: { server: SandboxServer; covered: boolean }) {
  const e = server.edge;
  if (!e) return null;
  const c = e.cert;
  const cert =
    c.state === "issuing" ? (
      <span className="inline-flex items-center gap-1">
        <CircleNotchIcon size={11} className="animate-spin" /> Getting a certificate
      </span>
    ) : c.state === "ready" && c.notAfter ? (
      <span>Certificate until {day(c.notAfter)}</span>
    ) : c.state === "failed" ? (
      <span className="text-destructive">Certificate failed. {c.error}</span>
    ) : e.https ? (
      <span>HTTPS</span>
    ) : covered ? (
      <span>No certificate yet</span>
    ) : (
      <span>No certificate. Cloudflare doesn't cover this domain, so put one on the server.</span>
    );
  const made = e.dns.made?.status;
  const point = e.dns.records.map((r) => r.content).join(" and ");
  return (
    <>
      <p className="truncate text-[12px] text-muted-foreground">
        <span className="font-mono text-[11.5px]">{e.domain}</span> · {cert}
      </p>
      {made === "conflict" ? (
        <p className="truncate text-[12px] text-destructive">{e.dns.name} has another record in Cloudflare.</p>
      ) : made !== "created" && made !== "exists" && point && !e.https ? (
        <p className="truncate text-[12px] text-muted-foreground">
          Point <span className="font-mono text-[11.5px]">{e.dns.name}</span> at{" "}
          <span className="font-mono text-[11.5px]">{point}</span>.
        </p>
      ) : null}
    </>
  );
}
