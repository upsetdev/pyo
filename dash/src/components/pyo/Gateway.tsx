import { useEffect, useState } from "react";
import { Status, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented, Switch } from "~/components/ui/Field";
import { LearnMore } from "~/components/ui/LearnMore";
import { Panel } from "~/components/ui/Panel";
import { timeAgo } from "~/lib/pyo/format";
import { api, clusterPaths, roleIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import {
  GATEWAY_DEFAULTS,
  agentBlock,
  changes,
  checkForm,
  dropLines,
  formOf,
  type GatewayForm,
} from "~/lib/pyo/gateway";
import type { Cluster, GatewayServerPart, GatewaySettings } from "~/lib/pyo/types";

// The Gateway section (docs/traffic-and-gateway-design.md §12): the
// gateway's parts on each server, and a few settings for the whole cluster.
// HTTP/3 and the TLS minimum drop open connections once, and say so before
// they're saved. Compression and the timeout defaults change routes only.
// PROXY protocol saves on its own, through setClusterPaths, after its
// confirm.

function partLine(p: { ready: boolean; version: string; reason?: string | null; startedAt?: number | null }) {
  const tone: Tone = p.ready ? "success" : "danger";
  const words = [p.ready ? "Ready" : "Not ready", p.version || null, p.ready ? (p.startedAt ? `started ${timeAgo(p.startedAt)}` : null) : p.reason];
  return <Status tone={tone} label={words.filter(Boolean).join(" · ")} className="text-[12.5px]" />;
}

/** The section's Gateway panel, next to Traffic. */
export function ClusterGateway({ cluster: c }: { cluster: Cluster }) {
  const s = usePyo();
  const admin = ["owner", "admin"].includes(roleIn(s) ?? "");
  const saved = c.gateway?.settings ?? GATEWAY_DEFAULTS;
  const status = c.gateway?.status ?? null;
  const [form, setForm] = useState<GatewayForm>(() => formOf(saved));
  const [confirming, setConfirming] = useState(false);
  const [saving, setSaving] = useState(false);
  const [proxy, setProxy] = useState<boolean | null>(null);
  // A save elsewhere (another tab, the CLI) shows here once nothing's typed.
  const savedKey = JSON.stringify(saved);
  const [base, setBase] = useState(savedKey);
  useEffect(() => {
    if (savedKey === base) return;
    const typed = checkForm(form);
    if ("settings" in typed && !Object.keys(changes(JSON.parse(base) as GatewaySettings, typed.settings)).length) {
      setForm(formOf(saved));
    }
    setBase(savedKey);
  }, [savedKey]);

  const checked = checkForm(form);
  const next = "settings" in checked ? checked.settings : null;
  const diff = next ? changes(saved, next) : {};
  const dirty = !next || Object.keys(diff).length > 0;
  const lines = next ? dropLines(c.name, saved, next) : [];
  const block = agentBlock(c);
  const publicOn = clusterPaths(c).public;
  const udpTaken = status?.udp443Taken ? `UDP port 443 is in use on the servers by ${status.udp443Taken}. Free it first.` : null;
  // Only the request timeout default works with any agent.
  const locked = !admin || !!block;

  const save = async () => {
    if (!next) return;
    setConfirming(false);
    setSaving(true);
    const r = await api.setClusterGateway(c.id, diff);
    setSaving(false);
    if (r?.ok) toast({ title: "Gateway saved", description: r.drops ? `${c.name}. Open connections drop once.` : c.name });
  };

  const http3Hint = (() => {
    if (!publicOn) return "Turn on public ports first.";
    if (udpTaken && !saved.http3) return udpTaken;
    if (status && saved.http3 !== status.http3) return saved.http3 ? "Turning on…" : "Turning off…";
    return "Needs UDP port 443 open to every server.";
  })();
  const tlsApplying = status && status.tlsMin && saved.tlsMin !== status.tlsMin;
  const proxyOn = !!(c.proxyProtocol ?? c.webProxyProtocol);

  const saveProxy = async (on: boolean) => {
    setProxy(null);
    const r = await api.setClusterPaths(c.id, { proxyProtocol: on });
    if (r?.ok) toast({ title: on ? "PROXY protocol on" : "PROXY protocol off", description: c.name });
  };

  return (
    <Panel title="Gateway" actions={<LearnMore page="gateway" />}>
      <div className="space-y-2 px-4 py-3">
        {status ? (
          <>
            <div className="flex flex-wrap items-center justify-between gap-2 text-[13px]">
              <span className="font-medium">Route updates</span>
              {partLine(status.controller)}
            </div>
            <table className="w-full text-left text-[12.5px]">
              <thead className="text-[12px] text-muted-foreground">
                <tr>
                  <th className="py-1 pr-3 font-medium">Server</th>
                  <th className="py-1 pr-3 font-medium">Proxy</th>
                  <th className="py-1 font-medium">Firewall</th>
                </tr>
              </thead>
              <tbody>
                {status.proxies.map((p) => {
                  const waf: GatewayServerPart | undefined = status.firewalls.find((f) => f.server === p.server);
                  return (
                    <tr key={p.server} className="border-t border-border">
                      <td className="py-1.5 pr-3 font-mono">{p.server}</td>
                      <td className="py-1.5 pr-3">{partLine(p)}</td>
                      <td className="py-1.5">
                        {waf ? partLine(waf) : <span className="text-muted-foreground">Off. No service uses the WAF.</span>}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </>
        ) : (
          <p className="text-[12.5px] text-muted-foreground">Not reported yet.</p>
        )}
      </div>
      <div className="space-y-3 border-t border-border px-4 py-3">
        {admin && block ? <p className="text-[12px] text-muted-foreground">{block}</p> : null}
        <p className="flex flex-wrap justify-between gap-2 text-[13px]">
          <span className="font-medium">HTTP/2</span>
          <span className="text-[12px] text-muted-foreground">On</span>
        </p>
        <Switch
          checked={form.http3}
          disabled={locked || saving || (!form.http3 && (!publicOn || !!udpTaken))}
          onChange={(http3) => setForm({ ...form, http3 })}
          label="HTTP/3"
          description={http3Hint}
        />
        <Switch
          checked={form.compression}
          disabled={locked || saving}
          onChange={(compression) => setForm({ ...form, compression })}
          label="Compression"
          description="Zstd, Brotli and Gzip. Your services' own compression is turned off."
        />
        <div className="flex flex-wrap items-center justify-between gap-3">
          <span>
            <span className="block text-[13px] font-medium">Minimum TLS version</span>
            <span className="block text-[12px] text-muted-foreground">
              {tlsApplying ? "Applying…" : "Clients on older versions can't connect over HTTPS."}
            </span>
          </span>
          <Segmented
            label="Minimum TLS version"
            value={form.tlsMin}
            onChange={(tlsMin) => !locked && !saving && setForm({ ...form, tlsMin })}
            options={[
              { value: "1.2", label: "1.2", disabled: locked || saving },
              { value: "1.3", label: "1.3", disabled: locked || saving },
            ]}
          />
        </div>
        <Switch
          checked={proxyOn}
          disabled={!admin || (!proxyOn && !publicOn)}
          onChange={setProxy}
          label="PROXY protocol"
          description={publicOn ? "Only for a load balancer that sends it on every connection." : "Turn on public ports first."}
        />
      </div>
      <div className="space-y-3 border-t border-border px-4 py-3">
        <p className="text-[13px] font-medium">Service defaults</p>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Request timeout (seconds)" hint="0 means no limit. A service's own setting overrides it.">
            {(p) => (
              <Input
                {...p}
                inputMode="numeric"
                value={form.requestTimeout}
                disabled={!admin || saving}
                onChange={(e) => setForm({ ...form, requestTimeout: e.target.value })}
              />
            )}
          </Field>
          <Field label="Idle timeout (minutes)" hint="How long a WebSocket can stay idle.">
            {(p) => (
              <Input
                {...p}
                inputMode="numeric"
                value={form.idleMinutes}
                disabled={locked || saving}
                onChange={(e) => setForm({ ...form, idleMinutes: e.target.value })}
              />
            )}
          </Field>
        </div>
        {"error" in checked ? <p className="text-[12px] text-destructive">{checked.error}</p> : null}
      </div>
      {admin && dirty ? (
        <div className="flex justify-end gap-2 border-t border-border px-4 py-3">
          <Button variant="secondary" size="sm" disabled={saving} onClick={() => setForm(formOf(saved))}>
            Cancel
          </Button>
          <Button size="sm" disabled={!next || saving} onClick={() => (lines.length ? setConfirming(true) : void save())}>
            Save
          </Button>
        </div>
      ) : null}
      {confirming ? (
        <ConfirmDialog
          open
          onClose={() => setConfirming(false)}
          title={`Save gateway settings for ${c.name}?`}
          description={
            <div className="space-y-1.5">
              {lines.map((l) => (
                <p key={l}>{l}</p>
              ))}
            </div>
          }
          confirmLabel="Save"
          onConfirm={() => void save()}
        />
      ) : null}
      {proxy !== null ? (
        <ConfirmDialog
          open
          onClose={() => setProxy(null)}
          tone="primary"
          title={`Turn ${proxy ? "on" : "off"} PROXY protocol on ${c.name}?`}
          description={`Open connections to every website on ${c.name} drop once. WebSockets and uploads in progress are cut.`}
          confirmLabel="Save"
          onConfirm={() => void saveProxy(proxy)}
        />
      ) : null}
    </Panel>
  );
}
