import { LearnMore } from "~/components/ui/LearnMore";
import { optionClass, Segmented, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster, FreeDomainAddress, TrafficMode } from "~/lib/pyo/types";

// How a cluster routes requests. Replicas are placed the same way in every
// mode (evenly across regions, then servers); this only changes where each
// request goes.

const MODES: { value: TrafficMode; label: string; hint: string }[] = [
  { value: "local", label: "Local", hint: "Keep traffic on the same server" },
  { value: "region", label: "Regional", hint: "Keep traffic within the same region" },
  { value: "spread", label: "Global", hint: "Distribute traffic across all servers" },
];

export function ClusterTrafficMode({ cluster: c }: { cluster: Cluster }) {
  const capable = !!c.capabilities.trafficModes;
  const current = c.trafficMode ?? "region";
  // The API checks the team role (owners and admins).
  const off = (m: TrafficMode) => !capable || (m === "local" && !c.capabilities.trafficLocal);
  return (
    <Panel title="Traffic routing" description={!capable ? "Not reported yet." : undefined}>
      <div
        role="radiogroup"
        aria-label="How traffic is routed"
        className="space-y-2 p-4"
      >
        {MODES.map((m) => (
          <label
            key={m.value}
            className={`${optionClass({ selected: current === m.value })} flex gap-3 px-4 py-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20 ${off(m.value) ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
          >
            <input
              type="radio"
              name={`traffic-${c.id}`}
              className="mt-0.5"
              checked={current === m.value}
              disabled={off(m.value)}
              onChange={async () => {
                const res = await api.setClusterTrafficMode(c.id, m.value);
                if (res?.ok) toast({ title: m.label, description: c.name });
              }}
            />
            <span>
              <span className="block text-[13px] font-medium">{m.label}</span>
              <span className="block text-[12px] leading-5 text-muted-foreground">
                {capable && off(m.value) ? "Not reported yet." : m.hint}
              </span>
            </span>
          </label>
        ))}
      </div>
      <div className="border-t border-border p-4">
        <Switch
          checked={!!c.webProxyProtocol}
          onChange={async (on) => {
            const res = await api.setClusterWebProxyProtocol(c.id, on);
            if (res?.ok)
              toast({
                title: on
                  ? "Websites accept PROXY protocol"
                  : "Websites no longer read PROXY protocol",
                description: c.name,
              });
          }}
          label="Accept PROXY protocol on web traffic"
          description={<>Only if all traffic comes through a load balancer that sends it. <LearnMore page="gateway" /></>}
        />
      </div>
    </Panel>
  );
}

/** Whether the gateway answers public traffic on the cluster's servers. */
export function ClusterPublicTraffic({ cluster: c }: { cluster: Cluster }) {
  const on = c.publicTraffic !== false;
  const mode = c.freeDomainAddress ?? "public";
  return (
    <Panel title="Public traffic">
      <div className="p-4">
        <Switch
          checked={on}
          onChange={async (v) => {
            const res = await api.setClusterPublicTraffic(c.id, v);
            if (res?.ok)
              toast({
                title: v ? "Public traffic on" : "Public traffic off",
                description: c.name,
              });
          }}
          label="Accept public traffic"
          description={
            <>
              {on
                ? "On every server except build-only servers."
                : mode === "tailscale"
                  ? "Services are reachable only from your tailnet."
                  : "Services are reachable only inside the cluster."}{" "}
              <LearnMore page="gateway" />
            </>
          }
        />
      </div>
      <div className="border-t border-border p-4">
        <p className="mb-2 text-[13px] font-medium">Free domains</p>
        <Segmented<FreeDomainAddress>
          label="Free domains point at"
          value={mode}
          options={[
            { value: "public", label: "Public IP" },
            { value: "tailscale", label: "Tailscale" },
          ]}
          onChange={async (v) => {
            const res = await api.setClusterFreeDomainAddress(c.id, v);
            if (res?.ok)
              toast({
                title: v === "public" ? "Free domains on the public IP" : "Free domains on Tailscale",
                description: c.name,
              });
          }}
        />
        <p className="mt-2 text-[12px] text-muted-foreground">
          {c.freeDomainIp ? (
            <span className="font-mono text-foreground/85">{c.freeDomainIp}</span>
          ) : mode === "tailscale" ? (
            "No Tailscale IP found."
          ) : on ? (
            "No public IP found yet."
          ) : (
            "Public traffic is off."
          )}
        </p>
        {c.behindNat && (
          <p className="mt-1 text-[12px] text-muted-foreground">
            Behind NAT. Choose Public IP if ports 80 and 443 are forwarded to this server.
          </p>
        )}
      </div>
    </Panel>
  );
}
