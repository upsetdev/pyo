import { Callout } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { fetchTailnetLock } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { roleIn, usePyo } from "~/lib/pyo/store";

/** The command a signing device runs to let a new machine on the tailnet. */
export function TailnetLockSign({ command, name }: { command: string; name?: string }) {
  return (
    <Callout tone="warning">
      <span className="flex flex-col gap-1.5">
        {name ? `${name} is waiting for a Tailnet Lock signature.` : "Waiting for a Tailnet Lock signature."} On a
        signing device, run:
        <span className="inline-flex items-center gap-1">
          <code className="break-all font-mono text-[12px] text-foreground">{command}</code>
          <CopyButton value={command} label="Copy command" />
        </span>
      </span>
    </Callout>
  );
}

/**
 * While a new machine joins: the sign command when Tailnet Lock keeps it
 * off the tailnet. `name` is its hostname. Without one (a new cluster,
 * whose hostname Pyo doesn't know), every machine of Pyo's that waits.
 */
export function TailnetLockWait({ name }: { name?: string }) {
  const s = usePyo();
  const team = s.currentTeamId;
  const role = roleIn(s);
  const allowed = role === "owner" || role === "admin";
  const q = useQuery(
    `tailnet-lock:${team}:${allowed}`,
    () => (allowed ? fetchTailnetLock(team) : Promise.resolve({ waiting: [] })),
    { refreshMs: 10_000 },
  );
  if (q.status !== "success") return null;
  const waiting = q.data.waiting.filter((w) => !name || w.name.toLowerCase() === name.toLowerCase());
  if (!waiting.length) return null;
  return (
    <div className="space-y-2">
      {waiting.map((w) => (
        <TailnetLockSign key={w.name + w.signCommand} command={w.signCommand} name={name ? undefined : w.name} />
      ))}
    </div>
  );
}
