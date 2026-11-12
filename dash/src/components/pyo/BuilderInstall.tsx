import { CheckCircleIcon, CircleNotchIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { CopyButton } from "~/components/ui/Table";
import { api, usePyo } from "~/lib/pyo/store";
import type { Builder } from "~/lib/pyo/types";

/**
 * The one command that joins a new server to a cluster as a
 * build-only server, and whether its builder is up yet.
 */
export function BuilderInstall({
  builderId,
  command,
}: {
  builderId: string;
  command: string;
}) {
  const b = usePyo().builders.find((x) => x.id === builderId);
  const script = command.match(/https?:\/\/\S+\/join\.sh/)?.[0];
  const ready = b?.status === "ready";
  return (
    <div className="space-y-3 text-[13px]">
      <p className="text-muted-foreground">
        Run this on the machine. It joins the cluster as a
        build-only server. Works for an hour.{" "}
        {script ? (
          <a
            href={script}
            target="_blank"
            rel="noreferrer"
            className="font-medium text-primary hover:text-primary/80"
          >
            View the script
          </a>
        ) : null}
      </p>
      <div className="flex items-start gap-2 rounded-lg border border-border bg-muted/40 p-3">
        <code className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[11.5px] leading-[18px]">
          {command}
        </code>
        <CopyButton value={command} label="Copy command" />
      </div>
      {!command.includes("--tailscale-key") ? (
        <p className="text-[12.5px] text-warning">
          No Tailscale key saved: the machine must reach Pyo already, or run it
          with <span className="font-mono">TS_AUTHKEY=tskey-auth-…</span>. You
          can save one in Integrations, under Tailscale.
        </p>
      ) : null}
      <div className="flex items-center gap-2 rounded-lg border border-border px-3 py-2.5">
        {ready ? (
          <>
            <CheckCircleIcon size={16} weight="fill" className="text-success" />
            <span>
              <span className="font-medium">{b.name}</span> is connected
              {b.info.hostname ? ` from ${b.info.hostname}` : ""}.
            </span>
          </>
        ) : (
          <>
            <CircleNotchIcon
              size={16}
              className="animate-spin text-muted-foreground"
            />
            <span className="text-muted-foreground">
              Waiting for {b?.name ?? "the builder"} to connect…
            </span>
          </>
        )}
      </div>
    </div>
  );
}

/** A fresh install command for a builder on a new server; the old token stops working. */
export function BuilderCommandDialog({
  builder,
  open,
  onClose,
}: {
  builder: Builder | null;
  open: boolean;
  onClose: () => void;
}) {
  const [command, setCommand] = useState("");
  const [busy, setBusy] = useState(false);
  const close = () => {
    setCommand("");
    onClose();
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      width="max-w-2xl"
      title={`Install command for ${builder?.name ?? "builder"}`}
      description={
        command
          ? undefined
          : "No builds until you run the new command."
      }
      onSubmit={
        command || !builder
          ? undefined
          : async () => {
              setBusy(true);
              const c = await api.rotateBuilderToken(builder.id);
              setBusy(false);
              if (c) setCommand(c);
            }
      }
      footer={
        command ? (
          <Button size="sm" variant="secondary" onClick={close}>
            Close
          </Button>
        ) : (
          <>
            <Button variant="ghost" size="sm" onClick={close}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={busy}>
              {busy ? "Preparing…" : "Get a new command"}
            </Button>
          </>
        )
      }
    >
      {command && builder ? (
        <BuilderInstall builderId={builder.id} command={command} />
      ) : null}
    </Dialog>
  );
}
