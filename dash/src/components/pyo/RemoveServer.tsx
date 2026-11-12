import {
  ArrowsClockwiseIcon,
  CheckCircleIcon,
  TrashIcon,
  WarningCircleIcon,
} from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Callout } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { CopyButton } from "~/components/ui/Table";
import { cpu, mem, timeAgo } from "~/lib/pyo/format";
import { parseLocation } from "~/lib/pyo/location";
import { api, serversOf, usePyo, type PyoState } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, ClusterServer } from "~/lib/pyo/types";

/** Run on the machine to finish a drained server's removal. */
export const UNINSTALL_COMMAND = "sudo /usr/local/bin/k3s-agent-uninstall.sh";

/** Does a service pinned to `pin` ("CC" or "City, CC") run in `loc`? */
function inPlace(pin: string, loc?: string) {
  const a = parseLocation(pin);
  const b = parseLocation(loc ?? "");
  return (
    !!a.country &&
    a.country === b.country &&
    (!a.city || a.city.toLowerCase() === b.city?.toLowerCase())
  );
}

/**
 * What removing a server means, from what's on it. The API and the agent
 * check the same things; this says it before you confirm.
 */
export function removalCheck(s: PyoState, n: ClusterServer) {
  const others = serversOf(s, n.clusterId).filter(
    (x) => x.id !== n.id && !x.removal,
  );
  const online = n.status === "ready";
  const blocked = n.roles.includes("control-plane")
    ? `${n.name} runs the cluster itself, so it can't be removed. Delete the cluster instead.`
    : !others.length
      ? `${n.name} is the cluster's only server. Delete the cluster instead.`
      : undefined;
  const replicasOn = (a: Application) =>
    (a.runtime.replicas ?? []).filter((r) => r.server === n.name).length;
  const running = s.services.filter(
    (a) => a.clusterId === n.clusterId && replicasOn(a) > 0,
  );
  const withData = running.filter((a) =>
    s.volumes.some((v) => v.serviceId === a.id),
  );
  const ready = others.filter((x) => x.status === "ready");
  const pinned = s.services.filter(
    (a) =>
      a.clusterId === n.clusterId &&
      a.location &&
      inPlace(a.location, n.location) &&
      !ready.some((x) => inPlace(a.location!, x.location)),
  );
  const moving = running.filter(
    (a) => !withData.includes(a) && !pinned.includes(a),
  );
  const builders = s.builders.filter(
    (b) =>
      b.kind === "cluster" && b.clusterId === n.clusterId && b.server === n.name,
  );
  // What the moving services ask for, against what's free on the rest.
  const need = moving.reduce(
    (t, a) => ({
      cpu: t.cpu + (a.resources.cpuRequest / 1000) * replicasOn(a),
      mem: t.mem + (a.resources.memoryRequest / 1024) * replicasOn(a),
    }),
    { cpu: 0, mem: 0 },
  );
  const free = ready
    .filter((x) => x.schedulable)
    .reduce(
      (t, x) => ({
        cpu: t.cpu + Math.max(0, x.capacity.cpu - x.usage.cpu),
        mem: t.mem + Math.max(0, x.capacity.memoryGi - x.usage.memoryGi),
      }),
      { cpu: 0, mem: 0 },
    );
  const tight = need.cpu > free.cpu || need.mem > free.mem;
  return {
    online,
    blocked,
    moving,
    withData,
    pinned,
    builders,
    need,
    free,
    tight,
  };
}

/** Confirm taking a server out of its cluster: gracefully, or by force when it's offline. */
export function RemoveServerDialog({
  node,
  onClose,
}: {
  node: ClusterServer | null;
  onClose: () => void;
}) {
  const s = usePyo();
  const [busy, setBusy] = useState(false);
  const n = node;
  const c = n ? removalCheck(s, n) : null;
  const force = !!c && !c.online;
  // Offline: what's on it is lost or restarts elsewhere anyway, so only control-plane/last-server block.
  const stop =
    c &&
    (c.blocked || (!force && (c.withData.length > 0 || c.pinned.length > 0)));
  // Names alone repeat across projects ("nginx" twice), so those get theirs.
  const names = (apps: Application[]) =>
    apps
      .map((a) =>
        apps.filter((x) => x.name === a.name).length > 1
          ? `${a.name} (${s.projects.find((p) => p.id === a.projectId)?.name ?? "?"})`
          : a.name,
      )
      .join(", ");
  return (
    <Dialog
      open={!!n}
      onClose={onClose}
      width="max-w-xl"
      title={n ? `Remove ${n.name}?` : ""}
      description={
        c?.blocked
          ? undefined
          : force
            ? "Offline: removed now. Services restart elsewhere."
            : "Services move elsewhere first."
      }
      onSubmit={async () => {
        if (!n || stop) return;
        setBusy(true);
        const r = await api.removeServer(n.id, force);
        setBusy(false);
        if (r) {
          toast({
            title: force ? `Removing ${n.name}` : `Draining ${n.name}`,
            description: force
              ? undefined
              : "Its services are moving to the other servers.",
          });
          onClose();
        }
      }}
      footer={
        c?.blocked ? (
          <Button variant="secondary" size="sm" onClick={onClose}>
            Close
          </Button>
        ) : (
          <>
            <Button variant="ghost" size="sm" onClick={onClose}>
              Cancel
            </Button>
            <Button
              type="submit"
              variant="danger"
              size="sm"
              disabled={!!stop || busy}
            >
              <TrashIcon size={13} />{" "}
              {busy ? "Removing…" : force ? "Remove by force" : "Remove server"}
            </Button>
          </>
        )
      }
    >
      {n && c ? (
        <div className="space-y-3 text-[13px]">
          {c.blocked ? <Callout tone="danger">{c.blocked}</Callout> : null}
          {!c.blocked && !force && c.withData.length ? (
            <Callout tone="danger">
              Their volumes keep data on {n.name}&apos;s disk, so they
              can&apos;t move. Move or delete them first:{" "}
              <strong>{names(c.withData)}</strong>.
            </Callout>
          ) : null}
          {!c.blocked && !force && c.pinned.length ? (
            <Callout tone="danger">
              Pinned to {n.location}, and {n.name} is the only server there.
              Change their location first: <strong>{names(c.pinned)}</strong>.
            </Callout>
          ) : null}
          {!c.blocked && force && c.withData.length ? (
            <Callout tone="danger">
              Data in these services&apos; volumes was on {n.name}&apos;s disk
              and is lost: <strong>{names(c.withData)}</strong>.
            </Callout>
          ) : null}
          {!c.blocked && force && c.pinned.length ? (
            <Callout tone="warning">
              Pinned to {n.location}, where no other server is: they wait, not
              running, until you change their location:{" "}
              <strong>{names(c.pinned)}</strong>.
            </Callout>
          ) : null}
          {!c.blocked ? (
            <ul className="list-rows rounded-lg border border-border">
              <Row
                label="Services that move"
                value={c.moving.length ? names(c.moving) : "None"}
              />
              {c.moving.length ? (
                <Row
                  label="They ask for"
                  value={
                    <span className={c.tight ? "text-warning" : undefined}>
                      {cpu(c.need.cpu * 1000)} and {mem(c.need.mem * 1024)};{" "}
                      {cpu(c.free.cpu * 1000)} and {mem(c.free.mem * 1024)} free
                      on the other servers
                      {c.tight ? ". Some may wait for room." : ""}
                    </span>
                  }
                />
              ) : null}
              {c.builders.length ? (
                <Row
                  label="Builders removed with it"
                  value={c.builders.map((b) => b.name).join(", ")}
                />
              ) : null}
            </ul>
          ) : null}
          {!c.blocked && !force ? (
            <p className="text-[12.5px] text-muted-foreground">
              Pyo&apos;s own system workloads on it move too. Nothing is
              deleted from the machine until you uninstall k3s there.
            </p>
          ) : null}
        </div>
      ) : null}
    </Dialog>
  );
}

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <li className="grid grid-cols-[9rem_minmax(0,1fr)] gap-3 px-3 py-2">
      <span className="text-muted-foreground">{label}</span>
      <span className="min-w-0 break-words">{value}</span>
    </li>
  );
}

/** Where a server's removal is, with what to do next. */
export function RemovalNote({ n }: { n: ClusterServer }) {
  const r = n.removal;
  const [busy, setBusy] = useState(false);
  if (!r) return null;
  const cancel = async () => {
    setBusy(true);
    if (await api.cancelServerRemoval(n.id))
      toast({
        title: `Kept ${n.name}`,
        description: "It takes workloads again.",
      });
    setBusy(false);
  };
  if (r.state === "draining")
    return (
      <StatusNote
        tone="progress"
        icon={<ArrowsClockwiseIcon size={15} className="animate-spin" />}
        title="Moving its services"
      >
        Pyo is moving {n.name}&apos;s services to the other servers
        {r.by
          ? `, as ${r.by} asked ${r.startedAt ? timeAgo(r.startedAt) : ""}`
          : ""}
        . This takes a minute or two.
      </StatusNote>
    );
  if (r.state === "removing")
    return (
      <StatusNote
        tone="progress"
        icon={<ArrowsClockwiseIcon size={15} className="animate-spin" />}
        title="Removing from the cluster"
      >
        {n.name} is being deleted from the cluster.
      </StatusNote>
    );
  if (r.state === "failed")
    return (
      <StatusNote
        tone="danger"
        icon={<WarningCircleIcon size={15} />}
        title="Couldn't remove it"
        action={
          <Button
            size="xs"
            variant="secondary"
            disabled={busy}
            onClick={() => void cancel()}
          >
            Keep server
          </Button>
        }
      >
        {r.message ?? "The removal failed."} It stays paused, so nothing new
        starts on it.
      </StatusNote>
    );
  return (
    <StatusNote
      tone="warning"
      icon={<CheckCircleIcon size={15} />}
      title="Drained: uninstall k3s on the machine to finish"
      action={
        <Button
          size="xs"
          variant="secondary"
          disabled={busy}
          onClick={() => void cancel()}
        >
          Keep server
        </Button>
      }
    >
      <span className="flex flex-col gap-1.5">
        Its services run on the other servers now. Run this on {n.name}. Once it
        goes offline, Pyo removes it from the cluster.
        <span className="inline-flex items-center gap-1">
          <code className="break-all font-mono text-[12px] text-foreground">
            {UNINSTALL_COMMAND}
          </code>
          <CopyButton value={UNINSTALL_COMMAND} label="Copy command" />
        </span>
      </span>
    </StatusNote>
  );
}
