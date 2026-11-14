import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Segmented } from "~/components/ui/Field";
import { api, usePyo } from "~/lib/pyo/store";
import type { Cluster } from "~/lib/pyo/types";
import { CopyButton } from "~/components/ui/Table";
import { LiveCommand } from "~/components/pyo/LiveCommand";

/** For a cluster that already exists: apply the agent with kubectl. */
export function agentInstallCommand(token: string) {
  const origin = typeof window === "undefined" ? "" : window.location.origin;
  return `kubectl apply -f "${origin}/install/agent.yaml?token=${token}"`;
}

/** A command to copy and run. */
export function Command({ value }: { value: string }) {
  return (
    <div className="flex items-start gap-2 rounded-lg border border-border bg-muted/40 p-3">
      <code className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[12px] leading-5 text-foreground">
        {value}
      </code>
      <CopyButton value={value} label="Copy command" />
    </div>
  );
}

/**
 * The one command that sets up a cluster: it installs k3s on the machine if
 * it isn't there, then the Pyo agent. The token in it is the agent's
 * credential and is only shown here, once.
 */
export function AgentInstall({
  token,
  command,
  stale = false,
  thisMachine = false,
}: {
  token: string;
  command: string;
  /** The form above changed: a new command is on its way. */
  stale?: boolean;
  /** Run on the machine Pyo runs on, where k3s is already set up. */
  thisMachine?: boolean;
}) {
  const [kubectl, setKubectl] = useState(false);
  return (
    <div className="space-y-2">
      <LiveCommand command={command} stale={stale} />
      <p className="text-[12px] leading-relaxed text-muted-foreground">
        Token shown once
        {thisMachine ? null : " · "}
        {thisMachine ? null : (
          <button
            type="button"
            onClick={() => setKubectl((v) => !v)}
            className="font-medium text-primary hover:text-primary/80"
          >
            {kubectl ? "Hide" : "Already have a cluster?"}
          </button>
        )}
      </p>
      {kubectl && !thisMachine ? (
        <div className="space-y-1.5 pt-1">
          <p className="text-[12px] text-muted-foreground">
            Or run this with kubectl:
          </p>
          <Command value={agentInstallCommand(token)} />
        </div>
      ) : null}
    </div>
  );
}

/**
 * The install command for a cluster whose agent hasn't connected. Pyo
 * makes a new token for it, so a command shown before stops working.
 */
export function ClusterCommandDialog({
  cluster,
  open,
  onClose,
}: {
  cluster: Cluster;
  open: boolean;
  onClose: () => void;
}) {
  const s = usePyo();
  // The machine Pyo runs on can be offered while no cluster is it yet.
  const localFree = !s.clusters.some(
    (c) => c.sameCluster && c.id !== cluster.id,
  );
  const [where, setWhere] = useState<"local" | "other">(
    cluster.sameCluster ? "local" : "other",
  );
  const [made, setMade] = useState<{ token: string; command: string } | null>(
    null,
  );
  const [busy, setBusy] = useState(false);
  const close = () => {
    setMade(null);
    onClose();
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      width="max-w-2xl"
      title={`Install command for ${cluster.name}`}
      description={
        made ? undefined : "Makes a new token. Earlier commands stop working."
      }
      onSubmit={
        made
          ? undefined
          : async () => {
              setBusy(true);
              const r = await api.rotateAgentToken(
                cluster.id,
                localFree && where === "local",
              );
              setBusy(false);
              if (r) setMade(r);
            }
      }
      footer={
        made ? (
          <Button size="sm" variant="secondary" onClick={close}>
            Close
          </Button>
        ) : (
          <>
            <Button variant="ghost" size="sm" onClick={close}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={busy}>
              {busy ? "Preparing…" : "Get the command"}
            </Button>
          </>
        )
      }
    >
      {made ? (
        <AgentInstall
          token={made.token}
          command={made.command}
          thisMachine={localFree && where === "local"}
        />
      ) : localFree ? (
        <Segmented
          label="Where it runs"
          value={where}
          onChange={setWhere}
          options={[
            { value: "local", label: "This machine" },
            { value: "other", label: "Another machine" },
          ]}
        />
      ) : null}
    </Dialog>
  );
}
