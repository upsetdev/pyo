import {
  DotsThreeIcon,
  GlobeIcon,
  PencilSimpleIcon,
  PlusIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AddDomainDialog } from "~/components/pyo/AddDomainDialog";
import { tlsLine } from "~/components/pyo/Certificates";
import { DeleteRecordSwitch, DnsRecordLine } from "~/components/pyo/Cloudflare";
import { DomainKind, DomainMark, DomainStatus } from "~/components/pyo/DomainState";
import {
  HeadersDialog,
  onlyRedirects,
  redirectTarget,
  RedirectsDialog,
} from "~/components/pyo/DomainRules";
import { RestoreRecordSwitch } from "~/components/pyo/TunnelRecord";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Menu } from "~/components/ui/Menu";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { Table, Td, Th } from "~/components/ui/Table";
import { hostOf, shownPath, type HostEntry } from "~/lib/pyo/hosts";
import { currentSlug } from "~/lib/pyo/paths";
import { api, roleIn, servicePath, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Domain } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// One hostname of the team: which service each path goes to, its
// redirects and header rules, and its DNS, certificate and tunnel.
export const Route = createFileRoute("/_app/$team/domains_/$hostname")({
  head: ({ params }) => ({ meta: seo({ title: params.hostname }) }),
  component: HostPage,
});

function HostPage() {
  const { hostname } = Route.useParams();
  const s = usePyo();
  const host = hostOf(s, hostname);
  const [redirecting, setRedirecting] = useState<Domain | null>(null);
  const [heading, setHeading] = useState<Domain | null>(null);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<HostEntry | null>(null);
  const [removing, setRemoving] = useState<HostEntry | null>(null);
  const [deleteRecord, setDeleteRecord] = useState(true);
  const [restoreRecord, setRestoreRecord] = useState(false);
  const navigate = useNavigate();
  // Viewers see the rules; developers and up change them.
  const editable = roleIn(s) !== "viewer";
  if (!host)
    return (
      <Page>
        <Panel>
          <EmptyState
            icon={GlobeIcon}
            title={`${hostname} isn't one of this team's domains`}
            description="It was removed, or it's in another team."
          />
        </Panel>
      </Page>
    );
  const { d, a } = host.primary;
  const routes = host.entries.filter((e) => !onlyRedirects(e.d));
  // The hostname's only entry: removing it removes the domain.
  const last = host.entries.length === 1;
  const redirects = host.entries.flatMap((e) =>
    (e.d.redirects ?? []).map((r) => ({ r, e })),
  );
  const headers = host.entries.flatMap((e) =>
    (["response", "request"] as const).flatMap((side) => [
      ...(e.d.headers?.[side]?.set ?? []).map((x) => ({
        side,
        text: `${x.name}: ${x.value}`,
        verb: "Set",
        path: e.d.path || "/",
      })),
      ...(e.d.headers?.[side]?.remove ?? []).map((n) => ({
        side,
        text: n,
        verb: "Remove",
        path: e.d.path || "/",
      })),
    ]),
  );
  return (
    <Page>
      <PageHeader
        title={<span className="font-mono">{host.hostname}</span>}
        meta={
          <span className="inline-flex items-center gap-2">
            <DomainMark d={d} clusterId={a.clusterId} logoOnly />
            <DomainKind hostname={d.hostname} />
            <DomainStatus
              d={d}
              certs={s.certificates}
              notDeployed={!a.currentDeploymentId}
            />
          </span>
        }
      />
      <Panel
        title="Routes"
        actions={
          editable ? (
            <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
              <PlusIcon size={12} weight="bold" /> Add path
            </Button>
          ) : undefined
        }
      >
        {routes.length ? (
          <Table>
            <thead>
              <tr>
                <Th>Path</Th>
                <Th>Service</Th>
                <Th>Port</Th>
                {editable ? (
                  <Th className="w-10">
                    <span className="sr-only">Actions</span>
                  </Th>
                ) : null}
              </tr>
            </thead>
            <tbody>
              {routes.map((e) => (
                <tr key={e.d.id}>
                  <Td className="font-mono text-[12.5px]">
                    {e.d.path || "/"}
                    {e.d.stripPrefix ? (
                      <span className="ml-2 font-sans text-[11.5px] text-muted-foreground">
                        Strip prefix
                      </span>
                    ) : null}
                  </Td>
                  <Td>
                    <AppLink
                      href={servicePath(e.a.id, "domains")}
                      className="font-medium hover:text-primary"
                    >
                      {e.a.name}
                    </AppLink>
                  </Td>
                  <Td className="font-mono text-[12.5px] text-muted-foreground">
                    {e.d.portName}
                  </Td>
                  {editable ? (
                    <Td>
                      <Menu
                        label={`Actions for ${e.d.path || "/"}`}
                        items={[
                          {
                            label: "Edit",
                            icon: <PencilSimpleIcon size={14} />,
                            onSelect: () => setEditing(e),
                          },
                          { type: "separator" as const },
                          {
                            label: "Remove",
                            icon: <TrashIcon size={14} />,
                            danger: true,
                            onSelect: () => {
                              setDeleteRecord(true);
                              setRestoreRecord(false);
                              setRemoving(e);
                            },
                          },
                        ]}
                        trigger={(t) => (
                          <Button
                            {...t}
                            variant="ghost"
                            size="icon"
                            className="h-7 w-7"
                            aria-label={`Actions for ${e.d.path || "/"}`}
                          >
                            <DotsThreeIcon size={16} weight="bold" />
                          </Button>
                        )}
                      />
                    </Td>
                  ) : null}
                </tr>
              ))}
            </tbody>
          </Table>
        ) : (
          <p className="px-4 py-3.5 text-[12.5px] text-muted-foreground">
            Only redirects.
          </p>
        )}
      </Panel>
      <Panel
        title="Redirects"
        actions={
          editable ? (
            <Button size="xs" variant="secondary" onClick={() => setRedirecting(d)}>
              Edit
            </Button>
          ) : undefined
        }
      >
        {redirects.length ? (
          <ul className="list-rows">
            {redirects.map(({ r, e }) => (
              <li
                key={`${e.d.id}:${r.path}`}
                className="flex flex-wrap items-center gap-x-3 px-4 py-2.5 font-mono text-[12.5px]"
              >
                <span>{r.path}</span>
                <span className="text-muted-foreground">→</span>
                <span>{redirectTarget(r)}</span>
                <span className="text-muted-foreground">{r.status}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="px-4 py-3.5 text-[12.5px] text-muted-foreground">
            No redirects.
          </p>
        )}
      </Panel>
      <Panel
        title="Headers"
        actions={
          editable ? (
            <Button size="xs" variant="secondary" onClick={() => setHeading(d)}>
              Edit
            </Button>
          ) : undefined
        }
      >
        {headers.length ? (
          <ul className="list-rows">
            {headers.map((h, i) => (
              <li
                key={i}
                className="flex flex-wrap items-center gap-x-3 px-4 py-2.5 text-[12.5px]"
              >
                <span className="w-16 text-muted-foreground">{h.verb}</span>
                <span className="font-mono">{h.text}</span>
                <span className="ml-auto text-[12px] text-muted-foreground">
                  {h.path === "/" ? "" : `${h.path} · `}
                  {h.side === "response" ? "Responses" : "Requests"}
                </span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="px-4 py-3.5 text-[12.5px] text-muted-foreground">
            No header rules.
          </p>
        )}
      </Panel>
      <Panel title="DNS and HTTPS">
        <div className="space-y-1.5 px-4 py-3.5 text-[12.5px]">
          <p className="text-muted-foreground">{tlsLine(d, s.certificates)}</p>
          <DnsRecordLine app={a} d={d} />
          <p className="text-muted-foreground">
            Set on{" "}
            <AppLink
              href={servicePath(a.id, "domains")}
              className="font-medium text-foreground hover:text-primary"
            >
              {a.name}
            </AppLink>
            .
          </p>
        </div>
      </Panel>
      <RedirectsDialog
        app={a}
        domain={redirecting}
        onClose={() => setRedirecting(null)}
      />
      <HeadersDialog
        app={a}
        domain={heading}
        onClose={() => setHeading(null)}
      />
      <AddDomainDialog
        open={adding}
        onClose={() => setAdding(false)}
        hostname={host.hostname}
      />
      <AddDomainDialog
        open={!!editing}
        onClose={() => setEditing(null)}
        editing={editing}
      />
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing ? shownPath(removing.d) : ""}?`}
        description={
          last
            ? "Traffic to this hostname stops."
            : `It stops going to ${removing?.a.name}.${removing?.d.id === d.id ? " The other paths keep working." : ""}`
        }
        confirmLabel={last ? "Remove domain" : "Remove route"}
        onConfirm={async () => {
          if (!removing) return;
          const { d: gone, a: from } = removing;
          // Putting back the old record replaces Pyo's own as well.
          const restore =
            last && !!gone.dnsRecord?.previous?.length && restoreRecord;
          // This page goes with the hostname's last entry.
          if (last)
            await navigate({
              to: "/$team/domains",
              params: { team: currentSlug() },
            });
          const res = await api.removeDomain(from.id, gone.id, {
            deleteDnsRecord:
              last && !restore && !!gone.dnsRecord?.mine && deleteRecord,
            restoreRecord: restore,
          });
          if (res) toast({ title: `Removed ${shownPath(gone)}` });
        }}
      >
        {last ? (
          <>
            <RestoreRecordSwitch
              d={removing?.d ?? null}
              checked={restoreRecord}
              onChange={setRestoreRecord}
            />
            {restoreRecord && removing?.d.dnsRecord?.previous?.length ? null : (
              <DeleteRecordSwitch
                d={removing?.d ?? null}
                checked={deleteRecord}
                onChange={setDeleteRecord}
              />
            )}
          </>
        ) : null}
      </ConfirmDialog>
    </Page>
  );
}
