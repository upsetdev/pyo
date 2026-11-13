import { fieldGroupClass, groupInputClass, optionClass } from "~/components/ui/Field";
import {
  ArrowUpRightIcon,
  CaretDownIcon,
  CaretRightIcon,
  CheckCircleIcon,
  CircleNotchIcon,
  GitBranchIcon,
  GlobeHemisphereEastIcon,
  LockSimpleIcon,
  PackageIcon,
  PlusIcon,
  PushPinIcon,
  TrashIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import { ProjectIcon } from "~/components/ui/ProjectIcon";
import { Menu } from "~/components/ui/Menu";
import {
  imageHost,
  imagePage,
  registryIcon,
} from "~/components/pyo/ProjectCard";
import { AppLink } from "~/components/ui/AppLink";
import { ReplicaPicker } from "~/components/pyo/ReplicaPicker";
import { RoomNote } from "~/components/pyo/RoomNote";
import { MAX_REPLICAS, freeRoom, roomWarning } from "~/lib/pyo/capacity";
import { Tags } from "~/components/ui/TagInput";
import { useState, type ReactNode } from "react";
import { locationLabel, parseLocation, placesOf } from "~/lib/pyo/location";
import { Flag } from "~/components/ui/Flag";
import { freeHostname, ingressIp, isFreeDomain } from "~/lib/pyo/freeDomain";
import { BrandMark, GITHUB_LOGO } from "~/components/pyo/common";
import { SearchInput } from "~/components/ui/Table";
import { fetchGithubRepos } from "~/lib/pyo/backend";
import { cpu, describeCron, mem, timeAgo } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import type { GithubRepo, TlsMode } from "~/lib/pyo/types";
import { Link } from "@tanstack/react-router";
import { Dot } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import {
  Field,
  Input,
  SecretInput,
  Segmented,
  Select,
  Switch,
} from "~/components/ui/Field";
import { Callout } from "~/components/ui/Panel";
import {
  draftId,
  NEW_PROJECT,
  syncTemplateEnv,
  type DeployDraft,
  type Issues,
} from "~/lib/pyo/draft";
import {
  clusterSummary,
  clustersIn,
  draftProject,
  projectsIn,
  usePyo,
  serversOf,
  newProjectFor,
} from "~/lib/pyo/store";
import { getTemplate } from "~/lib/pyo/templates";
import {
  EnvEditor,
  HealthCheckFields,
  PortEditor,
  ResourceFields,
} from "../editors";
import { currentSlug } from "~/lib/pyo/paths";
import type { ClusterServer, Resources } from "~/lib/pyo/types";
import { siGithub, siGitlab } from "simple-icons";

export type StepProps = {
  d: DeployDraft;
  set: (patch: Partial<DeployDraft>) => void;
  errors: Issues;
};

const err = (e: Issues, k: string) => e[k] || undefined;

// ── 1. Source ──────────────────────────────────────────────────────────────

/** Repositories of the team's GitHub connections, newest push first. */
function GithubRepoPicker({
  selected,
  onPick,
}: {
  selected: string;
  onPick: (r: GithubRepo) => void;
}) {
  const s = usePyo();
  const [q, setQ] = useState("");
  const repos = useQuery(`gh-repos:${s.currentTeamId}`, () =>
    fetchGithubRepos(s.currentTeamId),
  );
  const list =
    repos.status === "success"
      ? repos.data.repositories
          .filter(
            (r) =>
              !q.trim() ||
              r.fullName.toLowerCase().includes(q.trim().toLowerCase()),
          )
          .sort((a, b) => (b.pushedAt ?? "").localeCompare(a.pushedAt ?? ""))
      : [];
  return (
    <div className="overflow-hidden rounded-xl border border-border">
      <div className="flex items-center gap-2 border-b border-border px-3 py-2">
        <BrandMark icon={GITHUB_LOGO} size={14} />
        <span className="text-[13px] font-medium">
          Your GitHub repositories
        </span>
        <SearchInput
          value={q}
          onChange={setQ}
          placeholder="Search…"
          className="ml-auto w-48"
        />
      </div>
      <ul className="max-h-64 list-rows overflow-y-auto">
        {repos.status === "loading" ? (
          <li className="flex items-center gap-2 px-3 py-4 text-[12.5px] text-muted-foreground">
            <CircleNotchIcon size={13} className="animate-spin" /> Loading
            repositories…
          </li>
        ) : repos.status === "error" ? (
          <li className="px-3 py-4 text-[12.5px] text-destructive">
            {repos.error}
          </li>
        ) : list.length ? (
          list.slice(0, 100).map((r) => {
            const on = selected === r.htmlUrl;
            return (
              <li key={r.fullName}>
                <button
                  type="button"
                  onClick={() => onPick(r)}
                  className={`flex w-full items-center gap-3 px-3 py-2.5 text-left text-[13px] transition-colors ${on ? "bg-primary/10" : "hover:bg-accent/40"}`}
                >
                  <span className="min-w-0 flex-1 truncate">
                    <span className="text-muted-foreground">{r.account}/</span>
                    <span className="font-medium">{r.name}</span>
                  </span>
                  {r.private ? (
                    <LockSimpleIcon
                      size={12}
                      className="shrink-0 text-muted-foreground"
                      aria-label="Private"
                    />
                  ) : null}
                  {r.pushedAt ? (
                    <span className="hidden shrink-0 text-[12px] text-muted-foreground sm:block">
                      {timeAgo(Date.parse(r.pushedAt))}
                    </span>
                  ) : null}
                  {on ? (
                    <CheckCircleIcon
                      size={15}
                      weight="fill"
                      className="shrink-0 text-primary"
                    />
                  ) : null}
                </button>
              </li>
            );
          })
        ) : (
          <li className="px-3 py-4 text-[12.5px] text-muted-foreground">
            {q
              ? "No repositories match."
              : "No repositories shared with Pyo yet. Choose some on GitHub, or connect another account in "}
            {q ? null : (
              <Link
                to="/$team/integrations"
                params={{ team: currentSlug() }}
                className="underline"
              >
                Integrations
              </Link>
            )}
          </li>
        )}
      </ul>
    </div>
  );
}

/** Build settings (Git) or image reference (container image). */
export function SourceStep({ d, set, errors }: StepProps) {
  const s = usePyo();
  // null: open only when something in there is set; a click decides after that.
  const [buildAdvanced, setBuildAdvanced] = useState<boolean | null>(null);
  if (d.sourceType === "git") {
    const g = d.git;
    const setGit = (patch: Partial<DeployDraft["git"]>) =>
      set({ git: { ...g, ...patch } });
    const connected = s.githubInstallations.some(
      (x) => x.teamId === s.currentTeamId,
    );
    return (
      <div className="space-y-5">
        {connected ? (
          <GithubRepoPicker
            selected={g.installationId ? g.repoUrl : ""}
            onPick={(r) =>
              setGit({
                repoUrl: r.htmlUrl,
                branch: r.defaultBranch || "main",
                installationId: r.installationId,
                private: r.private,
                commit: "",
              })
            }
          />
        ) : null}
        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            label={connected ? "Or a public repository URL" : "Repository URL"}
            required={!connected}
            error={err(errors, "repoUrl")}
            className="sm:col-span-2"
            hint={
              g.installationId
                ? "Private repo, deploys on push."
                : connected
                  ? "Any public HTTPS Git URL."
                  : "A public HTTPS URL. Connect GitHub for private repos."
            }
          >
            {(p) => (
              <div className="relative">
                <span className="pointer-events-none absolute inset-y-0 left-3 flex items-center text-foreground/80">
                  <BrandMark
                    icon={/gitlab\.com/.test(g.repoUrl) ? siGitlab : siGithub}
                    size={15}
                  />
                </span>
                <Input
                  {...p}
                  mono
                  className="pl-9"
                  value={g.repoUrl}
                  onChange={(e) =>
                    setGit({
                      repoUrl: e.target.value.trim(),
                      installationId: undefined,
                      private: false,
                    })
                  }
                  placeholder="https://github.com/acme/storefront"
                />
              </div>
            )}
          </Field>
          <Field
            label="Branch"
            required
            error={err(errors, "branch")}
            className="sm:col-span-2"
          >
            {(p) => (
              <Input
                {...p}
                mono
                value={g.branch}
                onChange={(e) => setGit({ branch: e.target.value.trim() })}
              />
            )}
          </Field>
        </div>
        <Advanced
          title="Advanced: Dockerfile, build context and arguments"
          open={
            Object.keys(errors).some(
              (k) => k === "dockerfilePath" || k.startsWith("buildArg."),
            ) ||
            (buildAdvanced ??
              (g.dockerfilePath.trim() !== "Dockerfile" ||
                !["", ".", "./"].includes(g.buildContext.trim()) ||
                g.buildArgs.length > 0))
          }
          onToggle={(open) => setBuildAdvanced(!open)}
        >
          <div className="grid gap-4 sm:grid-cols-2">
            <Field
              label="Dockerfile path"
              error={err(errors, "dockerfilePath")}
              hint="Relative to the build context. The default also finds a Containerfile."
            >
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={g.dockerfilePath}
                  onChange={(e) => setGit({ dockerfilePath: e.target.value })}
                />
              )}
            </Field>
            <Field label="Build context" hint="Folder in the repo to build.">
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={g.buildContext}
                  onChange={(e) => setGit({ buildContext: e.target.value })}
                />
              )}
            </Field>
          </div>
          <div>
            <p className="mb-1.5 text-[12px] font-medium">Build arguments</p>
            <div className="space-y-2">
              {g.buildArgs.map((a) => (
                <div key={a.id}>
                  <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_32px] gap-2">
                    <Input
                      mono
                      value={a.key}
                      placeholder="ARG_NAME"
                      aria-label="Build argument name"
                      onChange={(e) =>
                        setGit({
                          buildArgs: g.buildArgs.map((x) =>
                            x.id === a.id
                              ? { ...x, key: e.target.value.toUpperCase() }
                              : x,
                          ),
                        })
                      }
                    />
                    <Input
                      mono
                      value={a.value}
                      placeholder="value"
                      aria-label="Build argument value"
                      onChange={(e) =>
                        setGit({
                          buildArgs: g.buildArgs.map((x) =>
                            x.id === a.id ? { ...x, value: e.target.value } : x,
                          ),
                        })
                      }
                    />
                    <Button
                      variant="ghost"
                      size="icon"
                      aria-label="Remove build argument"
                      onClick={() =>
                        setGit({
                          buildArgs: g.buildArgs.filter((x) => x.id !== a.id),
                        })
                      }
                    >
                      <TrashIcon size={14} />
                    </Button>
                  </div>
                  {errors[`buildArg.${a.id}`] ? (
                    <p className="mt-1 text-[11.5px] text-destructive">
                      {errors[`buildArg.${a.id}`]}
                    </p>
                  ) : null}
                </div>
              ))}
            </div>
            <Button
              variant="secondary"
              size="xs"
              className="mt-2"
              onClick={() =>
                setGit({
                  buildArgs: [
                    ...g.buildArgs,
                    { id: draftId(), key: "", value: "" },
                  ],
                })
              }
            >
              <PlusIcon size={12} /> Add build argument
            </Button>
            <p className="mt-1.5 text-[11.5px] text-muted-foreground">
              Visible in the image, so don&apos;t put secrets here.
            </p>
          </div>
        </Advanced>
      </div>
    );
  }
  const im = d.image;
  const setImg = (patch: Partial<DeployDraft["image"]>) =>
    set({ image: { ...im, ...patch } });
  const registries = s.registries.filter(
    (r) => r.teamId === null || r.teamId === s.currentTeamId,
  );
  return (
    <ImageReference
      image={im.image}
      reference={im.reference}
      registries={registries}
      onChange={(image, reference, registryId) =>
        setImg({ image, reference, ...(registryId ? { registryId } : {}) })
      }
      errors={errors}
    />
  );
}

// ── General ────────────────────────────────────────────────────────────────

/** Name, project and target cluster: the only choices every deploy needs. */
/** `fixedProject`: on a project's canvas, the project is that one (a name: a new one being made). */
export function GeneralFields({
  d,
  set,
  errors,
  fixedProject = false,
  part = "all",
}: StepProps & {
  fixedProject?: boolean | string;
  /** One half only, for a checkout that shows them as separate blocks. */
  part?: "all" | "name" | "placement";
}) {
  const s = usePyo();
  const projects = projectsIn(s);
  const clusters = clustersIn(s);
  const project = draftProject(s, d);
  return (
    <div className="space-y-5">
      {part !== "placement" ? (
      <div className="space-y-1.5">
        <p className={`text-[12px] font-medium text-muted-foreground ${part === "name" ? "sr-only" : ""}`}>
          Project and name
        </p>
        <div
          className={fieldGroupClass(!!(err(errors, "name") || err(errors, "projectId")))}
        >
          {fixedProject ? (
            <span className="flex h-full shrink-0 items-center gap-2 border-r border-border px-3 text-[13px]">
              <ProjectIcon size={15} className="shrink-0 text-muted-foreground" />
              <span className="max-w-[10rem] truncate sm:max-w-[14rem]">{typeof fixedProject === "string" ? fixedProject : project?.name}</span>
            </span>
          ) : (
          <Menu
            label="Project"
            align="start"
            items={[
              {
                label: `New project${project?.isNew || d.projectId !== NEW_PROJECT ? `: ${newProjectFor(s, d.name || "service").name}` : ""}`,
                icon: <PlusIcon size={14} />,
                onSelect: () => set({ projectId: NEW_PROJECT }),
              },
              ...(projects.length ? [{ type: "separator" as const }] : []),
              ...projects.map((x) => ({
                label: x.name,
                icon: <ProjectIcon size={14} />,
                onSelect: () => set({ projectId: x.id }),
              })),
            ]}
            trigger={(tp) => (
              <button
                {...tp}
                type="button"
                className="flex h-full shrink-0 items-center gap-2 border-r border-border px-3 text-[13px] transition-colors hover:bg-accent/50"
              >
                <ProjectIcon
                  size={15}
                  className="shrink-0 text-muted-foreground"
                />
                <span className="max-w-[10rem] truncate sm:max-w-[14rem]">
                  {project ? project.name : "Pick a project"}
                </span>
                {project?.isNew ? (
                  <span className="shrink-0 rounded bg-primary/15 px-1 text-[10.5px] font-medium text-primary">
                    new
                  </span>
                ) : null}
                <CaretDownIcon
                  size={11}
                  className="shrink-0 text-muted-foreground"
                />
              </button>
            )}
          />
          )}
          <span
            aria-hidden
            className="flex items-center pl-3 font-mono text-[15px] text-muted-foreground"
          >
            /
          </span>
          <input
            aria-label="Name"
            value={d.name}
            onChange={(e) =>
              set({
                name: e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, "-"),
              })
            }
            placeholder="web"
            spellCheck={false}
            className={`${groupInputClass} flex-1 px-2 font-mono`}
          />
        </div>
        {err(errors, "name") || err(errors, "projectId") ? (
          <p className="text-[11.5px] text-destructive" data-error>
            {err(errors, "name") ?? err(errors, "projectId")}
          </p>
        ) : (
          <p className="text-[12px] text-muted-foreground">
            {typeof fixedProject === "string"
              ? "A new project. Rename it in the header."
              : project?.isNew
              ? "Gets its own project."
              : "Other services in this project can reach it by name."}
          </p>
        )}
      </div>

      ) : null}

      {part !== "name" ? (
      <fieldset>
        <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
          <legend className={`text-[12px] font-medium text-muted-foreground ${part === "placement" ? "sr-only" : ""}`}>
            Where it runs
          </legend>
          {clusters.length ? (
            <Segmented
              label="Place it by"
              size="md"
              value={d.placement}
              onChange={(placement) =>
                // One way at a time: the others' choices are dropped.
                set({
                  placement,
                  location: placement === "location" ? d.location : "",
                  serverTags: placement === "tags" ? d.serverTags : [],
                  serverNames: placement === "servers" ? d.serverNames : [],
                })
              }
              options={[
                { value: "location", label: "Region" },
                { value: "tags", label: "Tags" },
                { value: "servers", label: "Servers" },
              ]}
            />
          ) : null}
        </div>
        {clusters.length && d.placement === "tags" ? (
          <TagPlacement d={d} set={set} />
        ) : clusters.length && d.placement === "servers" ? (
          <ServerPlacement d={d} set={set} />
        ) : clusters.length ? (
          <div
            role="radiogroup"
            aria-label="Region"
            className="space-y-2"
          >
            {clusters.flatMap((c) => {
              const sum = clusterSummary(s, c);
              const usable =
                c.status === "connected" || c.status === "degraded";
              const free = sum.cpu.capacity
                ? 1 - sum.cpu.used / sum.cpu.capacity
                : 0;
              const places = placesOf(c, s.servers);
              // One place: no pin (the service may use servers added elsewhere later).
              // Several: each place pins the service there; "Anywhere" doesn't.
              const options =
                places.length > 1
                  ? [
                      ...places.map((pl) => ({
                        pin: pl.location,
                        label: locationLabel(pl.location),
                        country: parseLocation(pl.location).country,
                        servers: pl.servers,
                      })),
                      {
                        pin: "",
                        label: `Anywhere in ${c.name}`,
                        country: "",
                        servers: sum.servers,
                      },
                    ]
                  : [
                      {
                        pin: "",
                        label: locationLabel(places[0].location),
                        country: parseLocation(places[0].location).country,
                        servers: sum.servers,
                      },
                    ];
              // Free CPU of the servers the option would use.
              const freeAt = (pin: string) => {
                const ns = s.servers.filter(
                  (n) =>
                    n.clusterId === c.id &&
                    n.status === "ready" &&
                    (!pin || (n.location ?? c.location) === pin),
                );
                const cap = ns.reduce((t, n) => t + n.capacity.cpu, 0);
                return cap
                  ? 1 - ns.reduce((t, n) => t + n.usage.cpu, 0) / cap
                  : places.length > 1
                    ? null
                    : free;
              };
              return options.map((o) => {
                const selected =
                  d.clusterId === c.id && (d.location ?? "") === o.pin;
                return (
                  <button
                    key={`${c.id}:${o.pin}`}
                    type="button"
                    role="radio"
                    aria-checked={selected}
                    // A service with GPUs only goes where they are.
                    disabled={
                      !usable ||
                      (c.capabilities.gpu?.maxPerServer ?? 0) <
                        (d.resources.gpu ?? 0)
                    }
                    onClick={() => {
                      const sc =
                        c.capabilities.storageClasses.find((x) => x.isDefault)
                          ?.name ?? "";
                      set({
                        clusterId: c.id,
                        location: o.pin,
                        volumes: d.volumes.map((v) => ({
                          ...v,
                          storageClass: c.capabilities.storageClasses.some(
                            (x) => x.name === v.storageClass,
                          )
                            ? v.storageClass
                            : sc,
                        })),
                      });
                    }}
                    className={`${optionClass({ selected, invalid: !!(errors.clusterId || errors.placement) && !d.clusterId })} flex w-full items-center gap-3 px-3.5 py-3`}
                  >
                    <span
                      aria-hidden
                      className={`flex h-4 w-4 shrink-0 items-center justify-center rounded-full border ${selected ? "border-primary" : "border-input"}`}
                    >
                      {selected ? (
                        <span className="h-2 w-2 rounded-full bg-primary" />
                      ) : null}
                    </span>
                    <Flag country={o.country} size={22} />
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center gap-2 text-[13.5px] font-medium">
                        <span className="truncate">{o.label}</span>
                      </span>
                      <span className="flex min-w-0 items-center gap-1.5 truncate font-mono text-[11.5px] text-muted-foreground">
                        {c.status !== "connected" ? (
                          <span
                            title={`${c.name} is ${c.status}`}
                            className={`h-1.5 w-1.5 shrink-0 rounded-full ${c.status === "degraded" ? "bg-warning" : "bg-destructive"}`}
                          />
                        ) : null}
                        {c.name}
                        <Tags tags={c.tags} />
                      </span>
                    </span>
                    <span className="shrink-0 text-right text-[12px] text-muted-foreground">
                      {o.servers} {o.servers === 1 ? "server" : "servers"}
                      <span className="block">
                        {freeAt(o.pin) === null
                          ? "Offline"
                          : `${Math.round((freeAt(o.pin) ?? 0) * 100)}% CPU free`}
                      </span>
                      {c.capabilities.gpu?.total ? (
                        <span className="block">
                          {c.capabilities.gpu.total -
                            (c.capabilities.gpu.used ?? 0)}{" "}
                          of {c.capabilities.gpu.total} GPUs free
                        </span>
                      ) : null}
                    </span>
                  </button>
                );
              });
            })}
          </div>
        ) : (
          <Callout tone="warning" icon={WarningIcon} title="No clusters yet">
            <Link
              to="/$team/clusters/new"
              params={{ team: currentSlug() }}
              className="font-medium text-foreground underline"
            >
              Add a cluster
            </Link>{" "}
            to deploy.
          </Callout>
        )}
        {(errors.clusterId || errors.placement) && clusters.length ? (
          <p className="mt-1.5 text-[11.5px] text-destructive" data-error>
            {errors.placement ?? errors.clusterId}
          </p>
        ) : null}
      </fieldset>
      ) : null}
    </div>
  );
}

/** Settings defined by the chosen template (version, users, passwords…). */
export function TemplateFields({ d, set, errors }: StepProps) {
  const t = getTemplate(d.templateId);
  if (!t) return null;
  const setValue = (key: string, v: string | number | boolean) => {
    const next: DeployDraft = {
      ...d,
      templateValues: { ...d.templateValues, [key]: v },
    };
    set(syncTemplateEnv(next));
  };
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      {t.fields.map((f) => {
        const v = d.templateValues[f.key];
        return f.type === "boolean" ? (
          <div key={f.key} className="sm:col-span-2">
            <Switch
              checked={!!v}
              onChange={(x) => setValue(f.key, x)}
              label={f.label}
              description={f.help}
            />
          </div>
        ) : (
          <Field
            key={f.key}
            label={f.label}
            hint={f.help}
            error={err(errors, `field.${f.key}`)}
          >
            {(p) =>
              f.type === "select" ? (
                <Select
                  {...p}
                  value={String(v)}
                  onChange={(e) => setValue(f.key, e.target.value)}
                >
                  {f.options?.map((o) => (
                    <option key={o} value={o}>
                      {o}
                    </option>
                  ))}
                </Select>
              ) : f.type === "password" ? (
                <SecretInput
                  {...p}
                  value={String(v ?? "")}
                  onChange={(e) => setValue(f.key, e.target.value)}
                  placeholder="Generated on deploy"
                />
              ) : (
                <Input
                  {...p}
                  mono={f.type !== "text"}
                  type={f.type === "number" ? "number" : "text"}
                  value={String(v ?? "")}
                  onChange={(e) =>
                    setValue(
                      f.key,
                      f.type === "number"
                        ? Number(e.target.value)
                        : e.target.value,
                    )
                  }
                />
              )
            }
          </Field>
        );
      })}
    </div>
  );
}

// ── 4. Environment ─────────────────────────────────────────────────────────

export function EnvStep({ d, set, errors }: StepProps) {
  const generated =
    getTemplate(d.templateId)
      ?.env.filter((e) => e.generate)
      .map((e) => e.key) ?? [];
  return (
    <div className="space-y-4">
      <p className="text-[12.5px] text-muted-foreground">
        Secrets are encrypted on the cluster and hidden here.
        {generated.length ? (
          <>
            {" "}
            Empty <span className="font-mono">{generated.join(", ")}</span> is
            generated on deploy.
          </>
        ) : null}
      </p>
      <EnvEditor
        rows={d.env}
        onChange={(env) => set({ env })}
        errors={errors}
      />
    </div>
  );
}

// ── 5. Resources and storage ───────────────────────────────────────────────

export function ResourcesStep({ d, set, errors }: StepProps) {
  const s = usePyo();
  const t = getTemplate(d.templateId);
  // The four compute fields sit behind "Advanced"; it
  // opens by itself for a custom size or a problem in there.
  // null: open only for a custom size; a click decides after that.
  const [showAdvanced, setShowAdvanced] = useState<boolean | null>(null);
  const hasAdvancedError = Object.keys(errors).some((k) => k !== "replicas");
  // Accepted either way: replicas that don't fit wait for room.
  const roomNote = d.cron
    ? null
    : roomWarning(
        "run",
        d.replicas,
        d.resources,
        0,
        freeRoom(s.servers, d.clusterId, {
          servers: d.placement === "servers" ? d.serverNames : [],
          serverTags: d.placement === "tags" ? d.serverTags : [],
          gpu: d.resources.gpu,
        }),
      );
  return (
    <div className="space-y-6">
      <section className="space-y-3">
        <div className="flex items-center justify-between gap-2">
          <h3 className="text-[13px] font-semibold">Size</h3>
          {t ? (
            <Button
              variant="ghost"
              size="xs"
              onClick={() =>
                set({ resources: { ...t.resources }, replicas: t.replicas })
              }
            >
              Reset to recommended
            </Button>
          ) : null}
        </div>
        <SizePresets
          value={d.resources}
          onChange={(resources) => set({ resources })}
        />
        {t ? (
          <p className="text-[11.5px] text-muted-foreground">
            Recommended for {t.name}: {cpu(t.resources.cpuRequest)} and{" "}
            {mem(t.resources.memoryRequest)} reserved.
          </p>
        ) : null}
      </section>
      <section className={d.cron ? "hidden" : "space-y-3"}>
        <h3 className="text-[13px] font-semibold">Replicas</h3>
        <ReplicaPicker
          value={d.replicas}
          onChange={(replicas) => set({ replicas })}
          max={t?.stateful ? 1 : MAX_REPLICAS}
          align="start"
        />
        <p className="text-[12px] text-muted-foreground">
          {t?.stateful
            ? "Keeps its data on one server, so it runs as one replica."
            : d.replicas > 1
              ? "Spread across servers, sharing the traffic."
              : "More replicas survive outages."}
        </p>
        <RoomNote text={roomNote} />
        {err(errors, "replicas") ? (
          <p className="text-[11.5px] text-destructive" data-error>
            {err(errors, "replicas")}
          </p>
        ) : null}
      </section>
      <Advanced
        open={
          hasAdvancedError ||
          (showAdvanced ?? !SIZES.some((x) => sameSize(x.r, d.resources)))
        }
        onToggle={(open) => setShowAdvanced(!open)}
      >
        <section className="space-y-3">
          <ResourceFields
            value={d.resources}
            onChange={(resources) => set({ resources })}
            errors={errors}
            cluster={s.clusters.find((c) => c.id === d.clusterId)}
          />
        </section>
      </Advanced>
    </div>
  );
}

// ── Health check ───────────────────────────────────────────────────────────

export function HealthStep({ d, set, errors }: StepProps) {
  return (
    <HealthCheckFields
      label="Type"
      value={d.healthCheck}
      onChange={(healthCheck) => set({ healthCheck })}
      errors={errors}
    />
  );
}

// ── 6. Networking ──────────────────────────────────────────────────────────

export function VolumesStep({ d, set, errors }: StepProps) {
  const s = usePyo();
  const t = getTemplate(d.templateId);
  const cluster = s.clusters.find((c) => c.id === d.clusterId);
  const classes = cluster?.capabilities.storageClasses ?? [];
  return (
    <div className="space-y-6">
      <section className="space-y-3">
        <p className="text-[12.5px] text-muted-foreground">
          Folders that survive restarts and deploys, like a database&apos;s
          data. Everything else is reset each time.
        </p>
        {t?.storage?.required ? (
          <p className="text-[12px] text-muted-foreground">
            {t.name} stores its data at{" "}
            <span className="font-mono">{t.storage.mountPath}</span>. Add a
            volume there to keep it.
          </p>
        ) : null}
        {errors.volumes ? (
          <Callout tone="warning" icon={WarningIcon}>
            {errors.volumes}
          </Callout>
        ) : null}
        {d.volumes.map((v) => {
          const setV = (patch: Partial<typeof v>) =>
            set({
              volumes: d.volumes.map((x) =>
                x.id === v.id ? { ...x, ...patch } : x,
              ),
            });
          return (
            <div key={v.id} className="rounded-md border border-border p-3">
              <div className="grid gap-3 sm:grid-cols-[minmax(0,1.1fr)_minmax(0,1fr)_80px_minmax(0,1.1fr)_32px] sm:items-end">
                <Field label="Name">
                  {(p) => (
                    <Input
                      {...p}
                      mono
                      value={v.name}
                      onChange={(e) =>
                        setV({ name: e.target.value.toLowerCase() })
                      }
                    />
                  )}
                </Field>
                <Field label="Folder in the service">
                  {(p) => (
                    <Input
                      {...p}
                      mono
                      value={v.mountPath}
                      onChange={(e) => setV({ mountPath: e.target.value })}
                    />
                  )}
                </Field>
                <Field label="Size, GB">
                  {(p) => (
                    <Input
                      {...p}
                      type="number"
                      min={1}
                      value={v.sizeGi}
                      onChange={(e) => setV({ sizeGi: Number(e.target.value) })}
                    />
                  )}
                </Field>
                <Field label="Storage">
                  {(p) => (
                    <Select
                      {...p}
                      value={v.storageClass}
                      onChange={(e) => setV({ storageClass: e.target.value })}
                      disabled={!classes.length}
                    >
                      {!classes.length ? (
                        <option value="" hidden>
                          Choose a cluster first
                        </option>
                      ) : null}
                      {classes.map((c) => (
                        <option key={c.name} value={c.name}>
                          {c.name}
                        </option>
                      ))}
                    </Select>
                  )}
                </Field>
                <Button
                  variant="ghost"
                  size="icon"
                  aria-label={`Remove ${v.name}`}
                  onClick={() =>
                    set({ volumes: d.volumes.filter((x) => x.id !== v.id) })
                  }
                >
                  <TrashIcon size={14} />
                </Button>
              </div>
              {errors[`vol.${v.id}`] ? (
                <p className="mt-1.5 text-[11.5px] text-destructive">
                  {errors[`vol.${v.id}`]}
                </p>
              ) : null}
            </div>
          );
        })}
        <div>
          <Button
            variant="secondary"
            size="xs"
            onClick={() =>
              set({
                volumes: [
                  ...d.volumes,
                  {
                    id: draftId(),
                    name: `${d.name || "app"}-data${d.volumes.length ? `-${d.volumes.length + 1}` : ""}`,
                    mountPath: "/data",
                    sizeGi: 5,
                    storageClass: classes.find((c) => c.isDefault)?.name ?? "",
                  },
                ],
              })
            }
          >
            <PlusIcon size={12} /> Add volume
          </Button>
        </div>
      </section>
    </div>
  );
}

const TLS_CHOICES: { value: TlsMode; label: string }[] = [
  { value: "auto", label: "HTTPS (automatic)" },
  { value: "letsencrypt", label: "HTTPS · Let's Encrypt" },
  { value: "selfsigned", label: "HTTPS · self-signed" },
  { value: "off", label: "HTTP only" },
];

/** Hostnames the service answers on: a free nip.io one or your own, with HTTPS. */
export function DomainsStep({ d, set, errors }: StepProps) {
  const s = usePyo();
  const cluster = s.clusters.find((c) => c.id === d.clusterId);
  const httpPorts = d.ports.filter((p) => p.protocol === "HTTP");
  const project = draftProject(s, d);
  const ip = d.clusterId ? ingressIp(s, d.clusterId) : undefined;
  const free = ip
    ? freeHostname({ name: d.name || "app" }, project?.namespace ?? "app", ip)
    : "";
  const add = (hostname = "") =>
    set({
      domains: [
        ...d.domains,
        {
          id: draftId(),
          hostname,
          portName: httpPorts[0].name,
          redirectHttps: true,
          tlsMode: "auto",
        },
      ],
    });
  if (!httpPorts.length)
    return (
      <p className="text-[12.5px] text-muted-foreground">
        Add an HTTP port in Networking to give it a domain.
      </p>
    );
  return (
    <div className="space-y-3">
      <p className="text-[12.5px] text-muted-foreground">
        Without a domain, only the cluster can reach it.
      </p>
      {d.domains.length ? (
        <ul className="list-rows overflow-hidden rounded-lg border border-border">
          {d.domains.map((dm) => {
            const setD = (patch: Partial<typeof dm>) =>
              set({
                domains: d.domains.map((x) =>
                  x.id === dm.id ? { ...x, ...patch } : x,
                ),
              });
            const freeOne = isFreeDomain(dm.hostname);
            const tls = dm.tlsMode ?? "auto";
            const error = errors[`domain.${dm.id}`];
            return (
              <li
                key={dm.id}
                className="space-y-2 px-3 py-3"
                data-error={error ? true : undefined}
              >
                <div className="flex flex-wrap items-center gap-2">
                  <div className="relative min-w-0 flex-[3_1_16rem]">
                    <Input
                      mono
                      aria-label="Hostname"
                      aria-invalid={error ? true : undefined}
                      value={dm.hostname}
                      placeholder="app.example.com"
                      className={freeOne ? "pr-14" : undefined}
                      onChange={(e) =>
                        setD({ hostname: e.target.value.trim().toLowerCase() })
                      }
                    />
                    {freeOne ? (
                      <span className="pointer-events-none absolute inset-y-0 right-2.5 flex items-center">
                        <span className="rounded bg-success/15 px-1.5 text-[10.5px] font-medium text-success">
                          free
                        </span>
                      </span>
                    ) : null}
                  </div>
                  <div className="flex-[1_1_9rem]">
                    <Select
                      aria-label="HTTPS"
                      value={tls}
                      onChange={(e) =>
                        setD({ tlsMode: e.target.value as TlsMode })
                      }
                    >
                      {TLS_CHOICES.map((c) => (
                        <option key={c.value} value={c.value}>
                          {c.label}
                        </option>
                      ))}
                    </Select>
                  </div>
                  {httpPorts.length > 1 ? (
                    <div className="flex-[1_1_8rem]">
                      <Select
                        aria-label="Route to port"
                        value={dm.portName}
                        onChange={(e) => setD({ portName: e.target.value })}
                      >
                        {httpPorts.map((x) => (
                          <option key={x.id} value={x.name}>
                            {x.name} · {x.containerPort}
                          </option>
                        ))}
                      </Select>
                    </div>
                  ) : null}
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label={`Remove ${dm.hostname || "domain"}`}
                    onClick={() =>
                      set({ domains: d.domains.filter((x) => x.id !== dm.id) })
                    }
                  >
                    <TrashIcon size={14} />
                  </Button>
                </div>
                <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-[11.5px]">
                  <span
                    className={
                      error ? "text-destructive" : "text-muted-foreground"
                    }
                  >
                    {error ??
                      (freeOne ? (
                        "No DNS record needed."
                      ) : dm.hostname ? (
                        `Point its DNS to ${ip ?? "the cluster's address"}.`
                      ) : free &&
                        !d.domains.some((x) => isFreeDomain(x.hostname)) ? (
                        <button
                          type="button"
                          onClick={() => setD({ hostname: free })}
                          className="font-medium text-primary hover:text-primary/80"
                        >
                          Use the free domain instead
                        </button>
                      ) : (
                        "Enter your domain, like app.example.com."
                      ))}
                  </span>
                  {tls !== "off" ? (
                    <label className="inline-flex cursor-pointer items-center gap-1.5 text-muted-foreground">
                      <input
                        type="checkbox"
                        checked={dm.redirectHttps}
                        onChange={(e) =>
                          setD({ redirectHttps: e.target.checked })
                        }
                        className="h-3.5 w-3.5 accent-[hsl(var(--primary))]"
                      />
                      Redirect HTTP to HTTPS
                    </label>
                  ) : null}
                </div>
              </li>
            );
          })}
        </ul>
      ) : null}
      {errors.domains ? (
        <p className="text-[11.5px] text-destructive">{errors.domains}</p>
      ) : null}
      <div className="flex flex-wrap gap-2">
        {free && !d.domains.some((x) => x.hostname === free) ? (
          <Button variant="secondary" size="xs" onClick={() => add(free)}>
            <PlusIcon size={12} /> Free domain
          </Button>
        ) : null}
        <Button variant="secondary" size="xs" onClick={() => add()}>
          <PlusIcon size={12} /> Your domain
        </Button>
      </div>
      {free && !d.domains.some((x) => isFreeDomain(x.hostname)) ? (
        <p className="text-[11.5px] text-muted-foreground">
          Free domain, ready instantly:{" "}
          <span className="font-mono text-foreground/85">{free}</span>
        </p>
      ) : null}
    </div>
  );
}

export function NetworkingStep({ d, set, errors }: StepProps) {
  const s = usePyo();
  const t = getTemplate(d.templateId);
  const cluster = s.clusters.find((c) => c.id === d.clusterId);
  const publicPorts = d.ports.filter(
    (p) => p.exposure === "nodeport" || p.exposure === "loadbalancer",
  );
  const project = draftProject(s, d);
  return (
    <div className="space-y-6">
      <section className="space-y-3">
        <p className="text-[12px] text-muted-foreground">
          Every port is reachable inside the cluster at{" "}
          <span className="font-mono text-foreground">
            {d.name || "name"}.{project?.namespace ?? "namespace"}.svc
          </span>
          . Only make a port public if it must be reached directly.
        </p>
        <PortEditor
          rows={d.ports}
          onChange={(ports) => set({ ports })}
          errors={errors}
          cluster={cluster}
        />
        {t?.category === "databases" && publicPorts.length ? (
          <Callout
            tone="warning"
            icon={WarningIcon}
            title="Database exposed outside the cluster"
          >
            Anyone can connect. Keep databases private.
          </Callout>
        ) : null}
        {d.ports.some((p) => p.protocol !== "HTTP") ? (
          <p className="text-[11.5px] text-muted-foreground">
            TCP and UDP need a public port or IP, not a domain.
          </p>
        ) : null}
      </section>
    </div>
  );
}

/** One-click sizes that fill in the compute fields below them (which stay editable). */
const SIZES: {
  label: string;
  detail: string;
  r: Pick<
    Resources,
    "cpuRequest" | "cpuLimit" | "memoryRequest" | "memoryLimit"
  >;
}[] = [
  {
    label: "Small",
    detail: "0.5 vCPU · 256 MiB",
    r: { cpuRequest: 100, cpuLimit: 500, memoryRequest: 128, memoryLimit: 256 },
  },
  {
    label: "Medium",
    detail: "1 vCPU · 512 MiB",
    r: {
      cpuRequest: 250,
      cpuLimit: 1000,
      memoryRequest: 256,
      memoryLimit: 512,
    },
  },
  {
    label: "Large",
    detail: "2 vCPU · 1 GiB",
    r: {
      cpuRequest: 500,
      cpuLimit: 2000,
      memoryRequest: 512,
      memoryLimit: 1024,
    },
  },
  {
    label: "Extra large",
    detail: "4 vCPU · 4 GiB",
    r: {
      cpuRequest: 1000,
      cpuLimit: 4000,
      memoryRequest: 2048,
      memoryLimit: 4096,
    },
  },
];

const sameSize = (r: (typeof SIZES)[number]["r"], v: Resources) =>
  r.cpuRequest === v.cpuRequest &&
  r.cpuLimit === v.cpuLimit &&
  r.memoryRequest === v.memoryRequest &&
  r.memoryLimit === v.memoryLimit;

/** The size a service's resources match ("Small"), if any. */
export const sizeName = (r: Resources) =>
  SIZES.find((x) => sameSize(x.r, r))?.label;

function SizePresets({
  value,
  onChange,
}: {
  value: Resources;
  onChange: (r: Resources) => void;
}) {
  const same = (r: (typeof SIZES)[number]["r"]) =>
    r.cpuRequest === value.cpuRequest &&
    r.cpuLimit === value.cpuLimit &&
    r.memoryRequest === value.memoryRequest &&
    r.memoryLimit === value.memoryLimit;
  return (
    <div
      role="radiogroup"
      aria-label="Size"
      className="grid grid-cols-2 gap-2 sm:grid-cols-4"
    >
      {SIZES.map((x) => {
        const on = same(x.r);
        return (
          <button
            key={x.label}
            type="button"
            role="radio"
            aria-checked={on}
            onClick={() => onChange({ ...value, ...x.r })}
            className={`${optionClass({ selected: on })} px-3 py-2`}
          >
            <span className="block text-[13px] font-medium">{x.label}</span>
            <span className="block text-[11.5px] text-muted-foreground">
              {x.detail}
            </span>
          </button>
        );
      })}
    </div>
  );
}

const usableCluster = (c: { status: string }) =>
  c.status === "connected" || c.status === "degraded";

/** Free share of a server's CPU, or null when it's offline. */
const cpuFree = (n: ClusterServer) =>
  n.status !== "ready" || !n.capacity.cpu
    ? null
    : Math.max(0, 1 - n.usage.cpu / n.capacity.cpu);

/** A server as a row: status, name, place, tags and free CPU. */
function ServerLine({ n }: { n: ClusterServer }) {
  const free = cpuFree(n);
  return (
    <>
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-2 text-[13.5px] font-medium">
          <Dot
            tone={n.status === "ready" ? "success" : "danger"}
            className="h-2 w-2"
          />
          <span className="truncate">{n.name}</span>
          <Tags tags={n.tags} />
        </span>
        <span className="flex items-center gap-1.5 pl-4 text-[11.5px] text-muted-foreground">
          <Flag country={parseLocation(n.location ?? "").country} size={13} />
          {n.location ? locationLabel(n.location) : "Region unknown"}
        </span>
      </span>
      <span className="shrink-0 text-right text-[12px] text-muted-foreground">
        {free === null ? "Offline" : `${Math.round(free * 100)}% CPU free`}
      </span>
    </>
  );
}

/**
 * Placed by tag: pick from the tags clusters and servers already have. A
 * server matches when it, or its cluster, has every picked tag; the cluster
 * follows the matches.
 */
function TagPlacement({
  d,
  set,
}: {
  d: DeployDraft;
  set: (patch: Partial<DeployDraft>) => void;
}) {
  const s = usePyo();
  const clusters = clustersIn(s).filter(usableCluster);
  const nodes = s.servers.filter((n) =>
    clusters.some((c) => c.id === n.clusterId),
  );
  const tagsOf = (n: ClusterServer) => [
    ...(n.tags ?? []),
    ...(clusters.find((c) => c.id === n.clusterId)?.tags ?? []),
  ];
  const serverTags = [...new Set(nodes.flatMap((n) => n.tags ?? []))];
  const clusterTags = [...new Set(clusters.flatMap((c) => c.tags ?? []))];
  const all = [...new Set([...serverTags, ...clusterTags])].sort();
  const picked = d.serverTags;
  const matching = (tags: string[]) =>
    tags.length
      ? nodes.filter((n) => tags.every((t) => tagsOf(n).includes(t)))
      : [];
  const matches = matching(picked);
  const inClusters = clusters.filter((c) =>
    matches.some((n) => n.clusterId === c.id),
  );
  const cluster = inClusters.find((c) => c.id === d.clusterId) ?? inClusters[0];
  const toggle = (tag: string) => {
    const next = picked.includes(tag)
      ? picked.filter((x) => x !== tag)
      : [...picked, tag];
    const cs = clusters.filter((c) =>
      matching(next).some((n) => n.clusterId === c.id),
    );
    set({
      serverTags: next,
      clusterId: cs.some((c) => c.id === d.clusterId)
        ? d.clusterId
        : (cs[0]?.id ?? d.clusterId),
    });
  };
  if (!all.length)
    return (
      <div className="rounded-lg border border-dashed border-border px-4 py-5 text-center text-[12.5px] text-muted-foreground">
        <p className="font-medium text-foreground">No tags yet</p>
        <p className="mt-1">
          Tag clusters or servers first.{" "}
          <Link
            to="/$team/clusters"
            params={{ team: currentSlug() }}
            className="font-medium text-primary hover:text-primary/80"
          >
            Open Clusters
          </Link>
        </p>
      </div>
    );
  return (
    <div className="space-y-3">
      <div role="group" aria-label="Tags" className="flex flex-wrap gap-1.5">
        {all.map((tag) => {
          const on = picked.includes(tag);
          const where = [
            serverTags.includes(tag) ? "servers" : "",
            clusterTags.includes(tag) ? "cluster" : "",
          ]
            .filter(Boolean)
            .join(" · ");
          return (
            <button
              key={tag}
              type="button"
              aria-pressed={on}
              title={`Tag on ${where}`}
              onClick={() => toggle(tag)}
              className={`${optionClass({ selected: on, pill: true })} inline-flex items-center gap-1.5 px-2.5 py-1 font-mono text-[12px] ${on ? "text-foreground" : "text-foreground/85"}`}
            >
              {tag}
              <span className="font-sans text-[11px] text-muted-foreground">
                {matching([tag]).length}
              </span>
            </button>
          );
        })}
      </div>
      {!picked.length ? (
        <p className="text-[12px] text-muted-foreground">
          Runs only on servers with every tag you pick.
        </p>
      ) : !matches.length ? (
        <p className="text-[12px] text-warning">
          No server has all of these tags. Pick fewer.
        </p>
      ) : (
        <>
          {inClusters.length > 1 ? (
            <Select
              value={cluster?.id}
              onChange={(e) => set({ clusterId: e.target.value })}
              aria-label="Cluster"
              heading="Cluster"
            >
              {inClusters.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </Select>
          ) : null}
          <ul className="list-rows overflow-hidden rounded-lg border border-border">
            {matches
              .filter((n) => n.clusterId === cluster?.id)
              .map((n) => (
                <li
                  key={n.id}
                  className="flex items-center gap-3 px-3.5 py-2.5"
                >
                  <ServerLine n={n} />
                </li>
              ))}
          </ul>
          <p className="text-[12px] text-muted-foreground">
            Servers tagged later are used from its next deploy.
          </p>
        </>
      )}
    </div>
  );
}

/** Placed on servers picked by hand, all in one cluster. */
function ServerPlacement({
  d,
  set,
}: {
  d: DeployDraft;
  set: (patch: Partial<DeployDraft>) => void;
}) {
  const s = usePyo();
  const clusters = clustersIn(s).filter(usableCluster);
  const toggle = (n: ClusterServer) => {
    const same = n.clusterId === d.clusterId;
    const picked = same ? d.serverNames : [];
    set({
      clusterId: n.clusterId,
      serverNames: picked.includes(n.name)
        ? picked.filter((x) => x !== n.name)
        : [...picked, n.name],
    });
  };
  return (
    <div className="space-y-3">
      {clusters.map((c) => {
        const nodes = serversOf(s, c.id);
        if (!nodes.length) return null;
        return (
          <div key={c.id}>
            {clusters.length > 1 ? (
              <p className="mb-1.5 font-mono text-[11.5px] text-muted-foreground">
                {c.name}
              </p>
            ) : null}
            <ul className="space-y-2">
              {nodes.map((n) => {
                const on =
                  d.clusterId === c.id && d.serverNames.includes(n.name);
                return (
                  <li key={n.id}>
                    <label
                      className={`${optionClass({ selected: on })} flex cursor-pointer items-center gap-3 px-3.5 py-2.5 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary/20`}
                    >
                      <input
                        type="checkbox"
                        checked={on}
                        onChange={() => toggle(n)}
                      />
                      <ServerLine n={n} />
                    </label>
                  </li>
                );
              })}
            </ul>
          </div>
        );
      })}
      <p className="text-[12px] text-muted-foreground">
        {d.serverNames.length
          ? `Runs only on ${d.serverNames.join(", ")}.${d.replicas > 1 ? ` Its ${d.replicas} replicas are spread across them.` : ""}`
          : "Pick servers from one cluster."}
      </p>
    </div>
  );
}

/** Settings most deploys leave alone, behind a quiet toggle. */
function Advanced({
  open,
  onToggle,
  children,
  title = "Advanced: CPU, memory and health check",
}: {
  open: boolean;
  /** Called with whether it's open now. */
  onToggle: (open: boolean) => void;
  children: ReactNode;
  title?: string;
}) {
  return (
    <div className="space-y-4">
      <button
        type="button"
        onClick={() => onToggle(open)}
        aria-expanded={open}
        className="inline-flex items-center gap-1.5 text-[12.5px] font-medium text-muted-foreground hover:text-foreground"
      >
        <CaretRightIcon
          size={12}
          className={`transition-transform ${open ? "rotate-90" : ""}`}
        />
        {title}
      </button>
      {open ? <div className="space-y-6">{children}</div> : null}
    </div>
  );
}

/** "ghcr.io/owner/app:1.2" or "nginx@sha256:…" → repository (with its
 * registry host) and tag or digest. A bare Docker Hub name gets library/. */
function splitReference(raw: string, host: string) {
  let repo = raw.trim();
  let reference: string | undefined;
  const at = repo.indexOf("@");
  if (at > 0) {
    reference = repo.slice(at + 1);
    repo = repo.slice(0, at);
  } else if (repo.lastIndexOf(":") > repo.lastIndexOf("/")) {
    reference = repo.slice(repo.lastIndexOf(":") + 1);
    repo = repo.slice(0, repo.lastIndexOf(":"));
  }
  const first = repo.split("/")[0];
  const hasHost =
    repo.includes("/") && (/[.:]/.test(first) || first === "localhost");
  let full = hasHost ? repo : `${host}/${repo}`;
  if (imageHost(full) === "docker.io" && full.split("/").length === 2)
    full = `docker.io/library/${full.split("/")[1]}`;
  return { image: full.toLowerCase(), reference };
}

/**
 * The image to run, as one reference: registry (with its logo), repository
 * and tag, like it's written. A full reference pasted anywhere is split up.
 * Under it: whether it pulls with a login, how the tag is pinned, and the
 * image's page.
 */
function ImageReference({
  image,
  reference,
  registries,
  onChange,
  errors,
}: {
  image: string;
  reference: string;
  registries: { id: string; name: string; host: string }[];
  onChange: (image: string, reference: string, registryId?: string) => void;
  errors: Issues;
}) {
  const s = usePyo();
  const host = imageHost(image || "docker.io/x");
  const path = image.startsWith(`${host}/`)
    ? image.slice(host.length + 1).replace(/^library\//, "")
    : image;
  const reg = registries.find((r) => r.host === host);
  const icon = registryIcon(host);
  const login = s.registryLogins.find(
    (l) => l.teamId === s.currentTeamId && l.host === host,
  );
  const page = image ? imagePage(image) : undefined;
  const digest = reference.startsWith("sha256:");
  const setPath = (raw: string) => {
    const parts = splitReference(raw, host);
    const r = registries.find((x) => x.host === imageHost(parts.image));
    onChange(parts.image, parts.reference ?? reference, r?.id);
  };
  const setHost = (h: string, id: string) =>
    onChange(
      `${h}/${h === "docker.io" && !path.includes("/") ? `library/${path}` : path}`,
      reference,
      id,
    );
  const bad = !!(err(errors, "image") || err(errors, "reference"));
  return (
    <div className="space-y-3">
      <div
        className={fieldGroupClass(!!bad)}
      >
        <Menu
          label="Registry"
          align="start"
          items={registries.map((r) => {
            const i = registryIcon(r.host);
            return {
              label: (
                <span className="flex items-center gap-2">
                  {r.name}
                  <span className="font-mono text-[11.5px] text-muted-foreground">
                    {r.host}
                  </span>
                </span>
              ),
              icon: i ? (
                <BrandMark icon={i} size={14} />
              ) : (
                <PackageIcon size={14} />
              ),
              onSelect: () => setHost(r.host, r.id),
            };
          })}
          trigger={(tp) => (
            <button
              {...tp}
              type="button"
              title={reg?.name ?? host}
              className="flex shrink-0 items-center gap-2 border-r border-border px-3 text-[12.5px] transition-colors hover:bg-accent/50"
            >
              {icon ? (
                <BrandMark icon={icon} size={16} />
              ) : (
                <PackageIcon size={16} />
              )}
              <span className="hidden font-mono text-muted-foreground sm:inline">
                {host}
              </span>
              <CaretDownIcon size={11} className="text-muted-foreground" />
            </button>
          )}
        />
        <input
          aria-label="Repository"
          value={path}
          onChange={(e) => setPath(e.target.value)}
          placeholder={
            host === "docker.io" ? "nginx or owner/app" : "owner/app"
          }
          spellCheck={false}
          className={`${groupInputClass} flex-1 font-mono`}
        />
        <span
          aria-hidden
          className="flex items-center font-mono text-[15px] text-muted-foreground"
        >
          {digest ? "@" : ":"}
        </span>
        <input
          aria-label="Tag or digest"
          value={reference}
          onChange={(e) => onChange(image, e.target.value.trim())}
          placeholder="latest"
          spellCheck={false}
          className={`${groupInputClass} w-28 shrink-0 font-mono sm:w-36`}
        />
      </div>
      {bad ? (
        <p className="text-[11.5px] text-destructive" data-error>
          {err(errors, "image") ?? err(errors, "reference")}
        </p>
      ) : null}
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 text-[12px] text-muted-foreground">
        {login ? (
          <span className="inline-flex items-center gap-1.5">
            <LockSimpleIcon
              size={13}
              className={login.verified ? "text-success" : ""}
            />
            Pulls as{" "}
            <span className="font-mono text-foreground/85">
              {login.username}
            </span>
          </span>
        ) : (
          <span className="inline-flex items-center gap-1.5">
            <GlobeHemisphereEastIcon size={13} />
            Public image.{" "}
            <AppLink
              href="/integrations"
              className="font-medium text-primary hover:text-primary/80"
            >
              Private? Add a login
            </AppLink>
          </span>
        )}
        <span className="inline-flex items-center gap-1.5">
          <PushPinIcon
            size={13}
            className={reference === "latest" ? "text-warning" : ""}
          />
          {digest ? (
            "Pinned to this digest"
          ) : reference === "latest" ? (
            <span className="text-warning">
              latest can change between rollbacks
            </span>
          ) : (
            "Pinned on deploy"
          )}
        </span>
        {page ? (
          <a
            href={page}
            target="_blank"
            rel="noreferrer"
            className="ml-auto inline-flex items-center gap-1 font-medium text-foreground/80 hover:text-foreground"
          >
            View on {reg?.name ?? host} <ArrowUpRightIcon size={12} />
          </a>
        ) : null}
      </div>
    </div>
  );
}

/** Whether it runs continuously or on a schedule, and when. */
const CRON_PRESETS: [string, string][] = [
  ["*/15 * * * *", "Every 15 min"],
  ["0 * * * *", "Hourly"],
  ["0 3 * * *", "Daily 03:00"],
  ["0 9 * * 1", "Mondays 09:00"],
];

/** A cron job's fields, shown as soon as it is one (the block adds and removes it). */
export function ScheduleStep({ d, set, errors }: StepProps) {
  const words = describeCron(d.schedule);
  return (
    <div className="space-y-4">
      <p className="text-[12px] text-muted-foreground">
        A cron job runs, finishes and stops on its schedule. It doesn&apos;t serve traffic.
      </p>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field
          label="Cron schedule"
          hint={words ? `${words}.` : "Five fields, in UTC: minute, hour, day, month, weekday."}
          error={err(errors, "schedule")}
        >
          {(p) => (
            <Input
              {...p}
              mono
              value={d.schedule}
              onChange={(e) => set({ schedule: e.target.value })}
              placeholder="0 3 * * *"
            />
          )}
        </Field>
        <Field label="Command" optional hint="Runs the image's default if empty.">
          {(p) => (
            <Input
              {...p}
              mono
              value={d.command}
              onChange={(e) => set({ command: e.target.value })}
              placeholder="npm run cleanup"
            />
          )}
        </Field>
      </div>
      <div className="flex flex-wrap gap-1.5" role="group" aria-label="Common schedules">
        {CRON_PRESETS.map(([expr, label]) => (
          <button
            key={expr}
            type="button"
            onClick={() => set({ schedule: expr })}
            aria-pressed={d.schedule.trim() === expr}
            className={`${optionClass({ selected: d.schedule.trim() === expr, pill: true })} px-2.5 py-1 text-[12px] font-medium`}
          >
            {label}
          </button>
        ))}
      </div>
    </div>
  );
}
