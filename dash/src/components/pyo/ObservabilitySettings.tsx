import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Field, optionClass, Select, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { IpMode, Retention } from "~/lib/pyo/types";

// Observability on or off (installation-wide; a new install on a small
// server starts with it off), what web analytics keeps of visitors' IP
// addresses, for every team, and how long analytics, metrics and build
// logs are kept (a team can keep its analytics shorter in its settings).
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
      <RetentionPanel
        key={JSON.stringify(s.observabilitySettings.retention)}
        saved={s.observabilitySettings.retention}
        admin={admin}
        note={note}
      />
    </div>
  );
}

const ANALYTICS_MONTHS = [3, 6, 12, 24];
const METRICS_DAYS = [7, 14, 30, 60, 90];
const BUILD_LOG_DAYS = [30, 60, 90, 180, 365];
const months = (n: number) => `${n} months`;
const days = (n: number) => `${n} days`;

/** How long the installation keeps analytics, metrics and build logs. */
function RetentionPanel({
  saved,
  admin,
  note,
}: {
  saved: Retention;
  admin: boolean;
  note?: string;
}) {
  const [r, setR] = useState(saved);
  const dirty =
    r.analyticsMonths !== saved.analyticsMonths ||
    r.metricsDays !== saved.metricsDays ||
    r.buildLogDays !== saved.buildLogDays;
  const field = (
    label: string,
    hint: string,
    key: keyof Retention,
    choices: number[],
    text: (n: number) => string,
  ) => (
    <Field label={label} hint={hint}>
      {(p) => (
        <Select
          {...p}
          value={String(r[key])}
          disabled={!admin}
          onChange={(e) => setR({ ...r, [key]: Number(e.target.value) })}
        >
          {[...new Set([...choices, saved[key]])]
            .sort((a, b) => a - b)
            .map((n) => (
              <option key={n} value={n}>
                {text(n)}
              </option>
            ))}
        </Select>
      )}
    </Field>
  );
  return (
    <Panel title="Retention" description={note}>
      <div className="grid gap-4 p-4 sm:grid-cols-3">
        {field(
          "Analytics",
          "Teams can keep less.",
          "analyticsMonths",
          ANALYTICS_MONTHS,
          months,
        )}
        {field(
          "Metrics",
          "Servers, services and databases.",
          "metricsDays",
          METRICS_DAYS,
          days,
        )}
        {field(
          "Build logs",
          "Builds and deploys.",
          "buildLogDays",
          BUILD_LOG_DAYS,
          days,
        )}
      </div>
      {admin ? (
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={!dirty}
            onClick={async () => {
              if (await api.saveRetentionSettings(r))
                toast({ title: "Retention saved" });
            }}
          >
            Save changes
          </Button>
        </div>
      ) : null}
    </Panel>
  );
}
