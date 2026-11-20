import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { serviceSettingsNav, SettingsFrame } from "~/components/pyo/SettingsFrame";
import { VolumeTable } from "~/components/pyo/tables";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Select } from "~/components/ui/Field";
import { Callout, Panel } from "~/components/ui/Panel";
import { api, usePyo, serviceIdOf } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, Volume } from "~/lib/pyo/types";

export const Route = createFileRoute("/_app/$team/$projectId_/$serviceId/volumes")({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const app = usePyo().services.find((a) => a.id === serviceId);
    return app ? (
      <SettingsFrame items={serviceSettingsNav(app.id)}>
        <Volumes key={app.id} app={app} />
      </SettingsFrame>
    ) : null;
  },
});

// Folders of an app that keep their files across restarts and deploys.

const SIZES = [1, 5, 10, 20, 50, 100];

function Volumes({ app }: { app: Application }) {
  const s = usePyo();
  const volumes = s.volumes.filter((v) => v.serviceId === app.id);
  // A first volume is usually /data; after that, no guess.
  const [name, setName] = useState(volumes.length ? "" : "data");
  const [path, setPath] = useState(volumes.length ? "" : "/data");
  const [size, setSize] = useState(5);
  const [saving, setSaving] = useState(false);
  const [removing, setRemoving] = useState<Volume | null>(null);
  const pathError =
    !/^\/[^\s]*[^/\s]$/.test(path.trim()) || path.includes("..")
      ? "An absolute folder path, e.g. /data."
      : volumes.some((v) => v.mountPath === path.trim())
        ? "Another volume uses this folder."
        : "";
  const nameError = !/^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(name.trim())
    ? "Lowercase letters, digits and dashes."
    : "";

  return (
    <div className="space-y-4">
      <Panel title="Volumes">
        <VolumeTable
          s={s}
          volumes={volumes}
          showCluster={false}
          showService={false}
          onDetach={setRemoving}
          empty="Otherwise files are lost on restart."
        />
        <form
          className="grid gap-3 border-t border-border p-4 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_110px_auto] sm:items-end"
          noValidate
          onSubmit={async (e) => {
            e.preventDefault();
            if (pathError || nameError || saving) return;
            setSaving(true);
            const ok = await api.addVolume(app.id, {
              name: name.trim(),
              mountPath: path.trim(),
              sizeGi: size,
            });
            setSaving(false);
            if (!ok) return;
            toast({
              title: `Added ${path.trim()}`,
              description: "Deploy to create and mount it.",
            });
            setName("");
            setPath("");
          }}
        >
          <Field label="Name" error={name && nameError ? nameError : undefined}>
            {(p) => (
              <Input
                {...p}
                mono
                value={name}
                onChange={(e) => setName(e.target.value.toLowerCase())}
                placeholder="data"
              />
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
          <Field label="Size">
            {(p) => (
              <Select
                {...p}
                value={size}
                onChange={(e) => setSize(Number(e.target.value))}
              >
                {SIZES.map((g) => (
                  <option key={g} value={g}>
                    {g} GB
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <Button
            type="submit"
            size="sm"
            variant="secondary"
            disabled={!name || !path || !!pathError || !!nameError || saving}
            className="sm:mb-px"
          >
            {saving ? "Adding…" : "Add volume"}
          </Button>
        </form>
      </Panel>

      {volumes.length && app.replicas > 1 ? (
        <Callout tone="warning">
          Volumes need 1 replica.
        </Callout>
      ) : null}

      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Detach ${removing?.mountPath}?`}
        description="Detached on the next deploy. Data stays."
        confirmLabel="Detach"
        tone="primary"
        onConfirm={async () => {
          if (!removing) return;
          if (await api.detachVolume(removing.id))
            toast({
              title: `Detached ${removing.mountPath}`,
              description: "Deploy to apply it.",
            });
        }}
      />
    </div>
  );
}
