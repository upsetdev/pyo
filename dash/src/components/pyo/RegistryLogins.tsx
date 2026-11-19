import { PackageIcon, PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { BrandMark, DOCKER_LOGO } from "~/components/pyo/common";
import {
  Busy,
  ConfirmNote,
  DialogRow,
  DialogRows,
  IntegrationDialog,
} from "~/components/pyo/IntegrationDialog";
import { AppLink } from "~/components/ui/AppLink";
import { registryIcon } from "~/components/pyo/ProjectCard";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Field, Input, SecretInput, Select } from "~/components/ui/Field";
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

const ICON = <BrandMark icon={DOCKER_LOGO} size={15} />;

function LoginRow({ l }: { l: RegistryLogin }) {
  const [confirming, setConfirming] = useState(false);
  const p = preset(l.host);
  const icon = registryIcon(l.host);
  return (
    <DialogRow
      icon={icon ? <BrandMark icon={icon} size={15} /> : <PackageIcon size={15} />}
      title={p?.name ?? l.host}
      meta={`${l.username} · ${l.lastUsedAt ? `used ${timeAgo(l.lastUsedAt)}` : "not used yet"}`}
      badge={l.verified ? <Badge tone="success">Logged in</Badge> : <Badge>Not checked</Badge>}
      actions={
        confirming ? null : (
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            aria-label={`Remove the login for ${l.host}`}
            onClick={() => setConfirming(true)}
          >
            <TrashIcon size={14} />
          </Button>
        )
      }
    >
      {confirming ? (
        <ConfirmNote
          title={`Remove the login for ${l.host}?`}
          confirmLabel="Remove login"
          onCancel={() => setConfirming(false)}
          onConfirm={async () => {
            setConfirming(false);
            if (await api.removeRegistryLogin(l.id)) toast({ title: `Removed the login for ${l.host}` });
          }}
        >
          Its private images can't redeploy until you add a login.
        </ConfirmNote>
      ) : null}
    </DialogRow>
  );
}

/**
 * Integrations › Container registries: the built-in registry, the team's
 * logins, and a form to add one. Opens on the form while there are none.
 */
export function RegistriesDialog({ onClose }: { onClose: () => void }) {
  const s = usePyo();
  const logins = s.registryLogins.filter((l) => l.teamId === s.currentTeamId);
  const [adding, setAdding] = useState(!logins.length);
  const [kind, setKind] = useState<string>("ghcr.io");
  const [other, setOther] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [saving, setSaving] = useState(false);
  const p = preset(kind);
  const host = kind === "other" ? other.trim() : kind;
  const ready = !!host && !!username.trim() && !!password;
  const toList = () => {
    setUsername("");
    setPassword("");
    setOther("");
    setAdding(false);
  };
  const back = () => (logins.length ? toList() : onClose());

  if (adding)
    return (
      <IntegrationDialog
        icon={ICON}
        name="Container registries"
        description="Add a login. Services pull private images with it."
        docs="registries"
        onClose={onClose}
        onSubmit={async () => {
          if (!ready || saving) return;
          setSaving(true);
          const r = await api.saveRegistryLogin({ host, username: username.trim(), password });
          setSaving(false);
          if (!r) return;
          toast({
            title: `Saved the login for ${r.host}`,
            description: r.verified
              ? "The registry accepted it."
              : "Couldn't reach the registry to check it. It's used on the next deploy.",
          });
          toList();
        }}
        footer={
          <>
            <Button variant="ghost" size="sm" onClick={back}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={!ready || saving}>
              {saving ? <Busy>Checking…</Busy> : "Save login"}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          <Field label="Registry">
            {(f) => (
              <Select {...f} value={kind} onChange={(e) => setKind(e.target.value)}>
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
            <Field label="Registry address" hint="As in image names, e.g. registry.example.com.">
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
          <Field label="Password or access token" hint={p?.hint ?? "Read access to its images is enough."}>
            {(f) => (
              <SecretInput
                {...f}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            )}
          </Field>
        </div>
      </IntegrationDialog>
    );

  return (
    <IntegrationDialog
      icon={ICON}
      name="Container registries"
      description="Logins for private images. Services pull with them."
      docs="registries"
      onClose={onClose}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => setAdding(true)}>
            <PlusIcon size={12} weight="bold" /> Add login
          </Button>
          <Button size="sm" onClick={onClose}>
            Done
          </Button>
        </>
      }
    >
      <DialogRows>
        <DialogRow
          icon={<PackageIcon size={15} />}
          title="Pyo registry (built in)"
          meta="Images Pyo builds from Git."
          actions={
            <AppLink href="/registry" className="px-2 text-[12px] font-medium text-primary hover:text-primary/80">
              Open
            </AppLink>
          }
        />
        {logins.map((l) => (
          <LoginRow key={l.id} l={l} />
        ))}
      </DialogRows>
    </IntegrationDialog>
  );
}
