import { DotsThreeIcon, SquaresFourIcon, TrashIcon } from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { TemplateMark, TemplateTip } from "~/components/pyo/sandbox/TemplateMark";
import { Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, ErrorState, Page, PageHeader, Panel, Progress } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { Tooltip } from "~/components/ui/Tooltip";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import {
  fieldsSwapped,
  pyosFirst,
  sandboxApi,
  templateProgress,
  templateStatus,
  usable,
  type SandboxTemplate,
} from "~/lib/pyo/sandbox";
import { roleIn, sandboxServersIn, usePyo } from "~/lib/pyo/store";
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
  unavailable: { label: "Not available yet", tone: "neutral" },
};

const NAME = /^[a-z0-9][a-z0-9._-]{0,62}$/;

function TemplatesPage() {
  const s = usePyo();
  const navigate = useNavigate();
  const team = s.currentTeamId;
  // Fast while a server makes one, so its bar moves.
  const [fast, setFast] = useState(false);
  const q = useQuery(`sandbox-templates:${team}`, () => sandboxApi.templates(team), { refreshMs: fast ? 1500 : 10_000 });
  const preparing = q.status === "success" && q.data.templates.some((t) => templateProgress(t));
  if (preparing !== fast) setFast(preparing);
  const canCreate = roleIn(s) !== "viewer";
  // New sandboxes need a server that runs them.
  const canUse = canCreate && sandboxServersIn(s).some((x) => x.state === "ready");
  const [name, setName] = useState("");
  const [image, setImage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [removing, setRemoving] = useState<SandboxTemplate | null>(null);
  const canRemove = (t: SandboxTemplate) => !t.builtin && (t.teamId ? canCreate : !!s.user.isAdmin);
  const swapped = fieldsSwapped(name, image);
  const nameError = swapped
    ? "This looks like the image. Swap the name and image."
    : name && !NAME.test(name)
      ? "Lowercase letters, digits, dashes, dots and underscores."
      : "";

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

  const templates = q.status === "success" ? pyosFirst(q.data.templates) : [];
  return (
    <Page width="max-w-3xl">
      <PageHeader title="Sandbox templates" description="What sandboxes start from." />
      {q.status === "error" ? (
        <Panel>
          <ErrorState title="Couldn't load templates" description={q.error} onRetry={q.retry} />
        </Panel>
      ) : templates.length ? (
        <Panel>
          <Table fluid>
            <thead>
              <tr>
                <Th>Template</Th>
                <Th className="w-44">Status</Th>
                <Th className="w-px">
                  <span className="sr-only">Actions</span>
                </Th>
              </tr>
            </thead>
            <tbody>
              {templates.map((t) => (
                <Tr key={t.id}>
                  <Td>
                    <Tooltip content={<TemplateTip template={t} />}>
                      <span className="flex min-w-0 items-center gap-3">
                        <TemplateMark template={t} />
                        <span className="truncate text-[13px] font-medium">{t.name}</span>
                      </span>
                    </Tooltip>
                  </Td>
                  <Td>
                    <TemplateState t={t} />
                  </Td>
                  <Td>
                    <div className="flex items-center justify-end gap-1">
                      {canUse && usable(t) ? (
                        <Button
                          size="xs"
                          variant="secondary"
                          onClick={() =>
                            void navigate({
                              to: "/$team/sandboxes/new",
                              params: { team: currentSlug() },
                              search: { template: t.name },
                            })
                          }
                        >
                          Use
                        </Button>
                      ) : null}
                      {/* The menu keeps its slot, so Use lines up on every row. */}
                      <div className="w-7">
                        {canRemove(t) ? (
                          <Menu
                            label={`Actions for ${t.name}`}
                            items={[{ label: "Delete", icon: <TrashIcon size={14} />, danger: true, onSelect: () => setRemoving(t) }]}
                            trigger={(p) => (
                              <Button {...p} variant="ghost" size="icon" className="h-7 w-7">
                                <DotsThreeIcon size={16} weight="bold" />
                              </Button>
                            )}
                          />
                        ) : null}
                      </div>
                    </div>
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
            <Field
              label="Image"
              error={error || undefined}
              hint={
                swapped ? (
                  <button
                    type="button"
                    className="font-medium text-foreground underline underline-offset-2"
                    onClick={() => {
                      setName(image.trim().toLowerCase());
                      setImage(name);
                    }}
                  >
                    Swap them
                  </button>
                ) : undefined
              }
            >
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

/** The status, with its error on hover when there is one. While servers
 * make it, how far the slowest got. */
function TemplateState({ t }: { t: SandboxTemplate }) {
  const p = t.state === "failed" ? null : templateProgress(t);
  if (p)
    return (
      <Tooltip content={p.servers > 1 ? `${p.label} on ${p.servers} servers` : p.label}>
        <span className="flex w-36 max-w-full flex-col gap-1">
          <span className="truncate text-[12.5px] tabular-nums">{p.label}</span>
          <Progress percent={p.percent} label={`${t.name}: ${p.label}`} />
        </span>
      </Tooltip>
    );
  const label = (
    <span className={`inline-flex items-center gap-1.5 whitespace-nowrap ${t.state === "failed" ? "text-destructive" : ""}`}>
      <Dot tone={STATE[t.state]?.tone ?? "neutral"} className="h-2 w-2" />
      {templateStatus(t)}
    </span>
  );
  const error = t.error ?? t.servers.find((x) => x.error)?.error;
  return error ? <Tooltip content={error}>{label}</Tooltip> : label;
}
