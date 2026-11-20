import {
  DownloadSimpleIcon,
  ListMagnifyingGlassIcon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { type ReactNode, useEffect, useState } from "react";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Sheet } from "~/components/ui/Dialog";
import { Input, Select } from "~/components/ui/Field";
import { ListSection } from "~/components/ui/Layered";
import {
  EmptyState,
  ErrorState,
  KeyValue,
  Page,
  PageHeader,
  Panel,
} from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { audit, type AuditFilter, type AuditPage } from "~/lib/pyo/backend";
import { dateTime, timeAgo } from "~/lib/pyo/format";
import { usePyo } from "~/lib/pyo/store";
import type { AuditEvent } from "~/lib/pyo/types";
import { seo } from "~/lib/seo";

// Who did what in the team, when and from where (audit.rs), for its owners
// and admins: filtered by person, kind of action and dates, and downloaded
// as CSV. Read from the API page by page, not from the snapshot.
export const Route = createFileRoute("/_app/$team/audit-log")({
  head: () => ({ meta: seo({ title: "Audit log" }) }),
  component: AuditLogPage,
});

/** Kinds of action, by the start of their name. */
const KINDS: Record<string, string> = {
  auth: "Sign-ins and two-factor",
  api_key: "API keys",
  member: "Members",
  secret: "Secrets shown",
  team: "Team settings",
  settings: "Installation settings",
  service: "Services",
  deployment: "Deployments",
  domain: "Domains",
  database: "Databases",
  cluster: "Clusters",
  server: "Servers",
};

const VIA: Record<AuditEvent["via"], string> = {
  dashboard: "Dashboard",
  api: "API key",
  mcp: "Assistant",
  system: "Pyo",
};

/** "Firefox on macOS", from a User-Agent. */
function browserOf(ua: string): string {
  if (!ua) return "";
  const browser = /Edg\//.test(ua)
    ? "Edge"
    : /OPR\//.test(ua)
      ? "Opera"
      : /Firefox\//.test(ua)
        ? "Firefox"
        : /Chrome\//.test(ua)
          ? "Chrome"
          : /Safari\//.test(ua)
            ? "Safari"
            : /^curl\//.test(ua)
              ? "curl"
              : /^pyo\//i.test(ua)
                ? "Pyo CLI"
                : ua.split(/[\s/]/)[0];
  const os = /Windows/.test(ua)
    ? "Windows"
    : /iPhone|iPad/.test(ua)
      ? "iOS"
      : /Mac OS X|Macintosh/.test(ua)
        ? "macOS"
        : /Android/.test(ua)
          ? "Android"
          : /Linux/.test(ua)
            ? "Linux"
            : "";
  return os ? `${browser} on ${os}` : browser;
}

const day = (s: string, end = false) =>
  s ? new Date(`${s}T${end ? "23:59:59.999" : "00:00:00"}`).getTime() : undefined;

function AuditLogPage() {
  const s = usePyo();
  const team = s.teams.find((t) => t.id === s.currentTeamId);
  const role = s.members.find(
    (m) => m.teamId === s.currentTeamId && m.email === s.user.email,
  )?.role;
  const allowed = role === "owner" || role === "admin";
  const [person, setPerson] = useState("");
  const [kind, setKind] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [page, setPage] = useState<AuditPage | null>(null);
  const [error, setError] = useState("");
  const [more, setMore] = useState(false);
  const [open, setOpen] = useState<AuditEvent | null>(null);
  const [tries, setTries] = useState(0);
  const filter: AuditFilter = {
    person: person || undefined,
    action: kind || undefined,
    from: day(from),
    to: day(to, true),
  };
  const key = JSON.stringify(filter);

  useEffect(() => {
    if (!team || !allowed) return;
    let live = true;
    setError("");
    audit
      .list(team.id, filter)
      .then((p) => live && setPage(p))
      .catch((e) => live && setError(e instanceof Error ? e.message : "Couldn't load the audit log."));
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [team?.id, allowed, key, tries]);

  async function loadMore() {
    if (!team || !page?.next) return;
    setMore(true);
    try {
      const next = await audit.list(team.id, filter, page.next);
      setPage({ ...next, events: [...page.events, ...next.events] });
    } catch (e) {
      setError(e instanceof Error ? e.message : "Couldn't load more.");
    }
    setMore(false);
  }

  if (!allowed)
    return (
      <Page>
        <PageHeader title="Audit log" />
        <Panel>
          <EmptyState
            icon={ListMagnifyingGlassIcon}
            title="For owners and admins"
            description="Ask an owner or admin of this team."
          />
        </Panel>
      </Page>
    );

  const filtered = !!(person || kind || from || to);
  return (
    <Page>
      <PageHeader
        title="Audit log"
        description={
          page
            ? `Kept ${page.days} days.${page.ips === "none" ? " No addresses." : page.ips === "anonymized" ? " Addresses anonymized." : ""}`
            : undefined
        }
        actions={
          team ? (
            <a
              href={audit.csvHref(team.id, filter)}
              download
              className="inline-flex h-8 items-center gap-1.5 rounded-full border border-border bg-background px-3 text-[12.5px] font-medium transition-colors hover:bg-accent"
            >
              <DownloadSimpleIcon size={14} />
              CSV
            </a>
          ) : null
        }
      />
      <ListSection
        toolbar={
          <>
            <div className="w-44">
              <Select
                value={person}
                onChange={(e) => setPerson(e.target.value)}
                aria-label="Person"
                heading="Person"
              >
                <option value="">Everyone</option>
                {page?.people.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name || p.email}
                  </option>
                ))}
                <option value="pyo">Pyo</option>
              </Select>
            </div>
            <div className="w-48">
              <Select
                value={kind}
                onChange={(e) => setKind(e.target.value)}
                aria-label="Action"
                heading="Action"
              >
                <option value="">All actions</option>
                {Object.entries(KINDS).map(([k, v]) => (
                  <option key={k} value={k}>
                    {v}
                  </option>
                ))}
              </Select>
            </div>
            <Input
              type="date"
              aria-label="From"
              value={from}
              max={to || undefined}
              onChange={(e) => setFrom(e.target.value)}
              className="!w-auto"
            />
            <span className="text-[12px] text-muted-foreground">to</span>
            <Input
              type="date"
              aria-label="To"
              value={to}
              min={from || undefined}
              onChange={(e) => setTo(e.target.value)}
              className="!w-auto"
            />
          </>
        }
      >
        <Panel>
          {error ? (
            <ErrorState title={error} onRetry={() => setTries((n) => n + 1)} />
          ) : !page ? (
            <p className="px-4 py-10 text-center text-[13px] text-muted-foreground">
              Loading…
            </p>
          ) : !page.events.length ? (
            <EmptyState
              icon={ListMagnifyingGlassIcon}
              title={filtered ? "Nothing matches" : "Nothing yet"}
              compact
            />
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>When</Th>
                  <Th>Person</Th>
                  <Th>Action</Th>
                  <Th>How</Th>
                  <Th>From</Th>
                </tr>
              </thead>
              <tbody>
                {page.events.map((e) => (
                  <Tr key={e.id} onClick={() => setOpen(e)}>
                    <Td className="whitespace-nowrap text-muted-foreground">
                      <span title={dateTime(e.at)}>{timeAgo(e.at)}</span>
                    </Td>
                    <Td>
                      <p className="max-w-[180px] truncate font-medium">
                        {e.actor.name}
                      </p>
                      {e.actor.email ? (
                        <p className="max-w-[180px] truncate text-[11.5px] text-muted-foreground">
                          {e.actor.email}
                        </p>
                      ) : null}
                    </Td>
                    <Td>
                      <span className="flex items-center gap-2">
                        {e.outcome === "failed" ? (
                          <Badge tone="danger">Failed</Badge>
                        ) : null}
                        <span className="line-clamp-2">{e.title}</span>
                      </span>
                    </Td>
                    <Td className="whitespace-nowrap">
                      {VIA[e.via]}
                      {e.key ? (
                        <p className="max-w-[140px] truncate text-[11.5px] text-muted-foreground">
                          {e.key}
                        </p>
                      ) : null}
                    </Td>
                    <Td className="whitespace-nowrap">
                      <span className="font-mono text-[11.5px]">
                        {e.ip ?? ""}
                      </span>
                      <p className="text-[11.5px] text-muted-foreground">
                        {browserOf(e.userAgent)}
                      </p>
                    </Td>
                  </Tr>
                ))}
              </tbody>
            </Table>
          )}
          {page?.next ? (
            <div className="flex justify-center border-t border-border px-4 py-3">
              <Button
                size="sm"
                variant="secondary"
                disabled={more}
                onClick={loadMore}
              >
                {more ? "Loading…" : "Load more"}
              </Button>
            </div>
          ) : null}
        </Panel>
      </ListSection>
      <Sheet
        open={!!open}
        onClose={() => setOpen(null)}
        title={open?.title ?? ""}
        description={open ? dateTime(open.at) : undefined}
      >
        {open ? (
          <div className="-mx-5 -mt-4">
            <KeyValue
              items={[
                [
                  "Action",
                  <span className="font-mono text-[11.5px]">{open.action}</span>,
                ],
                [
                  "Result",
                  open.outcome === "failed" ? (
                    <Badge tone="danger">Failed</Badge>
                  ) : (
                    <Badge tone="success">Done</Badge>
                  ),
                ],
                [
                  "Person",
                  open.actor.email
                    ? `${open.actor.name} · ${open.actor.email}`
                    : open.actor.name,
                ],
                ["How", open.key ? `${VIA[open.via]} · ${open.key}` : VIA[open.via]],
                ...(open.target.label
                  ? [["Target", open.target.label] as [string, string]]
                  : []),
                ["Address", open.ip ?? "Not kept"],
                ...(open.userAgent
                  ? [
                      [
                        "Browser",
                        <span className="break-all text-[11.5px]">
                          {open.userAgent}
                        </span>,
                      ] as [string, ReactNode],
                    ]
                  : []),
                [
                  "Entry ID",
                  <span className="font-mono text-[11.5px]">{open.id}</span>,
                ],
              ]}
            />
          </div>
        ) : null}
      </Sheet>
    </Page>
  );
}
