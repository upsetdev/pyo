import { ArrowSquareOutIcon } from "@phosphor-icons/react";
import { useEffect, useState, type ReactNode } from "react";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Switch, switchRowAlign, useWrapsPastTwoLines } from "~/components/ui/Field";
import { Panel, Skeleton } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { fetchDatabaseExtensions } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { api } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Database, DatabaseExtension, DatabaseExtensionState } from "~/lib/pyo/types";
import { useDatabase } from "./DatabaseLayout";

// Postgres extensions: the ones Pyo turns on and off (saved together, so a
// batch restarts the database once), and the ones the default role adds.
// KV: Valkey's modules, the same way.
export function DatabaseExtensions() {
  const d = useDatabase();
  return d?.engine === "postgres" || d?.engine === "valkey" ? <Extensions key={d.id} d={d} /> : null;
}

const byName = (a: DatabaseExtension, b: DatabaseExtension) =>
  a.name.localeCompare(b.name, undefined, { sensitivity: "base" });

const sameSet = (a: string[], b: string[]) => a.length === b.length && a.every((x) => b.includes(x));

/** "a", "a and b", "a, b and c". */
const list = (xs: string[]) => (xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs[xs.length - 1]}`);

/** The row's badge for what the cluster reports, if anything is under way or wrong. */
function stateBadge(s: DatabaseExtensionState | undefined): { tone: Tone; label: string } | null {
  if (!s) return null;
  switch (s.state) {
    case "pending":
      return { tone: "progress", label: "Enabling…" };
    case "restarting":
      return { tone: "progress", label: s.enabled ? "Pending restart" : "Disabling…" };
    case "disabling":
      return s.message && !s.message.startsWith("Restarts")
        ? { tone: "danger", label: "Not turned off" }
        : { tone: "progress", label: "Disabling…" };
    case "failed":
      return { tone: "danger", label: "Failed" };
    default:
      return null;
  }
}

function Extensions({ d }: { d: Database }) {
  const kv = d.engine === "valkey";
  const q = useQuery("database-extensions", fetchDatabaseExtensions);
  const all = q.status === "success" ? q.data.extensions.filter((e) => (e.engine ?? "postgres") === d.engine) : [];
  const states = d.extensions ?? [];
  const on = states.filter((s) => s.enabled).map((s) => s.name);
  // Switches change this draft; Save sends the whole list at once.
  const [draft, setDraft] = useState<string[] | null>(null);
  const [asking, setAsking] = useState(false);
  const [saving, setSaving] = useState(false);
  const wanted = draft ?? on;
  const turnOn = wanted.filter((n) => !on.includes(n));
  const turnOff = on.filter((n) => !wanted.includes(n));
  const changes = [...turnOn, ...turnOff];
  const restarts = all.filter((e) => e.restart && changes.includes(e.name)).map((e) => e.name);

  // Saved: the draft goes once the database reports the same list.
  useEffect(() => {
    if (draft && !saving && sameSet(draft, on)) setDraft(null);
  }, [draft, on, saving]);

  // Obsolete ones only while this database has them.
  const managed = all
    .filter((e) => e.section === "managed" && (!e.hidden || states.some((s) => s.name === e.name)))
    .sort(byName);
  const additional = all.filter((e) => e.section === "additional").sort(byName);

  const toggle = (name: string, v: boolean) =>
    setDraft((cur) => {
      const base = cur ?? on;
      return v ? [...base.filter((n) => n !== name), name] : base.filter((n) => n !== name);
    });

  const save = async () => {
    setSaving(true);
    const r = await api.setDatabaseExtensions(d.id, wanted);
    setSaving(false);
    if ("error" in r) toast({ tone: "error", title: "Not saved", description: r.error });
    else toast({ title: "Saved", description: r.data.restarts ? `${d.name} restarts to apply it.` : undefined });
  };

  const ha = d.instances > 1;
  const single = changes.length === 1 ? changes[0] : null;
  const restartText = restarts.length
    ? `${single ? `Turning ${turnOn.length ? "on" : "off"} ${single} restarts` : "Saving restarts"} this database${single ? "" : " once"}. ${
        ha ? "Replicas restart first and the primary hands over." : "It's unavailable for a moment."
      }`
    : null;
  const dropText = !turnOff.length
    ? null
    : kv
      ? `${list(turnOff)} ${turnOff.length > 1 ? "stay" : "stays"} on while the database still has ${turnOff.length > 1 ? "their" : "its"} keys or indexes.`
      : `Turning off ${list(turnOff)} drops ${turnOff.length > 1 ? "them" : "it"} and what belongs to ${turnOff.length > 1 ? "them" : "it"}, such as tables and scheduled jobs. If other objects depend on ${turnOff.length > 1 ? "one" : "it"}, it isn't dropped.`;

  const editable = d.state === "active" && !saving;
  const title = kv ? "Modules" : "Extensions";

  if (q.status === "error")
    return (
      <Panel title={title}>
        <p className="p-4 text-[12.5px] text-destructive">{q.error}</p>
      </Panel>
    );

  return (
    <div className="space-y-6">
      {q.status !== "success" ? (
        <Panel title={title}>
          <div className="space-y-3 p-4">
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
          </div>
        </Panel>
      ) : null}

      {managed.length ? (
        // overflow-clip, not hidden: the Save bar sticks to the bottom of the window.
        <Panel title={title} className="overflow-clip!" bodyClassName="overflow-clip!">
          <ul className="list-rows">
            {managed.map((e) => {
              const s = states.find((x) => x.name === e.name);
              const badge = stateBadge(s);
              const changed = changes.includes(e.name);
              // Under way: wait for it, except a refusal or a failure, which can be undone.
              const busy = !!s && (s.state === "pending" || s.state === "restarting" || (s.state === "disabling" && badge?.tone !== "danger"));
              const problem = badge?.tone === "danger" ? s?.message : "";
              return (
                <ExtensionRow key={e.name} e={e} badge={badge} changed={changed} problem={problem}>
                  <Switch
                    checked={wanted.includes(e.name)}
                    onChange={(v) => toggle(e.name, v)}
                    disabled={!editable || busy}
                    ariaLabel={e.name}
                  />
                </ExtensionRow>
              );
            })}
          </ul>
          {changes.length ? (
            // Pinned while the list runs past the window, so Save is always in reach.
            <div className="sticky bottom-0 z-10 flex flex-wrap items-center justify-between gap-3 border-t border-border bg-card px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]">
              <p className="text-[12.5px] text-muted-foreground">
                {changes.length} {changes.length === 1 ? "change" : "changes"}
                {restarts.length ? " · Restarts the database once" : ""}
              </p>
              <div className="flex gap-2">
                <Button size="sm" variant="ghost" disabled={saving} onClick={() => setDraft(null)}>
                  Discard
                </Button>
                <Button
                  size="sm"
                  disabled={!editable}
                  onClick={() => (restartText || dropText ? setAsking(true) : void save())}
                >
                  {saving ? "Saving…" : "Save changes"}
                </Button>
              </div>
            </div>
          ) : null}
        </Panel>
      ) : null}

      {additional.length ? (
        <Panel
          title="Additional extensions"
          description={
            <>
              Connect with the default role and run <code className="font-mono text-[11.5px]">CREATE EXTENSION</code>.
            </>
          }
        >
          <ul className="grid gap-px bg-border lg:grid-cols-2 [&>*]:bg-card">
            {additional.map((e) => (
              <li key={e.name} className="flex items-start gap-3 px-4 py-2.5">
                <div className="min-w-0 flex-1">
                  <Name e={e} />
                  <p className="mt-0.5 text-[12.5px] text-muted-foreground">{e.description}</p>
                </div>
                <CopyButton value={createSql(e.name)} label={`Copy ${createSql(e.name)}`} className="mt-px" />
              </li>
            ))}
            {additional.length % 2 ? <li aria-hidden className="hidden lg:block" /> : null}
          </ul>
        </Panel>
      ) : null}

      <ConfirmDialog
        open={asking}
        onClose={() => setAsking(false)}
        tone={turnOff.length && !kv ? "danger" : "primary"}
        title={single ? `Turn ${turnOn.length ? "on" : "off"} ${single}?` : `Save ${changes.length} changes?`}
        description={
          <span className="block space-y-2">
            {restartText ? <span className="block">{restartText}</span> : null}
            {dropText ? <span className="block">{dropText}</span> : null}
          </span>
        }
        confirmLabel={single ? `Turn ${turnOn.length ? "on" : "off"}` : "Save changes"}
        onConfirm={() => {
          setAsking(false);
          void save();
        }}
      />
    </div>
  );
}

/** A managed extension with its switch, aligned the way <Switch> rows are. */
function ExtensionRow({
  e,
  badge,
  changed,
  problem,
  children,
}: {
  e: DatabaseExtension;
  badge: { tone: Tone; label: string } | null;
  changed: boolean;
  problem?: string;
  children: ReactNode;
}) {
  const [descRef, long] = useWrapsPastTwoLines<HTMLParagraphElement>(e.description);
  return (
    <li className={`flex gap-4 px-4 py-3 ${switchRowAlign(long)}`}>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <Name e={e} />
          {e.edition ? <Badge tone="info">{e.edition}</Badge> : null}
          {badge ? (
            <Badge tone={badge.tone}>{badge.label}</Badge>
          ) : e.restart ? (
            <Badge>Restarts database</Badge>
          ) : null}
          {changed ? <span className="text-[11.5px] text-muted-foreground">Unsaved</span> : null}
        </div>
        <p ref={descRef} className="mt-0.5 text-[12.5px] text-muted-foreground">
          {e.description}
        </p>
        {problem ? <p className="mt-1 text-[12px] text-destructive">{problem}</p> : null}
      </div>
      {children}
    </li>
  );
}

/** CREATE EXTENSION, quoting a name that needs it (uuid-ossp). */
const createSql = (name: string) => `CREATE EXTENSION ${/^[a-z_][a-z0-9_]*$/.test(name) ? name : `"${name}"`};`;

function Name({ e }: { e: DatabaseExtension }) {
  return (
    <span className="inline-flex min-w-0 items-center gap-1">
      <span className="truncate font-mono text-[12.5px] font-medium text-foreground">{e.name}</span>
      <a
        href={e.docs}
        target="_blank"
        rel="noreferrer"
        aria-label={`${e.name} docs`}
        title="Docs"
        className="inline-flex h-5 w-5 shrink-0 items-center justify-center rounded text-muted-foreground transition hover:bg-accent hover:text-foreground"
      >
        <ArrowSquareOutIcon size={12} />
      </a>
    </span>
  );
}
