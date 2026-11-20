import { CheckCircleIcon, CircleNotchIcon, TerminalWindowIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { siHetzner } from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import {
  CloudProgress,
  CloudServerFields,
  HetznerConnectForm,
  hetznerOf,
  priceOf,
  useCloudOptions,
  type CloudChoice,
} from "~/components/pyo/Hetzner";
import { useHosted } from "~/components/pyo/Managed";
import { NeedsTailscale } from "~/components/pyo/Tailscale";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { OptionTile, OptionTiles } from "~/components/ui/OptionTile";
import { Callout, Panel } from "~/components/ui/Panel";
import { DNS_LABEL } from "~/lib/pyo/format";
import { sandboxApi } from "~/lib/pyo/sandbox";
import { refresh, roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
import { AddSandboxServer, RuntimeBadge } from "./common";

// Add a sandbox server: two square tiles first. "Hetzner" has Pyo buy the
// server and set it up (or connects Hetzner first). "Command line" is the
// one-line sandbox.sh for a server you have. A hosted Pyo has no Tailscale
// setup, and Pyo's Hetzner servers join over it, so there only the command
// shows.

type How = "hetzner" | "command";

/** What sandbox.sh takes for --edge-domain. */
const EDGE_DOMAIN = /^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z][a-z0-9-]{0,62}$/;

export function AddSandboxServerFlow() {
  const [how, setHow] = useState<How | null>(null);
  // Once Pyo is buying one, only its progress shows.
  const [made, setMade] = useState<{ id: string; name: string } | null>(null);
  const hosted = useHosted();
  if (made) return <CloudSetup id={made.id} name={made.name} />;
  if (hosted === undefined) return null;
  if (hosted)
    return (
      <Panel>
        <div className="p-5">
          <AddSandboxServer />
        </div>
      </Panel>
    );
  return (
    <div className="space-y-5">
      <OptionTiles label="How">
        <OptionTile
          mark={<BrandMark icon={siHetzner} size={22} tone="brand" />}
          label="Hetzner"
          detail="Pyo buys a server and sets it up."
          selected={how === "hetzner"}
          onSelect={() => setHow("hetzner")}
        />
        <OptionTile
          icon={TerminalWindowIcon}
          label="Command line"
          detail="Run one command on your server."
          selected={how === "command"}
          onSelect={() => setHow("command")}
        />
      </OptionTiles>
      {how === "hetzner" ? (
        <CloudServer onMade={setMade} />
      ) : how === "command" ? (
        <Panel>
          <div className="p-5">
            <AddSandboxServer />
          </div>
        </Panel>
      ) : null}
    </div>
  );
}

/** The first free "sandbox-<n>". */
function freeName(taken: string[]) {
  for (let i = taken.length + 1; ; i++) if (!taken.includes(`sandbox-${i}`)) return `sandbox-${i}`;
}

function CloudServer({ onMade }: { onMade: (m: { id: string; name: string }) => void }) {
  const s = usePyo();
  const role = roleIn(s);
  const canManage = role === "owner" || role === "admin";
  const servers = sandboxServersIn(s);
  const [suggested] = useState(() => freeName(servers.map((x) => x.name)));
  const [name, setName] = useState("");
  const [edge, setEdge] = useState("");
  const [choice, setChoice] = useState<CloudChoice>({ location: "", serverType: "", sshKey: "" });
  const [confirming, setConfirming] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState("");
  const connected = !!hetznerOf(s);
  const { options } = useCloudOptions(connected && !!s.tailscale);
  const value = name || suggested;
  const edgeDomain = edge.trim().replace(/\.$/, "");
  const nameError = !DNS_LABEL.test(value)
    ? "Lowercase letters, digits and dashes."
    : servers.some((x) => x.name === value && x.teamId === s.currentTeamId)
      ? "A sandbox server has this name."
      : "";
  const edgeError = !edgeDomain || EDGE_DOMAIN.test(edgeDomain) ? "" : "A domain, like sbx.example.com.";
  const price = priceOf(options, choice);

  if (!canManage)
    return (
      <Panel>
        <p className="p-5 text-[13px] text-muted-foreground">Ask a team owner or admin to add a sandbox server.</p>
      </Panel>
    );

  const create = async () => {
    setConfirming(false);
    setCreating(true);
    setError("");
    try {
      const r = await sandboxApi.addCloudServer({
        teamId: s.currentTeamId,
        name: value,
        provider: "hetzner",
        location: choice.location,
        serverType: choice.serverType,
        ...(edgeDomain ? { edgeDomain } : {}),
        ...(choice.sshKey.trim() ? { sshKey: choice.sshKey.trim() } : {}),
        confirm: true,
      });
      await refresh().catch(() => undefined);
      if (r.server) onMade({ id: r.server.id, name: value });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setCreating(false);
    }
  };

  return (
    <Panel>
      <div className="space-y-5 p-5">
        <NeedsTailscale />
        {s.tailscale && !connected ? (
          <div className="space-y-2">
            <p className="text-[13px] font-medium">Connect Hetzner</p>
            <HetznerConnectForm />
          </div>
        ) : null}
        {s.tailscale && connected ? (
          <>
            <div className="grid items-start gap-4 sm:grid-cols-2">
              <Field label="Name" error={nameError || undefined} hint="Also its name on your tailnet.">
                {(p) => (
                  <Input {...p} mono value={value} onChange={(e) => setName(e.target.value.toLowerCase())} />
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
            <CloudServerFields value={choice} onChange={setChoice} />
            <p className="text-[12px] text-muted-foreground">
              Hetzner Cloud servers have no KVM, so sandboxes run on gVisor.
            </p>
            {error ? <Callout tone="danger">{error}</Callout> : null}
            <Button
              size="sm"
              disabled={!!nameError || !!edgeError || !price || creating}
              onClick={() => setConfirming(true)}
            >
              {creating ? "Creating…" : price ? `Create for ${price}/mo` : "Pick a server type"}
            </Button>
            <ConfirmDialog
              open={confirming}
              onClose={() => setConfirming(false)}
              onConfirm={() => void create()}
              tone="primary"
              title={`Create ${value} at Hetzner?`}
              description={`${choice.serverType} in ${choice.location}. ${price ?? ""} a month, before VAT.`}
              confirmLabel="Create server"
            />
          </>
        ) : null}
      </div>
    </Panel>
  );
}

/** A server Pyo is buying, until it runs sandboxes. */
function CloudSetup({ id, name }: { id: string; name: string }) {
  const s = usePyo();
  const server = s.sandboxServers.find((x) => x.id === id);
  const cloud = s.cloudServers.find((c) => c.sandboxServerId === id);
  const ready = server?.state === "ready";
  return (
    <Panel title={`Setting up ${server?.name ?? name}`}>
      <div className="space-y-4 p-5 text-[13px]">
        {cloud ? (
          <CloudProgress server={cloud} />
        ) : !server ? (
          <p className="text-muted-foreground">It was removed.</p>
        ) : (
          <p className="flex items-center gap-2 text-muted-foreground">
            <CircleNotchIcon size={14} className="animate-spin" /> Creating it at Hetzner…
          </p>
        )}
        {server && ready ? (
          <div className="flex flex-wrap items-center gap-3 rounded-lg border border-border px-3 py-2.5">
            <CheckCircleIcon size={16} weight="fill" className="text-success" />
            <span className="min-w-0 flex-1">
              <span className="font-medium">{server.name}</span> runs sandboxes.
            </span>
            <RuntimeBadge server={server} />
            <ButtonLink href="/sandboxes/new" size="xs">
              New sandbox
            </ButtonLink>
          </div>
        ) : cloud && cloud.state !== "failed" && cloud.state !== "removing" ? (
          <p className="text-[12px] text-muted-foreground">
            Takes a few minutes. You can leave this page.
          </p>
        ) : null}
      </div>
    </Panel>
  );
}
