import {
  ArrowSquareOutIcon,
  ClockClockwiseIcon,
  CubeIcon,
  MonitorIcon,
  PauseIcon,
  PencilSimpleIcon,
  PlayIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { z } from "zod";
import { SandboxFiles } from "~/components/pyo/sandbox/SandboxFiles";
import { SandboxTerminal } from "~/components/pyo/sandbox/SandboxTerminal";
import { TemplateMark, templateOf, TemplateTip } from "~/components/pyo/sandbox/TemplateMark";
import {
  act,
  RuntimeBadge,
  SandboxStatus,
  useNow,
  useSandboxes,
} from "~/components/pyo/sandbox/common";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Input, Segmented } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import {
  Callout,
  EmptyState,
  ErrorState,
  KeyValue,
  Page,
  PageHeader,
  Panel,
} from "~/components/ui/Panel";
import { Tabs } from "~/components/ui/Tabs";
import { Tooltip } from "~/components/ui/Tooltip";
import { dateTime, timeAgo } from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import {
  LIFETIMES,
  NO_LIFETIME,
  connectionFor,
  daemonApi,
  openDesktop,
  ended,
  isDesktop,
  lifetimeAllowed,
  rememberConnection,
  sandboxApi,
  sizeLabel,
  timeLeft,
  type ExecResult,
  type Sandbox,
} from "~/lib/pyo/sandbox";
import { projectsIn, roleIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { seo } from "~/lib/seo";

const TABS = ["overview", "terminal", "files", "commands", "metrics"] as const;
type Tab = (typeof TABS)[number];

export const Route = createFileRoute("/_app/$team/sandboxes/$sandboxId")({
  head: ({ params }) => ({ meta: seo({ title: params.sandboxId }) }),
  validateSearch: z.object({
    tab: z.enum(TABS).optional().catch(undefined),
  }),
  component: SandboxPage,
});

// One sandbox: its overview, and a terminal, files and commands straight to
// its server's daemon with the client token.

function SandboxPage() {
  const { sandboxId } = Route.useParams();
  const { tab = "overview" } = Route.useSearch();
  const navigate = useNavigate();
  const s = usePyo();
  const q = useQuery(
    `sandbox:${sandboxId}`,
    () =>
      sandboxApi.get(sandboxId).then((x) => {
        if (x.connection) rememberConnection(x.connection);
        return x;
      }),
    { refreshMs: 4000 },
  );
  const tq = useQuery(`sandbox-templates:${s.currentTeamId}`, () => sandboxApi.templates(s.currentTeamId));
  const [deleting, setDeleting] = useState(false);
  const [busy, setBusy] = useState(false);
  const canUse = roleIn(s) !== "viewer";
  const setTab = (t: Tab) =>
    void navigate({ to: ".", search: t === "overview" ? {} : { tab: t }, replace: true });

  if (q.status === "error")
    return (
      <Page>
        <Panel>
          <ErrorState title="Couldn't load the sandbox" description={q.error} onRetry={q.retry} />
        </Panel>
      </Page>
    );
  if (q.status === "loading")
    return (
      <Page>
        <PageHeader title={sandboxId} />
      </Page>
    );

  const x = q.data;
  const template = templateOf(x, tq.status === "success" ? tq.data.templates : []);
  const run = async (what: "pause" | "resume") => {
    setBusy(true);
    await act(what, x);
    setBusy(false);
    q.retry();
  };

  return (
    <Page>
      <PageHeader
        icon={
          <Tooltip content={<TemplateTip template={template} />}>
            <TemplateMark template={template} size="lg" className="mt-1" />
          </Tooltip>
        }
        title={canUse ? <NameField x={x} onDone={q.retry} /> : x.name}
        description={<span className="font-mono text-[12px]">{x.id}</span>}
        meta={
          <>
            <SandboxStatus state={x.state} sandbox={x} />
            <span>{x.template}</span>
            <span>{sizeLabel(x.cpus, x.memoryMib)}</span>
            <span>Created {timeAgo(x.createdAt)}</span>
          </>
        }
        actions={
          canUse ? (
            <>
              {isDesktop(template) && !ended(x) ? (
                <Button size="sm" disabled={x.state !== "running"} onClick={() => void openDesktop(x.id)}>
                  <MonitorIcon size={13} /> Open desktop
                </Button>
              ) : null}
              {ended(x) ? null : x.state === "paused" ? (
                <Button size="sm" variant="secondary" disabled={busy} onClick={() => void run("resume")}>
                  <PlayIcon size={13} /> Resume
                </Button>
              ) : (
                <Button size="sm" variant="secondary" disabled={busy || x.state !== "running"} onClick={() => void run("pause")}>
                  <PauseIcon size={13} /> Pause
                </Button>
              )}
              {ended(x) ? null : (
                <Button size="sm" variant="secondary" className="text-destructive" onClick={() => setDeleting(true)}>
                  <TrashIcon size={13} /> Delete
                </Button>
              )}
            </>
          ) : undefined
        }
      />
      {x.error ? <Callout tone="danger">{x.error}</Callout> : null}
      <Tabs
        variant="underline"
        label="Sandbox"
        value={tab}
        onChange={setTab}
        items={[
          { value: "overview", label: "Overview" },
          { value: "terminal", label: "Terminal" },
          { value: "files", label: "Files" },
          { value: "commands", label: "Commands" },
          { value: "metrics", label: "Metrics" },
        ]}
      />
      {tab === "overview" ? (
        <Overview x={x} canUse={canUse} onChange={q.retry} />
      ) : ended(x) ? (
        <Panel>
          <EmptyState icon={CubeIcon} title="This sandbox has ended" />
        </Panel>
      ) : tab === "terminal" ? (
        <Panel>
          <SandboxTerminal sandbox={x} autoStart />
        </Panel>
      ) : tab === "files" ? (
        <Panel>
          <SandboxFiles sandbox={x} />
        </Panel>
      ) : tab === "commands" ? (
        <Commands x={x} canUse={canUse} />
      ) : (
        <Panel>
          {/* TODO(sandbox): per-sandbox CPU and memory once Pyo keeps them (metering in ClickHouse). */}
          <EmptyState title="No metrics yet" description="CPU and memory per sandbox come in a later release." />
        </Panel>
      )}
      <ConfirmDialog
        open={deleting}
        onClose={() => setDeleting(false)}
        title={`Delete ${x.name}?`}
        description="Its files are gone for good."
        confirmLabel="Delete sandbox"
        onConfirm={() => {
          void act("remove", x).then((ok) => {
            if (ok) void navigate({ to: "/$team/sandboxes", params: { team: currentSlug() } });
          });
        }}
      />
    </Page>
  );
}

/** The sandbox's name, edited in place. Enter or leaving the field saves,
 * Escape puts it back. Its id stays. */
function NameField({ x, onDone }: { x: Sandbox; onDone: () => void }) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(x.name);
  const [shown, setShown] = useState(x.name);
  const [busy, setBusy] = useState(false);
  // Follows renames from elsewhere while not editing.
  if (!editing && !busy && shown !== x.name) setShown(x.name);
  const done = async (keep: boolean) => {
    const n = name.trim();
    setEditing(false);
    if (!keep || !n || n === shown) return;
    setShown(n);
    setBusy(true);
    try {
      await sandboxApi.rename(x.id, n);
      onDone();
    } catch (e) {
      setShown(x.name);
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setBusy(false);
    }
  };
  if (editing)
    return (
      // The hidden copy sizes the field to its text, so it fits like the name.
      <span className="-mx-2 inline-flex max-w-full items-center gap-2 rounded-lg bg-muted px-2">
        <span className="inline-grid min-w-0">
        <span aria-hidden className="invisible col-start-1 row-start-1 truncate whitespace-pre">
          {name || " "}
        </span>
        <input
          autoFocus
          aria-label="Name"
          // Its own width would win over the copy that sizes it.
          size={1}
          value={name}
          maxLength={63}
          onFocus={(e) => e.currentTarget.select()}
          onChange={(e) => setName(e.target.value)}
          onBlur={() => void done(true)}
          onKeyDown={(e) => {
            if (e.key === "Enter") e.currentTarget.blur();
            if (e.key === "Escape") {
              setName(shown);
              void done(false);
            }
          }}
          className="col-start-1 row-start-1 w-full min-w-0 bg-transparent p-0 font-semibold tracking-tight text-foreground outline-none"
        />
        </span>
      </span>
    );
  return (
    <button
      type="button"
      title="Rename"
      onClick={() => {
        setName(shown);
        setEditing(true);
      }}
      className="group -mx-2 inline-flex max-w-full items-center gap-2 rounded-lg px-2 text-left outline-none hover:bg-muted focus-visible:bg-muted"
    >
      <span className="truncate">{shown}</span>
      <PencilSimpleIcon
        size={16}
        aria-hidden
        className="shrink-0 text-muted-foreground transition-colors group-hover:text-foreground"
      />
    </button>
  );
}

function Overview({ x, canUse, onChange }: { x: Sandbox; canUse: boolean; onChange: () => void }) {
  const s = usePyo();
  const now = useNow(1000);
  const server = s.sandboxServers.find((v) => v.id === x.serverId);
  const project = x.projectId ? projectsIn(s).find((p) => p.id === x.projectId) : undefined;
  const list = useSandboxes(x.teamId);
  const limits = list.status === "success" ? list.data.limits : null;
  const left = x.expiresAt === null ? "No limit" : timeLeft(x.expiresAt, now);
  const extend = async (ttl: string, label: string) => {
    try {
      await sandboxApi.extend(x.id, ttl);
      toast({ title: ttl === NO_LIFETIME ? "It runs until you delete it" : `Ends ${label} from now` });
      onChange();
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    }
  };
  const labels = Object.entries(x.labels ?? {});
  return (
    <div className="grid items-start gap-4 lg:grid-cols-2">
      <Panel title="Sandbox">
        <KeyValue
          items={[
            ["Status", <SandboxStatus key="s" state={x.state} sandbox={x} />],
            [
              "Time left",
              ended(x) ? (
                x.endedAt ? `Ended ${timeAgo(x.endedAt)}` : "Ended"
              ) : (
                <span key="t" className="inline-flex items-center gap-2">
                  <span className="tabular-nums">{left ?? "Ending"}</span>
                  {canUse ? (
                    <Menu
                      width="w-40"
                      label="Extend"
                      items={[
                        ...LIFETIMES.map((l) => ({
                          label: `${l.label} from now`,
                          disabled: !lifetimeAllowed(l.secs, limits),
                          onSelect: () => void extend(l.value, l.label),
                        })),
                        ...(limits?.maxTtlSecs === null
                          ? [{ label: "No limit", onSelect: () => void extend(NO_LIFETIME, "") }]
                          : []),
                      ]}
                      trigger={(t) => (
                        <Button {...t} size="xs" variant="secondary">
                          <ClockClockwiseIcon size={12} /> Extend
                        </Button>
                      )}
                    />
                  ) : null}
                </span>
              ),
            ],
            ["Template", x.template],
            ["CPU and memory", sizeLabel(x.cpus, x.memoryMib)],
            ["Disk", `${+(x.diskMib / 1024).toFixed(1)} GB`],
            ["Network", x.network === "internet" ? "Internet" : "No network"],
            ["Pause when idle", x.pauseWhenIdleSecs ? `After ${Math.round(x.pauseWhenIdleSecs / 60)} min` : "Off"],
            ["Created", `${dateTime(x.createdAt)}${x.createdBy ? ` by ${x.createdBy}` : ""}`],
            ...(project ? ([["Project", project.name]] as [string, string][]) : []),
          ]}
        />
      </Panel>
      <div className="space-y-4">
        <Panel title="Server">
          <KeyValue
            items={[
              ["Name", x.server ?? "—"],
              [
                "Runtime",
                server ? (
                  <span key="r" className="inline-flex flex-col items-end gap-1">
                    <RuntimeBadge server={server} />
                    {server.summary ? (
                      <span className="text-[11.5px] text-muted-foreground">{server.summary}</span>
                    ) : null}
                  </span>
                ) : (
                  x.runtime ?? "—"
                ),
              ],
              ["Isolation", x.isolation === "vm" ? "VM only" : "Auto"],
              [
                "Address",
                x.url ? (
                  <span key="a" className="inline-flex flex-col items-end">
                    <span className="font-mono text-[11.5px]">{x.url}</span>
                    <span className="text-[11.5px] text-muted-foreground">On your tailnet</span>
                  </span>
                ) : (
                  "—"
                ),
              ],
            ]}
          />
        </Panel>
        <Ports x={x} canUse={canUse} onChange={onChange} />
        <Panel title="Environment and labels">
          <KeyValue
            items={[
              [
                "Environment",
                x.env.length ? (
                  <span key="e" className="font-mono text-[11.5px]">
                    {x.env.join(", ")}
                  </span>
                ) : (
                  "None"
                ),
              ],
              [
                "Labels",
                labels.length ? (
                  <span key="l" className="inline-flex flex-wrap justify-end gap-1">
                    {labels.map(([k, v]) => (
                      <Badge key={k} mono>
                        {k}={v}
                      </Badge>
                    ))}
                  </span>
                ) : (
                  "None"
                ),
              ],
            ]}
          />
        </Panel>
      </div>
    </div>
  );
}

/** Ports open on the server's edge proxy, and a form to open one. */
function Ports({ x, canUse, onChange }: { x: Sandbox; canUse: boolean; onChange: () => void }) {
  const [port, setPort] = useState("");
  const [access, setAccess] = useState<"public" | "private">("public");
  const [busy, setBusy] = useState(false);
  const live = !ended(x);
  const n = Number(port);
  const valid = Number.isInteger(n) && n >= 1 && n <= 65535;
  const open = async () => {
    if (!valid || busy) return;
    setBusy(true);
    try {
      const r = await sandboxApi.openPort(x.id, n, access === "public");
      toast({ title: `Port ${n} is open`, description: r.url });
      setPort("");
      onChange();
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setBusy(false);
    }
  };
  const [closing, setClosing] = useState<number | null>(null);
  const close = async (p: number) => {
    setClosing(p);
    try {
      await sandboxApi.closePort(x.id, p);
      toast({ title: `Closed port ${p}` });
      onChange();
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setClosing(null);
    }
  };
  // A private port needs a sandbox token, which the edge takes once in the URL.
  const visit = async (p: { url: string | null; public: boolean }) => {
    if (!p.url) return;
    const tab = window.open("", "_blank");
    try {
      const url = p.public ? p.url : `${p.url}/?pyo_token=${encodeURIComponent((await connectionFor(x.id)).token)}`;
      if (tab) tab.location.href = url;
    } catch (e) {
      tab?.close();
      toast({ tone: "error", title: (e as Error).message });
    }
  };
  if (!x.edgeUrl && !x.ports.length && !live) return null;
  return (
    <Panel title="Ports">
      {x.ports.length ? (
        <KeyValue
          items={x.ports.map((p) => [
            <span key="p" className="inline-flex items-center gap-2">
              <span className="font-mono">{p.port}</span>
              <Badge>{p.public ? "Public" : "Private"}</Badge>
            </span>,
            <span key="u" className="inline-flex max-w-full items-center gap-3">
              {p.url ? (
                <button
                  type="button"
                  onClick={() => void visit(p)}
                  className="inline-flex min-w-0 items-center gap-1 font-mono text-[11.5px] text-primary hover:text-primary/80"
                >
                  <span className="truncate">{p.url}</span>
                  <ArrowSquareOutIcon size={12} className="shrink-0" />
                </button>
              ) : (
                "—"
              )}
              {canUse && live ? (
                <Button
                  size="xs"
                  variant="ghost"
                  disabled={closing === p.port}
                  onClick={() => void close(p.port)}
                  className="-my-1"
                  aria-label={`Close port ${p.port}`}
                >
                  {closing === p.port ? "Closing…" : "Close"}
                </Button>
              ) : null}
            </span>,
          ])}
        />
      ) : null}
      {!x.edgeUrl ? (
        <p className="px-4 py-3 text-[12.5px] text-muted-foreground">This server has no edge proxy.</p>
      ) : canUse && live ? (
        <form
          className="flex flex-wrap items-center gap-2 border-t border-border p-4 first:border-t-0"
          onSubmit={(e) => {
            e.preventDefault();
            void open();
          }}
        >
          <Input
            mono
            inputMode="numeric"
            aria-label="Port"
            placeholder="3000"
            value={port}
            onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
            className="w-24"
          />
          <Segmented
            label="Access"
            size="xs"
            value={access}
            onChange={setAccess}
            options={[
              { value: "public", label: "Public" },
              { value: "private", label: "Private" },
            ]}
          />
          <Button type="submit" size="sm" variant="secondary" disabled={!valid || busy}>
            {busy ? "Opening…" : "Open port"}
          </Button>
        </form>
      ) : !x.ports.length ? (
        <p className="px-4 py-3 text-[12.5px] text-muted-foreground">No open ports.</p>
      ) : null}
    </Panel>
  );
}

/**
 * Run a command and see its output. Pyo doesn't keep a history of commands
 * yet, so the list is this page's own.
 */
// TODO(sandbox): show the sandbox's exec history once the API keeps it.
function Commands({ x, canUse }: { x: Sandbox; canUse: boolean }) {
  const [cmd, setCmd] = useState("");
  const [busy, setBusy] = useState(false);
  const [runs, setRuns] = useState<{ cmd: string; at: number; result?: ExecResult; error?: string }[]>([]);
  const running = x.state === "running";
  const run = async () => {
    const c = cmd.trim();
    if (!c || busy) return;
    setBusy(true);
    const at = Date.now();
    try {
      const result = await daemonApi.exec(x.id, c);
      setRuns((r) => [{ cmd: c, at, result }, ...r]);
      setCmd("");
    } catch (e) {
      setRuns((r) => [{ cmd: c, at, error: (e as Error).message }, ...r]);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel>
      {canUse ? (
        <form
          className="flex gap-2 border-b border-border p-4"
          onSubmit={(e) => {
            e.preventDefault();
            void run();
          }}
        >
          <Input
            mono
            aria-label="Command"
            placeholder={running ? "uname -a" : "Resume the sandbox to run commands."}
            disabled={!running}
            value={cmd}
            onChange={(e) => setCmd(e.target.value)}
            className="min-w-0 flex-1"
          />
          <Button type="submit" size="sm" disabled={!running || busy || !cmd.trim()}>
            {busy ? "Running…" : "Run"}
          </Button>
        </form>
      ) : null}
      {runs.length ? (
        <ul className="list-rows">
          {runs.map((r, i) => (
            <li key={`${r.at}-${i}`} className="space-y-2 px-4 py-3">
              <div className="flex items-center gap-2 text-[12.5px]">
                <span className="min-w-0 flex-1 truncate font-mono">$ {r.cmd}</span>
                {r.result ? (
                  <Badge tone={r.result.exitCode === 0 ? "success" : "danger"}>
                    {r.result.timedOut ? "Timed out" : r.result.exitCode === null ? `Signal ${r.result.signal}` : `Exit ${r.result.exitCode}`}
                  </Badge>
                ) : null}
                <span className="text-[12px] tabular-nums text-muted-foreground">
                  {r.result ? `${r.result.durationMs} ms` : ""}
                </span>
              </div>
              {r.error ? (
                <p className="text-[12.5px] text-destructive">{r.error}</p>
              ) : r.result && (r.result.stdout || r.result.stderr) ? (
                <pre className="max-h-80 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-muted/40 p-3 font-mono text-[11.5px] leading-[18px]">
                  {r.result.stdout}
                  {r.result.stderr ? <span className="text-destructive">{r.result.stderr}</span> : null}
                </pre>
              ) : null}
            </li>
          ))}
        </ul>
      ) : (
        <EmptyState compact title="No commands yet" description="Pyo doesn't keep a history yet." />
      )}
    </Panel>
  );
}
