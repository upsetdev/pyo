import { useEffect, useState } from "react";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { op } from "~/lib/pyo/backend";
import type { SandboxLimits } from "~/lib/pyo/sandbox";
import { toast } from "~/lib/pyo/toast";
import { useSandboxes } from "./common";

// A team's sandbox limits. Only the installation's admin sets them
// (setSandboxLimits). Memory and disk show in GB, the lifetime in hours.

type Form = { running: string; cpus: string; memory: string; disk: string; hours: string };

const toForm = (l: SandboxLimits): Form => ({
  running: String(l.maxRunning),
  cpus: String(l.maxCpus),
  memory: String(+(l.maxMemoryMib / 1024).toFixed(1)),
  disk: String(+(l.maxDiskMib / 1024).toFixed(1)),
  hours: String(+(l.maxTtlSecs / 3600).toFixed(2)),
});

export function SandboxLimitsPanel({ teamId }: { teamId: string }) {
  const q = useSandboxes(teamId);
  const saved = q.status === "success" ? q.data.limits : null;
  const [f, setF] = useState<Form | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (saved && !f) setF(toForm(saved));
  }, [saved, f]);
  if (!f || !saved) return null;
  const num = (v: string) => Number(v);
  const bad = Object.values(f).some((v) => v.trim() === "" || !Number.isFinite(num(v)) || num(v) < 0);
  const tooLong = num(f.hours) > 24;
  const dirty = JSON.stringify(f) !== JSON.stringify(toForm(saved));
  const set = (k: keyof Form) => (e: { target: { value: string } }) => setF({ ...f, [k]: e.target.value });
  const save = async () => {
    setBusy(true);
    try {
      await op("setSandboxLimits", {
        teamId,
        limits: {
          maxRunning: Math.round(num(f.running)),
          maxCpus: Math.round(num(f.cpus)),
          maxMemoryMib: Math.round(num(f.memory) * 1024),
          maxDiskMib: Math.round(num(f.disk) * 1024),
          maxTtlSecs: Math.round(num(f.hours) * 3600),
        },
      });
      toast({ title: "Sandbox limits saved" });
      q.retry();
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setBusy(false);
    }
  };
  const field = (k: keyof Form, label: string, hint?: string) => (
    <Field label={label} hint={hint}>
      {(p) => <Input {...p} type="number" min={0} inputMode="decimal" value={f[k]} onChange={set(k)} />}
    </Field>
  );
  return (
    <Panel title="Sandboxes" description="Limits for this team. Only the installation's admin sets them.">
      <div className="grid gap-4 p-4 sm:grid-cols-3">
        {field("running", "Sandboxes at once", "Paused ones count. 0 turns sandboxes off.")}
        {field("cpus", "CPUs")}
        {field("memory", "Memory (GB)")}
        {field("disk", "Disk (GB)")}
        <Field label="Longest lifetime (hours)" error={tooLong ? "24 hours at most." : undefined}>
          {(p) => <Input {...p} type="number" min={0} max={24} inputMode="decimal" value={f.hours} onChange={set("hours")} />}
        </Field>
      </div>
      <div className="flex justify-end border-t border-border px-4 py-3">
        <Button size="sm" disabled={!dirty || bad || tooLong || busy} onClick={() => void save()}>
          {busy ? "Saving…" : "Save limits"}
        </Button>
      </div>
    </Panel>
  );
}
