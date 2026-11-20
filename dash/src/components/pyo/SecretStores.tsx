import { KeyIcon, PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { siVault } from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import {
  Busy,
  ConfirmNote,
  DialogRow,
  DialogRows,
  IntegrationDialog,
} from "~/components/pyo/IntegrationDialog";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Field, Input, SecretInput, Select, Textarea } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api, listedServices, usePyo } from "~/lib/pyo/store";
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

const ICON = <BrandMark icon={siVault} size={15} tone="mono" />;
const ABOUT = "Services import secrets from your store on each deploy.";

/** Connect a store, or change one. Its credential is never shown again. */
function StoreForm({
  store,
  onDone,
  onCancel,
  onClose,
}: {
  store?: SecretStore;
  onDone: () => void;
  onCancel: () => void;
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
      {(p) =>
        extra.password ? (
          <SecretInput
            {...p}
            autoComplete="off"
            value={form[key]}
            onChange={(e) => set({ [key]: e.target.value })}
            placeholder={placeholder}
          />
        ) : (
          <Input
            {...p}
            autoComplete="off"
            value={form[key]}
            onChange={(e) => set({ [key]: e.target.value })}
            placeholder={placeholder}
          />
        )
      }
    </Field>
  );
  return (
    <IntegrationDialog
      icon={ICON}
      name="Secret stores"
      description={store ? `Edit ${store.name}. Checked before saving.` : "Checked before saving."}
      docs="external-secrets"
      onClose={onClose}
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
        onDone();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onCancel}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!ready || busy}>
            {busy ? <Busy>Checking…</Busy> : store ? "Save" : "Connect"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="grid gap-3 sm:grid-cols-2">
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
            <div className="grid gap-3 sm:grid-cols-2">
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
            <div className="grid gap-3 sm:grid-cols-2">
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
            <div className="grid gap-3 sm:grid-cols-2">
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
            <div className="grid gap-3 sm:grid-cols-2">
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
    </IntegrationDialog>
  );
}

function StoreRow({
  store,
  used,
  onEdit,
}: {
  store: SecretStore;
  used: number;
  onEdit: () => void;
}) {
  const [confirming, setConfirming] = useState(false);
  return (
    <DialogRow
      icon={<KeyIcon size={15} />}
      title={store.name}
      meta={`${kindLabel(store.kind)} · ${used ? `${used} ${used === 1 ? "service" : "services"}` : "Not used"}`}
      actions={
        confirming ? null : (
          <>
            <Button size="xs" variant="ghost" onClick={onEdit}>
              Edit
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-7 w-7"
              disabled={used > 0}
              title={used ? "Services import from this store" : undefined}
              aria-label={`Remove ${store.name}`}
              onClick={() => setConfirming(true)}
            >
              <TrashIcon size={14} />
            </Button>
          </>
        )
      }
    >
      {confirming ? (
        <ConfirmNote
          title={`Remove ${store.name}?`}
          confirmLabel="Remove"
          onCancel={() => setConfirming(false)}
          onConfirm={() => {
            setConfirming(false);
            void api.removeSecretStore(store.id);
          }}
        >
          Your store isn't changed.
        </ConfirmNote>
      ) : null}
    </DialogRow>
  );
}

/**
 * Integrations › Secret stores: the default, the team's stores, and a form
 * to connect or edit one. Opens on the form while there are none.
 */
export function SecretStoresDialog({ onClose }: { onClose: () => void }) {
  const s = usePyo();
  const stores = s.secretStores.filter((x) => x.teamId === s.currentTeamId);
  const [form, setForm] = useState<"add" | SecretStore | null>(stores.length ? null : "add");
  const uses = (store: SecretStore) =>
    listedServices(s).filter((a) =>
      a.secretImports?.some((i) => i.storeId === store.id),
    ).length;

  if (form)
    return (
      <StoreForm
        key={form === "add" ? "add" : form.id}
        store={form === "add" ? undefined : form}
        onDone={() => setForm(null)}
        onCancel={() => (stores.length ? setForm(null) : onClose())}
        onClose={onClose}
      />
    );

  return (
    <IntegrationDialog
      icon={ICON}
      name="Secret stores"
      description={ABOUT}
      docs="external-secrets"
      onClose={onClose}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => setForm("add")}>
            <PlusIcon size={12} weight="bold" /> Connect a store
          </Button>
          <Button size="sm" onClick={onClose}>
            Done
          </Button>
        </>
      }
    >
      <DialogRows>
        <DialogRow
          icon={<KeyIcon size={15} />}
          title="Cluster Secrets"
          meta="Default. Stored in the target cluster."
          badge={<Badge tone="success">Active</Badge>}
        />
        {stores.map((store) => (
          <StoreRow key={store.id} store={store} used={uses(store)} onEdit={() => setForm(store)} />
        ))}
      </DialogRows>
    </IntegrationDialog>
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
