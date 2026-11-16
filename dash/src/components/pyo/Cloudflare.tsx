import { ArrowsClockwiseIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { BrandMark, CLOUDFLARE_LOGO } from "~/components/pyo/common";
import { Badge } from "~/components/ui/Badge";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { SecretInput, Switch } from "~/components/ui/Field";
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

/** Integrations › Cloudflare: connect, test, replace and remove the token. */
export function CloudflareSettings() {
  const s = usePyo();
  const account = cloudflareIn(s);
  const admin = isAdmin(roleIn(s));
  const [token, setToken] = useState("");
  const [replacing, setReplacing] = useState(false);
  const [busy, setBusy] = useState<"" | "save" | "test">("");
  const [error, setError] = useState("");
  const [problems, setProblems] = useState<string[]>([]);
  const [removing, setRemoving] = useState(false);
  const editable = account?.zones.filter((z) => z.canEdit) ?? [];
  const lacking = account?.zones.filter((z) => !z.canEdit) ?? [];

  const save = async () => {
    if (!token.trim()) return;
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

  const form = (
    <form
      className="flex w-full gap-2 sm:w-auto"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <div className="w-full sm:w-64">
        <SecretInput
          value={token}
          onChange={(e) => setToken(e.target.value)}
          placeholder="API token"
          aria-label="Cloudflare API token"
          className="h-8"
        />
      </div>
      <Button type="submit" size="xs" disabled={!!busy || !token.trim()}>
        {busy === "save" ? "Checking…" : account ? "Replace" : "Connect"}
      </Button>
      {replacing ? (
        <Button
          variant="ghost"
          size="xs"
          onClick={() => {
            setReplacing(false);
            setToken("");
            setError("");
          }}
        >
          Cancel
        </Button>
      ) : null}
    </form>
  );

  return (
    <>
      <ul className="list-rows">
        <li className="flex flex-wrap items-center gap-3 px-4 py-3">
          <span className="flex h-8 w-8 items-center justify-center rounded-full bg-muted">
            <BrandMark icon={CLOUDFLARE_LOGO} size={15} />
          </span>
          <div className="min-w-0 flex-1">
            <p className="text-[13px] font-medium">Cloudflare API token</p>
            <p className="text-[12px] text-muted-foreground">
              {account
                ? `${account.hint ? `Saved · ${account.hint} · ` : ""}Tested ${timeAgo(account.testedAt)}`
                : admin
                  ? "Zone:Read and DNS:Edit, on the zones Pyo may use."
                  : "An owner or admin can connect Cloudflare."}
            </p>
          </div>
          {!admin ? (
            account ? (
              <Badge tone="success">Connected</Badge>
            ) : (
              <Badge>Not connected</Badge>
            )
          ) : !account || replacing ? (
            form
          ) : (
            <span className="flex items-center gap-1">
              <Button
                variant="secondary"
                size="xs"
                disabled={!!busy}
                onClick={() => void test()}
              >
                <ArrowsClockwiseIcon size={12} />
                {busy === "test" ? "Testing…" : "Test"}
              </Button>
              <Button
                variant="ghost"
                size="xs"
                onClick={() => setReplacing(true)}
              >
                Replace
              </Button>
              <Button
                variant="ghost"
                size="xs"
                className="text-destructive"
                onClick={() => setRemoving(true)}
              >
                Remove
              </Button>
            </span>
          )}
          {error ? (
            <p className="w-full text-[12px] text-destructive">{error}</p>
          ) : null}
        </li>
        {account ? (
          <li className="px-4 py-3 text-[12px]">
            <p className="mb-1.5 text-muted-foreground">
              {editable.length === 1 ? "1 zone" : `${editable.length} zones`}
            </p>
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
              <ul className="mt-2 space-y-0.5 text-destructive">
                {problems.map((p) => (
                  <li key={p}>{p}</li>
                ))}
              </ul>
            ) : null}
          </li>
        ) : null}
      </ul>
      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title="Remove the Cloudflare token?"
        description="Records Pyo made stay in Cloudflare."
        confirmLabel="Remove"
        onConfirm={() => {
          void api.removeCloudflare();
          toast({ title: "Cloudflare token removed" });
        }}
      />
    </>
  );
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
      {r.error ? <span className="text-destructive">· {r.error}</span> : null}
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
