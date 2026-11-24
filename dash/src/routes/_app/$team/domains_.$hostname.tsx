import { GlobeIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { tlsLine } from "~/components/pyo/Certificates";
import { DnsRecordLine } from "~/components/pyo/Cloudflare";
import { DomainKind, DomainMark, DomainStatus } from "~/components/pyo/DomainState";
import {
  HeadersDialog,
  onlyRedirects,
  redirectTarget,
  RedirectsDialog,
} from "~/components/pyo/DomainRules";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import { EmptyState, Page, PageHeader, Panel } from "~/components/ui/Panel";
import { Table, Td, Th } from "~/components/ui/Table";
import { hostOf } from "~/lib/pyo/hosts";
import { servicePath, usePyo } from "~/lib/pyo/store";
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
  const redirects = host.entries.flatMap((e) =>
    (e.d.redirects ?? []).map((r) => ({ r, e })),
  );
  const headers = host.entries.flatMap((e) =>
    (["response", "request"] as const).flatMap((side) => [
      ...(e.d.headers?.[side]?.set ?? []).map((x) => ({
        side,
        text: `${x.name}: ${x.value}`,
        verb: "Set",
      })),
      ...(e.d.headers?.[side]?.remove ?? []).map((n) => ({
        side,
        text: n,
        verb: "Remove",
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
      <Panel title="Routes">
        {routes.length ? (
          <Table>
            <thead>
              <tr>
                <Th>Path</Th>
                <Th>Service</Th>
                <Th>Port</Th>
              </tr>
            </thead>
            <tbody>
              {routes.map((e) => (
                <tr key={e.d.id}>
                  <Td className="font-mono text-[12.5px]">/</Td>
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
          <Button size="xs" variant="secondary" onClick={() => setRedirecting(d)}>
            Edit
          </Button>
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
          <Button size="xs" variant="secondary" onClick={() => setHeading(d)}>
            Edit
          </Button>
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
    </Page>
  );
}
