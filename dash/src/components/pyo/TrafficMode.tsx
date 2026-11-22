import { useState, type ReactNode } from "react";
import { TunnelSettings, tunnelUnavailable } from "~/components/pyo/Tunnel";
import { LearnMore } from "~/components/ui/LearnMore";
import { optionClass, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api, clusterTraffic, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Cluster, ClusterTraffic as Traffic, TrafficMode } from "~/lib/pyo/types";

// How a cluster routes requests. Replicas are placed the same way in every
// mode (evenly across regions, then servers); this only changes where each
// request goes.

const MODES: { value: TrafficMode; label: string; hint: string }[] = [
  { value: "local", label: "Local", hint: "Keep traffic on the same server" },
  { value: "region", label: "Regional", hint: "Keep traffic within the same region" },
  { value: "spread", label: "Global", hint: "Distribute traffic across all servers" },
];

/** One choice of a radio group panel: a label and a hint line. */
function RadioOption({
  name,
  checked,
  disabled,
  label,
  hint,
  onSelect,
}: {
  name: string;
  checked: boolean;
  disabled: boolean;
  label: string;
  hint: ReactNode;
  onSelect: () => void;
}) {
  return (
    <label
      className={`${optionClass({ selected: checked })} flex gap-3 px-4 py-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20 ${disabled ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
    >
      <input
        type="radio"
        name={name}
        className="mt-0.5"
        checked={checked}
        disabled={disabled}
        onChange={onSelect}
      />
      <span>
        <span className="block text-[13px] font-medium">{label}</span>
        <span className="block text-[12px] leading-5 text-muted-foreground">{hint}</span>
      </span>
    </label>
  );
}

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
          <RadioOption
            key={m.value}
            name={`traffic-${c.id}`}
            checked={current === m.value}
            disabled={off(m.value)}
            label={m.label}
            hint={capable && off(m.value) ? "Not reported yet." : m.hint}
            onSelect={async () => {
              const res = await api.setClusterTrafficMode(c.id, m.value);
              if (res?.ok) toast({ title: m.label, description: c.name });
            }}
          />
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

const TRAFFIC: { value: Traffic; label: string }[] = [
  { value: "public", label: "Public ports" },
  { value: "tailscale", label: "Tailscale only" },
  { value: "tunnel", label: "Cloudflare Tunnel" },
];

/** How the cluster takes web traffic: public ports, only the tailnet, or Cloudflare Tunnel. */
export function ClusterTraffic({ cluster: c }: { cluster: Cluster }) {
  const s = usePyo();
  const current = clusterTraffic(c);
  // Cloudflare Tunnel waits for its settings and a confirm before it applies.
  const [picked, setPicked] = useState<Traffic | null>(null);
  const shown = picked ?? current;
  const unavailable = current === "tunnel" ? null : tunnelUnavailable(s, c);
  const hint = (m: Traffic) =>
    m === "public"
      ? "Ports 80 and 443 on every server except build-only servers."
      : m === "tailscale"
        ? c.tailnet || c.tailscale
          ? "Only from your tailnet."
          : "Only inside the cluster until you set up Tailscale."
        : (unavailable ?? "No open ports. Websites come in through Cloudflare.");
  const pick = async (m: Traffic) => {
    if (m === "tunnel" && current !== "tunnel") return setPicked(m);
    setPicked(null);
    if (m === current) return;
    const res = await api.setClusterTraffic(c.id, m);
    if (res?.ok)
      toast({ title: TRAFFIC.find((x) => x.value === m)!.label, description: c.name });
  };
  return (
    <Panel title="Traffic" actions={<LearnMore page="gateway" />}>
      <div role="radiogroup" aria-label="How the cluster takes web traffic" className="space-y-2 p-4">
        {TRAFFIC.map((m) => (
          <RadioOption
            key={m.value}
            name={`public-${c.id}`}
            checked={shown === m.value}
            disabled={m.value === "tunnel" && !!unavailable}
            label={m.label}
            hint={hint(m.value)}
            onSelect={() => void pick(m.value)}
          />
        ))}
      </div>
      {shown === "tunnel" || c.tunnel ? (
        <TunnelSettings
          c={c}
          editing={shown === "tunnel"}
          onCancel={() => setPicked(null)}
        />
      ) : null}
    </Panel>
  );
}
