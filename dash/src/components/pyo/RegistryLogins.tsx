import { PackageIcon, PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { BrandMark } from "~/components/pyo/common";
import { AppLink } from "~/components/ui/AppLink";
import { registryIcon } from "~/components/pyo/ProjectCard";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, SecretInput, Select } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { RegistryLogin } from "~/lib/pyo/types";

// Logins for private container registries. A service whose image is on a
// registry with a login pulls with it, with nothing to set on the app.

const PRESETS = [
  {
    host: "ghcr.io",
    name: "GitHub (ghcr.io)",
    user: "Your GitHub username",
    hint: "A classic token with read:packages.",
  },
  {
    host: "docker.io",
    name: "Docker Hub",
    user: "Your Docker Hub username",
    hint: "A read-only access token.",
  },
  {
    host: "registry.gitlab.com",
    name: "GitLab",
    user: "Your GitLab username",
    hint: "A token with read_registry.",
  },
  {
    host: "quay.io",
    name: "Quay",
    user: "Robot account or username",
    hint: "A robot token with read access.",
  },
] as const;

const preset = (host: string) => PRESETS.find((p) => p.host === host);

export function RegistryLoginsPanel() {
  const s = usePyo();
  const logins = s.registryLogins.filter((l) => l.teamId === s.currentTeamId);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<RegistryLogin | null>(null);
  return (
    <Panel
      title="Container registries"
      actions={
        <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
          <PlusIcon size={12} weight="bold" /> Add login
        </Button>
      }
    >
      <ul className="list-rows">
        <li className="flex flex-wrap items-center gap-3 px-4 py-3">
          <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
            <PackageIcon size={15} />
          </span>
          <div className="min-w-0 flex-1">
            <p className="text-[13px] font-medium">Pyo registry (built in)</p>
            <p className="truncate text-[12px] text-muted-foreground">Images Pyo builds from Git.</p>
          </div>
          <AppLink href="/registry" className="text-[12.5px] font-medium text-primary hover:text-primary/80">
            Open
          </AppLink>
        </li>
      </ul>
      {logins.length ? (
        <ul className="list-rows border-t border-border">
          {logins.map((l) => {
            const p = preset(l.host);
            const icon = registryIcon(l.host);
            return (
              <li
                key={l.id}
                className="flex flex-wrap items-center gap-3 px-4 py-3"
              >
                <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
                  {icon ? (
                    <BrandMark icon={icon} size={15} />
                  ) : (
                    <PackageIcon size={15} />
                  )}
                </span>
                <div className="min-w-0 flex-1">
                  <p className="text-[13px] font-medium">{p?.name ?? l.host}</p>
                  <p className="truncate text-[12px] text-muted-foreground">
                    {l.username} ·{" "}
                    {l.lastUsedAt
                      ? `used ${timeAgo(l.lastUsedAt)}`
                      : "not used yet"}
                  </p>
                </div>
                {l.verified ? (
                  <Badge tone="success">Logged in</Badge>
                ) : (
                  <Badge>Not checked</Badge>
                )}
                <Button
                  variant="ghost"
                  size="icon"
                  aria-label={`Remove the login for ${l.host}`}
                  onClick={() => setRemoving(l)}
                >
                  <TrashIcon size={14} />
                </Button>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="px-4 py-6 text-center text-[13px] text-muted-foreground">
          No logins yet. Public images don't need one.
        </p>
      )}
      <AddLoginDialog open={adding} onClose={() => setAdding(false)} />
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove the login for ${removing?.host}?`}
        description="Its private images can't redeploy until you add a login."
        confirmLabel="Remove login"
        onConfirm={async () => {
          if (removing && (await api.removeRegistryLogin(removing.id)))
            toast({ title: `Removed the login for ${removing.host}` });
        }}
      />
    </Panel>
  );
}

function AddLoginDialog({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const [kind, setKind] = useState<string>("ghcr.io");
  const [other, setOther] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [saving, setSaving] = useState(false);
  const p = preset(kind);
  const host = kind === "other" ? other.trim() : kind;
  const close = () => {
    setUsername("");
    setPassword("");
    setOther("");
    onClose();
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      title="Add a registry login"
      onSubmit={async () => {
        if (!host || !username.trim() || !password || saving) return;
        setSaving(true);
        const r = await api.saveRegistryLogin({
          host,
          username: username.trim(),
          password,
        });
        setSaving(false);
        if (!r) return;
        toast({
          title: `Saved the login for ${r.host}`,
          description: r.verified
            ? "The registry accepted it."
            : "Couldn't reach the registry to check it. It's used on the next deploy.",
        });
        close();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button
            type="submit"
            size="sm"
            disabled={!host || !username.trim() || !password || saving}
          >
            {saving ? "Checking…" : "Save login"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Field label="Registry">
          {(f) => (
            <Select
              {...f}
              value={kind}
              onChange={(e) => setKind(e.target.value)}
            >
              {PRESETS.map((x) => (
                <option key={x.host} value={x.host}>
                  {x.name}
                </option>
              ))}
              <option value="other">Another registry</option>
            </Select>
          )}
        </Field>
        {kind === "other" ? (
          <Field
            label="Registry address"
            hint="As in image names, e.g. registry.example.com."
          >
            {(f) => (
              <Input
                {...f}
                mono
                value={other}
                onChange={(e) => setOther(e.target.value)}
                placeholder="registry.example.com"
              />
            )}
          </Field>
        ) : null}
        <Field label="Username">
          {(f) => (
            <Input
              {...f}
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder={p?.user ?? "Username"}
              autoComplete="off"
            />
          )}
        </Field>
        <Field
          label="Password or access token"
          hint={p?.hint ?? "Read access to its images is enough."}
        >
          {(f) => (
            <SecretInput
              {...f}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          )}
        </Field>
      </div>
    </Dialog>
  );
}
