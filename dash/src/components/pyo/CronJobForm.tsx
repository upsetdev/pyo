import { useState } from "react";
import { Field, Input, Select, Switch } from "~/components/ui/Field";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, CronTask } from "~/lib/pyo/types";

const PRESETS: [string, string][] = [
  ["*/15 * * * *", "Every 15 minutes"],
  ["0 * * * *", "Every hour"],
  ["0 3 * * *", "Every day at 03:00"],
  ["0 9 * * 1", "Every Monday at 09:00"],
];

const validSchedule = (s: string) => s.trim().split(/\s+/).length === 5;

/**
 * A cron job's form: which service it runs with, its name, command and
 * schedule. Shared by the new cron job page and the edit dialog.
 */
export function useCronJobForm(edit?: { app: Application; task: CronTask }) {
  const s = usePyo();
  // A cron job runs with its service's image, so the service has to be
  // running and able to share its storage.
  const targets = s.services.filter(
    (a) =>
      a.teamId === s.currentTeamId &&
      a.workloadKind !== "CronJob" &&
      a.workloadKind !== "StatefulSet" &&
      !!a.currentDeploymentId &&
      !s.volumes.some((v) => v.serviceId === a.id),
  );
  const [serviceId, setServiceId] = useState(
    edit?.app.id ?? targets[0]?.id ?? "",
  );
  const [name, setName] = useState(edit?.task.name ?? "");
  const [schedule, setSchedule] = useState(edit?.task.schedule ?? "0 3 * * *");
  const [command, setCommand] = useState(edit?.task.command ?? "");
  const [paused, setPaused] = useState(edit?.task.paused ?? false);
  const [busy, setBusy] = useState(false);
  const ready =
    !!serviceId &&
    !!name.trim() &&
    validSchedule(schedule) &&
    !!command.trim() &&
    !busy;
  const project = (a: Application) =>
    s.projects.find((p) => p.id === a.projectId)?.name;

  /** Saves it; resolves true when it worked (a toast says so). */
  const save = async () => {
    if (!ready) return false;
    setBusy(true);
    const ok = await api.saveCronJob({
      serviceId,
      id: edit?.task.id,
      name: name.trim(),
      schedule: schedule.trim(),
      command: command.trim(),
      paused,
    });
    setBusy(false);
    if (ok) toast({ title: edit ? "Cron job saved" : `Added ${name.trim()}` });
    return ok;
  };

  const fields = (
    <div className="space-y-4">
      <Field
        label="Service"
        hint="Uses this service's image and variables."
      >
        {(p) => (
          <Select
            {...p}
            value={serviceId}
            disabled={!!edit}
            onChange={(e) => setServiceId(e.target.value)}
          >
            {(edit ? [edit.app] : targets).map((a) => (
              <option key={a.id} value={a.id}>
                {project(a) ? `${project(a)} / ${a.name}` : a.name}
              </option>
            ))}
          </Select>
        )}
      </Field>
      <Field label="Name">
        {(p) => (
          <Input
            {...p}
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Nightly cleanup"
            maxLength={60}
          />
        )}
      </Field>
      <Field label="Command" hint="Runs through sh.">
        {(p) => (
          <Input
            {...p}
            mono
            value={command}
            onChange={(e) => setCommand(e.target.value)}
            placeholder="npm run cleanup"
          />
        )}
      </Field>
      <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)] gap-3">
        <Field
          label="Schedule"
          hint="Cron syntax, in UTC."
          error={
            schedule && !validSchedule(schedule)
              ? "Use five fields, like 0 3 * * *."
              : undefined
          }
        >
          {(p) => (
            <Input
              {...p}
              mono
              value={schedule}
              onChange={(e) => setSchedule(e.target.value)}
            />
          )}
        </Field>
        <Field label="Common schedules">
          {(p) => (
            <Select
              {...p}
              value={PRESETS.find(([v]) => v === schedule.trim())?.[0] ?? ""}
              onChange={(e) => e.target.value && setSchedule(e.target.value)}
            >
              <option value="">Custom</option>
              {PRESETS.map(([v, label]) => (
                <option key={v} value={v}>
                  {label}
                </option>
              ))}
            </Select>
          )}
        </Field>
      </div>
      {edit ? (
        <Switch
          checked={paused}
          onChange={setPaused}
          label="Paused"
          description="It doesn't run until you turn this off."
        />
      ) : null}
    </div>
  );

  return {
    /** Whether there's a service it can run with. */
    canRun: targets.length > 0 || !!edit,
    ready,
    busy,
    name,
    save,
    fields,
  };
}
