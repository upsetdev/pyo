import { optionClass, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { IpMode } from "~/lib/pyo/types";

// Observability on or off (installation-wide; a new install on a small
// server starts with it off), and what web analytics keeps of visitors' IP
// addresses, for every team.
// Countries, cities and unique visitors work in every mode: they're worked
// out when each request comes in, then the address is dropped or kept.

const MODES: { value: IpMode; label: string; hint: string }[] = [
  {
    value: "none",
    label: "Don't store IP addresses",
    hint: "Recommended. Keeps nothing that identifies a visitor.",
  },
  {
    value: "anonymized",
    label: "Store them anonymized",
    hint: "Last part removed.",
  },
  {
    value: "full",
    label: "Store full IP addresses",
    hint: "Kept for 180 days. May need consent under laws like GDPR.",
  },
];

export function ObservabilitySettingsSection() {
  const s = usePyo();
  const { ipMode: current, enabled, small } = s.observabilitySettings;
  const admin = !!s.user.isAdmin;
  // The server's memory now, against the setting: a hint, never a change.
  const hint =
    small === true && enabled
      ? "This server has under 3.5 GB of memory. Turning observability off saves about 150 MB."
      : small === false && !enabled
        ? "This server has enough memory for observability now."
        : undefined;
  const note = admin
    ? undefined
    : "Only the installation's admin can change this.";
  return (
    <div className="space-y-6">
      <Panel description={note}>
        <div className="p-4">
          <Switch
            checked={enabled}
            disabled={!admin}
            onChange={async (on) => {
              if (await api.setObservability(on))
                toast({ title: on ? "Observability on" : "Observability off" });
            }}
            label="Observability"
            description={hint}
          />
        </div>
      </Panel>
      <Panel description={note}>
        <div
          role="radiogroup"
          aria-label="Visitor IP addresses"
          className="space-y-2 p-4"
        >
          {MODES.map((m) => (
            <label
              key={m.value}
              className={`${optionClass({ selected: current === m.value })} flex items-start gap-3 px-4 py-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20 ${admin ? "cursor-pointer" : "cursor-not-allowed opacity-80"}`}
            >
              <input
                type="radio"
                name="ip-mode"
                className="mt-0.5"
                checked={current === m.value}
                disabled={!admin}
                onChange={async () => {
                  if (await api.saveObservabilitySettings(m.value))
                    toast({ title: m.label });
                }}
              />
              <span>
                <span className="block text-[13px] font-medium">{m.label}</span>
                <span className="block text-[12px] text-muted-foreground">
                  {m.hint}
                </span>
              </span>
            </label>
          ))}
        </div>
      </Panel>
    </div>
  );
}
