import { DotsThreeIcon, PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Menu } from "~/components/ui/Menu";
import { Meter } from "~/components/ui/Panel";
import { refresh, roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
import { sandboxApi, type SandboxServer } from "~/lib/pyo/sandbox";
import { toast } from "~/lib/pyo/toast";
import {
  AddSandboxServerDialog,
  RuntimeBadge,
  ServerState,
  serverCapacity,
  serverUse,
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
  const [adding, setAdding] = useState(false);
  if (!servers.length) return null;
  const canRemove = (x: SandboxServer) =>
    x.teamId ? role === "owner" || role === "admin" : !!s.user.isAdmin;
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
          <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
            <PlusIcon size={12} /> Add server
          </Button>
        ) : null}
      </div>
      <ul className="list-rows">
        {servers.map((x) => {
          const use = serverUse(x);
          return (
            <li key={x.id} className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 px-5 py-3 md:grid-cols-[minmax(0,1fr)_128px_32px]">
              <div className="min-w-0">
                <div className="flex items-center gap-2.5">
                  <span className="truncate text-[13.5px] font-medium">{x.name}</span>
                  <RuntimeBadge server={x} />
                  <ServerState server={x} />
                  {x.teamId ? null : <span className="text-[12px] text-muted-foreground">Installation</span>}
                </div>
                <p className="truncate text-[12px] text-muted-foreground">
                  {x.address ? <span className="font-mono text-[11.5px]">{x.address} · </span> : null}
                  {serverCapacity(x)}
                </p>
                {x.state !== "ready" && (x.error || x.summary) ? (
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
      <AddSandboxServerDialog open={adding} onClose={() => setAdding(false)} />
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing?.name ?? "server"}?`}
        description={
          removing?.sandboxes
            ? `Its ${removing.sandboxes} sandbox${removing.sandboxes === 1 ? "" : "es"} end. Run sandbox.sh --uninstall on the server to take Pyo off it.`
            : "Run sandbox.sh --uninstall on the server to take Pyo off it."
        }
        confirmLabel="Remove server"
        onConfirm={() => {
          const x = removing;
          if (!x) return;
          void sandboxApi
            .removeServer(x.id)
            .then(() => toast({ title: `Removed ${x.name}` }))
            .catch((e: Error) => toast({ tone: "error", title: e.message }))
            .finally(() => void refresh().catch(() => undefined));
        }}
      />
    </section>
  );
}
