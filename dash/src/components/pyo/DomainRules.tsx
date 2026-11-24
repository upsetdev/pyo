import { PlusIcon, XIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Checkbox, Input, Select } from "~/components/ui/Field";
import { api } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  Application,
  Domain,
  DomainRedirect,
  HeaderRules,
} from "~/lib/pyo/types";

// A domain's rules in Pyo Gateway: its redirects and its header rules.
// Edited in dialogs from the domain's row, saved before they show.

/** Where a redirect goes, as typed: "example.com/new", or "/new". */
export function redirectTarget(r: DomainRedirect): string {
  return `${r.toHost ?? ""}${r.toPath ?? ""}` || "/";
}

/** "example.com/new" or "/new" as a redirect's target. */
export function parseTarget(to: string): { toHost: string; toPath: string } {
  const t = to.trim().replace(/^https?:\/\//, "");
  const i = t.indexOf("/");
  const host = (i < 0 ? t : t.slice(0, i)).replace(/\.$/, "").toLowerCase();
  return { toHost: host, toPath: i < 0 ? "" : t.slice(i) };
}

/** The whole domain redirects (it has no port). */
export function onlyRedirects(d: Domain): boolean {
  return !d.portName && !!d.redirects?.length;
}

/** How many header rules a domain has. */
export function headerCount(h?: HeaderRules): number {
  return [h?.response, h?.request].reduce(
    (n, x) => n + (x?.set?.length ?? 0) + (x?.remove?.length ?? 0),
    0,
  );
}

/** A domain's rules under its row: a line per redirect, and its headers. */
export function RulesLines({ d }: { d: Domain }) {
  const redirects = (d.redirects ?? []).filter(
    (r) => !(onlyRedirects(d) && r.path === "/"),
  );
  const headers = headerCount(d.headers);
  if (!redirects.length && !headers) return null;
  return (
    <div className="mt-1.5 space-y-0.5 text-[12px] text-muted-foreground">
      {redirects.map((r) => (
        <p key={r.path} className="font-mono text-[11.5px]">
          {r.path} → {redirectTarget(r)}{" "}
          <span className="text-muted-foreground/70">{r.status}</span>
        </p>
      ))}
      {headers ? (
        <p>
          {headers} header {headers === 1 ? "rule" : "rules"}
        </p>
      ) : null}
    </div>
  );
}

type RedirectRow = {
  path: string;
  to: string;
  status: 301 | 302;
  keepPath: boolean;
};

const toRow = (r: DomainRedirect): RedirectRow => ({
  path: r.path,
  to: redirectTarget(r),
  status: r.status,
  keepPath: r.keepPath,
});

/** A domain's redirects: path, where to, 301 or 302, keep the path. */
export function RedirectsDialog({
  app,
  domain,
  onClose,
}: {
  app: Application;
  domain: Domain | null;
  onClose: () => void;
}) {
  const [rows, setRows] = useState<RedirectRow[]>([]);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    if (!domain) return;
    const have = (domain.redirects ?? []).map(toRow);
    setRows(
      have.length
        ? have
        : [{ path: "/", to: "", status: 301, keepPath: true }],
    );
    setError("");
  }, [domain]);
  const set = (i: number, patch: Partial<RedirectRow>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const only = !!domain && !domain.portName;
  return (
    <Dialog
      open={!!domain}
      onClose={onClose}
      title={`Redirects of ${domain?.hostname ?? ""}`}
      width="max-w-2xl"
      onSubmit={async () => {
        if (!domain) return;
        const redirects: DomainRedirect[] = rows
          .filter((r) => r.to.trim())
          .map((r) => ({
            path: r.path.trim() || "/",
            ...parseTarget(r.to),
            status: r.status,
            keepPath: r.keepPath,
          }));
        if (only && !redirects.length) {
          setError("This domain only redirects. Remove the domain instead.");
          return;
        }
        setSaving(true);
        const why = await api.saveDomainRules(app.id, domain.id, {
          redirects,
        });
        setSaving(false);
        if (why) return setError(why);
        toast({ title: `Saved the redirects of ${domain.hostname}` });
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={saving}>
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <div className="hidden grid-cols-[1fr_1.4fr_5rem_2rem] gap-2 text-[12px] font-medium text-muted-foreground sm:grid">
          <span>Path</span>
          <span>To</span>
          <span>Status</span>
          <span />
        </div>
        {rows.map((r, i) => (
          <div key={i} className="space-y-1.5">
            <div className="grid grid-cols-[1fr_1.4fr_5rem_2rem] items-center gap-2">
              <Input
                mono
                aria-label="Path"
                value={r.path}
                onChange={(e) => set(i, { path: e.target.value })}
                placeholder="/old"
              />
              <Input
                mono
                aria-label="To"
                value={r.to}
                onChange={(e) => set(i, { to: e.target.value })}
                placeholder="example.com or /new"
              />
              <Select
                aria-label="Status"
                value={String(r.status)}
                onChange={(e) =>
                  set(i, { status: e.target.value === "302" ? 302 : 301 })
                }
              >
                <option value="301">301</option>
                <option value="302">302</option>
              </Select>
              <Button
                variant="ghost"
                size="icon"
                aria-label="Remove redirect"
                onClick={() => setRows((rs) => rs.filter((_, j) => j !== i))}
              >
                <XIcon size={14} />
              </Button>
            </div>
            <Checkbox
              checked={r.keepPath}
              onChange={(v) => set(i, { keepPath: v })}
              label="Keep the rest of the path"
              className="text-[12px]"
            />
          </div>
        ))}
        <Button
          variant="secondary"
          size="xs"
          onClick={() =>
            setRows((rs) => [
              ...rs,
              { path: "", to: "", status: 301, keepPath: true },
            ])
          }
        >
          <PlusIcon size={12} weight="bold" /> Add redirect
        </Button>
        <p className="text-[12px] text-muted-foreground">
          The query string is kept.
        </p>
        {error ? (
          <p className="text-[12px] text-destructive">{error}</p>
        ) : null}
      </div>
    </Dialog>
  );
}

type HeaderRow = {
  side: "response" | "request";
  action: "set" | "remove";
  name: string;
  value: string;
};

const PRESETS: { label: string; row: Omit<HeaderRow, "side"> }[] = [
  {
    label: "HSTS",
    row: {
      action: "set",
      name: "Strict-Transport-Security",
      value: "max-age=31536000",
    },
  },
  {
    label: "Frame options",
    row: { action: "set", name: "X-Frame-Options", value: "DENY" },
  },
  {
    label: "No sniffing",
    row: { action: "set", name: "X-Content-Type-Options", value: "nosniff" },
  },
  {
    label: "Hide X-Powered-By",
    row: { action: "remove", name: "X-Powered-By", value: "" },
  },
];

function rowsOf(h?: HeaderRules): HeaderRow[] {
  const out: HeaderRow[] = [];
  for (const side of ["response", "request"] as const) {
    for (const x of h?.[side]?.set ?? [])
      out.push({ side, action: "set", name: x.name, value: x.value });
    for (const n of h?.[side]?.remove ?? [])
      out.push({ side, action: "remove", name: n, value: "" });
  }
  return out;
}

function rulesOf(rows: HeaderRow[]): HeaderRules {
  const out: HeaderRules = {};
  for (const side of ["response", "request"] as const) {
    const mine = rows.filter((r) => r.side === side && r.name.trim());
    const set = mine
      .filter((r) => r.action === "set")
      .map((r) => ({ name: r.name.trim(), value: r.value.trim() }));
    const remove = mine
      .filter((r) => r.action === "remove")
      .map((r) => r.name.trim());
    if (set.length || remove.length) out[side] = { set, remove };
  }
  return out;
}

/** A domain's header rules: set or remove, on responses or requests. */
export function HeadersDialog({
  app,
  domain,
  onClose,
}: {
  app: Application;
  domain: Domain | null;
  onClose: () => void;
}) {
  const [rows, setRows] = useState<HeaderRow[]>([]);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    if (!domain) return;
    setRows(rowsOf(domain.headers));
    setError("");
  }, [domain]);
  const set = (i: number, patch: Partial<HeaderRow>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const add = (side: HeaderRow["side"], row?: Omit<HeaderRow, "side">) =>
    setRows((rs) => [
      ...rs,
      { side, ...(row ?? { action: "set", name: "", value: "" }) },
    ]);
  const section = (side: HeaderRow["side"], title: string) => (
    <div className="space-y-2">
      <p className="text-[12px] font-medium text-muted-foreground">{title}</p>
      {rows.map((r, i) =>
        r.side !== side ? null : (
          <div
            key={i}
            className="grid grid-cols-[6.5rem_1fr_1fr_2rem] items-center gap-2"
          >
            <Select
              aria-label="Action"
              value={r.action}
              onChange={(e) =>
                set(i, { action: e.target.value as HeaderRow["action"] })
              }
            >
              <option value="set">Set</option>
              <option value="remove">Remove</option>
            </Select>
            <Input
              mono
              aria-label="Header"
              value={r.name}
              onChange={(e) => set(i, { name: e.target.value })}
              placeholder="X-Frame-Options"
            />
            {r.action === "set" ? (
              <Input
                mono
                aria-label="Value"
                value={r.value}
                onChange={(e) => set(i, { value: e.target.value })}
                placeholder="DENY"
              />
            ) : (
              <span />
            )}
            <Button
              variant="ghost"
              size="icon"
              aria-label="Remove rule"
              onClick={() => setRows((rs) => rs.filter((_, j) => j !== i))}
            >
              <XIcon size={14} />
            </Button>
          </div>
        ),
      )}
      <div className="flex flex-wrap items-center gap-1.5">
        <Button variant="secondary" size="xs" onClick={() => add(side)}>
          <PlusIcon size={12} weight="bold" /> Add header
        </Button>
        {side === "response"
          ? PRESETS.filter(
              (p) =>
                !rows.some(
                  (r) =>
                    r.side === "response" &&
                    r.name.toLowerCase() === p.row.name.toLowerCase(),
                ),
            ).map((p) => (
              <Button
                key={p.label}
                variant="ghost"
                size="xs"
                onClick={() => add("response", p.row)}
              >
                {p.label}
              </Button>
            ))
          : null}
      </div>
    </div>
  );
  return (
    <Dialog
      open={!!domain}
      onClose={onClose}
      title={`Headers of ${domain?.hostname ?? ""}`}
      width="max-w-2xl"
      onSubmit={async () => {
        if (!domain) return;
        setSaving(true);
        const why = await api.saveDomainRules(app.id, domain.id, {
          headers: rulesOf(rows),
        });
        setSaving(false);
        if (why) return setError(why);
        toast({ title: `Saved the headers of ${domain.hostname}` });
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={saving}>
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-5">
        {section("response", "Responses")}
        {section("request", "Requests to the service")}
        {error ? (
          <p className="text-[12px] text-destructive">{error}</p>
        ) : null}
      </div>
    </Dialog>
  );
}
