import {
  ArrowSquareOutIcon,
  CubeIcon,
  DotsThreeIcon,
  HardDrivesIcon,
  PauseIcon,
  PlayIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { CloudProgress } from "~/components/pyo/Hetzner";
import {
  act,
  SandboxStatus,
  ServerState,
  useNow,
  useSandboxes,
} from "~/components/pyo/sandbox/common";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Select } from "~/components/ui/Field";
import { ListSection } from "~/components/ui/Layered";
import { Menu } from "~/components/ui/Menu";
import { NewButton } from "~/components/ui/NewButton";
import {
  EmptyState,
  ErrorState,
  Metric,
  MetricStrip,
  Page,
  PageHeader,
  Panel,
} from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { timeAgo } from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import {
  ended,
  sizeLabel,
  timeLeft,
  type Sandbox,
  type SandboxInUse,
  type SandboxLimits,
} from "~/lib/pyo/sandbox";
import { roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
import type { SandboxServer } from "~/lib/pyo/sandbox";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/sandboxes/")({
  head: () => ({ meta: seo({ title: "Sandboxes" }) }),
  component: SandboxesPage,
});

// The team's sandboxes. With no sandbox server and no sandboxes it's an
// empty state like the databases', whose button opens Add sandbox server
// (sandboxes/servers/new). Servers still being set up show above the list.
// Sandboxes aren't in the snapshot, so the list polls on its own.

type Filter = "live" | "running" | "paused" | "ended";

function SandboxesPage() {
  const s = usePyo();
  const servers = sandboxServersIn(s);
  const q = useSandboxes(s.currentTeamId);
  const [filter, setFilter] = useState<Filter>("live");
  const [template, setTemplate] = useState("");
  const [deleting, setDeleting] = useState<Sandbox | null>(null);
  const role = roleIn(s);
  const canUse = role !== "viewer";
  const canManage = role === "owner" || role === "admin";
  const now = useNow();
  const pending = servers.filter(
    (x) => x.state === "joining" || x.state === "refused" || (x.cloud && x.cloud.state !== "ready"),
  );

  const all = q.status === "success" ? q.data.sandboxes : [];
  const live = all.filter((x) => !ended(x));
  const templates = [...new Set(all.map((x) => x.template))].sort();
  const shown = all.filter(
    (x) =>
      (filter === "live"
        ? !ended(x)
        : filter === "ended"
          ? ended(x)
          : x.state === filter) && (!template || x.template === template),
  );

  const newMenu = (
    <NewButton
      width="w-48"
      items={[
        ...(canUse
          ? [{ label: "New sandbox", icon: <CubeIcon size={14} />, href: "/sandboxes/new" }]
          : []),
        ...(canManage
          ? [{ label: "Add server", icon: <HardDrivesIcon size={14} />, href: "/sandboxes/servers/new" }]
          : []),
      ]}
    />
  );

  // Nothing yet: the same empty state as the databases'.
  if (!servers.length && (q.status !== "success" || !all.length))
    return (
      <Page>
        <PageHeader title="Sandboxes" actions={canUse ? newMenu : undefined} />
        <Panel>
          <EmptyState
            icon={CubeIcon}
            title="No sandboxes yet"
            description="Isolated machines on demand, on your own servers."
            action={
              canManage ? (
                <ButtonLink href="/sandboxes/servers/new" size="sm">
                  Add a sandbox server
                </ButtonLink>
              ) : undefined
            }
          />
        </Panel>
      </Page>
    );

  return (
    <Page>
      <PageHeader
        title="Sandboxes"
        actions={
          <>
            {newMenu}
          </>
        }
      />
      {pending.length ? <PendingServers servers={pending} /> : null}
      {q.status === "success" ? (
        <Usage
          limits={q.data.limits}
          inUse={q.data.inUse}
          running={live.filter((x) => x.state === "running").length}
          paused={live.filter((x) => x.state === "paused").length}
        />
      ) : null}
      {q.status === "error" ? (
        <Panel>
          <ErrorState title="Couldn't load sandboxes" description={q.error} onRetry={q.retry} />
        </Panel>
      ) : q.status === "loading" ? (
        <Panel>
          <p className="px-4 py-8 text-center text-[13px] text-muted-foreground">Loading sandboxes…</p>
        </Panel>
      ) : !all.length ? (
        <Panel>
          <EmptyState
            icon={CubeIcon}
            title="No sandboxes yet"
            action={
              canUse ? (
                <ButtonLink href="/sandboxes/new" size="sm">
                  New sandbox
                </ButtonLink>
              ) : undefined
            }
          />
        </Panel>
      ) : (
        <ListSection
          toolbar={
            <>
              <div className="w-40 sm:w-44">
                <Select value={filter} onChange={(e) => setFilter(e.target.value as Filter)} aria-label="Status" heading="Status">
                  <option value="live">Live ({live.length})</option>
                  <option value="running">Running</option>
                  <option value="paused">Paused</option>
                  <option value="ended">Ended</option>
                </Select>
              </div>
              <div className="w-40 sm:w-44">
                <Select value={template} onChange={(e) => setTemplate(e.target.value)} aria-label="Template" heading="Template">
                  <option value="">All templates</option>
                  {templates.map((t) => (
                    <option key={t} value={t}>
                      {t}
                    </option>
                  ))}
                </Select>
              </div>
            </>
          }
        >
          <Panel>
            <SandboxTable rows={shown} now={now} canUse={canUse} onDelete={setDeleting} />
          </Panel>
        </ListSection>
      )}
      <ConfirmDialog
        open={!!deleting}
        onClose={() => setDeleting(null)}
        title={`Delete ${deleting?.id ?? "sandbox"}?`}
        description="Its files are gone for good."
        confirmLabel="Delete sandbox"
        onConfirm={() => {
          if (deleting) void act("remove", deleting).then(() => q.retry());
        }}
      />
    </Page>
  );
}

/** Sandbox servers not running sandboxes yet: Pyo's progress at Hetzner, or the join. */
function PendingServers({ servers }: { servers: SandboxServer[] }) {
  const s = usePyo();
  return (
    <Panel>
      <ul className="list-rows">
        {servers.map((x) => {
          const cloud = s.cloudServers.find((c) => c.sandboxServerId === x.id);
          return (
            <li key={x.id} className="space-y-2.5 px-4 py-3">
              <div className="flex flex-wrap items-center gap-2.5">
                <span className="text-[13.5px] font-medium">{x.name}</span>
                {cloud ? null : <ServerState server={x} />}
              </div>
              {cloud ? (
                <CloudProgress server={cloud} />
              ) : x.state === "refused" ? (
                <p className="text-[12px] text-destructive">{x.error ?? x.summary}</p>
              ) : (
                <p className="text-[12px] text-muted-foreground">Waiting for its command to run on the server.</p>
              )}
            </li>
          );
        })}
      </ul>
    </Panel>
  );
}

function Usage({
  limits,
  inUse,
  running,
  paused,
}: {
  limits: SandboxLimits;
  inUse: SandboxInUse;
  running: number;
  paused: number;
}) {
  const gb = (mib: number) => `${+(mib / 1024).toFixed(1)}`;
  return (
    <MetricStrip cols="grid-cols-2 sm:grid-cols-4">
      <Metric label="Running" value={running} />
      <Metric label="Paused" value={paused} />
      <Metric
        label="CPUs"
        value={`${inUse.cpus} of ${limits.maxCpus}`}
        sub={<Bar value={inUse.cpus} max={limits.maxCpus} />}
      />
      <Metric
        label="Memory"
        value={`${gb(inUse.memoryMib)} of ${gb(limits.maxMemoryMib)} GB`}
        sub={<Bar value={inUse.memoryMib} max={limits.maxMemoryMib} />}
      />
    </MetricStrip>
  );
}

/** A usage bar that fits in a Metric's line (a span, since the line is a <p>). */
function Bar({ value, max }: { value: number; max: number }) {
  const r = max > 0 ? Math.min(1, value / max) : 0;
  const color = r > 0.9 ? "bg-destructive" : r > 0.75 ? "bg-warning" : "bg-primary";
  return (
    <span className="mt-2 block h-1.5 overflow-hidden rounded-full bg-foreground/10" role="meter" aria-valuemin={0} aria-valuemax={max} aria-valuenow={value}>
      <span className={`block h-full rounded-full ${color}`} style={{ width: `${Math.max(r * 100, r > 0 ? 2 : 0)}%` }} />
    </span>
  );
}

function SandboxTable({
  rows,
  now,
  canUse,
  onDelete,
}: {
  rows: Sandbox[];
  now: number;
  canUse: boolean;
  onDelete: (x: Sandbox) => void;
}) {
  const navigate = useNavigate();
  const open = (x: Sandbox) =>
    void navigate({ to: "/$team/sandboxes/$sandboxId", params: { team: currentSlug(), sandboxId: x.id } });
  return (
    <Table>
      <thead>
        <tr>
          <Th>ID</Th>
          <Th>Template</Th>
          <Th>Status</Th>
          <Th>CPU and memory</Th>
          <Th>Age</Th>
          <Th>Time left</Th>
          <Th>Created by</Th>
          <Th className="w-10">
            <span className="sr-only">Actions</span>
          </Th>
        </tr>
      </thead>
      <tbody>
        {rows.map((x) => (
          <Tr key={x.id} onClick={() => open(x)}>
            <Td className="font-mono text-[12px]">{x.id}</Td>
            <Td>{x.template}</Td>
            <Td>
              <SandboxStatus state={x.state} />
            </Td>
            <Td className="tabular-nums text-muted-foreground">{sizeLabel(x.cpus, x.memoryMib)}</Td>
            <Td className="text-muted-foreground">{timeAgo(x.createdAt, now)}</Td>
            <Td className="tabular-nums text-muted-foreground">
              {ended(x) ? "—" : (timeLeft(x.expiresAt, now) ?? "Ending")}
            </Td>
            <Td className="max-w-40 truncate text-muted-foreground">{x.createdBy ?? "—"}</Td>
            <Td>
              <div onClick={(e) => e.stopPropagation()}>
                <Menu
                  label={`Actions for ${x.id}`}
                  items={[
                    { label: "Open", icon: <ArrowSquareOutIcon size={14} />, onSelect: () => open(x) },
                    ...(canUse && !ended(x)
                      ? [
                          x.state === "paused"
                            ? { label: "Resume", icon: <PlayIcon size={14} />, onSelect: () => void act("resume", x) }
                            : {
                                label: "Pause",
                                icon: <PauseIcon size={14} />,
                                disabled: x.state !== "running",
                                onSelect: () => void act("pause", x),
                              },
                          {
                            label: "Delete",
                            icon: <TrashIcon size={14} />,
                            danger: true,
                            onSelect: () => onDelete(x),
                          },
                        ]
                      : []),
                  ]}
                  trigger={(t) => (
                    <Button {...t} variant="ghost" size="icon" className="h-7 w-7">
                      <DotsThreeIcon size={16} weight="bold" />
                    </Button>
                  )}
                />
              </div>
            </Td>
          </Tr>
        ))}
        {!rows.length ? (
          <tr>
            <Td colSpan={8} className="py-8 text-center text-muted-foreground">
              No sandboxes match.
            </Td>
          </tr>
        ) : null}
      </tbody>
    </Table>
  );
}
