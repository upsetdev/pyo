import { SquaresFourIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { EmptyState, ErrorState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { timeAgo } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import { sandboxApi, type SandboxTemplate } from "~/lib/pyo/sandbox";
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

function TemplatesPage() {
  const s = usePyo();
  const team = s.currentTeamId;
  const q = useQuery(`sandbox-templates:${team}`, () => sandboxApi.templates(team), { refreshMs: 10_000 });
  const canCreate = roleIn(s) !== "viewer";
  const [name, setName] = useState("");
  const [image, setImage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
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
                  <Td className="tabular-nums text-muted-foreground">{t.servers.length}</Td>
                  <Td className="text-muted-foreground">{timeAgo(t.createdAt)}</Td>
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
    </Page>
  );
}
