import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Field, optionClass, Select, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import {
  api,
  currentTeam,
  isTeamAdmin,
  roleIn,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { REQUEST_LOG_DAYS } from "~/lib/pyo/requestLogs";
import { withinLimit } from "~/lib/pyo/teamObservability";
import type { IpMode, QueryStrings, Team } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// A team's observability: what its servers collect and keep, within the
// installation's limits (teamobs.rs in the API). Everything comes with the
// snapshot, so the page renders once with the right options. Owners and
// admins change it. Everyone else sees the values.

export const Route = createFileRoute("/_app/$team/settings/observability")({
  head: () => ({ meta: seo({ title: "Observability" }) }),
  component: () => {
    const ws = currentTeam(usePyo());
    return ws ? <TeamObservability key={ws.id} ws={ws} /> : null;
  },
});

/** Shown to members who can't change a panel's settings. */
const READ_ONLY = "An owner or admin can change this.";

const IP_MODES: { value: IpMode; label: string; hint: string }[] = [
  { value: "none", label: "Don't store", hint: "Keeps nothing that identifies a visitor." },
  { value: "anonymized", label: "Anonymized", hint: "Last part removed." },
  { value: "full", label: "Full", hint: "May need consent under laws like GDPR." },
];

const QUERY_STRINGS: { value: QueryStrings; label: string }[] = [
  { value: "keep", label: "Keep" },
  { value: "names", label: "Names only" },
  { value: "none", label: "Don't keep" },
];

function TeamObservability({ ws }: { ws: Team }) {
  const s = usePyo();
  const admin = isTeamAdmin(roleIn(s, ws.id));
  const now = ws.observability;
  const limits = ws.observabilityLimits;
  const installationOn = s.observabilitySettings.enabled;
  // An API older than this page sends neither: nothing to show.
  if (!now || !limits) return null;
  const note = admin ? undefined : READ_ONLY;
  const save = async (
    change: Parameters<typeof api.setTeamObservability>[1],
    title: string,
  ) => {
    if (await api.setTeamObservability(ws.id, change)) toast({ title });
  };
  return (
    <div className="space-y-4">
      <Panel
        title="Collection"
        description={
          installationOn ? note : "Observability is off for this installation."
        }
      >
        <div className="space-y-4 p-4">
          <Switch
            checked={now.on}
            disabled={!admin}
            onChange={(on) =>
              void save(
                { enabled: on },
                on ? "Observability on" : "Observability off",
              )
            }
            label="Observability"
            description={
              now.on
                ? "Analytics, request logs and usage history for this team."
                : "Your servers stop collecting. History is kept."
            }
          />
          <Switch
            checked={now.requestLogs}
            disabled={!admin || !now.on}
            onChange={(on) =>
              void save(
                { requestLogs: on },
                on ? "Request logs on" : "Request logs off",
              )
            }
            label="Request logs"
            description="Every request, kept on your servers."
          />
        </div>
      </Panel>
      <Panel title="Visitors" description={note}>
        <div className="space-y-4 p-4">
          <div
            role="radiogroup"
            aria-label="IP addresses"
            className="space-y-2"
          >
            <p className="text-[13px] font-medium">IP addresses</p>
            {IP_MODES.map((m) => {
              const allowed = withinLimit(m.value, limits.ipMode);
              const selected = now.ipMode === m.value;
              const enabled = admin && allowed;
              return (
                <label
                  key={m.value}
                  className={`${optionClass({ selected })} flex items-start gap-3 px-4 py-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20 ${enabled ? "cursor-pointer" : "cursor-not-allowed opacity-80"}`}
                >
                  <input
                    type="radio"
                    name="team-ip-mode"
                    className="mt-0.5"
                    checked={selected}
                    disabled={!enabled}
                    onChange={() =>
                      void save(
                        { ipMode: m.value },
                        `IP addresses: ${m.label.toLowerCase()}`,
                      )
                    }
                  />
                  <span>
                    <span className="block text-[13px] font-medium">
                      {m.label}
                    </span>
                    <span className="block text-[12px] text-muted-foreground">
                      {allowed ? m.hint : "Not available."}
                    </span>
                  </span>
                </label>
              );
            })}
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field
              label="Query strings"
              hint="Passwords and tokens are always hidden."
            >
              {(p) => (
                <Select
                  {...p}
                  value={now.queryStrings}
                  disabled={!admin}
                  onChange={(e) => {
                    const q = e.target.value as QueryStrings;
                    const label =
                      QUERY_STRINGS.find((x) => x.value === q)?.label ?? q;
                    void save(
                      { queryStrings: q },
                      `Query strings: ${label.toLowerCase()}`,
                    );
                  }}
                >
                  {QUERY_STRINGS.map((q) => {
                    const allowed = withinLimit(q.value, limits.queryStrings);
                    return (
                      <option key={q.value} value={q.value} disabled={!allowed}>
                        {allowed ? q.label : `${q.label} (not available)`}
                      </option>
                    );
                  })}
                </Select>
              )}
            </Field>
          </div>
        </div>
      </Panel>
      <RequestLogsPanel ws={ws} admin={admin} />
      {ws.analyticsMonthsLimit ? (
        <AnalyticsPanel
          key={`${ws.analyticsMonths}:${ws.analyticsMonthsLimit}`}
          ws={ws}
          installation={ws.analyticsMonthsLimit}
          admin={admin}
        />
      ) : null}
    </div>
  );
}

const SIZES = [500, 1024, 2048, 5120, 10240];
const sizeLabel = (mb: number) => (mb < 1024 ? `${mb} MB` : `${mb / 1024} GB`);

/** How much of its requests each of the team's clusters keeps. */
function RequestLogsPanel({ ws, admin }: { ws: Team; admin: boolean }) {
  const saved = ws.preferences.requestLogs ?? { maxMb: 1024, maxDays: 7 };
  const [limits, setLimits] = useState(saved);
  const dirty =
    limits.maxMb !== saved.maxMb || limits.maxDays !== saved.maxDays;
  return (
    <Panel title="Logs" description={admin ? undefined : READ_ONLY}>
      <div className="grid gap-4 p-4 sm:grid-cols-2">
        <Field label="Size" hint="Kept on your servers, per cluster.">
          {(p) => (
            <Select
              {...p}
              value={String(limits.maxMb)}
              disabled={!admin}
              onChange={(e) =>
                setLimits({ ...limits, maxMb: Number(e.target.value) })
              }
            >
              {[...new Set([...SIZES, saved.maxMb])]
                .sort((a, b) => a - b)
                .map((mb) => (
                  <option key={mb} value={mb}>
                    {sizeLabel(mb)}
                  </option>
                ))}
            </Select>
          )}
        </Field>
        <Field label="Days" hint="The oldest go first.">
          {(p) => (
            <Select
              {...p}
              value={String(limits.maxDays)}
              disabled={!admin}
              onChange={(e) =>
                setLimits({ ...limits, maxDays: Number(e.target.value) })
              }
            >
              {[...new Set([...REQUEST_LOG_DAYS, saved.maxDays])]
                .sort((a, b) => a - b)
                .map((d) => (
                  <option key={d} value={d}>
                    {d === 1 ? "1 day" : `${d} days`}
                  </option>
                ))}
            </Select>
          )}
        </Field>
      </div>
      {admin ? (
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={!dirty}
            onClick={async () => {
              const res = await api.setTeamObservability(ws.id, {
                requestLogMaxMb: limits.maxMb,
                requestLogMaxDays: limits.maxDays,
              });
              if (res) toast({ title: "Logs saved" });
            }}
          >
            Save changes
          </Button>
        </div>
      ) : null}
    </Panel>
  );
}

const ANALYTICS_MONTHS = [3, 6, 12];

/** How long the team's analytics are kept: as long as the installation
 * keeps them, or shorter. */
function AnalyticsPanel({
  ws,
  installation,
  admin,
}: {
  ws: Team;
  installation: number;
  admin: boolean;
}) {
  const saved = Math.min(ws.analyticsMonths ?? installation, installation);
  const [months, setMonths] = useState(saved);
  const choices = [
    ...ANALYTICS_MONTHS.filter((m) => m < installation),
    installation,
  ];
  return (
    <Panel title="Analytics" description={admin ? undefined : READ_ONLY}>
      <div className="grid gap-4 p-4 sm:grid-cols-2">
        <Field
          label="Kept for"
          hint={`Up to ${installation} months. Older analytics go each night.`}
        >
          {(p) => (
            <Select
              {...p}
              value={String(months)}
              disabled={!admin}
              onChange={(e) => setMonths(Number(e.target.value))}
            >
              {choices.map((m) => (
                <option key={m} value={m}>
                  {m} months
                </option>
              ))}
            </Select>
          )}
        </Field>
      </div>
      {admin ? (
        <div className="flex justify-end border-t border-border px-4 py-3">
          <Button
            size="sm"
            disabled={months === saved}
            onClick={async () => {
              const res = await api.setTeamObservability(ws.id, {
                analyticsMonths: months === installation ? null : months,
              });
              if (res) toast({ title: "Analytics saved" });
            }}
          >
            Save changes
          </Button>
        </div>
      ) : null}
    </Panel>
  );
}
