import {
  ArrowDownIcon,
  ArrowUpIcon,
  DotsThreeIcon,
  PencilSimpleIcon,
  PlusIcon,
  PowerIcon,
  TrashIcon,
  XIcon,
} from "@phosphor-icons/react";
import { useEffect, useId, useState, type ReactNode } from "react";
import { Badge, Status } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Input, Segmented, Select, Switch } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { TagInput } from "~/components/ui/TagInput";
import {
  CountryPicker,
  FieldPicker,
  METHODS,
  MethodChips,
  RuleDialog,
  RuleRow,
  cleanIp,
  inSentence,
  type ActionOption,
} from "~/components/pyo/RuleForm";
import { jsRegex, matches, type TestRule } from "~/lib/pyo/wafMatch";
import { Flag } from "~/components/ui/Flag";
import { Layered } from "~/components/ui/Layered";
import { Callout, EmptyState, Skeleton } from "~/components/ui/Panel";
import { Reveal } from "~/components/ui/Reveal";
import { fetchMyAddress, fetchWafHits } from "~/lib/pyo/backend";
import { countryName } from "~/lib/pyo/location";
import { compact } from "~/lib/pyo/format";
import { useQuery } from "~/lib/pyo/query";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type {
  Application,
  WafAction,
  WafCondition,
  WafField,
  WafOperator,
  WafRule,
  WafSettings,
} from "~/lib/pyo/types";
import { Arrow } from "~/components/ui/Arrow";

// The web application firewall: built in, on or off per service (Coraza with
// the OWASP Core Rule Set, running beside the gateway), with the service's
// custom rules running first. It starts on a cluster with the first service
// there that turns it on, and stops after the last one turns it off.

const LEVELS = [
  { value: 1, label: "Standard", hint: "" },
  { value: 2, label: "Strict", hint: "Some false alarms." },
  { value: 3, label: "Stricter", hint: "Expect exceptions." },
  { value: 4, label: "Strictest", hint: "Many false alarms." },
];

const MODE_LABEL: Record<string, string> = { off: "Off", log: "Log only", block: "Block" };
/** Inbound anomaly thresholds by name: how many matches block a request. */
const THRESHOLDS = [
  { value: 5, label: "Standard", hint: "One strong match blocks." },
  { value: 10, label: "Relaxed", hint: "Takes two strong matches." },
  { value: 20, label: "Lenient", hint: "Takes several matches." },
  { value: 50, label: "Most lenient", hint: "Blocks only blatant attacks." },
];
/** Custom rules' numbers start here; the Core Rule Set's are below. */
export const CUSTOM_FROM = 1_000_000;

/** A service's WAF mode: off unless it's set to log only or block. */
export const wafMode = (app: Application): WafSettings["mode"] =>
  app.waf?.mode === "log" || app.waf?.mode === "block" ? app.waf.mode : "off";

/** Its settings with the given changes, everything else as stored. */
function wafSettings(app: Application, patch: Partial<WafSettings>): WafSettings {
  const own = app.waf ?? {};
  return {
    mode: wafMode(app),
    level: own.level || 1,
    exceptions: own.exceptions ?? [],
    threshold: own.threshold || 5,
    ...patch,
  };
}

async function saveWaf(app: Application, patch: Partial<WafSettings>) {
  const res = await api.setServiceWaf(app.id, wafSettings(app, patch));
  if ("error" in res) toast({ title: "Not saved", description: res.error, tone: "error" });
  return res;
}

/** Where the service's WAF stands while it's on, beside its switch:
 * "Starting" while the WAF starts on its cluster, "Not active" when it
 * failed (the card says why). Nothing once it runs. */
function WafStatus({ app, on }: { app: Application; on: boolean }) {
  const w = app.wafState;
  if (!on || !w || w.state === "ready") return null;
  return (
    <Status
      tone={w.state === "failed" ? "danger" : "progress"}
      label={w.state === "failed" ? "Not active" : w.message || "Starting"}
      className="text-[12px]"
    />
  );
}

/** The header switch: on (block mode) or off. The store flips at once, so
 * the settings open before the API answers; a failed save flips it back. */
function WafSwitch({
  app,
  on,
  controls,
  disabled,
  onRollback,
}: {
  app: Application;
  on: boolean;
  controls: string;
  disabled?: boolean;
  onRollback: (on: boolean | null) => void;
}) {
  const id = useId();
  // The shared Switch takes no ARIA extras; it opens and closes the settings.
  useEffect(() => {
    const el = document.getElementById(id);
    el?.setAttribute("aria-expanded", String(on));
    el?.setAttribute("aria-controls", controls);
  }, [id, on, controls]);
  return (
    <div className="flex items-center gap-3">
      <WafStatus app={app} on={on} />
      <Switch
        id={id}
        checked={on}
        disabled={disabled}
        ariaLabel="Web application firewall"
        onChange={async (v) => {
          onRollback(null);
          const res = await saveWaf(app, { mode: v ? "block" : "off" });
          if ("error" in res) onRollback(!v);
          else toast({ title: v ? "Web application firewall on" : "Web application firewall off", description: app.name });
        }}
      />
    </div>
  );
}

const MODE_HINT: Record<string, string> = {
  off: "Requests aren't checked.",
  log: "Records attacks, lets them through.",
  block: "Refuses attacks with a 403.",
};

/** A section in the WAF's card: a small heading, an action, its content. */
function Section({ title, aside, action, children }: { title: ReactNode; aside?: ReactNode; action?: ReactNode; children: ReactNode }) {
  return (
    <section className="border-t border-border first:border-t-0">
      <div className="flex min-h-11 flex-wrap items-center justify-between gap-x-3 gap-y-1 px-4 pb-1 pt-3">
        <h3 className="flex min-w-0 items-baseline gap-2 text-[13px] font-medium">
          {title}
          {aside ? <span className="text-[12px] font-normal text-muted-foreground">{aside}</span> : null}
        </h3>
        {action}
      </div>
      {children}
    </section>
  );
}

/** A setting: its name and a one-line hint on the left, the control on the right. */
function SettingRow({ label, hint, children }: { label: ReactNode; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-4 py-2.5">
      <div className="min-w-0">
        <p className="text-[13px]">{label}</p>
        {hint ? <p className="text-[12px] text-muted-foreground">{hint}</p> : null}
      </div>
      {children}
    </div>
  );
}

// ── Who may change what ──────────────────────────────────────────────────────

/** Developers change a service's WAF; viewers don't. */
export function useCanEdit(app: Application) {
  const s = usePyo();
  const me = s.members.find((m) => m.teamId === app.teamId && m.email === s.user.email);
  if (!me) return true; // The server decides.
  return me.role !== "viewer";
}

/** What the WAF found in 24h, beside its title: "12 blocked · 40 logged · 24h". */
function HitsStat({ status, blocked, logged }: { status: "loading" | "success" | "error"; blocked: number; logged: number }) {
  if (status === "error") return null;
  return (
    <a href="#analytics" className="text-[12px] font-normal text-muted-foreground hover:text-foreground">
      {status === "loading" ? (
        <Skeleton className="inline-block h-3 w-28 align-middle" />
      ) : (
        <>
          <span className="tabular-nums text-foreground">{compact(blocked)}</span> blocked ·{" "}
          <span className="tabular-nums text-foreground">{compact(logged)}</span> logged · 24h
        </>
      )}
    </a>
  );
}

const cleanRuleId = (raw: string) => {
  const t = raw.trim();
  return /^\d{6}$/.test(t) && Number(t) >= 900000 && Number(t) <= 999999 ? t : null;
};

/** A service's OWASP settings, for all its domains; each saves at once. */
function ServiceSettings({ app, disabled }: { app: Application; disabled: boolean }) {
  const own = app.waf ?? {};
  const mode = wafMode(app);
  const level = own.level || 1;
  const threshold = own.threshold || 5;
  const exceptions = (own.exceptions ?? []).map(String);
  const off = mode === "off";
  const known = THRESHOLDS.find((t) => t.value === threshold);
  return (
    <section className="border-t border-border first:border-t-0">
      <div className="list-rows pb-1.5">
        <SettingRow label="Mode" hint={MODE_HINT[mode]}>
          <Segmented
            label="Mode"
            size="md"
            value={mode}
            onChange={(m) => {
              if (!disabled && m !== mode) void saveWaf(app, { mode: m });
            }}
            options={[
              { value: "off", label: "Off", disabled },
              { value: "log", label: "Log only", disabled },
              { value: "block", label: "Block", disabled },
            ]}
          />
        </SettingRow>
        <SettingRow label="Strictness" hint={LEVELS.find((l) => l.value === level)?.hint || "Few false alarms."}>
          <Select
            aria-label="Strictness"
            className="w-[168px]"
            disabled={disabled || off}
            value={level}
            onChange={(e) => void saveWaf(app, { level: Number(e.target.value) })}
          >
            {LEVELS.map((l) => (
              <option key={l.value} value={l.value}>
                {l.label}
              </option>
            ))}
          </Select>
        </SettingRow>
        <SettingRow label="Anomaly threshold" hint={known?.hint}>
          <Select
            aria-label="Anomaly threshold"
            className="w-[168px]"
            disabled={disabled || off}
            value={threshold}
            onChange={(e) => void saveWaf(app, { threshold: Number(e.target.value) })}
          >
            {THRESHOLDS.map((t) => (
              <option key={t.value} value={t.value}>
                {t.label}
              </option>
            ))}
          </Select>
        </SettingRow>
        <SettingRow label="Exceptions" hint="OWASP rules not applied.">
          <div className="w-[168px]">
            <TagInput
              aria-label="Exceptions"
              value={exceptions}
              onChange={(next) => void saveWaf(app, { exceptions: next.map(Number) })}
              placeholder="942100"
              clean={cleanRuleId}
              invalid="A rule number from 900000 to 999999."
              max={200}
              disabled={disabled || off}
            />
          </div>
        </SettingRow>
      </div>
    </section>
  );
}

// ── Custom rules ─────────────────────────────────────────────────────────────

const FIELD_GROUPS: { label: string; fields: { value: WafField; label: string }[] }[] = [
  { label: "Request", fields: [{ value: "path", label: "Path" }, { value: "query", label: "Query string" }, { value: "method", label: "Method" }, { value: "host", label: "Host" }] },
  { label: "Headers", fields: [{ value: "userAgent", label: "User agent" }, { value: "header", label: "Header" }, { value: "cookie", label: "Cookie" }] },
  { label: "Client", fields: [{ value: "ip", label: "IP address" }, { value: "country", label: "Country" }] },
  { label: "Body", fields: [{ value: "body", label: "Body" }] },
];
const FIELDS = FIELD_GROUPS.flatMap((g) => g.fields);
/** Operators as the form offers them: "!" marks a negated one. */
const OP_LABEL: Record<string, string> = {
  equals: "equals",
  "!equals": "doesn't equal",
  contains: "contains",
  "!contains": "doesn't contain",
  startsWith: "starts with",
  "!startsWith": "doesn't start with",
  endsWith: "ends with",
  "!endsWith": "doesn't end with",
  regex: "matches regex",
  "!regex": "doesn't match regex",
  in: "is in",
  "!in": "is not in",
};
const ACTIONS: ActionOption<WafAction>[] = [
  { value: "block", label: "Block", tone: "danger", hint: "Returns 403." },
  { value: "allow", label: "Allow", tone: "success", hint: "Skips later rules and OWASP." },
  { value: "log", label: "Log", tone: "neutral", hint: "Records it, goes on." },
  { value: "skipOwasp", label: "Skip OWASP", tone: "warning", hint: "Later rules still run." },
];
/** The operators a field offers. */
const opsFor = (f: WafField): string[] =>
  f === "ip"
    ? ["in", "!in"]
    : f === "country" || f === "method"
      ? ["equals", "in", "!in"]
      : ["equals", "contains", "startsWith", "endsWith", "regex", "!equals", "!contains"];
const opLabel = (f: WafField, op: string) => ((f === "country" || f === "method") && op === "equals" ? "is" : OP_LABEL[op] ?? op);
const fieldLabel = (f: WafField) => FIELDS.find((x) => x.value === f)?.label ?? f;
const actionOf = (a: WafAction) => ACTIONS.find((x) => x.value === a) ?? ACTIONS[0];
const listField = (f: WafField) => f === "ip" || f === "country" || f === "method";
const PLACEHOLDER: Record<WafField, string> = {
  path: "/wp-admin",
  query: "id=1",
  method: "POST",
  host: "admin.example.com",
  header: "secret",
  userAgent: "curl",
  cookie: "1",
  body: "<script",
  ip: "203.0.113.7",
  country: "DE",
};
const shownValues = (c: WafCondition) => (c.field === "country" ? c.values.map((v) => countryName(v) || v) : c.values);

/** "Path starts with /wp-admin and Method is POST", as text. */
export function conditionsSummary(r: Pick<WafRule, "conditions" | "match">) {
  const part = (c: WafCondition) =>
    `${fieldLabel(c.field)}${c.name ? ` ${c.name}` : ""} ${opLabel(c.field, `${c.negate ? "!" : ""}${c.op}`)} ${shownValues(c).join(", ")}`;
  return r.conditions.map(part).join(r.match === "any" ? " or " : " and ");
}

/** "path starts with /wp-admin and method is POST", for the form's line. */
function sentenceOf(r: Pick<WafRule, "conditions" | "match">) {
  return r.conditions
    .map((c) => `${inSentence(fieldLabel(c.field))}${c.name ? ` ${c.name}` : ""} ${opLabel(c.field, `${c.negate ? "!" : ""}${c.op}`)} ${shownValues(c).join(", ")}`)
    .join(r.match === "any" ? " or " : " and ");
}

/** "… then Block", for a rule in one line (an arrow between). */
export function ruleSummary(r: Pick<WafRule, "conditions" | "match" | "action">) {
  return (
    <>
      {conditionsSummary(r)}
      <Arrow label="then" />
      {actionOf(r.action).label}
    </>
  );
}

type Row = { field: WafField; name: string; op: string; text: string; list: string[] };
const toRow = (c: WafCondition): Row => ({
  field: c.field,
  name: c.name ?? "",
  op: `${c.negate ? "!" : ""}${c.op}`,
  text: listField(c.field) || c.op === "in" ? "" : (c.values[0] ?? ""),
  list: listField(c.field) || c.op === "in" ? c.values : [],
});
const fromRow = (r: Row): WafCondition => {
  const negate = r.op.startsWith("!");
  const op = r.op.replace("!", "") as WafOperator;
  const values = listField(r.field) || op === "in" ? r.list : [r.text];
  return { field: r.field, ...(r.name ? { name: r.name.trim() } : {}), op, values, ...(negate ? { negate } : {}) };
};
const blank = (field: WafField = "path"): Row => ({ field, name: "", op: opsFor(field)[field === "path" ? 2 : 0], text: "", list: [] });

const cleanMethod = (raw: string) => {
  const t = raw.trim().toUpperCase();
  return /^[A-Z]{1,20}$/.test(t) ? t : null;
};
const cleanAny = (raw: string) => (raw.length && raw.length <= 256 && !/[\u0000-\u001f\u007f]/.test(raw) ? raw : null);

/** What's wrong with a condition, if anything, in a few words. */
function rowError(r: Row): string | null {
  const op = r.op.replace("!", "");
  if ((r.field === "header" || r.field === "cookie") && !/^[A-Za-z0-9!#$%&'*+.^_`|~-]{1,64}$/.test(r.name.trim()))
    return r.name.trim() ? `That isn't a ${r.field} name.` : `Name the ${r.field}.`;
  if (listField(r.field) || op === "in") {
    if (!r.list.length) return r.field === "country" ? "Pick a country." : r.field === "method" ? "Pick a method." : "Add a value.";
    if (op === "equals" && r.list.length > 1) return "Pick one, or use “is in”.";
    return null;
  }
  if (!r.text) return "Add a value.";
  if (r.text.length > 256) return "Up to 256 characters.";
  if (op === "regex") {
    try {
      jsRegex(r.text);
    } catch (e) {
      // "Invalid regular expression: /(a/: Unterminated group" → the reason.
      return `Regex: ${String((e as Error).message).split(": ").pop()}`;
    }
  }
  return null;
}

export type RuleDraft = { name: string; match: "all" | "any"; conditions: WafCondition[]; action: WafAction };

/** A name from what the rule does, until one's typed: "Block /wp-admin". */
function autoNameOf(action: WafAction, rows: Row[], match: "all" | "any") {
  const what = rows
    .slice(0, 2)
    .map((r) => {
      const vals = listField(r.field) || r.op.replace("!", "") === "in" ? r.list : r.text ? [r.text] : [];
      const shown = r.field === "country" ? vals.map((v) => countryName(v) || v) : vals;
      const neg = r.op.startsWith("!") ? "not " : "";
      return shown.length ? `${neg}${shown.slice(0, 3).join(", ")}${shown.length > 3 ? "…" : ""}` : "";
    })
    .filter(Boolean)
    .join(match === "any" ? " or " : " ");
  return what ? `${actionOf(action).label} ${what}`.slice(0, 80) : "";
}

function ValueInput({
  row,
  onChange,
  label,
  onEnterEmpty,
}: {
  row: Row;
  onChange: (p: Partial<Row>) => void;
  label: string;
  onEnterEmpty?: () => void;
}) {
  const op = row.op.replace("!", "");
  if (row.field === "country")
    return (
      <CountryPicker value={row.list} onChange={(list) => onChange({ list })} label={label} max={op === "equals" ? 1 : 50} onEnterEmpty={onEnterEmpty} />
    );
  if (row.field === "method") return <MethodChips value={row.list} onChange={(list) => onChange({ list })} single={op === "equals"} />;
  if (op === "in")
    return (
      <div className="min-w-[12rem] flex-1">
        <TagInput
          aria-label={label}
          value={row.list}
          onChange={(list) => onChange({ list })}
          placeholder={PLACEHOLDER[row.field]}
          clean={row.field === "ip" ? cleanIp : row.field === "host" ? (v) => cleanAny(v.trim().toLowerCase()) : cleanAny}
          invalid={row.field === "ip" ? "An address or range, like 203.0.113.0/24." : "Up to 256 characters."}
          max={50}
          onEnterEmpty={onEnterEmpty}
        />
      </div>
    );
  return (
    <div className="min-w-[12rem] flex-1">
      <Input
        aria-label={label}
        mono
        value={row.text}
        maxLength={256}
        placeholder={op === "regex" ? "^/api/v[0-9]+/" : PLACEHOLDER[row.field]}
        onChange={(e) => onChange({ text: e.target.value })}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.metaKey && !e.ctrlKey) {
            e.preventDefault();
            onEnterEmpty?.();
          }
        }}
      />
    </div>
  );
}

/** Try the rule on a request, the way pyo-waf would (not shown yet). */
export function TryIt({ rule, action, me }: { rule: TestRule; action: WafAction; me: { ip: string; country: string } | null }) {
  const [path, setPath] = useState("/wp-admin/");
  const [method, setMethod] = useState("GET");
  const [ip, setIp] = useState(me?.ip ?? "");
  const [ua, setUa] = useState("Mozilla/5.0");
  useEffect(() => {
    if (me?.ip && !ip) setIp(me.ip);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [me?.ip]);
  const country = me && ip === me.ip ? me.country : "";
  const hit = rule.conditions.length ? matches(rule, { method, host: "", uri: path || "/", headers: { "user-agent": ua }, client: ip.trim(), country }) : false;
  const act = actionOf(action);
  return (
    <div className="space-y-3 rounded-lg border border-border p-3">
      <div className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_110px]">
        <Input aria-label="Test path" mono value={path} onChange={(e) => setPath(e.target.value)} placeholder="/path?query" />
        <Select aria-label="Test method" value={method} onChange={(e) => setMethod(e.target.value)}>
          {METHODS.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
        </Select>
        <Input aria-label="Test IP address" mono value={ip} onChange={(e) => setIp(e.target.value)} placeholder="203.0.113.7" />
        <span className="flex items-center gap-1.5 text-[12px] text-muted-foreground">
          {country ? (
            <>
              <Flag country={country} size={12} /> {country}
            </>
          ) : (
            "Country unknown"
          )}
        </span>
        <div className="sm:col-span-2">
          <Input aria-label="Test user agent" value={ua} onChange={(e) => setUa(e.target.value)} placeholder="User agent" />
        </div>
      </div>
      <p aria-live="polite" className="text-[12.5px]">
        {hit ? (
          <>
            Matches
            <Arrow label="then" />
            <Badge tone={act.tone}>{act.label}</Badge>
          </>
        ) : (
          <span className="text-muted-foreground">No match</span>
        )}
      </p>
    </div>
  );
}

function WafRuleDialog({
  open,
  initial,
  editing,
  onClose,
  onSave,
}: {
  open: boolean;
  initial: Partial<RuleDraft> | null;
  editing: boolean;
  onClose: () => void;
  onSave: (r: RuleDraft) => Promise<string | null>;
}) {
  const [name, setName] = useState("");
  const [match, setMatch] = useState<"all" | "any">("all");
  const [rows, setRows] = useState<Row[]>([blank()]);
  const [action, setAction] = useState<WafAction>("block");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const fill = (d: Omit<RuleDraft, "name"> & { name?: string }) => {
    setName(d.name ?? "");
    setMatch(d.match);
    setRows(d.conditions.length ? d.conditions.map(toRow) : [blank()]);
    setAction(d.action);
    setError(null);
  };
  useEffect(() => {
    if (!open) return;
    fill({ name: initial?.name ?? "", match: initial?.match ?? "all", conditions: initial?.conditions ?? [], action: initial?.action ?? "block" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, initial]);
  const set = (i: number, patch: Partial<Row>) =>
    setRows(
      rows.map((r, j) => {
        if (j !== i) return r;
        const next = { ...r, ...patch };
        if (patch.field && patch.field !== r.field) {
          // A field takes only some operators; values start afresh.
          if (!opsFor(next.field).includes(next.op)) next.op = opsFor(next.field)[next.field === "path" ? 2 : 0];
          next.list = [];
          next.text = "";
          next.name = "";
        }
        const isList = (x: Row) => listField(x.field) || x.op.replace("!", "") === "in";
        if (patch.op && isList(next) && !isList(r) && !next.list.length && next.text) next.list = [next.text];
        if (patch.op && !isList(next) && isList(r) && !next.text && next.list.length) next.text = next.list[0];
        return next;
      }),
    );
  const addRow = () => {
    if (rows.length >= 10) return;
    setRows([...rows, blank()]);
  };
  const errors = rows.map(rowError);
  const firstBad = errors.findIndex(Boolean);
  const autoName = autoNameOf(action, rows, match);
  const finalName = (name || autoName).trim();
  const blocked = firstBad >= 0 ? (rows.length > 1 ? `Condition ${firstBad + 1}: ${errors[firstBad]}` : errors[firstBad]) : !finalName ? "Name the rule." : null;
  const conditions = rows.map(fromRow);
  return (
    <RuleDialog
      open={open}
      onClose={onClose}
      title={editing ? "Edit WAF rule" : "Add a WAF rule"}
      name={name}
      autoName={autoName}
      onName={setName}
      actions={ACTIONS}
      action={action}
      onAction={setAction}
      submitLabel={editing ? "Save" : "Deploy"}
      busy={busy}
      blocked={blocked}
      error={error}
      sentence={firstBad < 0 ? `If ${sentenceOf({ match, conditions })}` : null}
      onSubmit={async () => {
        setBusy(true);
        const err = await onSave({ name: finalName, match, conditions, action });
        setBusy(false);
        if (err) setError(err);
        else onClose();
      }}
      conditions={
        <>
          <ul className="space-y-2">
            {rows.map((r, i) => (
              <li key={i} className="space-y-1">
                {i > 0 ? (
                  <button
                    type="button"
                    onClick={() => setMatch(match === "all" ? "any" : "all")}
                    aria-label={`Conditions are combined with ${match === "all" ? "and" : "or"}. Switch to ${match === "all" ? "or" : "and"}`}
                    className="rounded-md border border-border px-2 py-0.5 text-[11px] font-medium uppercase tracking-wide text-muted-foreground hover:border-border-strong hover:text-foreground"
                  >
                    {match === "all" ? "and" : "or"}
                  </button>
                ) : null}
                <div className="flex flex-wrap items-start gap-2">
                  <FieldPicker label={`Condition ${i + 1} field`} value={r.field} groups={FIELD_GROUPS} onChange={(field) => set(i, { field })} />
                  {r.field === "header" || r.field === "cookie" ? (
                    <div className="w-[130px]">
                      <Input
                        aria-label={`Condition ${i + 1} ${r.field} name`}
                        mono
                        value={r.name}
                        placeholder={r.field === "header" ? "X-Api-Key" : "session"}
                        onChange={(e) => set(i, { name: e.target.value })}
                      />
                    </div>
                  ) : null}
                  <Select aria-label={`Condition ${i + 1} operator`} className="w-[160px]" value={r.op} onChange={(e) => set(i, { op: e.target.value })}>
                    {(opsFor(r.field).includes(r.op) ? opsFor(r.field) : [...opsFor(r.field), r.op]).map((o) => (
                      <option key={o} value={o}>
                        {opLabel(r.field, o)}
                      </option>
                    ))}
                  </Select>
                  <ValueInput
                    key={`${i}-${r.field}`}
                    row={r}
                    label={`Condition ${i + 1} value`}
                    onChange={(p) => set(i, p)}
                    onEnterEmpty={i === rows.length - 1 ? addRow : undefined}
                  />
                  {rows.length > 1 ? (
                    <Button variant="ghost" size="icon" aria-label={`Remove condition ${i + 1}`} onClick={() => setRows(rows.filter((_, j) => j !== i))}>
                      <XIcon size={14} />
                    </Button>
                  ) : null}
                </div>
                {errors[i] && (r.text || r.list.length || r.name || i !== rows.length - 1) ? <p className="text-[12px] text-destructive">{errors[i]}</p> : null}
              </li>
            ))}
          </ul>
          {rows.length < 10 ? (
            <Button variant="ghost" size="xs" onClick={addRow}>
              <PlusIcon size={12} /> Add condition
            </Button>
          ) : null}
        </>
      }
    />
  );
}

function RuleList({
  rules,
  hits,
  editable,
  onEdit,
  onMove,
  onRemove,
}: {
  rules: WafRule[];
  hits: Map<number, number> | null | "loading";
  editable: boolean;
  onEdit?: (r: WafRule) => void;
  onMove?: (from: number, to: number) => void;
  onRemove?: (r: WafRule) => void;
}) {
  return (
    <ul className="list-rows">
      {rules.map((r, i) => (
        <RuleRow
          key={r.id}
          name={r.name}
          summary={<>If {conditionsSummary(r)}</>}
          action={actionOf(r.action)}
          dim={!r.enabled}
          meta={
            !r.enabled ? (
              "Off"
            ) : hits === "loading" ? (
              <Skeleton className="h-3 w-12" />
            ) : hits ? (
              <span title="Matches in the last 24 hours">{compact(hits.get(r.number) ?? 0)} in 24h</span>
            ) : null
          }
          controls={
            editable ? (
              <Menu
                label={`Actions for ${r.name}`}
                items={[
                  { label: "Edit", icon: <PencilSimpleIcon size={14} />, onSelect: () => onEdit?.(r) },
                  {
                    label: r.enabled ? "Turn off" : "Turn on",
                    icon: <PowerIcon size={14} />,
                    onSelect: async () => {
                      const res = await api.updateWafRule(r.id, { enabled: !r.enabled });
                      if ("error" in res) toast({ title: "Not saved", description: res.error, tone: "error" });
                    },
                  },
                  ...(rules.length > 1
                    ? [
                        { label: "Move up", icon: <ArrowUpIcon size={14} />, disabled: i === 0, onSelect: () => onMove?.(i, i - 1) },
                        { label: "Move down", icon: <ArrowDownIcon size={14} />, disabled: i === rules.length - 1, onSelect: () => onMove?.(i, i + 1) },
                      ]
                    : []),
                  { type: "separator" as const },
                  { label: "Delete", icon: <TrashIcon size={14} />, danger: true, onSelect: () => onRemove?.(r) },
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
      ))}
    </ul>
  );
}

/** The WAF on a service's Firewall tab: its switch, settings and custom rules. */
export function WafSection({ app }: { app: Application }) {
  const s = usePyo();
  const stored = wafMode(app) !== "off";
  // Where a failed save left the switch, until the store agrees again.
  const [rollback, setRollback] = useState<boolean | null>(null);
  useEffect(() => {
    if (rollback !== null && stored === rollback) setRollback(null);
  }, [stored, rollback]);
  const on = rollback ?? stored;
  const bodyId = useId();
  const editable = useCanEdit(app);
  const rules = s.wafRules.filter((r) => r.serviceId === app.id);
  const [dialog, setDialog] = useState<{ initial: Partial<RuleDraft> | null; rule: WafRule | null } | null>(null);
  const [removing, setRemoving] = useState<WafRule | null>(null);
  const me = useQuery("firewall-me", fetchMyAddress, { refreshMs: 0 });
  const hitsQ = useQuery(
    `waf-rule-hits:${app.id}:${on}`,
    () => (on ? fetchWafHits(app.id, "24h") : Promise.resolve(null)),
    { refreshMs: 60_000 },
  );
  const data = hitsQ.status === "success" ? hitsQ.data : null;
  const hits: Map<number, number> | null | "loading" =
    hitsQ.status === "loading" ? "loading" : data ? new Map((data.custom ?? []).map((h) => [Number(h.rule), h.count])) : null;
  const move = async (from: number, to: number) => {
    const ids = rules.map((r) => r.id);
    [ids[from], ids[to]] = [ids[to], ids[from]];
    const res = await api.reorderWafRules(app.id, ids);
    if ("error" in res) toast({ title: "Not moved", description: res.error, tone: "error" });
  };
  const templates: { label: string; draft: Partial<RuleDraft> }[] = [
    {
      label: "Block /wp-admin",
      draft: { name: "Block wp-admin", match: "all", conditions: [{ field: "path", op: "startsWith", values: ["/wp-admin"] }], action: "block" },
    },
    {
      label: "Allow my IP",
      draft: {
        name: "Allow my IP",
        match: "all",
        conditions: [{ field: "ip", op: "in", values: me.status === "success" && me.data.ip ? [me.data.ip] : [] }],
        action: "allow",
      },
    },
    {
      label: "Block a country",
      draft: { name: "Block a country", match: "all", conditions: [{ field: "country", op: "in", values: [] }], action: "block" },
    },
    {
      label: "Skip a path",
      draft: { name: "Skip OWASP on webhooks", match: "all", conditions: [{ field: "path", op: "startsWith", values: ["/api/webhooks/"] }], action: "skipOwasp" },
    },
  ];
  const addRule = editable ? (
    <Menu
      label="Add rule"
      items={[
        { label: "New rule", icon: <PlusIcon size={14} />, onSelect: () => setDialog({ initial: null, rule: null }) },
        { type: "label" as const, label: "Presets" },
        ...templates.map((t) => ({ label: t.label, onSelect: () => setDialog({ initial: t.draft, rule: null }) })),
      ]}
      trigger={(t) => (
        <Button size="xs" {...t}>
          <PlusIcon size={12} /> Add rule
        </Button>
      )}
    />
  ) : null;
  return (
    <>
      <Layered
        bare
        // The switch stays beside the title on phones; the title wraps instead.
        className="[&>div:first-child]:flex-nowrap"
        title={
          <span className="flex flex-wrap items-baseline gap-x-2.5 gap-y-0.5">
            Web Application Firewall
            {on ? (
              <HitsStat
                status={hitsQ.status === "loading" ? "loading" : hitsQ.status === "error" ? "error" : "success"}
                blocked={data?.blocked ?? 0}
                logged={data?.logged ?? 0}
              />
            ) : null}
          </span>
        }
        action={<WafSwitch app={app} on={on} controls={bodyId} disabled={!editable} onRollback={setRollback} />}
      >
        {/* Off, only the header shows; on, the settings open below it. */}
        <Reveal open={on} id={bodyId}>
          <div className="min-w-0 overflow-hidden rounded-[14px] border border-card-border bg-card shadow-card">
            {on && app.wafState?.state === "failed" ? (
              <div className="px-4 pt-3">
                <Callout tone="warning">{app.wafState.message || "The WAF couldn't start on this cluster."}</Callout>
              </div>
            ) : null}
            <ServiceSettings key={app.id} app={app} disabled={!editable} />
            <Section title="Custom rules" aside="Run first" action={rules.length ? addRule : null}>
              {rules.length ? (
                <RuleList
                  rules={rules}
                  hits={on ? hits : null}
                  editable={editable}
                  onEdit={(r) => setDialog({ initial: r, rule: r })}
                  onMove={(a, b) => void move(a, b)}
                  onRemove={setRemoving}
                />
              ) : (
                <EmptyState compact title="No custom rules" action={addRule} />
              )}
            </Section>
          </div>
        </Reveal>
      </Layered>
      <WafRuleDialog
        open={dialog !== null}
        initial={dialog?.initial ?? null}
        editing={!!dialog?.rule}
        onClose={() => setDialog(null)}
        onSave={async (r) => {
          const res = dialog?.rule
            ? await api.updateWafRule(dialog.rule.id, r)
            : await api.addWafRule({ serviceId: app.id, ...r });
          if ("error" in res) return res.error;
          toast({ title: dialog?.rule ? "Rule saved" : "Rule added", description: "Applies within about a minute." });
          return null;
        }}
      />
      <Dialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Delete ${removing?.name ?? "rule"}?`}
        footer={
          <>
            <Button variant="ghost" size="sm" onClick={() => setRemoving(null)}>
              Cancel
            </Button>
            <Button
              variant="danger"
              size="sm"
              onClick={async () => {
                const r = removing;
                setRemoving(null);
                if (r) await api.removeWafRule(r.id);
              }}
            >
              Delete
            </Button>
          </>
        }
      >
        <p className="text-[13px] text-muted-foreground">{removing ? ruleSummary(removing) : ""}</p>
      </Dialog>
    </>
  );
}
