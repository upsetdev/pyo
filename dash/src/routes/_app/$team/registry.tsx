import { TrashIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { TimeAgo } from "~/components/pyo/common";
import { AppLink } from "~/components/ui/AppLink";
import { Status } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented, Select } from "~/components/ui/Field";
import { Page, PageHeader, Panel, Skeleton } from "~/components/ui/Panel";
import { CopyButton, Table, Td, Th, Tr } from "~/components/ui/Table";
import { fetchRegistry, type RegistryInfo, type RegistryToken } from "~/lib/pyo/backend";
import { mem, plural } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import { api, currentTeam, servicePath, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Team } from "~/lib/pyo/types";

const TAILSCALE_DOCS = "https://tailscale.com/kb/1017/install";
import { seo } from "~/lib/seo";
import { MenuPath } from "~/components/ui/MenuPath";

// Pyo registry: where the images Pyo builds from Git are kept, the team's
// repositories in it, and how long they're kept. Read only, but for cleanup.
export const Route = createFileRoute("/_app/$team/registry")({
  head: () => ({ meta: seo({ title: "Registry" }) }),
  component: Registry,
});

const bytes = (n: number) => mem(n / 1048576);

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <li className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 px-4 py-3 text-[13px]">
      <span className="text-muted-foreground">{label}</span>
      <span className="min-w-0">{children}</span>
    </li>
  );
}

function Registry() {
  const s = usePyo();
  const team = currentTeam(s);
  const q = useQuery(`registry:${s.currentTeamId}`, () => fetchRegistry(s.currentTeamId), { refreshMs: 60_000 });
  const data = q.status === "success" ? q.data : null;
  const cluster = data?.location ? s.clusters.find((c) => c.id === data.location?.clusterId) : undefined;
  const serviceName = (id: string | null) => (id ? s.services.find((a) => a.id === id)?.name : undefined);
  const me = s.members.find((m) => m.teamId === s.currentTeamId && m.email === s.user.email);
  const admin = !me || me.role === "owner" || me.role === "admin";
  const [connecting, setConnecting] = useState(false);
  return (
    <Page>
      <PageHeader
        title="Registry"
        description="Pyo registry keeps the images Pyo builds from Git, and the ones your team pushes."
        actions={
          admin && data?.teamPath ? (
            <Button size="sm" variant="secondary" onClick={() => setConnecting(true)}>
              Connect
            </Button>
          ) : undefined
        }
      />
      {data?.teamPath && team ? <ConnectDialog open={connecting} onClose={() => setConnecting(false)} data={data} teamId={team.id} /> : null}
      <div className="space-y-6">
        <Panel title="Pyo registry">
          {q.status === "loading" ? (
            <div className="p-4">
              <Skeleton className="h-24 w-full" />
            </div>
          ) : !data ? (
            <p className="px-4 py-6 text-center text-[12.5px] text-muted-foreground">Couldn't load the registry's details.</p>
          ) : (
            <ul className="list-rows">
              {data.pushAddress ? (
                <Row label="Address">
                  <span className="flex items-center gap-2">
                    <code className="truncate font-mono text-[12.5px]">{data.pushAddress}</code>
                    <CopyButton value={data.pushAddress} label="Copy address" />
                  </span>
                </Row>
              ) : (
                <li className="px-4 py-3 text-[13px]">
                  <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1">
                    <span className="text-muted-foreground">Address</span>
                    <span className="flex min-w-0 items-center gap-2">
                      <code className="truncate font-mono text-[12.5px]">{data.address}</code>
                      <span className="shrink-0 text-[12px] text-muted-foreground">Inside the cluster only</span>
                    </span>
                  </div>
                  <p className="mt-1 text-[12px] text-muted-foreground">
                    Pushing your own images needs{" "}
                    <a href={TAILSCALE_DOCS} target="_blank" rel="noreferrer" className="font-medium text-primary hover:text-primary/80">
                      Tailscale
                    </a>
                    .
                  </p>
                </li>
              )}
              {cluster ? (
                <Row label="Runs on">
                  <AppLink href={`/clusters/${encodeURIComponent(cluster.name)}`} className="hover:text-primary">
                    {cluster.name}
                  </AppLink>
                  {data.location?.server ? <span className="text-muted-foreground"> · {data.location.server}</span> : null}
                </Row>
              ) : null}
              <Row label="Status">
                <Status tone={data.reachable ? "success" : "danger"} label={data.reachable ? "Reachable" : "Not reachable"} />
              </Row>
              <Row label="Space used">{data.usedBytes != null ? bytes(data.usedBytes) : "—"}</Row>
            </ul>
          )}
        </Panel>

        <Panel
          title="Repositories"
          actions={
            <AppLink href="/images" className="text-[12.5px] text-muted-foreground hover:text-foreground">
              All images
            </AppLink>
          }
        >
          {!data ? (
            q.status === "loading" ? (
              <div className="p-4">
                <Skeleton className="h-16 w-full" />
              </div>
            ) : (
              <p className="px-4 py-6 text-center text-[12.5px] text-muted-foreground">Couldn't load the repositories.</p>
            )
          ) : data.repositories.length ? (
            <Table>
              <thead>
                <tr>
                  <Th>Image</Th>
                  <Th className="text-right">Tags</Th>
                  <Th className="text-right">Size</Th>
                  <Th>Last build</Th>
                </tr>
              </thead>
              <tbody>
                {data.repositories.map((r) => (
                  <Tr key={r.repository}>
                    <Td>
                      <span className="block truncate font-medium">
                        {r.kind === "pushed" ? (
                          <>
                            {r.repository.split("/").pop()} <span className="font-normal text-muted-foreground">· pushed</span>
                          </>
                        ) : r.serviceId && serviceName(r.serviceId) ? (
                          <AppLink href={servicePath(r.serviceId)} className="hover:text-primary">
                            {serviceName(r.serviceId)}
                          </AppLink>
                        ) : (
                          r.serviceId
                        )}
                      </span>
                      {/* Pushed: the name that works from the tailnet. Built:
                          none, as only servers can pull those. */}
                      {r.kind === "pushed" ? (
                        r.tailnetRepository ? (
                          <span className="flex min-w-0 items-center gap-1">
                            <span className="truncate font-mono text-[11.5px] text-muted-foreground">{r.tailnetRepository}</span>
                            <CopyButton value={r.tailnetRepository} label="Copy name" />
                          </span>
                        ) : (
                          <span className="block truncate font-mono text-[11.5px] text-muted-foreground">
                            {r.repository.startsWith(`${data.address}/`) ? r.repository.slice(data.address.length + 1) : r.repository}
                          </span>
                        )
                      ) : null}
                    </Td>
                    <Td className="text-right tabular-nums">{r.tags}</Td>
                    <Td className="text-right tabular-nums text-muted-foreground">{bytes(r.bytes)}</Td>
                    <Td className="text-muted-foreground">{r.lastPushAt ? <TimeAgo at={r.lastPushAt} /> : "—"}</Td>
                  </Tr>
                ))}
              </tbody>
            </Table>
          ) : (
            <p className="px-4 py-6 text-center text-[12.5px] text-muted-foreground">No images yet.</p>
          )}
        </Panel>

        {data?.tokens ? <Tokens tokens={data.tokens} admin={admin} onCreate={() => setConnecting(true)} /> : null}

        {team ? <Cleanup key={team.id} team={team} lastCleanupAt={data?.lastCleanupAt ?? null} /> : null}
      </div>
    </Page>
  );
}

/** How many deployments' images are kept per service (the team's setting). */
function Cleanup({ team, lastCleanupAt }: { team: Team; lastCleanupAt: number | null }) {
  const [keep, setKeep] = useState(team.preferences.retainDeployments);
  const valid = Number.isInteger(keep) && keep >= 5 && keep <= 100;
  return (
    <Panel title="Cleanup" description="After each deploy, older images Pyo built are removed. Pushed images are never removed.">
      <div className="flex flex-wrap items-end gap-3 p-4">
        <Field label="Deployments to keep per service" className="max-w-[220px]" error={valid ? undefined : "5 to 100."}>
          {(p) => <Input {...p} type="number" min={5} max={100} value={keep} onChange={(e) => setKeep(Number(e.target.value))} />}
        </Field>
        <Button
          size="sm"
          disabled={!valid || keep === team.preferences.retainDeployments}
          onClick={async () => {
            const res = await api.updateTeam(team.id, { preferences: { ...team.preferences, retainDeployments: keep } });
            if (res) toast({ title: "Saved", description: `Keeping ${plural(keep, "deployment")} per service.` });
          }}
        >
          Save
        </Button>
      </div>
      {lastCleanupAt ? (
        <p className="border-t border-border px-4 py-3 text-[12.5px] text-muted-foreground">
          Last cleanup <TimeAgo at={lastCleanupAt} />.
        </p>
      ) : null}
    </Panel>
  );
}

const expiryLabel = (t: RegistryToken) => (t.expiresAt ? (t.expiresAt < Date.now() ? "expired" : null) : "never expires");

/** The team's registry tokens: name, scope, when made, last used, expiry. */
function Tokens({ tokens, admin, onCreate }: { tokens: RegistryToken[]; admin: boolean; onCreate: () => void }) {
  const [revoking, setRevoking] = useState<RegistryToken | null>(null);
  return (
    <Panel
      title="Tokens"
      actions={
        admin ? (
          <Button size="xs" variant="secondary" onClick={onCreate}>
            New token
          </Button>
        ) : undefined
      }
    >
      {tokens.length ? (
        <ul className="list-rows">
          {tokens.map((t) => (
            <li key={t.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-3 text-[12.5px]">
              <div className="min-w-0 flex-1">
                <p className="text-[13px] font-medium">{t.name}</p>
                <p className="text-muted-foreground">
                  {t.scope === "push" ? "Push and pull" : "Pull"} · made <TimeAgo at={t.createdAt} />
                  {t.createdBy ? ` by ${t.createdBy}` : ""} · {t.lastUsedAt ? <>used <TimeAgo at={t.lastUsedAt} /></> : "not used yet"} ·{" "}
                  {expiryLabel(t) ?? <>expires <TimeAgo at={t.expiresAt!} /></>}
                </p>
              </div>
              {admin ? (
                <Button variant="ghost" size="icon" aria-label={`Revoke ${t.name}`} onClick={() => setRevoking(t)}>
                  <TrashIcon size={14} />
                </Button>
              ) : null}
            </li>
          ))}
        </ul>
      ) : (
        <p className="px-4 py-6 text-center text-[12.5px] text-muted-foreground">No tokens.</p>
      )}
      <ConfirmDialog
        open={!!revoking}
        onClose={() => setRevoking(null)}
        title={`Revoke ${revoking?.name}?`}
        description="Logins with it stop working."
        confirmLabel="Revoke token"
        onConfirm={async () => {
          if (revoking && (await api.revokeRegistryToken(revoking.id))) toast({ title: `Revoked ${revoking.name}` });
        }}
      />
    </Panel>
  );
}

function Command({ text }: { text: string }) {
  return (
    <div className="flex items-start gap-2 rounded-lg border border-border bg-muted/40 px-3 py-2">
      <code className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[12px]">{text}</code>
      <CopyButton value={text} label="Copy" />
    </div>
  );
}

/** Push the team's own images: a new token, then docker login, tag and push. */
function ConnectDialog({ open, onClose, data, teamId }: { open: boolean; onClose: () => void; data: RegistryInfo; teamId: string }) {
  const [name, setName] = useState("");
  const [scope, setScope] = useState<"push" | "pull">("push");
  const [expiry, setExpiry] = useState<"30" | "90" | "never">("30");
  const [made, setMade] = useState<{ token: string; username: string } | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  // Which service it's for: the path is <team>/<project>/<service>.
  const s = usePyo();
  const projects = s.projects.filter((p) => p.teamId === teamId);
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  const services = s.services.filter((a) => a.projectId === projectId);
  const [serviceId, setServiceId] = useState("");
  const service = services.find((a) => a.id === serviceId) ?? services[0];
  const project = projects.find((p) => p.id === projectId);
  const close = () => {
    setMade(null);
    setName("");
    setError("");
    onClose();
  };
  const addr = data.pushAddress;
  if (!addr)
    return (
      <Dialog open={open} onClose={close} title="Connect" footer={<Button onClick={close}>Close</Button>}>
        <p className="text-[13px] text-muted-foreground">
          Pushing works from your tailnet over HTTPS, and this Pyo isn't there yet. Put its machine on{" "}
          <a href={TAILSCALE_DOCS} target="_blank" rel="noreferrer" className="font-medium text-primary hover:text-primary/80">
            Tailscale
          </a>
          , turn on MagicDNS and HTTPS Certificates (<MenuPath steps={["Tailscale admin console", "DNS"]} />), and run Pyo's setup again.
        </p>
      </Dialog>
    );
  const image = `${addr}/${data.teamPath}/${project?.namespace ?? "<project>"}/${service?.name ?? "<service>"}:<tag>`;
  return (
    <Dialog
      open={open}
      onClose={close}
      title="Connect"
      description="From a device on your team's tailnet."
      width="max-w-xl"
      footer={
        made ? (
          <Button onClick={close}>Done</Button>
        ) : (
          <>
            <Button variant="ghost" onClick={close}>
              Cancel
            </Button>
            <Button
              disabled={!name.trim() || !service || saving}
              onClick={async () => {
                setSaving(true);
                const res = await api.createRegistryToken(teamId, { name: name.trim(), scope, expiresInDays: expiry === "never" ? null : (Number(expiry) as 30 | 90) });
                setSaving(false);
                if ("error" in res) setError(res.error);
                else setMade({ token: res.data.token, username: res.data.username });
              }}
            >
              Create token
            </Button>
          </>
        )
      }
    >
      {made ? (
        <div className="space-y-3 text-[12.5px]">
          <p className="text-muted-foreground">The token is shown once. Log in, tag and push:</p>
          <Command text={`echo ${made.token} | docker login ${addr} -u ${made.username} --password-stdin`} />
          <Command text={`docker tag <image> ${image}`} />
          <Command text={`docker push ${image}`} />
          <p className="text-muted-foreground">Then deploy {service?.name ?? "the service"} with that image.</p>
        </div>
      ) : (
        <div className="space-y-4">
          <p className="flex items-center gap-2 text-[12.5px]">
            <span className="text-muted-foreground">Address</span>
            <code className="font-mono">{addr}</code>
            <CopyButton value={addr} label="Copy address" />
          </p>
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Project">
              {(p) => (
                <Select
                  {...p}
                  value={projectId}
                  onChange={(e) => {
                    setProjectId(e.target.value);
                    setServiceId("");
                  }}
                >
                  {projects.map((x) => (
                    <option key={x.id} value={x.id}>
                      {x.name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
            <Field label="Service">
              {(p) => (
                <Select {...p} value={service?.id ?? ""} onChange={(e) => setServiceId(e.target.value)}>
                  {services.map((x) => (
                    <option key={x.id} value={x.id}>
                      {x.name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          </div>
          <Field label="Token name" error={error || undefined}>
            {(p) => <Input {...p} value={name} placeholder="Laptop" onChange={(e) => setName(e.target.value)} />}
          </Field>
          <div className="flex flex-wrap gap-6">
            <Field label="Can">
              {() => (
                <Segmented
                  label="Can"
                  size="md"
                  value={scope}
                  onChange={setScope}
                  options={[
                    { value: "push", label: "Push and pull" },
                    { value: "pull", label: "Pull" },
                  ]}
                />
              )}
            </Field>
            <Field label="Expires">
              {() => (
                <Segmented
                  label="Expires"
                  size="xs"
                  value={expiry}
                  onChange={setExpiry}
                  options={[
                    { value: "30", label: "30 days" },
                    { value: "90", label: "90 days" },
                    { value: "never", label: "Never" },
                  ]}
                />
              )}
            </Field>
          </div>
        </div>
      )}
    </Dialog>
  );
}
