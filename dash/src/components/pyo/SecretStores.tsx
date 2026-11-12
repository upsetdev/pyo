import { KeyIcon, PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, Select, Textarea } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  Application,
  SecretImport,
  SecretStore,
  SecretStoreKind,
} from "~/lib/pyo/types";

// External secret stores: connected per team under Integrations, and imported
// by each service under its environment variables.

export const STORE_KINDS: { value: SecretStoreKind; label: string }[] = [
  { value: "vault", label: "HashiCorp Vault" },
  { value: "openbao", label: "OpenBao" },
  { value: "infisical", label: "Infisical" },
  { value: "aws", label: "AWS Secrets Manager" },
  { value: "gcp", label: "GCP Secret Manager" },
  { value: "azure", label: "Azure Key Vault" },
  { value: "doppler", label: "Doppler" },
];

const kindLabel = (k: SecretStoreKind) =>
  STORE_KINDS.find((x) => x.value === k)?.label ?? k;

/** What a store's path means, for each provider. */
const PATHS: Record<SecretStoreKind, { label: string; placeholder: string }> = {
  vault: { label: "Path", placeholder: "app/production" },
  openbao: { label: "Path", placeholder: "app/production" },
  infisical: { label: "Folder", placeholder: "/backend" },
  aws: { label: "Secret name", placeholder: "app/production" },
  gcp: { label: "Secret name", placeholder: "app-production" },
  azure: { label: "Secret name", placeholder: "app-production" },
  doppler: { label: "Path", placeholder: "" },
};

type Form = {
  kind: SecretStoreKind;
  name: string;
  addr: string;
  mount: string;
  namespace: string;
  projectId: string;
  environment: string;
  region: string;
  project: string;
  config: string;
  token: string;
  clientId: string;
  clientSecret: string;
  tenantId: string;
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken: string;
  serviceAccount: string;
};

/** The credential as the API takes it, or "" until it's complete. */
function credentialOf(f: Form) {
  const t = (s: string) => s.trim();
  switch (f.kind) {
    case "infisical":
      return t(f.clientId) && t(f.clientSecret)
        ? JSON.stringify({
            clientId: t(f.clientId),
            clientSecret: t(f.clientSecret),
          })
        : "";
    case "aws":
      return t(f.accessKeyId) && t(f.secretAccessKey)
        ? JSON.stringify({
            accessKeyId: t(f.accessKeyId),
            secretAccessKey: t(f.secretAccessKey),
            sessionToken: t(f.sessionToken),
          })
        : "";
    case "azure":
      return t(f.tenantId) && t(f.clientId) && t(f.clientSecret)
        ? JSON.stringify({
            tenantId: t(f.tenantId),
            clientId: t(f.clientId),
            clientSecret: t(f.clientSecret),
          })
        : "";
    case "gcp":
      return t(f.serviceAccount);
    default:
      return t(f.token);
  }
}

function optionsOf(f: Form): Record<string, string> {
  const t = (s: string) => s.trim();
  switch (f.kind) {
    case "infisical":
      return { projectId: t(f.projectId), environment: t(f.environment) };
    case "aws":
      return { region: t(f.region) };
    case "gcp":
      return { project: t(f.project) };
    case "azure":
      return {};
    case "doppler":
      return { project: t(f.project), config: t(f.config) };
    default:
      return { mount: t(f.mount), namespace: t(f.namespace) };
  }
}

/** Connect a store, or change one. Its credential is never shown again. */
function StoreDialog({
  store,
  onClose,
}: {
  store?: SecretStore;
  onClose: () => void;
}) {
  const s = usePyo();
  const o = store?.options;
  const [form, setForm] = useState<Form>({
    kind: store?.kind ?? "vault",
    name: store?.name ?? "",
    addr: store?.addr ?? "",
    mount: o?.mount ?? "secret",
    namespace: o?.namespace ?? "",
    projectId: o?.projectId ?? "",
    environment: o?.environment ?? "prod",
    region: o?.region ?? "",
    project: o?.project ?? "",
    config: o?.config ?? "",
    token: "",
    clientId: "",
    clientSecret: "",
    tenantId: "",
    accessKeyId: "",
    secretAccessKey: "",
    sessionToken: "",
    serviceAccount: "",
  });
  const [busy, setBusy] = useState(false);
  const set = (patch: Partial<Form>) => setForm({ ...form, ...patch });
  const { kind } = form;
  const credential = credentialOf(form);
  // These live at one public address; Pyo works it out.
  const hasAddress = !["aws", "gcp", "doppler"].includes(kind);
  const ready =
    !!form.name.trim() &&
    (!hasAddress || !!form.addr.trim()) &&
    (kind !== "infisical" ||
      (!!form.projectId.trim() && !!form.environment.trim())) &&
    (kind !== "aws" || !!form.region.trim()) &&
    (!!store || !!credential);
  const unchanged = store ? "Unchanged" : "";
  const text = (
    label: string,
    key: keyof Form,
    placeholder: string,
    extra: { password?: boolean; hint?: string; optional?: boolean } = {},
  ) => (
    <Field label={label} hint={extra.hint} optional={extra.optional}>
      {(p) => (
        <Input
          {...p}
          type={extra.password ? "password" : "text"}
          autoComplete="off"
          value={form[key]}
          onChange={(e) => set({ [key]: e.target.value })}
          placeholder={placeholder}
        />
      )}
    </Field>
  );
  return (
    <Dialog
      open
      onClose={onClose}
      title={store ? `Edit ${store.name}` : "Connect a secret store"}
      description="Checked before saving."
      onSubmit={async () => {
        if (!ready || busy) return;
        setBusy(true);
        const ok = await api.saveSecretStore({
          id: store?.id,
          teamId: s.currentTeamId,
          kind,
          name: form.name.trim(),
          addr: hasAddress ? form.addr.trim() : "",
          options: optionsOf(form),
          credential: credential || undefined,
        });
        setBusy(false);
        if (!ok) return;
        toast({ title: store ? "Saved" : `Connected ${form.name.trim()}` });
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!ready || busy}>
            {busy ? "Checking…" : store ? "Save" : "Connect"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="grid grid-cols-2 gap-3">
          <Field label="Provider">
            {(p) => (
              <Select
                {...p}
                value={kind}
                disabled={!!store}
                onChange={(e) =>
                  set({ kind: e.target.value as SecretStoreKind })
                }
              >
                {STORE_KINDS.map((k) => (
                  <option key={k.value} value={k.value}>
                    {k.label}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          {text("Name", "name", "Production secrets")}
        </div>
        {kind === "vault" || kind === "openbao" ? (
          <>
            {text("Address", "addr", "https://vault.example.com")}
            <div className="grid grid-cols-2 gap-3">
              {text("KV v2 mount", "mount", "secret")}
              {text("Namespace", "namespace", "", { optional: true })}
            </div>
            {text("Token", "token", unchanged || "hvs.…", {
              password: true,
              hint: "Use a read-only token, renewed before it expires.",
            })}
          </>
        ) : null}
        {kind === "infisical" ? (
          <>
            {text("Address", "addr", "https://app.infisical.com")}
            <div className="grid grid-cols-2 gap-3">
              {text("Project ID", "projectId", "a1b2c3…")}
              {text("Environment", "environment", "prod")}
            </div>
            {text("Client ID", "clientId", unchanged, {
              hint: "From a machine identity with Universal Auth.",
            })}
            {text("Client secret", "clientSecret", unchanged, {
              password: true,
            })}
          </>
        ) : null}
        {kind === "aws" ? (
          <>
            {text("Region", "region", "us-east-1")}
            <div className="grid grid-cols-2 gap-3">
              {text("Access key ID", "accessKeyId", unchanged || "AKIA…")}
              {text("Secret access key", "secretAccessKey", unchanged, {
                password: true,
              })}
            </div>
            {text("Session token", "sessionToken", "", {
              password: true,
              optional: true,
              hint: "Temporary credentials only.",
            })}
          </>
        ) : null}
        {kind === "gcp" ? (
          <>
            {text("Project", "project", "Taken from the key", {
              optional: true,
            })}
            <Field
              label="Service account key"
              hint="JSON key with Secret Manager Secret Accessor."
            >
              {(p) => (
                <Textarea
                  {...p}
                  value={form.serviceAccount}
                  onChange={(e) => set({ serviceAccount: e.target.value })}
                  placeholder={unchanged || '{ "type": "service_account", … }'}
                  spellCheck={false}
                  className="min-h-[96px] font-mono text-[11px]"
                />
              )}
            </Field>
          </>
        ) : null}
        {kind === "azure" ? (
          <>
            {text("Vault URL", "addr", "https://my-vault.vault.azure.net")}
            {text("Tenant ID", "tenantId", unchanged)}
            <div className="grid grid-cols-2 gap-3">
              {text("Client ID", "clientId", unchanged)}
              {text("Client secret", "clientSecret", unchanged, {
                password: true,
              })}
            </div>
          </>
        ) : null}
        {kind === "doppler" ? (
          text("Service token", "token", unchanged || "dp.st.…", {
            password: true,
            hint: "Each service token reads one config.",
          })
        ) : null}
      </div>
    </Dialog>
  );
}

/** Integrations: the team's stores. */
export function SecretStoresPanel() {
  const s = usePyo();
  const stores = s.secretStores.filter((x) => x.teamId === s.currentTeamId);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<SecretStore | null>(null);
  const [removing, setRemoving] = useState<SecretStore | null>(null);
  const uses = (store: SecretStore) =>
    s.services.filter((a) =>
      a.secretImports?.some((i) => i.storeId === store.id),
    ).length;
  return (
    <Panel
      title="Secret stores"
      actions={
        <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
          <PlusIcon size={12} weight="bold" /> Connect
        </Button>
      }
    >
      {stores.length ? (
        <ul className="list-rows">
          {stores.map((store) => {
            const used = uses(store);
            return (
              <li key={store.id} className="flex items-center gap-3 px-4 py-3">
                <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-muted-foreground">
                  <KeyIcon size={15} />
                </span>
                <div className="min-w-0 flex-1">
                  <p className="truncate text-[13px] font-medium">
                    {store.name}
                  </p>
                  <p className="truncate text-[12px] text-muted-foreground">
                    {kindLabel(store.kind)} ·{" "}
                    {used
                      ? `${used} ${used === 1 ? "service" : "services"}`
                      : "Not used"}
                  </p>
                </div>
                <Button
                  size="xs"
                  variant="secondary"
                  onClick={() => setEditing(store)}
                >
                  Edit
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  disabled={used > 0}
                  title={used ? "Services import from this store" : undefined}
                  aria-label={`Remove ${store.name}`}
                  onClick={() => setRemoving(store)}
                >
                  <TrashIcon size={14} />
                </Button>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="px-4 py-3 text-[13px] text-muted-foreground">
          Vault, AWS, GCP, Azure, Doppler and more.
        </p>
      )}
      {adding ? <StoreDialog onClose={() => setAdding(false)} /> : null}
      {editing ? (
        <StoreDialog store={editing} onClose={() => setEditing(null)} />
      ) : null}
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing?.name}?`}
        description="Your store isn't changed."
        confirmLabel="Remove"
        onConfirm={() => {
          if (removing) void api.removeSecretStore(removing.id);
        }}
      />
    </Panel>
  );
}

/** A service's imports, read each time it deploys. */
export function SecretImports({ app }: { app: Application }) {
  const s = usePyo();
  const stores = s.secretStores.filter((x) => x.teamId === app.teamId);
  const imports = app.secretImports ?? [];
  const [storeId, setStoreId] = useState("");
  const [path, setPath] = useState("");
  const [checked, setChecked] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const store = storeId || stores[0]?.id || "";
  const chosen = stores.find((x) => x.id === store);
  const kind = chosen?.kind ?? "vault";
  const noPath = kind === "doppler";
  const save = (next: SecretImport[]) =>
    api.updateService(app.id, { secretImports: next }, { requiresRedeploy: true });
  const reset = (p: string) => {
    setPath(p);
    setChecked(null);
  };
  if (!stores.length) return null;
  return (
    <Panel title="Imported secrets">
      <div className="space-y-3 p-4">
        <p className="text-[12.5px] text-muted-foreground">
          Read on each deploy. Variables above win.
        </p>
        {imports.length ? (
          <ul className="list-rows rounded-lg border border-border">
            {imports.map((i) => (
              <li
                key={`${i.storeId}:${i.path}`}
                className="flex items-center gap-3 px-3 py-2 text-[12.5px]"
              >
                <span className="min-w-0 flex-1 truncate">
                  <span className="font-medium">
                    {stores.find((x) => x.id === i.storeId)?.name}
                  </span>{" "}
                  <span className="font-mono text-[11.5px] text-muted-foreground">
                    {i.path || "/"}
                  </span>
                </span>
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label={`Remove ${i.path}`}
                  onClick={() => void save(imports.filter((x) => x !== i))}
                >
                  <TrashIcon size={14} />
                </Button>
              </li>
            ))}
          </ul>
        ) : null}
        <div
          className={`grid items-end gap-2 ${noPath ? "grid-cols-[minmax(0,1fr)_auto]" : "grid-cols-[180px_minmax(0,1fr)_auto]"}`}
        >
          <Field label="Store">
            {(p) => (
              <Select
                {...p}
                value={store}
                onChange={(e) => {
                  setStoreId(e.target.value);
                  setChecked(null);
                }}
              >
                {stores.map((x) => (
                  <option key={x.id} value={x.id}>
                    {x.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          {noPath ? null : (
            <Field label={PATHS[kind].label}>
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={path}
                  onChange={(e) => reset(e.target.value)}
                  placeholder={PATHS[kind].placeholder}
                />
              )}
            </Field>
          )}
          <Button
            variant="secondary"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              const r = await api.checkSecretImport(
                store,
                noPath ? "" : path.trim(),
              );
              setBusy(false);
              if (!r) return;
              setChecked(r.keys);
            }}
          >
            {busy ? "Checking…" : "Check"}
          </Button>
        </div>
        {checked ? (
          <div className="flex items-center justify-between gap-3 rounded-lg bg-muted/50 px-3 py-2.5">
            <p className="min-w-0 text-[12.5px] text-muted-foreground">
              {checked.length
                ? `Found ${checked.length}: ${checked.join(", ")}`
                : "Nothing at this path."}
            </p>
            <Button
              size="xs"
              disabled={!checked.length}
              onClick={() => {
                void save([
                  ...imports,
                  { storeId: store, path: noPath ? "" : path.trim() },
                ]);
                reset("");
                toast({
                  title: "Import added",
                  description: "Redeploy to roll it out.",
                });
              }}
            >
              Add
            </Button>
          </div>
        ) : null}
      </div>
    </Panel>
  );
}
