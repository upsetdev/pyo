import { CubeIcon, DotsThreeIcon, SquaresFourIcon, TrashIcon } from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import {
  siAlmalinux,
  siAlpinelinux,
  siArchlinux,
  siBun,
  siCentos,
  siDebian,
  siDeno,
  siDocker,
  siFedora,
  siGo,
  siNodedotjs,
  siOpenjdk,
  siPhp,
  siPython,
  siRockylinux,
  siRuby,
  siRust,
  siUbuntu,
  type SimpleIcon,
} from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import { Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, ErrorState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { Tooltip } from "~/components/ui/Tooltip";
import { megabytes, timeAgo } from "~/lib/pyo/format";
import { currentSlug } from "~/lib/pyo/paths";
import { useQuery } from "~/lib/pyo/query";
import {
  fieldsSwapped,
  isBase,
  sandboxApi,
  templateImage,
  templateStatus,
  usable,
  type SandboxTemplate,
  type SandboxTemplateServer,
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
  const navigate = useNavigate();
  const team = s.currentTeamId;
  const q = useQuery(`sandbox-templates:${team}`, () => sandboxApi.templates(team), { refreshMs: 10_000 });
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

  const templates = q.status === "success" ? q.data.templates : [];
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
                <Th className="w-40">Status</Th>
                <Th className="w-px">
                  <span className="sr-only">Actions</span>
                </Th>
              </tr>
            </thead>
            <tbody>
              {templates.map((t) => (
                <Tr key={t.id}>
                  <Td>
                    <Tooltip content={<TemplateDetails t={t} />}>
                      <span className="flex min-w-0 items-center gap-3">
                        <TemplateLogo t={t} />
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

// Logos by what the image runs: languages first, since node:22 is Debian
// underneath but reads as Node.
const LOGOS: [RegExp, SimpleIcon][] = [
  [/\bnode(js)?\b/, siNodedotjs],
  [/\bpython\b/, siPython],
  [/\bbun\b/, siBun],
  [/\bdeno\b/, siDeno],
  [/\bgolang\b|(^|\/)go(:|$)/, siGo],
  [/\brust\b/, siRust],
  [/\bruby\b/, siRuby],
  [/\bphp\b/, siPhp],
  [/\b(openjdk|java|temurin)\b/, siOpenjdk],
  [/\bubuntu\b/, siUbuntu],
  [/\bdebian\b/, siDebian],
  [/\balpine\b/, siAlpinelinux],
  [/\bfedora\b/, siFedora],
  [/\barch(linux)?\b/, siArchlinux],
  [/\bcentos\b/, siCentos],
  [/\brocky(linux)?\b/, siRockylinux],
  [/\balma(linux)?\b/, siAlmalinux],
  [/\bdocker\b/, siDocker],
];

function logoFor(t: SandboxTemplate) {
  if (isBase(t)) return siDebian;
  const text = `${t.source?.image ?? ""} ${t.source?.os ?? ""} ${t.name}`.toLowerCase();
  return LOGOS.find(([re]) => re.test(text))?.[1];
}

function TemplateLogo({ t }: { t: SandboxTemplate }) {
  const logo = logoFor(t);
  return (
    <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-frame-border bg-card text-muted-foreground">
      {logo ? <BrandMark icon={logo} size={16} tone={logo === siDebian ? "brand" : "auto"} /> : <CubeIcon size={16} />}
    </span>
  );
}

/** The status, with its error on hover when there is one. */
function TemplateState({ t }: { t: SandboxTemplate }) {
  const label = (
    <span className={`inline-flex items-center gap-1.5 whitespace-nowrap ${t.state === "failed" ? "text-destructive" : ""}`}>
      <Dot tone={STATE[t.state]?.tone ?? "neutral"} className="h-2 w-2" />
      {templateStatus(t)}
    </span>
  );
  const error = t.error ?? t.servers.find((x) => x.error)?.error;
  return error ? <Tooltip content={error}>{label}</Tooltip> : label;
}

/** What hovering a template shows: its image, its servers and when it was added. */
function TemplateDetails({ t }: { t: SandboxTemplate }) {
  const image = templateImage(t);
  const added = t.createdAt ? `Added ${timeAgo(t.createdAt)}${t.createdBy ? ` by ${t.createdBy}` : ""}` : "";
  return (
    <span className="block space-y-1.5 text-left">
      {/* Base says what it runs ("Debian 13 · ghcr.io/…"): the OS on its own line. */}
      {image.split(" · ").map((line, i, all) => (
        <span key={line} className={`block break-all ${all.length > 1 && !i ? "font-medium" : "font-mono text-[11.5px]"}`}>
          {line}
        </span>
      ))}
      {t.servers.length ? (
        <span className="block">
          {t.servers.map((x) => (
            <span key={x.id} className="flex items-center gap-1.5">
              <Dot tone={STATE[x.state]?.tone ?? "neutral"} />
              <span className="truncate">{x.name}</span>
              <span className={`ml-auto pl-3 tabular-nums ${x.state === "failed" ? "text-destructive" : "text-muted-foreground"}`}>
                {serverLabel(x)}
              </span>
            </span>
          ))}
        </span>
      ) : isBase(t) ? (
        <span className="block text-muted-foreground">Made on first use</span>
      ) : null}
      {added || !t.teamId ? (
        <span className="block text-muted-foreground">{[t.teamId ? "" : "Installation", added].filter(Boolean).join(" · ")}</span>
      ) : null}
    </span>
  );
}
