import { fieldState } from "~/components/ui/Field";
import {
  ArrowClockwiseIcon,
  CopyIcon,
  DotsThreeIcon,
  PlusIcon,
  ShieldCheckIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { type ReactNode, useState } from "react";
import { Alert } from "~/components/ui/Alert";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, Select, Switch } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { isFreeDomain, isPrivateHost } from "~/lib/pyo/freeDomain";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster, OriginPullCertificate } from "~/lib/pyo/types";
import { expiry } from "./Certificates";
import { copyText } from "~/lib/pyo/clipboard";

// mTLS: a certificate your CDN presents, which a cluster's HTTPS requires.
// Certificates are the team's; each cluster's settings turn it on.

const NEW_CERTIFICATE = "new";

type Created = {
  id: string;
  name: string;
  certificate: string;
  privateKey: string;
};

type Form = { name: string; commonName: string; years: string };
const BLANK: Form = { name: "", commonName: "", years: "5" };

const complete = (f: Form) => !!f.name.trim() && !!f.commonName.trim();

async function create(teamId: string, f: Form): Promise<Created | null> {
  const name = f.name.trim();
  const r = await api.createOriginPullCertificate({
    teamId,
    name,
    commonName: f.commonName.trim(),
    years: Number(f.years),
  });
  return r && { id: r.id, name, certificate: r.certificate, privateKey: r.privateKey };
}

/** What a cluster enforces now, and what it was last asked to. */
function originPull(c: Cluster) {
  const o = c.originPull;
  return {
    enforced: o?.enforcedIds ?? [],
    wanted: o?.certificateIds ?? [],
    applying: o?.state === "applying",
    failed: o?.state === "failed",
    error: o?.error,
  };
}

function CertificateFields({
  value,
  onChange,
}: {
  value: Form;
  onChange: (f: Form) => void;
}) {
  return (
    <>
      <Field label="Name">
        {(p) => (
          <Input
            {...p}
            value={value.name}
            onChange={(e) => onChange({ ...value, name: e.target.value })}
            placeholder="Cloudflare production"
            maxLength={60}
          />
        )}
      </Field>
      <div className="grid grid-cols-[minmax(0,1fr)_120px] gap-3">
        <Field label="Common name">
          {(p) => (
            <Input
              {...p}
              value={value.commonName}
              onChange={(e) =>
                onChange({ ...value, commonName: e.target.value })
              }
              placeholder="cdn.example.com"
              maxLength={60}
            />
          )}
        </Field>
        <Field label="Valid for">
          {(p) => (
            <Select
              {...p}
              value={value.years}
              onChange={(e) => onChange({ ...value, years: e.target.value })}
            >
              {[1, 3, 5, 10, 15].map((y) => (
                <option key={y} value={y}>
                  {y} {y === 1 ? "year" : "years"}
                </option>
              ))}
            </Select>
          )}
        </Field>
      </div>
    </>
  );
}

function Pem({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="mb-1.5 flex items-center justify-between">
        <p className="text-[12.5px] font-medium">{label}</p>
        <CopyButton value={value} label={`Copy ${label.toLowerCase()}`} />
      </div>
      <textarea
        readOnly
        aria-label={label}
        value={value}
        spellCheck={false}
        onFocus={(e) => e.target.select()}
        className={`h-[72px] w-full resize-none rounded-xl px-3 py-2 font-mono text-[11px] leading-[17px] text-muted-foreground ${fieldState}`}
      />
    </div>
  );
}

/** The one time the certificate and key exist anywhere but the CDN. */
function SaveCertificate({
  created,
  saved,
  onSaved,
}: {
  created: Created;
  saved: boolean;
  onSaved: (v: boolean) => void;
}) {
  return (
    <div className="space-y-4">
      <Pem label="Certificate" value={created.certificate} />
      <Pem label="Private key" value={created.privateKey} />
      <Switch
        checked={saved}
        onChange={onSaved}
        label="I've saved both"
        description="Keep the key secret."
      />
    </div>
  );
}

const SAVE_TITLE = "Save your certificate";
const SAVE_DESCRIPTION =
  "Shown only once. Upload them to your CDN.";

/** Create a certificate, and show it once. */
function CreateCertificateDialog({
  teamId,
  onClose,
}: {
  teamId: string;
  onClose: () => void;
}) {
  const [form, setForm] = useState(BLANK);
  const [busy, setBusy] = useState(false);
  const [created, setCreated] = useState<Created | null>(null);
  const [saved, setSaved] = useState(false);
  const close = () => (!created || saved) && onClose();

  if (created)
    return (
      <Dialog
        open
        onClose={close}
        title={SAVE_TITLE}
        description={SAVE_DESCRIPTION}
        footer={
          <Button size="sm" disabled={!saved} onClick={close}>
            Done
          </Button>
        }
      >
        <SaveCertificate created={created} saved={saved} onSaved={setSaved} />
      </Dialog>
    );

  return (
    <Dialog
      open
      onClose={close}
      title="Create certificate"
      description="Your CDN presents this."
      onSubmit={async () => {
        if (!complete(form) || busy) return;
        setBusy(true);
        const r = await create(teamId, form);
        setBusy(false);
        if (r) setCreated(r);
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!complete(form) || busy}>
            {busy ? "Creating…" : "Create"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <CertificateFields value={form} onChange={setForm} />
      </div>
    </Dialog>
  );
}

/** Every HTTPS hostname on a cluster, and whether it can sit behind mTLS. */
function useHttpsDomains(clusterId: string) {
  return usePyo()
    .services.filter((a) => a.clusterId === clusterId)
    .flatMap((a) => a.domains)
    .filter((d) => d.tlsMode !== "off")
    .map((d) => ({
      hostname: d.hostname,
      direct: isFreeDomain(d.hostname) || isPrivateHost(d.hostname),
    }));
}

function Affected({ clusterId }: { clusterId: string }) {
  const domains = useHttpsDomains(clusterId);
  if (!domains.length)
    return (
      <p className="text-[12.5px] text-muted-foreground">
        No HTTPS domains yet.
      </p>
    );
  return (
    <div>
      <p className="mb-1.5 text-[12.5px] font-medium">
        Applies to {domains.length}{" "}
        {domains.length === 1 ? "domain" : "domains"}
      </p>
      <ul className="max-h-32 list-rows [--row-min:0] overflow-y-auto rounded-lg border border-border">
        {domains.map((d) => (
          <li
            key={d.hostname}
            className="flex items-center justify-between gap-3 px-2.5 py-1.5 text-[12px]"
          >
            <span className="truncate font-mono text-[11.5px]">
              {d.hostname}
            </span>
            {d.direct ? (
              <span className="shrink-0 text-destructive">
                Will stop working
              </span>
            ) : null}
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Turn mTLS on, from nothing if need be: choose or create a certificate, save
 * it, then confirm the CDN has it, seeing what's affected before anything
 * changes.
 */
function TurnOnDialog({
  cluster,
  onClose,
}: {
  cluster: Cluster;
  onClose: () => void;
}) {
  const s = usePyo();
  const certs = s.originPullCertificates.filter(
    (c) => c.teamId === cluster.teamId && c.notAfter > Date.now(),
  );
  const [step, setStep] = useState<"certificate" | "save" | "confirm">(
    "certificate",
  );
  const [choice, setChoice] = useState(certs[0]?.id ?? NEW_CERTIFICATE);
  const [form, setForm] = useState(BLANK);
  const [created, setCreated] = useState<Created | null>(null);
  const [saved, setSaved] = useState(false);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const creating = choice === NEW_CERTIFICATE;
  const total = creating ? 3 : 2;
  const number =
    step === "certificate" ? 1 : step === "save" ? 2 : total;
  const of = `Step ${number} of ${total}`;
  const close = () => (step !== "save" || saved) && onClose();
  const cancel = (
    <Button variant="ghost" size="sm" onClick={close}>
      Cancel
    </Button>
  );

  if (step === "save" && created)
    return (
      <Dialog
        open
        onClose={close}
        title={SAVE_TITLE}
        description={`${of}. ${SAVE_DESCRIPTION}`}
        footer={
          <Button
            size="sm"
            disabled={!saved}
            onClick={() => setStep("confirm")}
          >
            Continue
          </Button>
        }
      >
        <SaveCertificate created={created} saved={saved} onSaved={setSaved} />
      </Dialog>
    );

  if (step === "confirm")
    return (
      <Dialog
        open
        onClose={close}
        title="Turn on mTLS"
        description={`${of}. Do this once your CDN has the certificate.`}
        onSubmit={async () => {
          if (!ready || busy) return;
          setBusy(true);
          const ok = await api.setClusterOriginPull(cluster.id, [
            created?.id ?? choice,
          ]);
          setBusy(false);
          if (ok) onClose();
        }}
        footer={
          <>
            {cancel}
            <Button type="submit" size="sm" disabled={!ready || busy}>
              {busy ? "Turning on…" : "Turn on mTLS"}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          <Affected clusterId={cluster.id} />
          <Switch
            checked={ready}
            onChange={setReady}
            label="My CDN is presenting this certificate"
            description="Domains not behind it stop working over HTTPS."
          />
        </div>
      </Dialog>
    );

  return (
    <Dialog
      open
      onClose={close}
      title="Turn on mTLS"
      description={`${of}. Choose the certificate ${cluster.name} requires.`}
      onSubmit={async () => {
        if (busy) return;
        if (!creating) return setStep("confirm");
        if (!complete(form)) return;
        setBusy(true);
        const r = await create(cluster.teamId, form);
        setBusy(false);
        if (!r) return;
        setCreated(r);
        setStep("save");
      }}
      footer={
        <>
          {cancel}
          <Button
            type="submit"
            size="sm"
            disabled={busy || (creating && !complete(form))}
          >
            {busy ? "Creating…" : creating ? "Create certificate" : "Continue"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {certs.length ? (
          <Field label="Certificate">
            {(p) => (
              <Select
                {...p}
                value={choice}
                onChange={(e) => setChoice(e.target.value)}
              >
                {certs.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
                <option value={NEW_CERTIFICATE}>New certificate</option>
              </Select>
            )}
          </Field>
        ) : null}
        {creating ? (
          <CertificateFields value={form} onChange={setForm} />
        ) : null}
      </div>
    </Dialog>
  );
}

/** Change which certificates a cluster accepts, usually to rotate one. */
function ManageDialog({
  cluster,
  onClose,
}: {
  cluster: Cluster;
  onClose: () => void;
}) {
  const s = usePyo();
  const { enforced, wanted } = originPull(cluster);
  const certs = s.originPullCertificates.filter(
    (c) =>
      c.teamId === cluster.teamId &&
      (c.notAfter > Date.now() || enforced.includes(c.id)),
  );
  const [picked, setPicked] = useState(wanted.length ? wanted : enforced);
  const [ready, setReady] = useState(false);
  const adding = picked.some((id) => !enforced.includes(id));
  const same =
    [...picked].sort().join() === [...enforced].sort().join();
  const blocked = !picked.length || same || (adding && !ready);
  return (
    <Dialog
      open
      onClose={onClose}
      title="Manage certificates"
      description="To replace one, keep both until your CDN uses the new one."
      onSubmit={async () => {
        if (blocked) return;
        if (await api.setClusterOriginPull(cluster.id, picked)) onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={blocked}>
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        {certs.map((c) => (
          <Switch
            key={c.id}
            checked={picked.includes(c.id)}
            onChange={(on) =>
              setPicked(
                on ? [...picked, c.id] : picked.filter((x) => x !== c.id),
              )
            }
            label={c.name}
            description={`${c.commonName} · ${expiry(c).text}`}
          />
        ))}
        {adding ? (
          <div className="border-t border-border pt-3">
            <Switch
              checked={ready}
              onChange={setReady}
              label="My CDN is presenting the new certificate"
            />
          </div>
        ) : null}
        <p className="text-[12.5px] text-muted-foreground">
          Need another?{" "}
          <AppLink
            href="/mtls"
            className="font-medium text-primary hover:text-primary/80"
          >
            Create a certificate
          </AppLink>
        </p>
      </div>
    </Dialog>
  );
}

/** A cluster's mTLS. The switch shows what the cluster really enforces. */
export function ClusterMtls({ cluster }: { cluster: Cluster }) {
  const s = usePyo();
  const { enforced, wanted, applying, failed, error } = originPull(cluster);
  const [dialog, setDialog] = useState<"on" | "manage" | "off" | null>(null);
  const on = applying ? wanted.length > 0 : enforced.length > 0;
  const shown = s.originPullCertificates.filter((c) => enforced.includes(c.id));
  const close = () => setDialog(null);
  return (
    <Panel
      title="mTLS"
      actions={
        on && !applying ? (
          <Button size="xs" variant="secondary" onClick={() => setDialog("manage")}>
            Manage
          </Button>
        ) : null
      }
    >
      <div className="space-y-3 p-4">
        <Switch
          checked={on}
          disabled={applying}
          onChange={(v) => setDialog(v ? "on" : "off")}
          label="Require your CDN's certificate"
          description={
            applying
              ? "Applying to this cluster…"
              : on
                ? "Connections that skip your CDN are refused."
                : "Refuse HTTPS connections that skip your CDN."
          }
        />
        {shown.length && !applying ? (
          <ul className="space-y-1 border-t border-border pt-3 text-[12.5px]">
            {shown.map((c) => (
              <li key={c.id} className="flex items-center justify-between gap-3">
                <span className="truncate font-medium">{c.name}</span>
                <span className={`shrink-0 ${expiry(c).tone}`}>
                  {expiry(c).text}
                </span>
              </li>
            ))}
          </ul>
        ) : null}
        {failed ? (
          <Alert>
            <p>
              {wanted.length ? "Couldn't apply this." : "Couldn't turn this off."}{" "}
              {error}
            </p>
            <button
              type="button"
              onClick={() =>
                void api.setClusterOriginPull(cluster.id, wanted)
              }
              className="mt-1 inline-flex items-center gap-1 font-medium underline underline-offset-2"
            >
              <ArrowClockwiseIcon size={12} /> Try again
            </button>
          </Alert>
        ) : null}
      </div>
      {dialog === "on" ? <TurnOnDialog cluster={cluster} onClose={close} /> : null}
      {dialog === "manage" ? (
        <ManageDialog cluster={cluster} onClose={close} />
      ) : null}
      <ConfirmDialog
        open={dialog === "off"}
        onClose={close}
        tone="primary"
        title={`Turn off mTLS on ${cluster.name}?`}
        description="Anyone can connect directly again, around your CDN."
        confirmLabel="Turn off mTLS"
        onConfirm={() => void api.setClusterOriginPull(cluster.id, [])}
      />
    </Panel>
  );
}

function UsedBy({ clusters }: { clusters: Cluster[] }): ReactNode {
  if (!clusters.length) return "Not used";
  return (
    <>
      Used by{" "}
      {clusters.map((c, i) => (
        <span key={c.id}>
          {i ? ", " : ""}
          <AppLink
            href={`/clusters/${c.name}/settings`}
            className="text-foreground hover:text-primary"
          >
            {c.name}
          </AppLink>
        </span>
      ))}
    </>
  );
}

/** The mTLS tab: the team's certificates. Clusters turn them on. */
export function OriginPullCertificates() {
  const s = usePyo();
  const certs = s.originPullCertificates.filter(
    (c) => c.teamId === s.currentTeamId,
  );
  const [creating, setCreating] = useState(false);
  const [removing, setRemoving] = useState<OriginPullCertificate | null>(null);
  const usedBy = (c: OriginPullCertificate) =>
    s.clusters.filter((x) => {
      const { enforced, wanted, applying } = originPull(x);
      return enforced.includes(c.id) || (applying && wanted.includes(c.id));
    });
  const add = (
    <Button size="sm" variant="secondary" onClick={() => setCreating(true)}>
      <PlusIcon size={13} weight="bold" /> Create certificate
    </Button>
  );
  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between gap-4">
        <p className="text-[13px] text-muted-foreground">
          Accept traffic only from your CDN. Set per cluster.
        </p>
        {certs.length ? add : null}
      </div>
      <Panel>
        {certs.length ? (
          <ul className="list-rows">
            {certs.map((c) => {
              const e = expiry(c);
              const used = usedBy(c);
              return (
                <li key={c.id} className="flex items-center gap-3 px-4 py-3">
                  <ShieldCheckIcon
                    size={16}
                    className="shrink-0 text-muted-foreground"
                  />
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-[13px] font-medium">{c.name}</p>
                    <p className="truncate text-[12px] text-muted-foreground">
                      {c.commonName} · <span className={e.tone}>{e.text}</span>{" "}
                      · <UsedBy clusters={used} />
                    </p>
                  </div>
                  <Menu
                    label={`Actions for ${c.name}`}
                    items={[
                      {
                        label: "Copy CA certificate",
                        icon: <CopyIcon size={14} />,
                        onSelect: () => {
                          void copyText(c.caCertificate).then(
                            () => toast({ title: "Copied the CA certificate" }),
                            () => toast({ tone: "error", title: "Couldn't copy" }),
                          );
                        },
                      },
                      { type: "separator" as const },
                      {
                        label: "Remove",
                        icon: <TrashIcon size={14} />,
                        danger: true,
                        disabled: used.length > 0,
                        hint: used.length ? "In use" : undefined,
                        onSelect: () => setRemoving(c),
                      },
                    ]}
                    trigger={(t) => (
                      <Button
                        {...t}
                        variant="ghost"
                        size="icon"
                        aria-label={`Actions for ${c.name}`}
                      >
                        <DotsThreeIcon size={16} weight="bold" />
                      </Button>
                    )}
                  />
                </li>
              );
            })}
          </ul>
        ) : (
          <EmptyState
            compact
            icon={ShieldCheckIcon}
            title="No certificates"
            description="For mTLS with your CDN."
            action={add}
          />
        )}
      </Panel>
      {creating ? (
        <CreateCertificateDialog
          teamId={s.currentTeamId}
          onClose={() => setCreating(false)}
        />
      ) : null}
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing?.name}?`}
        description="It can't be chosen again."
        confirmLabel="Remove"
        onConfirm={() => {
          if (removing) void api.removeOriginPullCertificate(removing.id);
        }}
      />
    </div>
  );
}
