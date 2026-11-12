import { Layered } from "~/components/ui/Layered";
import {
  DotsThreeIcon,
  HammerIcon,
  HardDrivesIcon,
  PlusIcon,
  TerminalWindowIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { createFileRoute, Link } from "@tanstack/react-router";
import { currentSlug } from "~/lib/pyo/paths";
import { useState } from "react";
import { BrandMark } from "~/components/pyo/common";
import { BuilderCommandDialog } from "~/components/pyo/BuilderInstall";
import { Dot, type Tone } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { timeAgo } from "~/lib/pyo/format";
import { api, buildersIn, usePyo, type PyoState } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Builder } from "~/lib/pyo/types";
import { BUILDER_STATUS, builderWhere, removeCopy } from "~/lib/pyo/builders";
import { seo } from "~/lib/seo";
import { siK3s } from "simple-icons";

export const Route = createFileRoute("/_app/$team/builders/")({
  head: () => ({ meta: seo({ title: "Builders" }) }),
  component: BuildersPage,
});

// Builders turn Git sources into images: BuildKit plus Pyo's builder,
// pinned to a server of a cluster or on a new build-only server. Any ready
// builder of the team picks up the next build.

function BuildersPage() {
  const s = usePyo();
  const builders = buildersIn(s);
  const [removing, setRemoving] = useState<Builder | null>(null);
  const [commandFor, setCommandFor] = useState<Builder | null>(null);
  const ready = builders.filter((b) => b.status === "ready").length;

  return (
    <Page>
      <PageHeader
        title="Builders"
        description={
          builders.length
            ? `${ready} of ${builders.length} ready`
            : "Where Git deploys are built."
        }
        actions={
          <ButtonLink href="/builders/new" size="sm" variant="secondary">
            <PlusIcon size={13} weight="bold" /> Add builder
          </ButtonLink>
        }
      />

      {builders.length ? (
        <Layered bare>
          <ul className="list-rows overflow-hidden rounded-[14px] border border-card-border bg-card">
            {builders.map((b) => (
              <BuilderRow
                key={b.id}
                b={b}
                s={s}
                onRemove={() => setRemoving(b)}
                onCommand={() => setCommandFor(b)}
              />
            ))}
          </ul>
        </Layered>
      ) : (
        <Panel>
          <EmptyState
            icon={HammerIcon}
            title="No builders yet"
            description="Add one to deploy from Git."
            action={
              <ButtonLink href="/builders/new" size="sm">
                Add builder
              </ButtonLink>
            }
          />
        </Panel>
      )}

      <BuilderCommandDialog
        builder={commandFor}
        open={!!commandFor}
        onClose={() => setCommandFor(null)}
      />
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing?.name ?? "builder"}?`}
        description={removeCopy(removing)}
        confirmLabel="Remove builder"
        onConfirm={() => {
          if (!removing) return;
          void api
            .removeBuilder(removing.id)
            .then((r) => r && toast({ title: `Removing ${removing.name}` }));
        }}
      />
    </Page>
  );
}

function BuilderRow({
  b,
  s,
  onRemove,
  onCommand,
}: {
  b: Builder;
  s: PyoState;
  onRemove: () => void;
  onCommand: () => void;
}) {
  const st = BUILDER_STATUS[b.status];
  const cluster = s.clusters.find((c) => c.id === b.clusterId);
  const where = builderWhere(b, cluster?.name);
  return (
    <li className="relative px-5 py-4 transition-colors hover:bg-accent/30">
      <div className="flex items-center gap-4">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-border bg-background">
          {b.kind === "cluster" ? (
            <BrandMark icon={siK3s} size={18} />
          ) : b.kind === "server" ? (
            <HardDrivesIcon size={18} />
          ) : (
            <span className="text-[11px] font-semibold">VM</span>
          )}
        </span>
        <div className="min-w-0 flex-1">
          <Link
            to="/$team/builders/$builderId"
            params={{ team: currentSlug(), builderId: b.id }}
            className="block truncate text-[14px] font-medium after:absolute after:inset-0"
          >
            {b.name}
          </Link>
          <p className="truncate text-[12.5px] text-muted-foreground">
            {where}
            {b.info.cpus ? ` · ${b.info.cpus} CPU · ${b.info.memoryGi} GB` : ""}
            {b.lastSeenAt ? ` · seen ${timeAgo(b.lastSeenAt)}` : ""}
          </p>
        </div>
        <span
          className={`hidden shrink-0 items-center gap-1.5 text-[12.5px] sm:inline-flex ${st.tone === "danger" ? "text-destructive" : "text-muted-foreground"}`}
        >
          <Dot tone={st.tone} className="h-2 w-2" />
          {b.info.busyWith && b.status === "ready" ? "Building" : st.label}
        </span>
        <div className="relative z-10">
          <Menu
            label={`Actions for ${b.name}`}
            items={[
              ...(b.kind === "server"
                ? [
                    {
                      label: "Show install command",
                      icon: <TerminalWindowIcon size={14} />,
                      onSelect: onCommand,
                    },
                  ]
                : []),
              {
                label: "Remove builder",
                icon: <TrashIcon size={14} />,
                danger: true,
                disabled: b.status === "removing",
                onSelect: onRemove,
              },
            ]}
            trigger={(t) => (
              <Button {...t} variant="ghost" size="icon" className="h-7 w-7">
                <DotsThreeIcon size={16} weight="bold" />
              </Button>
            )}
          />
        </div>
      </div>
      {b.status === "error" ||
        (b.status === "provisioning" && b.statusMessage) ? (
        <p
          className={`mt-2 text-[12.5px] ${b.status === "error" ? "text-destructive" : "text-muted-foreground"}`}
        >
          {b.statusMessage}
        </p>
      ) : b.status === "provisioning" ? (
        <p className="mt-2 text-[12.5px] text-muted-foreground">
          Starting BuildKit and the builder on the server.
        </p>
      ) : null}
    </li>
  );
}
