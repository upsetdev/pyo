import { SpinnerGapIcon, TrashIcon } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import {
  Checkbox,
  Field,
  Input,
  Select,
  Stepper,
} from "~/components/ui/Field";
import { Meter } from "~/components/ui/Panel";
import { deleteVolumeSnapshot, fetchVolumeSnapshots } from "~/lib/pyo/backend";
import { dateTime, gib, timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  Application,
  Project,
  StrayVolume,
  Volume,
  VolumeSnapshot,
} from "~/lib/pyo/types";
import { formatBytes } from "./BackupHeatmap";

// Volumes after they're made: attach a detached one (to another project's
// service too), restore one from a backup, grow one, delete one for good,
// and the choice to delete volumes with their service or project.

/** The server a volume's data is on, when its class keeps it on one server. */
export function pinnedServer(
  s: ReturnType<typeof usePyo>,
  v: Volume,
): string | undefined {
  if (!v.server) return undefined;
  const cluster = s.clusters.find((c) => c.id === v.clusterId);
  const sc = cluster?.capabilities.storageClasses.find(
    (c) => c.name === v.storageClass,
  );
  return sc?.replicated ? undefined : v.server;
}

/** "2 volumes · 15 GB" for detached ones; empty when none. */
export function detachedSummary(volumes: Volume[]) {
  const d = volumes.filter((v) => !v.serviceId && !v.deleting);
  if (!d.length) return "";
  const size = d.reduce((n, v) => n + v.sizeGi, 0);
  return `${d.length} detached · ${gib(size)}`;
}

/** "Deleted project" when its project is gone; else the project's name. */
export function volumeProject(s: ReturnType<typeof usePyo>, v: Volume) {
  return s.projects.find((p) => p.id === v.projectId);
}

/**
 * Attach a detached volume to a service on its cluster: in its project, or
 * another one (Pyo moves it there first, data and all).
 */
export function AttachVolumeDialog({
  volume,
  onClose,
}: {
  volume: Volume | null;
  onClose: () => void;
}) {
  const s = usePyo();
  // Services on its cluster, by project: a volume can't change cluster.
  const onCluster = volume
    ? s.services.filter(
        (a) => a.clusterId === volume.clusterId && a.teamId === volume.teamId,
      )
    : [];
  const projects = s.projects.filter((p) =>
    onCluster.some((a) => a.projectId === p.id),
  );
  const [projectId, setProjectId] = useState("");
  const [serviceId, setServiceId] = useState("");
  const [path, setPath] = useState("");
  const [saving, setSaving] = useState(false);
  const services = onCluster.filter((a) => a.projectId === projectId);
  useEffect(() => {
    if (!volume) return;
    const own = projects.some((p) => p.id === volume.projectId);
    const first = own ? volume.projectId! : (projects[0]?.id ?? "");
    setProjectId(first);
    setServiceId(onCluster.find((a) => a.projectId === first)?.id ?? "");
    setPath(volume.mountPath ?? "/data");
    // Only when another volume is picked.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [volume?.id]);
  const moves = !!volume && projectId !== volume.projectId;
  const pathError =
    !/^\/[^\s]*[^/\s]$/.test(path.trim()) || path.includes("..")
      ? "An absolute folder path, e.g. /data."
      : s.volumes.some(
            (v) => v.serviceId === serviceId && v.mountPath === path.trim(),
          )
        ? "Another volume uses this folder."
        : "";
  const nameTaken =
    moves &&
    s.volumes.some(
      (v) =>
        v.id !== volume?.id &&
        v.projectId === projectId &&
        v.name === volume?.name,
    );
  return (
    <Dialog
      open={!!volume}
      onClose={onClose}
      title={`Attach ${volume?.name ?? "volume"}`}
      onSubmit={async (e) => {
        e.preventDefault();
        if (!volume || !serviceId || pathError || nameTaken || saving) return;
        setSaving(true);
        const res = await api.attachVolume(volume.id, serviceId, path.trim());
        setSaving(false);
        if (!res) return;
        toast(
          res.moving
            ? { title: `Moving ${volume.name}`, description: "Deploy once it's attached." }
            : { title: `Attached ${volume.name}`, description: "Deploy to mount it." },
        );
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button
            type="submit"
            size="sm"
            disabled={!serviceId || !!pathError || nameTaken || saving}
          >
            {saving ? "Attaching…" : "Attach"}
          </Button>
        </>
      }
    >
      {projects.length ? (
        <div className="space-y-3">
          <Field
            label="Project"
            hint={
              nameTaken
                ? undefined
                : moves
                  ? "Moves the volume into this project, data and backups kept."
                  : "Projects with services on its cluster."
            }
            error={
              nameTaken
                ? `This project already has a volume named ${volume?.name}.`
                : undefined
            }
          >
            {(p) => (
              <Select
                {...p}
                value={projectId}
                onChange={(e) => {
                  setProjectId(e.target.value);
                  setServiceId(
                    onCluster.find((a) => a.projectId === e.target.value)?.id ??
                      "",
                  );
                }}
              >
                {projects.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <Field label="Service">
            {(p) => (
              <Select
                {...p}
                value={serviceId}
                onChange={(e) => setServiceId(e.target.value)}
              >
                {services.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <Field
            label="Folder in the service"
            error={path && pathError ? pathError : undefined}
          >
            {(p) => (
              <Input
                {...p}
                mono
                value={path}
                onChange={(e) => setPath(e.target.value.trim())}
                placeholder="/data"
              />
            )}
          </Field>
        </div>
      ) : (
        <p className="text-[13px] text-muted-foreground">
          No service on its cluster.
        </p>
      )}
    </Dialog>
  );
}

/** Pick one of a volume's snapshots, confirm, and Pyo restores it; or
 * delete one (the dialog is for owners and admins). */
export function RestoreVolumeDialog({
  volume,
  onClose,
}: {
  volume: Volume | null;
  onClose: () => void;
}) {
  const s = usePyo();
  const [list, setList] = useState<VolumeSnapshot[] | null>(null);
  const [error, setError] = useState("");
  const [picked, setPicked] = useState("");
  const [confirming, setConfirming] = useState(false);
  const [saving, setSaving] = useState(false);
  const [forgetting, setForgetting] = useState<VolumeSnapshot | null>(null);
  const [deleting, setDeleting] = useState<string[]>([]);
  const app = s.services.find((a) => a.id === volume?.serviceId);
  const forget = async (x: VolumeSnapshot) => {
    if (!volume) return;
    setDeleting((d) => [...d, x.id]);
    try {
      await deleteVolumeSnapshot(volume.id, x.id);
      setList((l) => l?.filter((y) => y.id !== x.id) ?? l);
      setPicked((p) => (p === x.id ? "" : p));
      toast({ title: "Backup deleted" });
    } catch (e) {
      toast.error("Couldn't delete the backup", {
        description: e instanceof Error ? e.message : undefined,
      });
    } finally {
      setDeleting((d) => d.filter((id) => id !== x.id));
    }
  };
  useEffect(() => {
    if (!volume) return;
    let live = true;
    setList(null);
    setError("");
    setConfirming(false);
    fetchVolumeSnapshots(volume.id)
      .then((r) => {
        if (!live) return;
        setList(r.snapshots);
        setPicked(r.snapshots[0]?.id ?? "");
      })
      .catch((e: unknown) => {
        if (live) setError(e instanceof Error ? e.message : "Couldn't list its backups.");
      });
    return () => {
      live = false;
    };
  }, [volume?.id]);
  const snap = list?.find((x) => x.id === picked);
  // The confirm step starts on the dialog, not on the Restore button that
  // Continue turned into (Enter twice would restore).
  const back = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (confirming) back.current?.closest("dialog")?.focus({ preventScroll: true });
  }, [confirming]);
  return (
    <>
      <Dialog
        open={!!volume}
        onClose={onClose}
        title={`Restore ${volume?.name ?? "volume"}`}
        onSubmit={async (e) => {
          e.preventDefault();
          if (!volume || !snap || saving) return;
          if (!confirming) return setConfirming(true);
          setSaving(true);
          const ok = await api.restoreVolume(volume.id, snap.id);
          setSaving(false);
          if (!ok) return;
          toast({ title: `Restoring ${volume.name}` });
          onClose();
        }}
        footer={
          <>
            <Button
              ref={back}
              variant="ghost"
              size="sm"
              onClick={confirming ? () => setConfirming(false) : onClose}
            >
              {confirming ? "Back" : "Cancel"}
            </Button>
            <Button
              type="submit"
              size="sm"
              variant={confirming ? "danger" : "primary"}
              disabled={!snap || saving}
            >
              {saving ? "Restoring…" : confirming ? "Restore" : "Continue"}
            </Button>
          </>
        }
      >
        {confirming && snap ? (
          <div className="space-y-2 text-[13px] leading-5 text-muted-foreground">
            <p>
              Replaces the data in{" "}
              <span className="font-mono text-foreground">{volume?.name}</span>{" "}
              with the backup from{" "}
              <span className="text-foreground">{dateTime(snap.time)}</span>.
            </p>
            {app ? (
              <p>
                <span className="text-foreground">{app.name}</span> stops while
                it runs and starts again after.
              </p>
            ) : null}
          </div>
        ) : error ? (
          <p className="text-[13px] text-destructive">{error}</p>
        ) : !list ? (
          <div className="flex items-center gap-2 text-[13px] text-muted-foreground">
            <SpinnerGapIcon size={14} className="animate-spin" />
            Reading backups from the cluster…
          </div>
        ) : !list.length ? (
          <p className="text-[13px] text-muted-foreground">
            No backups of this volume yet.
          </p>
        ) : (
          <fieldset className="space-y-1.5">
            <legend className="sr-only">Backup</legend>
            {list.map((x, i) => (
              <label
                key={x.id}
                className={`flex cursor-pointer items-center gap-3 rounded-lg border px-3 py-2 text-[13px] transition-colors ${picked === x.id ? "border-primary bg-primary/5" : "border-border hover:bg-muted/50"}`}
              >
                <input
                  type="radio"
                  name="snapshot"
                  className="accent-[var(--color-primary)]"
                  checked={picked === x.id}
                  onChange={() => setPicked(x.id)}
                />
                <span className="min-w-0 flex-1">
                  <span className="text-foreground">{dateTime(x.time)}</span>
                  <span className="text-muted-foreground">
                    {" "}
                    · {timeAgo(x.time)}
                    {i === 0 ? " · latest" : ""}
                  </span>
                </span>
                <span className="shrink-0 tabular-nums text-muted-foreground">
                  {x.sizeBytes != null ? formatBytes(x.sizeBytes) : "—"}
                </span>
                <span className="hidden shrink-0 font-mono text-[11px] text-muted-foreground sm:inline">
                  {x.shortId ?? x.id.slice(0, 8)}
                </span>
                <button
                  type="button"
                  aria-label={`Delete the backup from ${dateTime(x.time)}`}
                  title="Delete backup"
                  disabled={deleting.includes(x.id)}
                  className="-mr-1 shrink-0 rounded p-1 text-muted-foreground hover:bg-muted hover:text-destructive disabled:opacity-60"
                  onClick={(e) => {
                    e.preventDefault();
                    setForgetting(x);
                  }}
                >
                  {deleting.includes(x.id) ? (
                    <SpinnerGapIcon size={14} className="animate-spin" />
                  ) : (
                    <TrashIcon size={14} />
                  )}
                </button>
              </label>
            ))}
          </fieldset>
        )}
      </Dialog>
      <ConfirmDialog
        open={!!forgetting}
        onClose={() => setForgetting(null)}
        title="Delete backup?"
        description={
          forgetting
            ? `Deletes the backup from ${dateTime(forgetting.time)}. This can't be undone.`
            : ""
        }
        confirmLabel="Delete backup"
        onConfirm={() => {
          if (forgetting) void forget(forgetting);
        }}
      />
    </>
  );
}

/** Grow a volume (never shrink), on a class that can. */
export function ResizeVolumeDialog({
  volume,
  onClose,
}: {
  volume: Volume | null;
  onClose: () => void;
}) {
  const now = volume?.resizeTo ?? volume?.sizeGi ?? 1;
  const [size, setSize] = useState(now + 1);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    if (volume) setSize(Math.min(1000, now + Math.max(1, Math.ceil(now / 2))));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [volume?.id]);
  return (
    <Dialog
      open={!!volume}
      onClose={onClose}
      title={`Resize ${volume?.name ?? "volume"}`}
      onSubmit={async (e) => {
        e.preventDefault();
        if (!volume || size <= now || saving) return;
        setSaving(true);
        const ok = await api.resizeVolume(volume.id, size);
        setSaving(false);
        if (!ok) return;
        toast({ title: `Growing ${volume.name} to ${size} GB` });
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={size <= now || saving}>
            {saving ? "Resizing…" : "Resize"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {volume?.usedGi !== undefined ? (
          <div className="space-y-1">
            <Meter
              value={volume.usedGi}
              max={volume.sizeGi}
              label={`${volume.name} usage`}
            />
            <p className="text-[12px] text-muted-foreground">
              {gib(volume.usedGi)} used of {gib(volume.sizeGi)}
            </p>
          </div>
        ) : null}
        <Field label="Size (GB)" hint={`Now ${now} GB. Volumes only grow.`}>
          {(p) => (
            <Stepper
              {...p}
              value={size}
              onChange={setSize}
              min={now + 1}
              max={1000}
              suffix="GB"
              className="w-40"
            />
          )}
        </Field>
      </div>
    </Dialog>
  );
}

/** Why a volume can't grow on its class; empty when it can. */
export function resizeRefusal(
  s: ReturnType<typeof usePyo>,
  v: Volume,
): string {
  const sc = s.clusters
    .find((c) => c.id === v.clusterId)
    ?.capabilities.storageClasses.find((c) => c.name === v.storageClass);
  if (!sc) return "Its storage class isn't reported.";
  if (sc.provisioner === "rancher.io/local-path")
    return "local-path has no size limit: it uses its server's free disk.";
  if (!sc.allowVolumeExpansion) return `${sc.name} can't grow volumes.`;
  return "";
}

/** Delete a detached volume, or one on a cluster that Pyo has no record of. */
export function DeleteVolumeDialog({
  volume,
  stray,
  onClose,
}: {
  volume?: Volume | null;
  stray?: { clusterId: string; volume: StrayVolume } | null;
  onClose: () => void;
}) {
  const s = usePyo();
  const name = volume?.name ?? stray?.volume.name ?? "";
  const server = volume ? pinnedServer(s, volume) : stray?.volume.server;
  return (
    <ConfirmDialog
      open={!!volume || !!stray}
      onClose={onClose}
      title={`Delete ${name}?`}
      description={
        server
          ? `Deletes the data on ${server}. This can't be undone.`
          : "Deletes its data. This can't be undone."
      }
      confirmText={name}
      confirmLabel="Delete volume"
      onConfirm={async () => {
        const ok = volume
          ? await api.deleteVolume(volume.id)
          : stray
            ? await api.deleteVolume(stray.volume.volumeId, stray.clusterId)
            : null;
        if (ok) toast({ title: `Deleting ${name}` });
      }}
    />
  );
}

/** Its volumes: kept (listed) or, when asked, deleted too. */
function VolumesChoice({
  volumes,
  remove,
  setRemove,
  label,
}: {
  volumes: Volume[];
  remove: boolean;
  setRemove: (v: boolean) => void;
  label: string;
}) {
  if (!volumes.length) return null;
  return (
    <div className="space-y-1.5">
      <Checkbox checked={remove} onChange={setRemove} label={label} />
      <p className="text-[12px]">
        {remove
          ? "Their data is deleted for good."
          : `Kept, detached: ${volumes.map((v) => v.name).join(", ")}.`}
      </p>
    </div>
  );
}

export function DeleteServiceDialog({
  app,
  open,
  onClose,
  onDeleted,
}: {
  app: Application | null | undefined;
  open: boolean;
  onClose: () => void;
  onDeleted?: () => void;
}) {
  const s = usePyo();
  const [remove, setRemove] = useState(false);
  useEffect(() => {
    if (open) setRemove(false);
  }, [open]);
  const volumes = app ? s.volumes.filter((v) => v.serviceId === app.id) : [];
  return (
    <ConfirmDialog
      open={open}
      onClose={onClose}
      title={`Delete ${app?.name ?? "service"}?`}
      description="This can't be undone."
      confirmText={app?.name}
      confirmLabel="Delete"
      onConfirm={() => {
        if (!app) return;
        void api.deleteService(app.id, remove && volumes.length > 0).then((ok) => {
          if (!ok) return;
          toast({ title: `Deleted ${app.name}` });
          onDeleted?.();
        });
      }}
    >
      <VolumesChoice
        volumes={volumes}
        remove={remove}
        setRemove={setRemove}
        label="Also delete its volumes"
      />
    </ConfirmDialog>
  );
}

export function DeleteProjectDialog({
  project,
  services,
  open,
  onClose,
  onDeleted,
}: {
  project: Project | null | undefined;
  services: number;
  open: boolean;
  onClose: () => void;
  onDeleted?: () => void;
}) {
  const s = usePyo();
  const [remove, setRemove] = useState(false);
  useEffect(() => {
    if (open) setRemove(false);
  }, [open]);
  const volumes = project
    ? s.volumes.filter((v) => v.projectId === project.id && !v.deleting)
    : [];
  return (
    <ConfirmDialog
      open={open}
      onClose={onClose}
      title={`Delete ${project?.name ?? "project"}?`}
      description={
        services
          ? `Removes the project and its ${services === 1 ? "1 service" : `${services} services`}.`
          : "Removes the project."
      }
      confirmText={project?.namespace}
      confirmLabel="Delete project"
      onConfirm={() => {
        if (!project) return;
        void api.deleteProject(project.id, remove && volumes.length > 0).then((ok) => {
          if (!ok) return;
          toast({ title: `Deleted ${project.name}` });
          onDeleted?.();
        });
      }}
    >
      <VolumesChoice
        volumes={volumes}
        remove={remove}
        setRemove={setRemove}
        label="Also delete volumes"
      />
    </ConfirmDialog>
  );
}
