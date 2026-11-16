import { useEffect, useState } from "react";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { Select } from "~/components/ui/Select";
import { op } from "~/lib/pyo/backend";
import type { SandboxLimits } from "~/lib/pyo/sandbox";
import { toast } from "~/lib/pyo/toast";
import { useSandboxes } from "./common";
import { CustomLifetimeInput, customError, customSecs, toCustom, type CustomLifetime } from "./Lifetime";

// A team's sandbox limits. Only the installation's admin sets them
// (setSandboxLimits). Memory and disk show in GB. The longest lifetime is a
// preset, a custom one up to 365 days, or no limit.

const MAX_LIFETIMES = [
  { value: "3600", label: "1 hour" },
  { value: "86400", label: "1 day" },
  { value: "604800", label: "7 days" },
  { value: "2592000", label: "30 days" },
  { value: "31536000", label: "365 days" },
] as const;

const UNITS = ["m", "h", "d"] as const;

type Form = {
  running: string;
  cpus: string;
  memory: string;
  disk: string;
  /** A preset's seconds, "custom" or "none". */
  ttl: string;
  custom: CustomLifetime;
};

const toForm = (l: SandboxLimits): Form => {
  const t = l.maxTtlSecs;
  const preset = t !== null && MAX_LIFETIMES.some((x) => x.value === String(t));
  return {
    running: String(l.maxRunning),
    cpus: String(l.maxCpus),
    memory: String(+(l.maxMemoryMib / 1024).toFixed(1)),
    disk: String(+(l.maxDiskMib / 1024).toFixed(1)),
    ttl: t === null ? "none" : preset ? String(t) : "custom",
    custom: t === null || preset ? { amount: "2", unit: "d" } : toCustom(t, UNITS),
  };
};

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
  const numbers = [f.running, f.cpus, f.memory, f.disk];
  const bad = numbers.some((v) => v.trim() === "" || !Number.isFinite(num(v)) || num(v) < 0);
  const ttlError = f.ttl === "custom" ? customError(f.custom) : null;
  const maxTtlSecs = f.ttl === "none" ? null : f.ttl === "custom" ? customSecs(f.custom) : num(f.ttl);
  const dirty = JSON.stringify(f) !== JSON.stringify(toForm(saved));
  const set = (k: "running" | "cpus" | "memory" | "disk") => (e: { target: { value: string } }) =>
    setF({ ...f, [k]: e.target.value });
  const save = async () => {
    setBusy(true);
    try {
      const limits: SandboxLimits = {
        maxRunning: Math.round(num(f.running)),
        maxCpus: Math.round(num(f.cpus)),
        maxMemoryMib: Math.round(num(f.memory) * 1024),
        maxDiskMib: Math.round(num(f.disk) * 1024),
        maxTtlSecs,
      };
      await op("setSandboxLimits", { teamId, limits });
      toast({ title: "Sandbox limits saved" });
      setF(toForm(limits));
      q.retry();
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setBusy(false);
    }
  };
  const field = (k: "running" | "cpus" | "memory" | "disk", label: string, hint?: string) => (
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
        <Field
          label="Longest lifetime"
          error={ttlError ?? undefined}
          hint={f.ttl === "none" ? "Sandboxes may run until they're deleted." : undefined}
        >
          {(p) => (
            <div className="space-y-2">
              <Select {...p} value={f.ttl} onChange={(e) => setF({ ...f, ttl: e.target.value })}>
                {MAX_LIFETIMES.map((x) => (
                  <option key={x.value} value={x.value}>
                    {x.label}
                  </option>
                ))}
                <option value="custom">Custom</option>
                <option value="none">No limit</option>
              </Select>
              {f.ttl === "custom" ? (
                <CustomLifetimeInput
                  aria-label="Longest lifetime"
                  value={f.custom}
                  onChange={(custom) => setF({ ...f, custom })}
                  units={UNITS}
                />
              ) : null}
            </div>
          )}
        </Field>
      </div>
      <div className="flex justify-end border-t border-border px-4 py-3">
        <Button size="sm" disabled={!dirty || bad || !!ttlError || busy} onClick={() => void save()}>
          {busy ? "Saving…" : "Save limits"}
        </Button>
      </div>
    </Panel>
  );
}
