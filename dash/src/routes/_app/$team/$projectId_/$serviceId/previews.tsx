import {
  ArrowClockwiseIcon,
  ArrowSquareOutIcon,
  GitPullRequestIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { GithubPermissionsNote } from "~/components/pyo/GithubSettings";
import { EnvEditor } from "~/components/pyo/editors";
import { TimeAgo } from "~/components/pyo/common";
import { AppLink } from "~/components/ui/AppLink";
import { Badge, Status, type Tone } from "~/components/ui/Badge";
import { Button, ButtonLink } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import {
  Field,
  Input,
  Segmented,
  Select,
  Stepper,
  Switch,
} from "~/components/ui/Field";
import { EmptyState, Panel } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { type DraftEnv } from "~/lib/pyo/draft";
import { ENV_KEY } from "~/lib/pyo/format";
import {
  deploymentPath,
  api,
  previewsOf,
  serviceIdOf,
  servicePath,
  usePyo,
} from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  Application,
  Preview,
  PreviewSettings,
  PreviewStatus,
} from "~/lib/pyo/types";

// Previews: a temporary copy of a Git service for each open pull request
// (GitHub) or merge request (GitLab). The copies are services of their own
// (previewOf), hidden from lists. This tab lists them and holds the settings.

export const Route = createFileRoute(
  "/_app/$team/$projectId_/$serviceId/previews",
)({
  component: () => {
    const serviceId = serviceIdOf(Route.useParams());
    const app = usePyo().services.find((a) => a.id === serviceId);
    return app ? <Previews key={app.id} app={app} /> : null;
  },
});

const DEFAULTS: PreviewSettings = {
  enabled: false,
  pulls: "all",
  label: "preview",
  limit: 5,
  idleDays: 0,
  access: "public",
  copySecrets: false,
  forks: "off",
  env: [],
};

const STATUS: Record<PreviewStatus, { tone: Tone; label: string }> = {
  waiting: { tone: "warning", label: "Waiting" },
  building: { tone: "progress", label: "Building" },
  ready: { tone: "success", label: "Ready" },
  failed: { tone: "danger", label: "Failed" },
  removing: { tone: "neutral", label: "Removing" },
  removed: { tone: "neutral", label: "Removed" },
};

const IDLE_DAYS = [0, 1, 3, 7, 14];

function Previews({ app }: { app: Application }) {
  const s = usePyo();
  const src = app.source.type === "git" ? app.source : null;
  const provider = src?.installationId
    ? "github"
    : src?.gitConnectionId
      ? "gitlab"
      : null;
  if (!src || !provider)
    return (
      <Panel title="Previews">
        <EmptyState
          compact
          icon={GitPullRequestIcon}
          title="Previews need a GitHub or GitLab repository"
          action={
            <ButtonLink href="/integrations" size="sm" variant="secondary">
              Integrations
            </ButtonLink>
          }
        />
      </Panel>
    );
  const settings = app.previews ?? DEFAULTS;
  const previews = previewsOf(s, app.id);
  const installs = s.githubInstallations.filter(
    (g) => g.id === src.installationId,
  );
  return (
    <div className="space-y-4">
      {provider === "github" ? <GithubPermissionsNote installs={installs} /> : null}
      {settings.enabled || previews.length ? (
        <PreviewList
          previews={previews}
          provider={provider}
          settings={settings}
        />
      ) : null}
      <PreviewForm
        key={JSON.stringify(app.previews ?? null)}
        app={app}
        provider={provider}
      />
    </div>
  );
}

function PreviewList({
  previews,
  provider,
  settings,
}: {
  previews: Preview[];
  provider: "github" | "gitlab";
  settings: PreviewSettings;
}) {
  const [deleting, setDeleting] = useState<Preview | null>(null);
  const term = provider === "github" ? "pull request" : "merge request";
  const sign = provider === "github" ? "#" : "!";
  return (
    <Panel title="Previews">
      {previews.length ? (
        <Table>
          <thead>
            <tr>
              <Th>{provider === "github" ? "Pull request" : "Merge request"}</Th>
              <Th>Status</Th>
              <Th>URL</Th>
              <Th>
                <span className="sr-only">Actions</span>
              </Th>
            </tr>
          </thead>
          <tbody>
            {previews.map((p) => {
              const st = STATUS[p.status];
              const removing = p.status === "removing";
              return (
                <Tr key={p.id}>
                  <Td className="max-w-[300px]">
                    <a
                      href={p.url}
                      target="_blank"
                      rel="noreferrer"
                      className="group inline-flex max-w-full items-center gap-1.5 hover:text-primary"
                    >
                      <span className="shrink-0 font-mono text-muted-foreground group-hover:text-primary">
                        {sign}
                        {p.number}
                      </span>
                      <span className="truncate font-medium">{p.title}</span>
                    </a>
                    <span className="mt-0.5 flex min-w-0 items-center gap-1.5 font-mono text-[11.5px] text-muted-foreground">
                      {p.previewServiceId ? (
                        <AppLink
                          href={servicePath(p.previewServiceId)}
                          title="Its service"
                          className="shrink-0 hover:text-foreground"
                        >
                          {p.name}
                        </AppLink>
                      ) : (
                        <span className="shrink-0">{p.name}</span>
                      )}
                      <span aria-hidden>·</span>
                      <span className="truncate" title="Branch">
                        {p.branch}
                      </span>
                      {p.fork ? <Badge className="font-sans">Fork</Badge> : null}
                    </span>
                  </Td>
                  <Td>
                    <span className="inline-flex items-center gap-2">
                      <Status tone={st.tone} label={st.label} />
                      <TimeAgo
                        at={p.updatedAt}
                        className="text-[12px] text-muted-foreground"
                      />
                    </span>
                    {p.message && p.status !== "ready" ? (
                      <span
                        title={p.message}
                        className={`mt-0.5 block max-w-[200px] truncate text-[11.5px] ${p.status === "failed" ? "text-destructive" : "text-muted-foreground"}`}
                      >
                        {p.message}
                      </span>
                    ) : null}
                  </Td>
                  <Td>
                    {p.hostname ? (
                      <a
                        href={`https://${p.hostname}`}
                        target="_blank"
                        rel="noreferrer"
                        className={`inline-flex max-w-[230px] items-center gap-1 font-mono text-[12px] hover:text-primary ${p.status === "ready" ? "" : "text-muted-foreground"}`}
                      >
                        <span className="truncate">{p.hostname}</span>
                        <ArrowSquareOutIcon size={12} className="shrink-0" aria-hidden />
                      </a>
                    ) : (
                      <span className="text-muted-foreground">None yet</span>
                    )}
                  </Td>
                  <Td>
                    <span className="flex items-center justify-end gap-1">
                      {p.status === "failed" || p.status === "waiting" ? (
                        <Button
                          size="xs"
                          variant="secondary"
                          onClick={async () => {
                            if (await api.redeployPreview(p.id))
                              toast({ title: `Redeploying ${p.name}` });
                          }}
                        >
                          <ArrowClockwiseIcon size={12} /> Redeploy
                        </Button>
                      ) : null}
                      {p.deploymentId ? (
                        <ButtonLink
                          href={deploymentPath(p.deploymentId)}
                          size="xs"
                          variant="ghost"
                        >
                          Logs
                        </ButtonLink>
                      ) : null}
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7 text-muted-foreground hover:text-destructive"
                        aria-label={`Delete ${p.name}`}
                        title="Delete preview"
                        disabled={removing}
                        onClick={() => setDeleting(p)}
                      >
                        <TrashIcon size={14} />
                      </Button>
                    </span>
                  </Td>
                </Tr>
              );
            })}
          </tbody>
        </Table>
      ) : (
        <EmptyState
          compact
          icon={GitPullRequestIcon}
          title="No previews"
          description={
            settings.pulls === "label"
              ? `Each open ${term} labeled ${settings.label} gets one.`
              : `Each open ${term} gets one.`
          }
        />
      )}
      <ConfirmDialog
        open={!!deleting}
        onClose={() => setDeleting(null)}
        title={`Delete ${deleting?.name}?`}
        description="The next push brings it back."
        confirmLabel="Delete preview"
        onConfirm={async () => {
          if (!deleting) return;
          if (await api.deletePreview(deleting.id))
            toast({ title: `Deleting ${deleting.name}` });
        }}
      />
    </Panel>
  );
}

const toRows = (env: PreviewSettings["env"]): DraftEnv[] =>
  env.map(({ id, key, value, secret, hasValue }) => ({
    id,
    key,
    value,
    secret,
    hasValue,
  }));

function PreviewForm({
  app,
  provider,
}: {
  app: Application;
  provider: "github" | "gitlab";
}) {
  const s = usePyo();
  const saved = app.previews ?? DEFAULTS;
  const [f, setF] = useState<Omit<PreviewSettings, "env">>(saved);
  const [rows, setRows] = useState<DraftEnv[]>(() => toRows(saved.env));
  const [showErrors, setShowErrors] = useState(false);
  const [saving, setSaving] = useState(false);
  const set = (p: Partial<PreviewSettings>) => setF((x) => ({ ...x, ...p }));
  const cluster = s.clusters.find((c) => c.id === app.clusterId);
  const tailnet = !!cluster?.tailnet;
  const requests = provider === "github" ? "Pull requests" : "Merge requests";
  const term = provider === "github" ? "pull request" : "merge request";

  const errors: Record<string, string> = {};
  const seen = new Set<string>();
  for (const r of rows) {
    if (!r.key) errors[`env.${r.id}`] = "Key is required.";
    else if (!ENV_KEY.test(r.key))
      errors[`env.${r.id}`] =
        "Use letters, digits and underscores. Don't start with a digit.";
    else if (seen.has(r.key)) errors[`env.${r.id}`] = "Duplicate key.";
    seen.add(r.key);
  }
  const labelError =
    f.pulls === "label" && !f.label.trim() ? "Add a label." : undefined;
  const accessError =
    f.access === "private" && !tailnet
      ? `${cluster?.name ?? "The cluster"} isn't on Tailscale.`
      : undefined;
  const pick = ({ enabled, pulls, label, limit, idleDays, access, copySecrets, forks }: PreviewSettings | typeof f) =>
    JSON.stringify({ enabled, pulls, label, limit, idleDays, access, copySecrets, forks });
  const dirty =
    pick(f) !== pick(saved) ||
    JSON.stringify(rows) !== JSON.stringify(toRows(saved.env));
  const idle = IDLE_DAYS.includes(f.idleDays)
    ? IDLE_DAYS
    : [...IDLE_DAYS, f.idleDays].sort((a, b) => a - b);

  const save = async () => {
    setShowErrors(true);
    if (Object.keys(errors).length || labelError) return;
    const now = Date.now();
    const before = new Map(saved.env.map((e) => [e.key, e]));
    setSaving(true);
    const res = await api.updateService(app.id, {
      previews: {
        ...f,
        label: f.label.trim(),
        env: rows.map((r) => ({
          ...r,
          updatedAt:
            before.get(r.key)?.value === r.value
              ? (before.get(r.key)?.updatedAt ?? now)
              : now,
        })),
      },
    });
    setSaving(false);
    if (res)
      toast({
        title:
          f.enabled !== saved.enabled
            ? f.enabled
              ? "Previews on"
              : "Previews off"
            : "Previews saved",
      });
  };

  return (
    <Panel title="Settings">
      <div className="space-y-5 p-4">
        <Switch
          checked={f.enabled}
          onChange={(enabled) => set({ enabled })}
          label={`Deploy a preview of each ${term}`}
          description="Removed when it closes or merges."
        />
        <div className="grid items-start gap-4 sm:grid-cols-2">
          <Field label={requests}>
            {(p) => (
              <Segmented
                {...p}
                label={requests}
                value={f.pulls}
                onChange={(pulls) => set({ pulls })}
                options={[
                  { value: "all", label: "All" },
                  { value: "label", label: "With a label" },
                ]}
              />
            )}
          </Field>
          <Field label="Label" error={showErrors ? labelError : undefined}>
            {(p) => (
              <Input
                {...p}
                mono
                value={f.label}
                disabled={f.pulls !== "label"}
                placeholder="preview"
                onChange={(e) => set({ label: e.target.value })}
              />
            )}
          </Field>
          <Field label="At once" hint="Others wait.">
            {(p) => (
              <Stepper
                {...p}
                min={1}
                max={50}
                value={f.limit}
                onChange={(limit) => set({ limit })}
              />
            )}
          </Field>
          <Field label="Remove when idle" hint="Days without a push.">
            {(p) => (
              <Select
                {...p}
                value={String(f.idleDays)}
                onChange={(e) => set({ idleDays: Number(e.target.value) })}
              >
                {idle.map((d) => (
                  <option key={d} value={d}>
                    {d === 0 ? "Off" : `${d} ${d === 1 ? "day" : "days"}`}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <Field
            label="Access"
            error={accessError}
            hint={
              f.access === "private"
                ? "Only on your tailnet."
                : "Anyone with the URL."
            }
          >
            {(p) => (
              <Segmented
                {...p}
                label="Access"
                value={f.access}
                onChange={(access) => set({ access })}
                options={[
                  { value: "public", label: "Public" },
                  {
                    value: "private",
                    label: "Private",
                    disabled: !tailnet && f.access !== "private",
                    title: tailnet ? undefined : "Needs Tailscale on the cluster",
                  },
                ]}
              />
            )}
          </Field>
          <Field
            label={`${requests} from forks`}
            hint={
              f.forks === "secrets"
                ? "Their code can read your secret variables."
                : undefined
            }
          >
            {(p) => (
              <Select
                {...p}
                value={f.forks}
                onChange={(e) =>
                  set({ forks: e.target.value as PreviewSettings["forks"] })
                }
              >
                <option value="off">Don't deploy</option>
                <option value="no-secrets">Deploy without secrets</option>
                <option value="secrets">Deploy with secrets</option>
              </Select>
            )}
          </Field>
        </div>
        <Switch
          checked={f.copySecrets}
          onChange={(copySecrets) => set({ copySecrets })}
          label="Copy secret variables"
          description="Other variables are always copied."
        />
      </div>
      <div className="space-y-3 border-t border-border p-4">
        <div>
          <p className="text-[13px] font-medium">Variables</p>
          <p className="text-[12px] text-muted-foreground">
            Replace or add to the service's variables.
          </p>
        </div>
        <EnvEditor
          rows={rows}
          onChange={setRows}
          errors={showErrors ? errors : {}}
        />
      </div>
      <div className="flex justify-end gap-2 border-t border-border px-4 py-3">
        <Button
          variant="ghost"
          size="sm"
          disabled={!dirty || saving}
          onClick={() => {
            setF(saved);
            setRows(toRows(saved.env));
            setShowErrors(false);
          }}
        >
          Discard
        </Button>
        <Button
          size="sm"
          disabled={!dirty || saving || !!accessError}
          onClick={() => void save()}
        >
          {saving ? "Saving…" : "Save"}
        </Button>
      </div>
    </Panel>
  );
}
