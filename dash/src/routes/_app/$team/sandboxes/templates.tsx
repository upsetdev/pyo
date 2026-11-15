import { DotsThreeIcon, SquaresFourIcon, TrashIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, ErrorState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { megabytes, timeAgo } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import { sandboxApi, type SandboxTemplate, type SandboxTemplateServer } from "~/lib/pyo/sandbox";
import { roleIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/sandboxes/templates")({
  head: () => ({ meta: seo({ title: "Sandbox templates" }) }),
  component: TemplatesPage,
});

// What sandboxes start from: the team's own templates and the
// installation's. Always "Sandbox templates", since /templates is
// one-click service templates.

const STATE: Record<SandboxTemplate["state"], { label: string; tone: "success" | "info" | "danger" | "neutral" }> = {
  pending: { label: "Pending", tone: "neutral" },
  building: { label: "Building", tone: "info" },
  ready: { label: "Ready", tone: "success" },
  failed: { label: "Failed", tone: "danger" },
};

const NAME = /^[a-z0-9][a-z0-9._-]{0,62}$/;

const PHASE: Record<NonNullable<SandboxTemplateServer["phase"]>, string> = {
  queued: "Queued",
  pulling: "Pulling",
  unpacking: "Unpacking",
  snapshotting: "Booting",
};

/** How far one server got with a template. */
function serverLabel(x: SandboxTemplateServer) {
  if (x.state === "building" && x.phase) {
    const pct = x.phase === "pulling" && x.imageBytes ? ` ${Math.floor(((x.pulled ?? 0) / x.imageBytes) * 100)}%` : "";
    return PHASE[x.phase] + pct;
  }
  if (x.state === "ready" && x.bytes) return `Ready · ${megabytes(x.bytes / 1024 ** 2)}`;
  return STATE[x.state]?.label ?? x.state;
}

function TemplatesPage() {
  const s = usePyo();
  const team = s.currentTeamId;
  const q = useQuery(`sandbox-templates:${team}`, () => sandboxApi.templates(team), { refreshMs: 10_000 });
  const canCreate = roleIn(s) !== "viewer";
  const [name, setName] = useState("");
  const [image, setImage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [removing, setRemoving] = useState<SandboxTemplate | null>(null);
  const canRemove = (t: SandboxTemplate) => (t.teamId ? canCreate : !!s.user.isAdmin);
  const nameError = name && !NAME.test(name) ? "Lowercase letters, digits, dashes, dots and underscores." : "";

  const create = async () => {
    if (!name || !image.trim() || nameError || busy) return;
    setBusy(true);
    setError("");
    try {
      await sandboxApi.createTemplate({ teamId: team, name, image: image.trim() });
      toast({ title: `Added ${name}` });
      setName("");
      setImage("");
      q.retry();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const templates = q.status === "success" ? q.data.templates : [];
  return (
    <Page width="max-w-4xl">
      <PageHeader title="Sandbox templates" description="What sandboxes start from." />
      {q.status === "error" ? (
        <Panel>
          <ErrorState title="Couldn't load templates" description={q.error} onRetry={q.retry} />
        </Panel>
      ) : templates.length ? (
        <Panel>
          <Table>
            <thead>
              <tr>
                <Th>Name</Th>
                <Th>Image</Th>
                <Th>Status</Th>
                <Th>Servers</Th>
                <Th>Added</Th>
                <Th />
              </tr>
            </thead>
            <tbody>
              {templates.map((t) => (
                <Tr key={t.id}>
                  <Td className="font-medium">
                    {t.name}
                    {t.teamId ? null : <span className="ml-2 text-[11.5px] font-normal text-muted-foreground">Installation</span>}
                  </Td>
                  <Td className="font-mono text-[12px] text-muted-foreground">{t.source?.image ?? "—"}</Td>
                  <Td>
                    <span className={`inline-flex items-center gap-1.5 ${t.state === "failed" ? "text-destructive" : ""}`} title={t.error ?? undefined}>
                      <Dot tone={STATE[t.state]?.tone ?? "neutral"} className="h-2 w-2" />
                      {STATE[t.state]?.label ?? t.state}
                    </span>
                  </Td>
                  <Td className="text-[12.5px]">
                    {t.servers.length
                      ? t.servers.map((x) => (
                          <div key={x.id} className="flex items-center gap-1.5 whitespace-nowrap" title={x.error ?? undefined}>
                            <Dot tone={STATE[x.state]?.tone ?? "neutral"} className="h-1.5 w-1.5" />
                            <span className="truncate">{x.name}</span>
                            <span className={x.state === "failed" ? "text-destructive" : "text-muted-foreground"}>{serverLabel(x)}</span>
                          </div>
                        ))
                      : <span className="text-muted-foreground">—</span>}
                  </Td>
                  <Td className="text-muted-foreground">{timeAgo(t.createdAt)}</Td>
                  <Td className="text-right">
                    {canRemove(t) ? (
                      <Menu
                        label={`Actions for ${t.name}`}
                        items={[{ label: "Delete template", icon: <TrashIcon size={14} />, danger: true, onSelect: () => setRemoving(t) }]}
                        trigger={(p) => (
                          <Button {...p} variant="ghost" size="icon" className="h-7 w-7">
                            <DotsThreeIcon size={16} weight="bold" />
                          </Button>
                        )}
                      />
                    ) : null}
                  </Td>
                </Tr>
              ))}
            </tbody>
          </Table>
        </Panel>
      ) : q.status === "success" ? (
        <Panel>
          <EmptyState icon={SquaresFourIcon} title="No sandbox templates yet" />
        </Panel>
      ) : null}
      {canCreate ? (
        <Panel title="New template">
          <form
            className="grid items-start gap-4 p-4 sm:grid-cols-[1fr_1fr_auto]"
            onSubmit={(e) => {
              e.preventDefault();
              void create();
            }}
          >
            <Field label="Name" error={nameError || undefined}>
              {(p) => (
                <Input {...p} mono placeholder="node-22" value={name} onChange={(e) => setName(e.target.value.toLowerCase())} />
              )}
            </Field>
            <Field label="Image" error={error || undefined}>
              {(p) => <Input {...p} mono placeholder="node:22" value={image} onChange={(e) => setImage(e.target.value)} />}
            </Field>
            <Button type="submit" size="sm" className="sm:mt-[22px]" disabled={busy || !name || !image.trim() || !!nameError}>
              {busy ? "Adding…" : "Add template"}
            </Button>
          </form>
        </Panel>
      ) : null}
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Delete ${removing?.name ?? "template"}?`}
        description="The sandbox servers delete their copies too."
        confirmLabel="Delete template"
        onConfirm={() => {
          const t = removing;
          if (!t) return;
          void sandboxApi
            .removeTemplate(t.id)
            .then(() => toast({ title: `Deleted ${t.name}` }))
            .catch((e: Error) => toast({ tone: "error", title: e.message }))
            .finally(() => q.retry());
        }}
      />
    </Page>
  );
}
