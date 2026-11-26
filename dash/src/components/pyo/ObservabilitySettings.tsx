import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Field, Select, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { REQUEST_LOG_MAX_DAYS } from "~/lib/pyo/requestLogs";
import { withinLimit } from "~/lib/pyo/teamObservability";
import type { IpMode, QueryStrings, Retention } from "~/lib/pyo/types";

// Observability on or off (installation-wide; a new install on a small
// server starts with it off), the defaults and limits teams choose within
// for visitors' IP addresses and query strings (each team picks in Team ›
// Settings › Observability), and how long analytics, metrics and build
// logs are kept (a team can keep its analytics shorter in its settings).
// Countries, cities and unique visitors work in every mode: they're worked
// out when each request comes in, then the address is dropped or kept.

export const MODES: { value: IpMode; label: string; hint: string }[] = [
  {
    value: "none",
    label: "Don't store",
    hint: "Recommended. Keeps nothing that identifies a visitor.",
  },
  {
    value: "anonymized",
    label: "Anonymized",
    hint: "Last part removed.",
  },
  {
    value: "full",
    label: "Full",
    // Addresses stay with the requests on each cluster, never longer.
    hint: `Kept with each team's request logs, up to ${REQUEST_LOG_MAX_DAYS} days. May need consent under laws like GDPR.`,
  },
];

const QUERY_STRINGS: { value: QueryStrings; label: string }[] = [
  { value: "keep", label: "Keep" },
  { value: "names", label: "Names only" },
  { value: "none", label: "Don't keep" },
];

export function ObservabilitySettingsSection() {
  const s = usePyo();
  const { ipMode: current, enabled, small, retention } = s.observabilitySettings;
  const admin = !!s.user.isAdmin;
  // Only the installation's admins get these, and only they open this page.
  if (!current || !retention) return null;
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
      <VisitorsPanel
        key={JSON.stringify([
          current,
          s.observabilitySettings.maxIpMode,
          s.observabilitySettings.queryStrings,
          s.observabilitySettings.maxQueryStrings,
        ])}
        saved={{
          ipMode: current,
          maxIpMode: s.observabilitySettings.maxIpMode ?? current,
          queryStrings: s.observabilitySettings.queryStrings ?? "keep",
          maxQueryStrings: s.observabilitySettings.maxQueryStrings ?? "keep",
        }}
        admin={admin}
      />
      <RetentionPanel
        key={JSON.stringify(retention)}
        saved={retention}
        admin={admin}
        note={note}
      />
    </div>
  );
}

type Visitors = {
  ipMode: IpMode;
  maxIpMode: IpMode;
  queryStrings: QueryStrings;
  maxQueryStrings: QueryStrings;
};

/** What teams keep of visitors: a default, and the most a team may pick. */
function VisitorsPanel({ saved, admin }: { saved: Visitors; admin: boolean }) {
  const [v, setV] = useState(saved);
  const dirty = (Object.keys(saved) as (keyof Visitors)[]).some(
    (k) => v[k] !== saved[k],
  );
  const ipError = withinLimit(v.ipMode, v.maxIpMode)
    ? undefined
    : "The default can't be above the limit.";
  const queryError = withinLimit(v.queryStrings, v.maxQueryStrings)
    ? undefined
    : "The default can't be above the limit.";
  const select = <K extends keyof Visitors>(
    key: K,
    options: { value: Visitors[K]; label: string }[],
    p: { id: string },
  ) => (
    <Select
      {...p}
      value={v[key]}
      disabled={!admin}
      onChange={(e) => setV({ ...v, [key]: e.target.value as Visitors[K] })}
    >
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </Select>
  );
  const limitHint = MODES.find((m) => m.value === v.maxIpMode)?.hint;
  return (
    <Panel title="Visitors" description="Teams choose within these limits.">
      <div className="grid gap-4 p-4 sm:grid-cols-2">
        <Field label="IP addresses: default" error={ipError}>
          {(p) => select("ipMode", MODES, p)}
        </Field>
        <Field label="IP addresses: limit" hint={limitHint}>
          {(p) => select("maxIpMode", MODES, p)}
        </Field>
        <Field
          label="Query strings: default"
          hint="Passwords and tokens are always hidden."
          error={queryError}
        >
          {(p) => select("queryStrings", QUERY_STRINGS, p)}
        </Field>
        <Field label="Query strings: limit">
          {(p) => select("maxQueryStrings", QUERY_STRINGS, p)}
        </Field>
      </div>
      {admin ? (
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={!dirty || !!ipError || !!queryError}
            onClick={async () => {
              if (await api.saveObservabilitySettings(v))
                toast({ title: "Visitor settings saved" });
            }}
          >
            Save changes
          </Button>
        </div>
      ) : null}
    </Panel>
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
