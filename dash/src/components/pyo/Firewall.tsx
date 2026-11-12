import { ArrowDownIcon, ArrowUpIcon, DotsThreeIcon, PencilSimpleIcon, PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Input, Select } from "~/components/ui/Field";
import { Flag } from "~/components/ui/Flag";
import { Menu } from "~/components/ui/Menu";
import { TagInput } from "~/components/ui/TagInput";
import { CountryPicker, FieldPicker, RuleDialog, RuleRow, cleanIp, inSentence, type ActionOption } from "~/components/pyo/RuleForm";
import { Callout, EmptyState, Panel } from "~/components/ui/Panel";
import { TimeAgo } from "~/components/pyo/common";
import { useCanEdit } from "~/components/pyo/Waf";
import { fetchMyAddress } from "~/lib/pyo/backend";
import { countryName } from "~/lib/pyo/location";
import { useQuery } from "~/lib/pyo/query";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Application, Cluster, FirewallRule, FirewallRuleInput } from "~/lib/pyo/types";

// The firewall's access rules: who may reach a service. Allow and block
// rules are checked in order and the first that matches decides (no match:
// the client gets in); rate limits (websites) apply after, each one that
// matches counting the request per client IP.

const isCountry = (s: string) => /^[A-Z]{2}$/.test(s);

/** An address inside a range ("203.0.113.0/24"), IPv4 or IPv6. */
function inRange(ip: string, range: string) {
  const [net, bitsText] = range.trim().split("/");
  const host = net.includes(":") ? 128 : 32;
  const bits = bitsText === undefined ? host : Number(bitsText);
  if (!Number.isInteger(bits) || bits < 0 || bits > host) return false;
  const v4 = (a: string) =>
    a.split(".").reduce((n, o) => (n << 8n) | BigInt(Number(o)), 0n);
  const v6 = (a: string) => {
    const [head, tail = ""] = a.split("::");
    const h = head ? head.split(":") : [];
    const t = tail ? tail.split(":") : [];
    const all = a.includes("::")
      ? [...h, ...Array(8 - h.length - t.length).fill("0"), ...t]
      : h;
    return all.reduce((n, g) => (n << 16n) | BigInt(parseInt(g || "0", 16)), 0n);
  };
  const four = ip.includes(".") && !ip.includes(":");
  if (four !== (net.includes(".") && !net.includes(":"))) return false;
  const width = four ? 32n : 128n;
  const shift = width - BigInt(bits);
  const parse = four ? v4 : v6;
  try {
    return parse(ip) >> shift === parse(net) >> shift;
  } catch {
    return false;
  }
}

type Rule = Pick<FirewallRule, "action" | "operator" | "sources">;

/** Whether a rule matches a client. */
const matches = (r: Rule, ip: string, country: string) => {
  if (!r.sources.length) return true;
  const listed = r.sources.some((s) => (isCountry(s) ? s === country : inRange(ip, s)));
  return listed !== (r.operator === "notIn");
};

/** Whether an address gets through the rules (as the agent applies them): the first allow or block rule that matches decides. */
export function admits(rules: Rule[], ip: string, country: string) {
  const hit = rules.find((r) => r.action !== "rateLimit" && matches(r, ip, country));
  return hit?.action !== "block";
}

/** A single address as a range ("/32" for IPv4, "/128" for IPv6). */
const isHost = (s: string) => !s.includes("/") || (s.includes(":") ? s.endsWith("/128") : s.endsWith("/32"));
const shownSource = (s: string) => (isCountry(s) || !isHost(s) ? s : s.split("/")[0]);
const fieldLabel = (r: Rule) => (r.sources.some(isCountry) ? "country" : "client IP");
const perLabel = (per: string | null) => (per === "second" ? "a second" : "a minute");

/** A rule in words: "Client IP is not in 203.0.113.0/24". */
function ruleText(r: Rule) {
  if (!r.sources.length) return "Any client";
  const list = r.sources.slice(0, 3).map((s) => (isCountry(s) ? countryName(s) : shownSource(s)));
  const more = r.sources.length > 3 ? ` and ${r.sources.length - 3} more` : "";
  const what = fieldLabel(r);
  return `${what.charAt(0).toUpperCase()}${what.slice(1)} ${r.operator === "notIn" ? "is not in" : "is in"} ${list.join(", ")}${more}`;
}

/** What turned a request away: a rule's name, or why. */
export const ruleLabel = (rule: string, rules: FirewallRule[]) => {
  if (rule === "rate-limit") return "Over a rate limit";
  if (rule === "blocked") return "A firewall rule";
  const r = rules.find((x) => x.id === rule);
  return r ? r.note || ruleText(r) : "A removed rule";
};

/** In evaluation order: allow and block rules as stored, then rate limits. */
const ordered = (rules: FirewallRule[]) => [
  ...rules.filter((r) => r.action !== "rateLimit"),
  ...rules.filter((r) => r.action === "rateLimit"),
];

/** A service's firewall access rules. */
export function FirewallEditor({
  cluster,
  app,
}: {
  /** The service's cluster: Pyo may run there (the lockout check). */
  cluster: Cluster;
  app: Application;
}) {
  const s = usePyo();
  const rules = ordered(s.firewallRules.filter((r) => r.serviceId === app.id));
  const editable = useCanEdit(app);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<FirewallRule | null>(null);
  const [confirm, setConfirm] = useState<null | {
    what: string;
    run: () => Promise<void>;
    cancel?: () => void;
  }>(null);
  const me = useQuery("firewall-me", fetchMyAddress, { refreshMs: 0 });

  // The dashboard comes from this cluster: warn before a change would shut
  // this browser out.
  const guard = (next: Rule[], what: string, run: () => Promise<void>, cancel?: () => void) => {
    const addr = me.status === "success" ? me.data : null;
    if (cluster.sameCluster && addr?.ip && !admits(next, addr.ip, addr.country)) {
      setConfirm({ what, run, cancel });
      return;
    }
    void run();
  };

  const access = rules.filter((r) => r.action !== "rateLimit");
  const move = (from: number, to: number) => {
    const ids = access.map((r) => r.id);
    [ids[from], ids[to]] = [ids[to], ids[from]];
    const next = ids.map((id) => access.find((r) => r.id === id)!);
    guard(next, "Move the rule", async () => {
      const res = await api.reorderFirewallRules(app.id, [...ids, ...rules.filter((r) => r.action === "rateLimit").map((r) => r.id)]);
      if ("error" in res) toast({ title: "Not moved", description: res.error, tone: "error" });
    });
  };

  // TCP and UDP ports never see a client's country: "is not in" a list of
  // countries matches every connection there.
  const portsShut =
    app.ports.some((p) => p.exposure === "loadbalancer") &&
    access.some((r) => r.action === "block" && r.operator === "notIn" && r.sources.some(isCountry));

  const add = (
    <Button size="xs" onClick={() => setAdding(true)}>
      <PlusIcon size={12} /> Add rule
    </Button>
  );

  return (
    <div className="space-y-6">
      <Panel title="Access" actions={rules.length && editable ? add : undefined}>
        {portsShut ? (
          <div className="border-b border-border p-4">
            <Callout tone="warning">TCP and UDP ports can't see countries, so they let no one in.</Callout>
          </div>
        ) : null}
        {rules.length ? (
          <RuleList
            rules={rules}
            editable={editable}
            onEdit={(r) => setEditing(r)}
            onMove={move}
            onRemove={(r) =>
              guard(
                rules.filter((x) => x.id !== r.id),
                `Delete "${r.note || ruleText(r)}"`,
                async () => {
                  await api.removeFirewallRule(r.id);
                },
              )
            }
          />
        ) : (
          <EmptyState compact action={editable ? add : undefined} title={editable ? undefined : "No rules"} />
        )}
      </Panel>

      <FwRuleDialog
        open={adding || !!editing}
        rule={editing}
        onClose={() => {
          setAdding(false);
          setEditing(null);
        }}
        onSave={(d) =>
          new Promise<string | null>((resolve) => {
            const what = `${actionOf(d.action).label}: ${ruleText(d)}`;
            const r = editing;
            const next = r ? rules.map((x) => (x.id === r.id ? { ...x, ...d } : x)) : [...rules, { ...d }];
            const finish = (res: { error: string } | object) => resolve("error" in res ? res.error : null);
            guard(
              next,
              what,
              async () => finish(r ? await api.updateFirewallRule(r.id, d) : await api.addFirewallRule({ serviceId: app.id, ...d })),
              () => resolve("Not saved."),
            );
          })
        }
      />

      <Dialog
        open={!!confirm}
        onClose={() => {
          confirm?.cancel?.();
          setConfirm(null);
        }}
        title="This would block you"
        description={`You're using Pyo from ${me.status === "success" ? me.data.ip : "this address"}, and Pyo runs on this cluster. "${confirm?.what}" would turn this browser away from ${app.name}.`}
      >
        <LockoutConfirm
          onCancel={() => {
            confirm?.cancel?.();
            setConfirm(null);
          }}
          onConfirm={async () => {
            const c = confirm;
            setConfirm(null);
            await c?.run();
          }}
        />
      </Dialog>
    </div>
  );
}

function LockoutConfirm({
  onCancel,
  onConfirm,
}: {
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [text, setText] = useState("");
  return (
    <div className="space-y-4">
      <Field label='Type "confirm" to apply it anyway'>
        {(p) => (
          <Input
            {...p}
            value={text}
            onChange={(e) => setText(e.target.value)}
          />
        )}
      </Field>
      <div className="flex justify-end gap-2">
        <Button variant="ghost" size="sm" onClick={onCancel}>
          Cancel
        </Button>
        <Button
          size="sm"
          variant="danger"
          disabled={text.trim().toLowerCase() !== "confirm"}
          onClick={onConfirm}
        >
          Apply anyway
        </Button>
      </div>
    </div>
  );
}

type FwField = "ip" | "range" | "country" | "any";
const FW_FIELDS: { value: FwField; label: string }[] = [
  { value: "ip", label: "IP address" },
  { value: "range", label: "IP range" },
  { value: "country", label: "Country" },
  { value: "any", label: "Any client" },
];
type FwAction = FirewallRule["action"];
const FW_ACTIONS: ActionOption<FwAction>[] = [
  { value: "block", label: "Block", tone: "danger", hint: "Turns them away. Later rules are skipped." },
  { value: "allow", label: "Allow", tone: "success", hint: "Lets them in. Later rules are skipped." },
  { value: "rateLimit", label: "Rate limit", tone: "warning", hint: "Websites only. Counted per client IP, on each server." },
];
const actionOf = (a: string) => FW_ACTIONS.find((x) => x.value === a) ?? FW_ACTIONS[0];
const fieldOf = (r: Rule): FwField =>
  !r.sources.length ? "any" : r.sources.some(isCountry) ? "country" : r.sources.every(isHost) ? "ip" : "range";

/** "If country is not in Germany" for one rule, with flags. */
function FwSummary({ rule }: { rule: FirewallRule }) {
  const limit = rule.action === "rateLimit" && rule.requests ? `· ${rule.requests.toLocaleString()} requests ${perLabel(rule.per)}` : "";
  if (!rule.sources.length) return <span>Any client {limit}</span>;
  const countries = rule.sources.some(isCountry);
  return (
    <span className="inline-flex flex-wrap items-center gap-x-1.5 gap-y-1">
      If {fieldLabel(rule)} {rule.operator === "notIn" ? "is not in" : "is in"}
      {rule.sources.slice(0, 6).map((s, i, shown) => {
        const comma = i < shown.length - 1 ? "," : "";
        return countries ? (
          <span key={s} className="inline-flex items-center gap-1">
            <Flag country={s} size={14} />
            {countryName(s)}
            {comma}
          </span>
        ) : (
          <span key={s}>
            <span className="font-mono text-[12.5px]">{shownSource(s)}</span>
            {comma}
          </span>
        );
      })}
      {rule.sources.length > 6 ? <span>and {rule.sources.length - 6} more</span> : null}
      {limit ? <span>{limit}</span> : null}
    </span>
  );
}

function RuleList({
  rules,
  editable,
  onRemove,
  onEdit,
  onMove,
}: {
  rules: FirewallRule[];
  editable: boolean;
  onRemove: (r: FirewallRule) => void;
  onEdit: (r: FirewallRule) => void;
  onMove: (from: number, to: number) => void;
}) {
  const ordered = rules.filter((r) => r.action !== "rateLimit").length;
  return (
    <ul className="list-rows">
      {rules.map((r, i) => {
        const inOrder = r.action !== "rateLimit";
        return (
          <RuleRow
            key={r.id}
            order={inOrder ? i + 1 : null}
            name={r.note || undefined}
            summary={<FwSummary rule={r} />}
            action={actionOf(r.action)}
            meta={
              <>
                {r.createdBy ? `${r.createdBy}, ` : ""}
                <TimeAgo at={r.createdAt} />
              </>
            }
            controls={
              editable ? (
                <Menu
                  label={`Actions for ${r.note || ruleText(r)}`}
                  items={[
                    { label: "Edit", icon: <PencilSimpleIcon size={14} />, onSelect: () => onEdit(r) },
                    ...(inOrder && ordered > 1
                      ? [
                          { label: "Move up", icon: <ArrowUpIcon size={14} />, disabled: i === 0, onSelect: () => onMove(i, i - 1) },
                          { label: "Move down", icon: <ArrowDownIcon size={14} />, disabled: i === ordered - 1, onSelect: () => onMove(i, i + 1) },
                        ]
                      : []),
                    { type: "separator" as const },
                    { label: "Delete", icon: <TrashIcon size={14} />, danger: true, onSelect: () => onRemove(r) },
                  ]}
                  trigger={(t) => (
                    <Button variant="ghost" size="icon" {...t}>
                      <DotsThreeIcon size={16} />
                    </Button>
                  )}
                />
              ) : undefined
            }
          />
        );
      })}
    </ul>
  );
}

const cleanAddress = (raw: string) => {
  const t = cleanIp(raw);
  return t && !t.includes("/") ? t : null;
};
const cleanRange = (raw: string) => {
  const t = cleanIp(raw);
  return t && t.includes("/") ? t : null;
};

type FwDraft = FirewallRuleInput;

const FW_GROUPS: { label: string; fields: { value: FwField; label: string }[] }[] = [{ label: "Client", fields: FW_FIELDS }];

/** Add a rule, or change one. */
function FwRuleDialog({
  open,
  rule,
  onClose,
  onSave,
}: {
  open: boolean;
  rule: FirewallRule | null;
  onClose: () => void;
  onSave: (d: FwDraft) => Promise<string | null>;
}) {
  const [note, setNote] = useState("");
  const [field, setField] = useState<FwField>("ip");
  const [operator, setOperator] = useState<"in" | "notIn">("in");
  const [values, setValues] = useState<string[]>([]);
  const [action, setAction] = useState<FwAction>("block");
  const [requests, setRequests] = useState("100");
  const [per, setPer] = useState<"second" | "minute">("minute");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!open) return;
    const f = rule ? fieldOf(rule) : "ip";
    setNote(rule?.note ?? "");
    setField(f);
    setOperator(rule?.operator ?? "in");
    setValues(rule ? rule.sources.map((s) => (f === "range" ? s : shownSource(s))) : []);
    setAction(rule?.action ?? "block");
    setRequests(String(rule?.requests ?? 100));
    setPer(rule?.per ?? "minute");
    setError(null);
  }, [open, rule]);
  const label = FW_FIELDS.find((f) => f.value === field)?.label ?? "";
  const any = field === "any";
  const sources = any ? [] : values;
  const shown = field === "country" ? values.map((v) => countryName(v) || v) : values;
  const list = `${shown.slice(0, 3).join(", ")}${shown.length > 3 ? "…" : ""}`;
  const act = actionOf(action).label;
  const autoName = any ? `${act} any client` : shown.length ? `${act} ${operator === "notIn" ? "all but " : ""}${list}` : "";
  const n = Number(requests);
  const limited = action === "rateLimit";
  const blocked =
    !any && !values.length
      ? field === "country"
        ? "Pick a country."
        : "Add a value."
      : limited && field === "country"
        ? "Rate limits can't use countries."
        : limited && operator === "notIn" && values.length > 1
          ? "Rate limits take one value with “is not in”."
          : limited && !(Number.isInteger(n) && n >= 1 && n <= 1_000_000)
            ? "Between 1 and 1,000,000 requests."
            : null;
  return (
    <RuleDialog
      open={open}
      onClose={onClose}
      title={rule ? "Edit rule" : "Add rule"}
      name={note}
      autoName={rule ? "" : autoName}
      onName={setNote}
      nameLabel="Name"
      actions={FW_ACTIONS}
      action={action}
      onAction={(a) => setAction(a)}
      actionFields={
        limited ? (
          <div className="flex flex-wrap items-center gap-2 pt-2">
            <div className="w-[120px]">
              <Input aria-label="Requests" mono type="number" min={1} value={requests} onChange={(e) => setRequests(e.target.value)} />
            </div>
            <span className="text-[12.5px] text-muted-foreground">requests</span>
            <Select aria-label="Per" className="w-[130px]" value={per} onChange={(e) => setPer(e.target.value as "second" | "minute")}>
              <option value="second">a second</option>
              <option value="minute">a minute</option>
            </Select>
          </div>
        ) : null
      }
      submitLabel={rule ? "Save" : "Deploy"}
      busy={busy}
      blocked={blocked}
      error={error}
      sentence={any ? "If any client" : values.length ? `If ${inSentence(label)} ${operator === "notIn" ? "is not in" : "is in"} ${shown.join(", ")}` : null}
      onSubmit={async () => {
        setBusy(true);
        const err = await onSave({
          note: (note || (rule ? "" : autoName)).trim(),
          action,
          operator: any ? "in" : operator,
          sources,
          ...(limited ? { requests: n, per } : {}),
        });
        setBusy(false);
        if (err) setError(err);
        else onClose();
      }}
      conditions={
        <div className="flex flex-wrap items-start gap-2">
          <FieldPicker
            label="Field"
            value={field}
            groups={FW_GROUPS}
            onChange={(f) => {
              setField(f);
              setValues([]);
            }}
          />
          {any ? null : (
            <>
              <Select aria-label="Operator" className="w-[120px]" value={operator} onChange={(e) => setOperator(e.target.value as "in" | "notIn")}>
                <option value="in">is in</option>
                <option value="notIn">is not in</option>
              </Select>
              {field === "country" ? (
                <CountryPicker key="country" value={values} onChange={setValues} label="Countries" />
              ) : (
                <div className="min-w-[12rem] flex-1">
                  <TagInput
                    key={field}
                    aria-label={field === "ip" ? "Addresses" : "Ranges"}
                    value={values}
                    onChange={setValues}
                    placeholder={field === "ip" ? "203.0.113.7" : "198.51.100.0/24"}
                    clean={field === "ip" ? cleanAddress : cleanRange}
                    invalid={field === "ip" ? "An address, like 203.0.113.7." : "A range, like 198.51.100.0/24."}
                    max={50}
                  />
                </div>
              )}
            </>
          )}
        </div>
      }
    />
  );
}
