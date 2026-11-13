import { LearnMore } from "~/components/ui/LearnMore";
import { MenuPath } from "~/components/ui/MenuPath";
import {
  ArchiveIcon,
  CubeIcon,
  DatabaseIcon,
  KeyIcon,
  LockKeyIcon,
  ShieldCheckIcon,
  SwapIcon,
  TreeStructureIcon,
  UserGearIcon,
  VaultIcon,
  type Icon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { Badge, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Switch } from "~/components/ui/Field";
import { Page, PageHeader, Panel } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";
import { dateTime, plural, timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import type { AgentScope, Cluster, ClusterSecurity } from "~/lib/pyo/types";

// The team's security at a glance: each protection, whether it's in place,
// and what to do when it isn't. Clusters report theirs through the agent.
export const Route = createFileRoute("/_app/$team/security")({
  head: () => ({ meta: seo({ title: "Security" }) }),
  component: () => (
    <Page width="max-w-4xl">
      <PageHeader title="Security" />
      <Security />
    </Page>
  ),
});

type Check = {
  icon: Icon;
  title: string;
  tone: Tone;
  status: string;
  detail: ReactNode;
  action?: ReactNode;
};

function Row({ c }: { c: Check }) {
  const Ico = c.icon;
  return (
    <li className="flex items-start gap-3.5 px-5 py-4">
      <span
        className={`mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-border ${TILE[c.tone]}`}
      >
        <Ico size={15} />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex items-start justify-between gap-3">
          <p className="pt-1 text-[13.5px] font-medium">{c.title}</p>
          <Badge tone={c.tone} className="mt-0.5 shrink-0">
            {c.status}
          </Badge>
        </div>
        <div className="mt-1 max-w-[60ch] text-[12.5px] leading-5 text-muted-foreground">
          {c.detail}
        </div>
        {c.action ? <div className="mt-2.5">{c.action}</div> : null}
      </div>
    </li>
  );
}

/** The icon tile takes the check's state, quietly. */
const TILE: Record<Tone, string> = {
  success: "bg-success/10 text-success",
  warning: "bg-warning/10 text-warning",
  danger: "bg-destructive/10 text-destructive",
  neutral: "bg-muted/60 text-muted-foreground",
  info: "bg-muted/60 text-muted-foreground",
  progress: "bg-muted/60 text-muted-foreground",
};

const Code = ({ children }: { children: ReactNode }) => (
  <code className="rounded bg-muted px-1 py-px font-mono text-[11.5px] text-foreground/85">
    {children}
  </code>
);

/** One cluster's datastore encryption, as its agent reports it. */
function datastore(sec: ClusterSecurity | undefined): {
  tone: Tone;
  label: string;
} {
  if (!sec || sec.secretsEncryption === null)
    return { tone: "neutral", label: "Unknown" };
  if (sec.secretsEncryption === "secretbox")
    return { tone: "success", label: "Encrypted" };
  if (sec.secretsEncryption) return { tone: "warning", label: "Older encryption" };
  return { tone: "danger", label: "Off" };
}

/** One cluster's agent: what it may do there. */
function agentAccess(sc: AgentScope | null | undefined): { tone: Tone; label: string } {
  if (!sc) return { tone: "neutral", label: "Unknown" };
  if (sc.access === "full") return { tone: "warning", label: "Full access" };
  if (sc.admission !== "enforced") return { tone: "warning", label: "Not enforced" };
  return { tone: "success", label: "Limited" };
}

/** The switches the cluster's owner turned on, in words. */
function agentSwitches(sc: AgentScope): string[] {
  return [
    sc.policy.readOnly ? "read only" : "",
    sc.policy.noTerminal ? "terminals off" : "",
    sc.policy.noForceRemove ? "no forced server removal" : "",
    sc.servers ? "" : "can't manage servers",
  ].filter(Boolean);
}

function Security() {
  const s = usePyo();
  const [rotating, setRotating] = useState(false);
  const teamId = s.currentTeamId;
  const role = s.members.find(
    (m) => m.teamId === teamId && m.email === s.user.email,
  )?.role;
  const admin = role === "owner" || role === "admin";
  const clusters = s.clusters.filter((c) => c.teamId === teamId);
  const projects = s.projects.filter((p) => p.teamId === teamId);
  const enc = s.encryption;
  const key = enc?.teams.find((t) => t.teamId === teamId);
  const reports = clusters.map((c) => ({ c, sec: c.capabilities.security }));
  const sum = (f: (x: ClusterSecurity) => number) =>
    reports.reduce((n, r) => n + (r.sec ? f(r.sec) : 0), 0);
  const namespaces = sum((x) => x.namespaces);
  const isolated = projects.filter((p) => p.networkIsolation).length;

  const perCluster = (
    render: (c: Cluster, sec?: ClusterSecurity) => ReactNode,
  ) =>
    reports.length ? (
      <ul className="mt-1.5 space-y-1">
        {reports.map(({ c, sec }) => (
          <li key={c.id} className="flex items-center gap-2">
            <TreeStructureIcon size={12} className="shrink-0" />
            <span className="font-medium text-foreground/85">{c.name}</span>
            {render(c, sec)}
          </li>
        ))}
      </ul>
    ) : null;

  const datastoreStates = reports.map((r) => datastore(r.sec));
  const allSecretbox =
    reports.length > 0 && datastoreStates.every((d) => d.label === "Encrypted");
  const anyOff = datastoreStates.some((d) => d.label === "Off");

  const checks: Check[] = [
    {
      icon: DatabaseIcon,
      title: "Secrets on your servers",
      tone: !reports.length
        ? "neutral"
        : allSecretbox
          ? "success"
          : anyOff
            ? "danger"
            : "warning",
      status: !reports.length
        ? "No clusters"
        : allSecretbox
          ? "Enabled"
          : anyOff
            ? "Off on some clusters"
            : "Check clusters",
      detail: (
        <>
          Secrets encrypted on disk.
          {perCluster((_, sec) => {
            const d = datastore(sec);
            return (
              <>
                <Badge tone={d.tone}>{d.label}</Badge>
                {d.label === "Off" ? (
                  <span>
                    On the server: <Code>k3s secrets-encrypt enable</Code>,
                    restart k3s, then <Code>k3s secrets-encrypt reencrypt</Code>
                  </span>
                ) : d.label === "Unknown" ? (
                  <span>Not reported yet.</span>
                ) : null}
              </>
            );
          })}
        </>
      ),
    },
    {
      icon: LockKeyIcon,
      title: "Environment variables",
      tone: enc?.enabled ? "success" : "danger",
      status: enc?.enabled ? "Encrypted" : "Not encrypted",
      detail: enc?.enabled ? (
        <>
          Sealed with {enc.cipher} and this team's key.
        </>
      ) : (
        <>
          Set <Code>PYO_SECRET_KEY</Code> on the API. <LearnMore page="encryption" />
        </>
      ),
    },
    {
      icon: KeyIcon,
      title: "Team key",
      tone: key ? "success" : "neutral",
      status: key
        ? timeAgo(Math.max(key.keyCreatedAt, key.keyRewrappedAt ?? 0))
        : "No key yet",
      detail: key ? (
        <>
          Made {dateTime(key.keyCreatedAt)}
          {key.keyRewrappedAt ? `, re-wrapped ${dateTime(key.keyRewrappedAt)}` : ""}.
        </>
      ) : (
        "Made on the first saved variable."
      ),
      action:
        admin && key ? (
          <Button
            variant="secondary"
            size="xs"
            onClick={() => setRotating(true)}
          >
            Rotate key
          </Button>
        ) : undefined,
    },
    {
      icon: VaultIcon,
      title: "Key storage",
      tone: enc?.provider === "openbao" ? "success" : "neutral",
      status: enc?.provider === "openbao" ? "OpenBao" : "On this server",
      detail:
        enc?.provider === "openbao" ? (
          <>
            Wrapped by <Code>{enc.kekId}</Code>.
          </>
        ) : (
          <>
            Local master key. <LearnMore page="encryption" />
          </>
        ),
    },
    {
      icon: CubeIcon,
      title: "Service sandboxing",
      tone: !namespaces
        ? "neutral"
        : sum((x) => x.podSecurityEnforced) === namespaces
          ? "success"
          : "warning",
      status: namespaces
        ? `${sum((x) => x.podSecurityEnforced)} of ${plural(namespaces, "project")}`
        : "No projects deployed",
      detail: (
        <>
          No privileged services.
          {namespaces ? (
            <>
              {" "}
              {plural(
                sum((x) => x.podSecurityRestricted),
                "project",
              )}{" "}
              also meet the strictest level.
            </>
          ) : null}
          {perCluster((_, sec) =>
            sec ? (
              <span>
                {sec.podSecurityEnforced} of {sec.namespaces} sandboxed
              </span>
            ) : (
              <span>Not reported yet.</span>
            ),
          )}
        </>
      ),
    },
    {
      icon: TreeStructureIcon,
      title: "Network isolation",
      tone: !projects.length
        ? "neutral"
        : isolated === projects.length
          ? "success"
          : "warning",
      status: projects.length
        ? `${isolated} of ${plural(projects.length, "project")}`
        : "No projects",
      detail:
        "Projects start isolated.",
    },
    {
      icon: ArchiveIcon,
      title: "Backups of Pyo",
      tone: s.controlBackup?.settings ? "success" : "neutral",
      status: s.controlBackup?.settings
        ? "Enabled"
        : s.user.isAdmin
          ? "Not set up"
          : "Admin's setting",
      detail: s.controlBackup?.settings
        ? "Backed up, encrypted."
        : (
          <>
            Set up in <MenuPath steps={["Settings", "Backups"]} />.
          </>
        ),
    },
    (() => {
      const states = clusters.map((c) => agentAccess(c.capabilities.agentScope));
      const known = states.filter((a) => a.label !== "Unknown");
      const full = states.some((a) => a.label === "Full access");
      const loose = states.some((a) => a.label === "Not enforced");
      return {
        icon: UserGearIcon,
        title: "Agent access",
        tone: !clusters.length || !known.length
          ? "neutral"
          : full || loose
            ? "warning"
            : known.length === states.length
              ? "success"
              : "neutral",
        status: !clusters.length
          ? "No clusters"
          : !known.length
            ? "Not reported yet"
            : full
              ? "Full access"
              : loose
                ? "Not enforced"
                : known.length === states.length
                  ? "Limited"
                  : "Check clusters",
        detail: (
          <>
            {full
              ? "Full access: the agent can change anything on the cluster."
              : "Limited: the agent changes only what Pyo manages."}
            {perCluster((c) => {
              const sc = c.capabilities.agentScope;
              const a = agentAccess(sc);
              const on = sc ? agentSwitches(sc) : [];
              return (
                <>
                  <Badge tone={a.tone}>{a.label}</Badge>
                  {!sc ? (
                    <span>Not reported yet.</span>
                  ) : sc.access === "full" ? (
                    <span>It can change anything on this cluster.</span>
                  ) : sc.admission === "unsupported" ? (
                    <span>This cluster can't hold it to its limits.</span>
                  ) : sc.admission === "missing" ? (
                    <span>Its limits aren't enforced. Run the update command in its settings.</span>
                  ) : null}
                  {on.length ? <span>Your policy: {on.join(", ")}.</span> : null}
                </>
              );
            })}
          </>
        ),
      } satisfies Check;
    })(),
    (() => {
      const states = clusters.map((c) => {
        const p = c.capabilities.agentScope?.proxy;
        if (!p) return { tone: "neutral" as Tone, label: "Unknown" };
        return p.controllerReadsOtherSecrets || p.proxiesHaveCredentials
          ? { tone: "warning" as Tone, label: "Can read secrets" }
          : { tone: "success" as Tone, label: "Isolated" };
      });
      const known = states.filter((x) => x.label !== "Unknown");
      const exposed = states.some((x) => x.tone === "warning");
      return {
        icon: SwapIcon,
        title: "Gateway can't read your secrets",
        tone: !known.length ? "neutral" : exposed ? "warning" : known.length === states.length ? "success" : "neutral",
        status: !clusters.length
          ? "No clusters"
          : !known.length
            ? "Not reported yet"
            : exposed
              ? "Check clusters"
              : known.length === states.length
                ? "Isolated"
                : "Check clusters",
        detail: (
          <>
            It holds no credentials.
            {perCluster((c) => {
              const p = c.capabilities.agentScope?.proxy;
              const i = clusters.indexOf(c);
              const st = states[i];
              return (
                <>
                  <Badge tone={st.tone}>{st.label}</Badge>
                  {!p ? (
                    <span>Not reported yet.</span>
                  ) : p.controllerReadsOtherSecrets ? (
                    <span>Can read other secrets.</span>
                  ) : p.proxiesHaveCredentials ? (
                    <span>Carries credentials.</span>
                  ) : null}
                </>
              );
            })}
          </>
        ),
      } satisfies Check;
    })(),
  ];

  const good = checks.filter((c) => c.tone === "success").length;
  const byTitle = (t: string) => checks.find((c) => c.title === t)!;
  const groups: { title: string; description: string; items: string[] }[] = [
    {
      title: "Encryption",
      description: "How secrets and variables are kept.",
      items: ["Secrets on your servers", "Environment variables", "Team key", "Key storage"],
    },
    {
      title: "Services",
      description: "What running services can reach.",
      items: ["Service sandboxing", "Network isolation"],
    },
    {
      title: "Pyo",
      description: "Pyo's own data and access.",
      items: ["Backups of Pyo", "Agent access", "Gateway can't read your secrets"],
    },
  ];
  const issues = checks.filter((c) => c.tone === "warning" || c.tone === "danger").length;

  return (
    <div className="space-y-5">
      <Panel>
        <div className="flex items-center gap-4 px-5 py-4">
          <span
            className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border border-border ${issues ? TILE.warning : TILE.success}`}
          >
            <ShieldCheckIcon size={18} />
          </span>
          <div className="min-w-0 flex-1">
            <p className="text-[14px] font-semibold">
              {good} of {checks.length} protections in place
            </p>
            <p className="text-[12.5px] text-muted-foreground">
              {issues
                ? `${plural(issues, "item")} to look at below.`
                : "Everything Pyo checks is in place."}
            </p>
          </div>
          <div className="hidden w-40 shrink-0 sm:block" aria-hidden>
            <div className="flex h-1.5 gap-0.5 overflow-hidden rounded-full">
              {checks.map((c) => (
                <span
                  key={c.title}
                  className={`flex-1 ${c.tone === "success" ? "bg-success" : c.tone === "danger" ? "bg-destructive" : c.tone === "warning" ? "bg-warning" : "bg-border-strong"}`}
                />
              ))}
            </div>
          </div>
        </div>
      </Panel>

      {groups.map((g) => (
        <Panel key={g.title} title={g.title} description={g.description}>
          <ul className="list-rows">
            {g.items.map((t) => (
              <Row key={t} c={byTitle(t)} />
            ))}
          </ul>
          {g.title === "Services" && projects.length ? (
            <div className="border-t border-border px-5 py-4">
              <p className="mb-2.5 text-[12px] font-medium text-muted-foreground">
                Isolation per project
              </p>
              <ul className="space-y-3">
                {projects.map((p) => (
                  <li key={p.id}>
                    <Switch
                      checked={!!p.networkIsolation}
                      disabled={!admin}
                      onChange={(on) => void api.setProjectIsolation(p.id, on)}
                      label={p.name}
                      description={
                        p.networkIsolation
                          ? "Only its own services and public ports reach it."
                          : "Any service on the cluster can reach it."
                      }
                    />
                  </li>
                ))}
              </ul>
            </div>
          ) : null}
        </Panel>
      ))}

      <ConfirmDialog
        open={rotating}
        onClose={() => setRotating(false)}
        tone="primary"
        title="Rotate this team's encryption key?"
        description="Running services aren't affected."
        confirmLabel="Rotate key"
        onConfirm={() => void api.rotateDataKey(teamId)}
      />
    </div>
  );
}
