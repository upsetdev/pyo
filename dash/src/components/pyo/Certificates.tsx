import {
  ArrowsClockwiseIcon,
  DotsThreeIcon,
  LockSimpleIcon,
  LockSimpleOpenIcon,
  MagicWandIcon,
  SealIcon,
  ShieldCheckIcon,
  TrashIcon,
  UploadSimpleIcon,
  type Icon,
} from "@phosphor-icons/react";
import { useRef, useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, Textarea } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Panel } from "~/components/ui/Panel";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { isPrivateHost } from "~/lib/pyo/freeDomain";
import type {
  Application,
  Certificate,
  Domain,
  TlsMode,
} from "~/lib/pyo/types";

// Certificates a team uploads for its domains (HTTPS mode "Your
// certificate"): one per domain, or a wildcard for many. The private key is
// kept encrypted on the server and only sent to the team's clusters.

const DAY = 86_400_000;

/** Whether a certificate's names cover a hostname, wildcards included (mirrors the API). */
export function covers(c: Pick<Certificate, "hostnames">, host: string) {
  const h = host.toLowerCase();
  return c.hostnames.some((n) =>
    n.startsWith("*.")
      ? h.indexOf(".") > 0 && h.slice(h.indexOf(".") + 1) === n.slice(2)
      : n === h,
  );
}

const fullDate = (at: number) =>
  new Date(at).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    year: "numeric",
  });

/** "Expires Mar 3, 2027", or how soon, or that it has. */
export function expiry(c: Pick<Certificate, "notAfter">) {
  const left = Math.ceil((c.notAfter - Date.now()) / DAY);
  if (left <= 0)
    return {
      text: `Expired ${fullDate(c.notAfter)}`,
      tone: "text-destructive",
    };
  if (left <= 30)
    return {
      text: `Expires in ${left} day${left === 1 ? "" : "s"}`,
      tone: left <= 7 ? "text-destructive" : "text-warning",
    };
  return {
    text: `Expires ${fullDate(c.notAfter)}`,
    tone: "text-muted-foreground",
  };
}

/** A PEM field: paste it, or pick the file. */
function PemField({
  label,
  hint,
  value,
  onChange,
  placeholder,
}: {
  label: string;
  hint?: string;
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
}) {
  const file = useRef<HTMLInputElement>(null);
  return (
    <Field
      label={
        <>
          {label}
          <button
            type="button"
            onClick={() => file.current?.click()}
            className="ml-auto text-[12px] font-medium text-primary hover:text-primary/80"
          >
            Choose file
          </button>
        </>
      }
      hint={hint}
    >
      {(p) => (
        <>
          <Textarea
            {...p}
            value={value}
            onChange={(e) => onChange(e.target.value)}
            placeholder={placeholder}
            spellCheck={false}
            className="min-h-[96px] font-mono text-[11px]"
          />
          <input
            ref={file}
            type="file"
            accept=".pem,.crt,.cer,.key,.txt"
            className="hidden"
            onChange={async (e) => {
              const f = e.target.files?.[0];
              if (f) onChange(await f.text());
              e.target.value = "";
            }}
          />
        </>
      )}
    </Field>
  );
}

/** Upload a certificate, or replace one with its renewal. */
export function CertificateDialog({
  open,
  onClose,
  replacing,
  forHost,
  onSaved,
}: {
  open: boolean;
  onClose: () => void;
  replacing?: Certificate;
  forHost?: string;
  onSaved?: (id: string) => void;
}) {
  const [name, setName] = useState("");
  const [certificate, setCertificate] = useState("");
  const [privateKey, setPrivateKey] = useState("");
  const [saving, setSaving] = useState(false);
  const close = () => {
    setName("");
    setCertificate("");
    setPrivateKey("");
    onClose();
  };
  const ready =
    certificate.includes("BEGIN CERTIFICATE") &&
    privateKey.includes("PRIVATE KEY");
  return (
    <Dialog
      open={open}
      onClose={close}
      width="max-w-xl"
      title={replacing ? `Replace ${replacing.name}` : "Upload certificate"}
      description={
        replacing
          ? "Must cover the same domains."
          : `PEM files from your provider${forHost ? `, valid for ${forHost}` : ""}.`
      }
      onSubmit={async () => {
        if (!ready || saving) return;
        setSaving(true);
        const r = replacing
          ? await api.replaceCertificate(replacing.id, {
              certificate,
              privateKey,
            })
          : await api.uploadCertificate({ name, certificate, privateKey });
        setSaving(false);
        if (!r) return;
        toast({
          title: replacing
            ? `Replaced ${replacing.name}`
            : "Certificate uploaded",
          description: [
            `Valid for ${r.hostnames.join(", ")}.`,
            ...("updated" in r && r.updated
              ? [
                  `Applied to ${r.updated} service${r.updated === 1 ? "" : "s"}.`,
                ]
              : []),
            ...r.warnings,
          ].join(" "),
        });
        if ("id" in r && typeof r.id === "string") onSaved?.(r.id);
        close();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!ready || saving}>
            {saving ? "Checking…" : replacing ? "Replace" : "Upload"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {!replacing ? (
          <Field label="Name" hint="Defaults to the first hostname.">
            {(p) => (
              <Input
                {...p}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. Wildcard example.com"
                maxLength={60}
              />
            )}
          </Field>
        ) : null}
        <PemField
          label="Certificate"
          hint="Your certificate first, then the intermediates."
          value={certificate}
          onChange={setCertificate}
          placeholder="-----BEGIN CERTIFICATE-----"
        />
        <PemField
          label="Private key"
          hint="Not password-protected. Never shown again."
          value={privateKey}
          onChange={setPrivateKey}
          placeholder="-----BEGIN PRIVATE KEY-----"
        />
      </div>
    </Dialog>
  );
}

/** The team's certificates page: what they cover, when they expire, where they're used. */
export function CertificatesList() {
  const s = usePyo();
  const certs = s.certificates.filter((c) => c.teamId === s.currentTeamId);
  const [uploading, setUploading] = useState(false);
  const [replacing, setReplacing] = useState<Certificate | null>(null);
  const [removing, setRemoving] = useState<Certificate | null>(null);
  const usedBy = (c: Certificate) =>
    s.services.flatMap((a) =>
      a.domains
        .filter((d) => d.tlsMode === "custom" && d.certificateId === c.id)
        .map((d) => d.hostname),
    );

  const upload = (
    <Button size="sm" variant="secondary" onClick={() => setUploading(true)}>
      <UploadSimpleIcon size={13} weight="bold" /> Upload certificate
    </Button>
  );

  return (
    <>
      <div className="mb-3 flex items-center justify-between gap-4">
        <p className="text-[13px] text-muted-foreground">
          Your own certificates. Let's Encrypt is automatic.
        </p>
        {certs.length ? upload : null}
      </div>
      <Panel>
        {certs.length ? (
          <ul className="list-rows">
            {certs.map((c) => {
              const e = expiry(c);
              const used = usedBy(c);
              return (
                <li key={c.id} className="flex items-center gap-3 px-4 py-3">
                  <LockSimpleIcon
                    size={16}
                    className="shrink-0 text-muted-foreground"
                  />
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-[13px] font-medium">{c.name}</p>
                    <p className="truncate text-[12px] text-muted-foreground">
                      {c.hostnames.join(", ")} ·{" "}
                      <span className={e.tone}>{e.text}</span> ·{" "}
                      {used.length ? `Used by ${used.join(", ")}` : "Not used"}
                    </p>
                  </div>
                  <Menu
                    label={`Actions for ${c.name}`}
                    items={[
                      {
                        label: "Replace with renewal",
                        icon: <ArrowsClockwiseIcon size={14} />,
                        onSelect: () => setReplacing(c),
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
            icon={LockSimpleIcon}
            title="No certificates"
            description="Use your own, like a wildcard."
            action={upload}
          />
        )}
        <CertificateDialog
          open={uploading}
          onClose={() => setUploading(false)}
        />
        <CertificateDialog
          open={!!replacing}
          onClose={() => setReplacing(null)}
          replacing={replacing ?? undefined}
        />
        <ConfirmDialog
          open={!!removing}
          onClose={() => setRemoving(null)}
          title={`Remove ${removing?.name}?`}
          description="Its private key is deleted from Pyo."
          confirmLabel="Remove certificate"
          onConfirm={async () => {
            if (removing && (await api.removeCertificate(removing.id)))
              toast({ title: `Removed ${removing.name}` });
          }}
        />
      </Panel>
    </>
  );
}

export const TLS_MODES: {
  value: TlsMode;
  label: string;
  hint: string;
  icon: Icon;
}[] = [
  {
    value: "auto",
    label: "Automatic",
    icon: MagicWandIcon,
    hint: "Let's Encrypt, or self-signed for private domains.",
  },
  {
    value: "letsencrypt",
    label: "Let's Encrypt",
    icon: ShieldCheckIcon,
    hint: "Needs DNS pointing here and port 80 open.",
  },
  {
    value: "selfsigned",
    label: "Self-signed",
    icon: SealIcon,
    hint: "Browsers warn about it. For private domains.",
  },
  {
    value: "custom",
    label: "Your certificate",
    icon: LockSimpleIcon,
    hint: "One you upload. You renew it, and Pyo warns before it expires.",
  },
  {
    value: "off",
    label: "Off (HTTP only)",
    icon: LockSimpleOpenIcon,
    hint: "Served over plain HTTP.",
  },
];

/** Switch a domain to one of the team's certificates, or upload one for it. */
export function PickCertificateDialog({
  app,
  domain,
  onClose,
}: {
  app: Application;
  domain: Domain | null;
  onClose: () => void;
}) {
  const s = usePyo();
  const [uploading, setUploading] = useState(false);
  const host = domain?.hostname ?? "";
  const matching = s.certificates.filter(
    (c) => c.teamId === app.teamId && covers(c, host),
  );
  const use = (id: string) => {
    if (!domain) return;
    api.updateDomain(app.id, domain.id, {
      tlsMode: "custom",
      certificateId: id,
    });
    toast({ title: `HTTPS for ${host}: your certificate` });
    onClose();
  };
  return (
    <Dialog
      open={!!domain}
      onClose={onClose}
      title={`Your certificate for ${host}`}
      description="One that covers this hostname."
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button
            size="sm"
            variant="secondary"
            onClick={() => setUploading(true)}
          >
            Upload new
          </Button>
        </>
      }
    >
      {matching.length ? (
        <ul className="list-rows overflow-hidden rounded-lg border border-border">
          {matching.map((c) => {
            const e = expiry(c);
            const current =
              domain?.tlsMode === "custom" && domain.certificateId === c.id;
            return (
              <li key={c.id}>
                <button
                  type="button"
                  onClick={() => use(c.id)}
                  className="flex w-full items-center gap-3 px-3 py-2.5 text-left transition-colors hover:bg-accent/50"
                >
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-[13px] font-medium">
                      {c.name}
                    </span>
                    <span className="block truncate text-[12px] text-muted-foreground">
                      <span className={e.tone}>{e.text}</span> · {c.issuer}
                    </span>
                  </span>
                  {current ? (
                    <span className="text-[12px] text-muted-foreground">
                      In use
                    </span>
                  ) : null}
                </button>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="text-[13px] text-muted-foreground">
          No certificate covers {host} yet. Upload one for it, or a wildcard.
        </p>
      )}
      <CertificateDialog
        open={uploading}
        onClose={() => setUploading(false)}
        forHost={host}
        onSaved={use}
      />
    </Dialog>
  );
}

/** What a domain's certificate is, in a line (the HTTPS badge's tooltip). */
export function tlsLine(d: Domain, certs: Certificate[]) {
  if (d.tlsMode === "off") return "Served over plain HTTP.";
  if (d.tlsMode === "custom") {
    const c = certs.find((x) => x.id === d.certificateId);
    if (!c) return "Its certificate was removed. Pick another one.";
    return `${d.tls === "issued" ? "HTTPS with" : "Uses"} your certificate ${c.name}. ${expiry(c).text}.`;
  }
  const issuer =
    (d.tlsMode ?? "auto") === "auto"
      ? isPrivateHost(d.hostname)
        ? "self-signed"
        : "Let's Encrypt"
      : d.tlsMode === "selfsigned"
        ? "self-signed"
        : "Let's Encrypt";
  if (d.tls === "issued") return `HTTPS with a ${issuer} certificate.`;
  if (d.tls === "failed")
    return `The ${issuer} certificate couldn't be issued. Pyo tries again on its own.`;
  return d.status === "active"
    ? `Issuing a ${issuer} certificate…`
    : `Gets a ${issuer} certificate once it's live.`;
}
