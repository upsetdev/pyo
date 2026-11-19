import { useState, type ReactNode } from "react";
import { BrandMark, CLOUDFLARE_LOGO } from "~/components/pyo/common";
import { Badge } from "~/components/ui/Badge";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import {
  Busy,
  ConfirmNote,
  DialogSection,
  ExternalLink,
  IntegrationDialog,
  Summary,
} from "~/components/pyo/IntegrationDialog";
import { Field, SecretInput, Switch } from "~/components/ui/Field";
import { timeAgo } from "~/lib/pyo/format";
import { isFreeDomain } from "~/lib/pyo/freeDomain";
import {
  api,
  cloudflareIn,
  cloudflareZoneFor,
  roleIn,
  servicePath,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, Domain } from "~/lib/pyo/types";

// The team's Cloudflare account: Pyo creates the DNS records of domains in
// its zones. Only owners and admins see or change the token.

const isAdmin = (role?: string) => role === "owner" || role === "admin";

const ICON = <BrandMark icon={CLOUDFLARE_LOGO} size={15} />;
const TOKENS = "https://dash.cloudflare.com/profile/api-tokens";

/** Integrations › Cloudflare: connect, test, replace and disconnect the token. */
export function CloudflareDialog({ onClose }: { onClose: () => void }) {
  const s = usePyo();
  const account = cloudflareIn(s);
  const admin = isAdmin(roleIn(s));
  const [token, setToken] = useState("");
  const [replacing, setReplacing] = useState(false);
  const [busy, setBusy] = useState<"" | "save" | "test">("");
  const [error, setError] = useState("");
  const [problems, setProblems] = useState<string[]>([]);
  const [confirming, setConfirming] = useState(false);
  const editable = account?.zones.filter((z) => z.canEdit) ?? [];
  const lacking = account?.zones.filter((z) => !z.canEdit) ?? [];

  const save = async () => {
    if (!token.trim() || busy) return;
    setBusy("save");
    setError("");
    const r = await api.connectCloudflare(token.trim());
    setBusy("");
    if ("error" in r) return setError(r.error);
    setToken("");
    setReplacing(false);
    setProblems(r.data.problems ?? []);
    toast({
      title: "Cloudflare connected",
      description: `${r.data.zones.filter((z) => z.canEdit).length} of ${r.data.zones.length} zones editable.`,
    });
  };
  const test = async () => {
    setBusy("test");
    const r = await api.testCloudflare();
    setBusy("");
    if (!r) return;
    setProblems(r.problems ?? []);
    if (r.ok) toast({ title: "Cloudflare token works" });
  };

  const zones = account ? (
    <DialogSection title={editable.length === 1 ? "1 zone" : `${editable.length} zones`}>
      <div className="flex flex-wrap gap-1.5">
        {editable.map((z) => (
          <Badge key={z.id}>
            <span className="font-mono">{z.name}</span>
          </Badge>
        ))}
        {lacking.map((z) => (
          <Badge key={z.id} tone="warning">
            <span className="font-mono">{z.name}</span>
            {z.missing ? ` · needs ${z.missing}` : ""}
          </Badge>
        ))}
      </div>
      {admin && problems.length ? (
        <ul className="space-y-0.5 text-[12.5px] text-destructive" role="alert">
          {problems.map((p) => (
            <li key={p}>{p}</li>
          ))}
        </ul>
      ) : null}
    </DialogSection>
  ) : null;

  const shell = (p: { body: ReactNode; footer?: ReactNode; onSubmit?: () => void }) => (
    <IntegrationDialog
      icon={ICON}
      name="Cloudflare"
      description="Creates DNS records for domains in your zones."
      docs="cloudflare"
      onClose={onClose}
      onSubmit={p.onSubmit}
      footer={p.footer}
    >
      {p.body}
    </IntegrationDialog>
  );

  if (!admin)
    return shell({
      body: (
        <div className="space-y-4">
          <Summary
            items={
              account
                ? [
                    ["Status", <Badge tone="success">Connected</Badge>],
                    ["Tested", timeAgo(account.testedAt)],
                  ]
                : [["Status", <Badge>Not connected</Badge>]]
            }
          />
          {account ? zones : <p className="text-[12.5px] text-muted-foreground">An owner or admin can connect Cloudflare.</p>}
        </div>
      ),
      footer: (
        <Button size="sm" onClick={onClose}>
          Done
        </Button>
      ),
    });

  if (!account || replacing)
    return shell({
      onSubmit: () => void save(),
      body: (
        <div className="space-y-2">
          <Field
            label="API token"
            hint="Zone:Read and DNS:Edit, on the zones Pyo may use."
            error={error || undefined}
          >
            {(p) => (
              <SecretInput
                {...p}
                value={token}
                onChange={(e) => setToken(e.target.value)}
                autoComplete="off"
              />
            )}
          </Field>
          <p className="text-[12px]">
            <ExternalLink href={TOKENS}>Create a token in Cloudflare</ExternalLink>
          </p>
        </div>
      ),
      footer: (
        <>
          <Button
            variant="ghost"
            size="sm"
            onClick={
              account
                ? () => {
                    setReplacing(false);
                    setToken("");
                    setError("");
                  }
                : onClose
            }
          >
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!!busy || !token.trim()}>
            {busy === "save" ? <Busy>Checking…</Busy> : account ? "Replace" : "Connect"}
          </Button>
        </>
      ),
    });

  return shell({
    body: (
      <div className="space-y-4">
        <Summary
          items={[
            [
              "Token",
              <>
                <span className="font-mono">{account.hint ?? "Saved"}</span>
                <Button size="xs" variant="ghost" className="-my-1 -mr-2" onClick={() => setReplacing(true)}>
                  Replace
                </Button>
              </>,
            ],
            ["Tested", timeAgo(account.testedAt)],
          ]}
        />
        {zones}
        {confirming ? (
          <ConfirmNote
            title="Disconnect Cloudflare?"
            confirmLabel="Disconnect"
            onCancel={() => setConfirming(false)}
            onConfirm={() => {
              setConfirming(false);
              void api.removeCloudflare();
              toast({ title: "Cloudflare token removed" });
            }}
          >
            Records Pyo made stay in Cloudflare.
          </ConfirmNote>
        ) : null}
      </div>
    ),
    footer: confirming ? null : (
      <>
        <Button variant="ghost" size="sm" className="mr-auto text-destructive! hover:bg-destructive/10" onClick={() => setConfirming(true)}>
          Disconnect
        </Button>
        <Button variant="secondary" size="sm" disabled={!!busy} onClick={() => void test()}>
          {busy === "test" ? <Busy>Testing…</Busy> : "Test"}
        </Button>
        <Button size="sm" onClick={onClose}>
          Done
        </Button>
      </>
    ),
  });
}

/** What to do about a new domain's DNS record in Cloudflare. */
export type CloudflareChoice = { create: boolean; proxied: boolean };

/**
 * "Create the DNS record in Cloudflare" and "Proxied by Cloudflare", when a
 * connected zone covers the hostname. Nothing otherwise.
 */
export function CloudflareFields({
  hostname,
  clusterId,
  value,
  onChange,
}: {
  hostname: string;
  clusterId?: string;
  value: CloudflareChoice;
  onChange: (v: CloudflareChoice) => void;
}) {
  const s = usePyo();
  const zone = cloudflareZoneFor(s, hostname);
  if (!zone || !hostname || isFreeDomain(hostname)) return null;
  const ip = s.clusters.find((c) => c.id === clusterId)?.publicIp;
  const usable = zone.canEdit && !!ip;
  return (
    <div className="space-y-3">
      <Switch
        checked={usable && value.create}
        disabled={!usable}
        onChange={(create) => onChange({ ...value, create })}
        label="Create the DNS record in Cloudflare"
        description={
          !zone.canEdit
            ? `The token can't edit DNS in ${zone.name}.`
            : !ip
              ? "The cluster has no public IP yet."
              : `A ${ip} in ${zone.name}`
        }
      />
      {usable && value.create ? (
        <Switch
          checked={value.proxied}
          onChange={(proxied) => onChange({ ...value, proxied })}
          label="Proxied by Cloudflare"
          description={
            value.proxied
              ? "On once the certificate is issued. Use SSL Full (strict) and turn off Always Use HTTPS in Cloudflare."
              : undefined
          }
        />
      ) : null}
    </div>
  );
}

/** Whether the dialog should create the record: a connected zone covers it. */
export function useCloudflareFor(hostname: string, clusterId?: string) {
  const s = usePyo();
  const zone = cloudflareZoneFor(s, hostname);
  const ip = s.clusters.find((c) => c.id === clusterId)?.publicIp;
  return !!zone?.canEdit && !!ip && !isFreeDomain(hostname);
}

/** A domain's record in Cloudflare on the service's Domains: how it stands, and what to do. */
export function DnsRecordLine({ app, d }: { app: Application; d: Domain }) {
  const s = usePyo();
  const r = d.dnsRecord;
  const admin = isAdmin(roleIn(s));
  const zone = cloudflareZoneFor(s, d.hostname);
  const [busy, setBusy] = useState(false);
  const run = async (replace: boolean) => {
    setBusy(true);
    await api.createDnsRecord(app.id, d.id, {
      proxied: r?.proxy ?? false,
      replace,
    });
    setBusy(false);
  };
  if (!r) {
    // None yet: offer it while the domain waits for DNS.
    if (!zone?.canEdit || d.status === "active" || isFreeDomain(d.hostname))
      return null;
    return (
      <Button
        size="xs"
        variant="secondary"
        className="mt-2"
        disabled={busy}
        onClick={() => void run(false)}
      >
        <BrandMark icon={CLOUDFLARE_LOGO} size={12} />
        {busy ? "Creating…" : "Create in Cloudflare"}
      </Button>
    );
  }
  if (r.status === "conflict")
    return (
      <div className="mt-3 rounded-lg border border-warning/40 bg-warning/5 px-3 py-2.5 text-[12px]">
        <p className="mb-1.5">Cloudflare already has a record for it:</p>
        {r.existing.map((x) => (
          <div
            key={x.id}
            className="flex flex-wrap gap-x-4 font-mono text-[11.5px]"
          >
            <span>{x.type}</span>
            <span>{x.name}</span>
            <span>{x.content}</span>
            {x.proxied ? <span>proxied</span> : null}
          </div>
        ))}
        <div className="mt-2 flex items-center gap-2">
          {admin ? (
            <Button
              size="xs"
              variant="secondary"
              disabled={busy}
              onClick={() => void run(true)}
            >
              {busy ? "Replacing…" : `Replace with A ${r.content}`}
            </Button>
          ) : (
            <span className="text-muted-foreground">
              An owner or admin can replace it.
            </span>
          )}
          <Button
            size="xs"
            variant="ghost"
            disabled={busy}
            onClick={() => void run(false)}
          >
            Check again
          </Button>
        </div>
      </div>
    );
  if (r.status === "failed")
    return (
      <p className="mt-1.5 text-[12px] text-destructive">
        Cloudflare: {r.error ?? "the record wasn't created."}
        <button
          type="button"
          disabled={busy}
          onClick={() => void run(false)}
          className="ml-1.5 font-medium text-primary hover:text-primary/80"
        >
          Try again
        </button>
      </p>
    );
  return (
    <p className="mt-1.5 inline-flex items-center gap-1.5 text-[12px] text-muted-foreground">
      <BrandMark icon={CLOUDFLARE_LOGO} size={12} />
      {r.status === "created" ? "Record created" : "Record already there"}
      <span className="font-mono">
        {r.type} {r.content}
      </span>
      {r.proxied
        ? "· Proxied"
        : r.proxy
          ? "· Proxied once the certificate is issued"
          : null}
      {r.error ? <span className="text-destructive! hover:bg-destructive/10">· {r.error}</span> : null}
      {(r.proxied || r.proxy) && !app.behindCdn ? (
        <AppLink
          href={servicePath(app.id, "firewall", s)}
          className="font-medium text-primary hover:text-primary/80"
        >
          · Turn on Behind Cloudflare
        </AppLink>
      ) : null}
    </p>
  );
}

/** The remove dialog's choice to delete Pyo's own record too. */
export function DeleteRecordSwitch({
  d,
  checked,
  onChange,
}: {
  d: Domain | null;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  if (!d?.dnsRecord?.mine) return null;
  return (
    <Switch
      checked={checked}
      onChange={onChange}
      label="Delete the DNS record in Cloudflare"
      description={`${d.dnsRecord.type} ${d.dnsRecord.content}, made by Pyo`}
    />
  );
}
