import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { useHosted } from "~/components/pyo/Managed";
import { Button } from "~/components/ui/Button";
import { Field, Select, Switch } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { SettingsSection } from "../_settings";

export const Route = createFileRoute("/_app/_settings/security")({
  component: () => (
    <SettingsSection description="Two-factor authentication and the audit log, for every team.">
      <SecuritySettingsSection />
    </SettingsSection>
  ),
});

const DAYS = [90, 180, 365, 730];

function SecuritySettingsSection() {
  const s = usePyo();
  const hosted = useHosted();
  const saved = s.securitySettings ?? { requireTwoFactor: false, auditDays: 365 };
  const admin = !!s.user.isAdmin && !hosted;
  const mine = !!s.user.twoFactor?.enabled;
  const [days, setDays] = useState(saved.auditDays);
  const note = hosted
    ? "Pyo Cloud keeps the audit log a year. Its staff always use two-factor."
    : admin
      ? undefined
      : "Only the installation's admin can change this.";
  return (
    <div className="space-y-6">
      <Panel description={note}>
        <div className="p-4">
          <Switch
            checked={saved.requireTwoFactor}
            disabled={!admin || (!saved.requireTwoFactor && !mine)}
            onChange={async (on) => {
              if (await api.saveSecuritySettings({ ...saved, requireTwoFactor: on }))
                toast({
                  title: on ? "Two-factor required" : "Two-factor not required",
                });
            }}
            label="Require two-factor for everyone"
            description={
              admin && !saved.requireTwoFactor && !mine
                ? "Turn it on for your own account first, in your profile."
                : "Accounts without it can only open their profile."
            }
          />
        </div>
      </Panel>
      <Panel title="Audit log" description={note}>
        <div className="p-4">
          <Field label="Keep entries" hint="Teams choose what it keeps of addresses.">
            {(p) => (
              <Select
                {...p}
                value={String(hosted ? 365 : days)}
                disabled={!admin}
                onChange={(e) => setDays(Number(e.target.value))}
              >
                {DAYS.map((n) => (
                  <option key={n} value={n}>
                    {n === 365 ? "1 year" : n === 730 ? "2 years" : `${n} days`}
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
              disabled={days === saved.auditDays}
              onClick={async () => {
                if (await api.saveSecuritySettings({ ...saved, auditDays: days }))
                  toast({ title: "Audit log retention saved" });
              }}
            >
              Save changes
            </Button>
          </div>
        ) : null}
      </Panel>
    </div>
  );
}
