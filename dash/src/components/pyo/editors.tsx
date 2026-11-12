import { LearnMore } from "~/components/ui/LearnMore";
import {
  PlusIcon,
  TrashIcon,
  WarningIcon,
} from "@phosphor-icons/react";
import {
  useEffect,
  useRef,
  useState,
  type ClipboardEvent,
  type KeyboardEvent,
} from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import {
  Checkbox,
  Field,
  Input,
  SecretInput,
  Select,
  Switch,
  Textarea,
} from "~/components/ui/Field";
import {
  draftId,
  gpuError,
  publicPortError,
  MAX_GPUS,
  type DraftEnv,
  type DraftPort,
} from "~/lib/pyo/draft";

export { gpuError, publicPortError };
import { cpu, mem } from "~/lib/pyo/format";
import { envFromPaste, lastWins, SECRET_HINT, type EnvEntry } from "~/lib/pyo/dotenv";
import { toast } from "~/lib/pyo/toast";
import type {
  Cluster,
  Exposure,
  HealthCheck,
  PortProtocol,
  Resources,
} from "~/lib/pyo/types";

// Editors shared by the deploy wizard and the application settings tabs, so
// both paths edit configuration the same way.

// ── Environment variables ──────────────────────────────────────────────────

export function EnvEditor({
  rows,
  onChange,
  errors = {},
  onReveal,
}: {
  rows: DraftEnv[];
  onChange: (rows: DraftEnv[]) => void;
  errors?: Record<string, string>;
  /** Fetch a stored secret's value (admins); absent, its eye is disabled. */
  onReveal?: (r: DraftEnv) => Promise<string | null>;
}) {
  // Revealed values are shown, not put in the rows: revealing isn't an edit.
  const [revealed, setRevealed] = useState<Record<string, string>>({});
  const [revealing, setRevealing] = useState<string | null>(null);
  const [shown, setShown] = useState<Record<string, boolean>>({});
  // The trailing empty row; typing into it makes it a row of its own.
  const [blank, setBlank] = useState(() => ({ id: draftId(), secret: false }));
  // Rows added here get a Type suggested from their key until one is picked.
  const [fresh, setFresh] = useState<Record<string, true>>({});
  const [picked, setPicked] = useState<Record<string, true>>({});
  const [conflict, setConflict] = useState<PendingPaste | null>(null);
  const list = useRef<HTMLDivElement>(null);
  const focusBlank = useRef(false);
  useEffect(() => {
    if (!focusBlank.current) return;
    focusBlank.current = false;
    list.current
      ?.querySelector<HTMLInputElement>(`[data-env-key="${blank.id}"]`)
      ?.focus();
  });
  const all: DraftEnv[] = [
    ...rows,
    { id: blank.id, key: "", value: "", secret: blank.secret },
  ];
  const forget = (id: string) => {
    setShown(({ [id]: _, ...rest }) => rest);
    setRevealed(({ [id]: _, ...rest }) => rest);
  };
  const set = (id: string, patch: Partial<DraftEnv>) => {
    if (id !== blank.id) {
      onChange(rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));
      return;
    }
    onChange([...rows, { id, key: "", value: "", secret: blank.secret, ...patch }]);
    setFresh((m) => ({ ...m, [id]: true }));
    setBlank({ id: draftId(), secret: false });
  };
  const setKey = (r: DraftEnv, key: string) =>
    set(
      r.id,
      (fresh[r.id] || r.id === blank.id) && !picked[r.id]
        ? { key, secret: SECRET_HINT.test(key) }
        : { key },
    );

  // Adds pasted entries: the first fills the row pasted into if it's empty,
  // existing keys are overwritten unless `skip` has them.
  const addPasted = (p: PendingPaste, skip: Set<string>) => {
    const next = [...rows];
    const index = new Map(rows.map((r, i) => [r.key, i]));
    const target =
      p.targetId === blank.id
        ? null
        : rows.find((r) => r.id === p.targetId && !r.key && !r.value && !r.hasValue);
    let fill = p.targetId === blank.id || !!target;
    let blankUsed = false;
    const made: Record<string, true> = {};
    let added = 0;
    let updated = 0;
    for (const [key, value] of p.entries) {
      const at = index.get(key);
      if (at !== undefined && next[at].id !== target?.id) {
        if (skip.has(key)) continue;
        next[at] = { ...next[at], value };
        forget(next[at].id);
        updated++;
        continue;
      }
      added++;
      if (fill && target) {
        next[next.indexOf(target)] = {
          ...target,
          key,
          value,
          secret: picked[target.id] ? target.secret : SECRET_HINT.test(key),
        };
      } else if (fill) {
        next.push({
          id: blank.id,
          key,
          value,
          secret: picked[blank.id] ? blank.secret : SECRET_HINT.test(key),
        });
        made[blank.id] = true;
        blankUsed = true;
      } else {
        const id = draftId();
        next.push({ id, key, value, secret: SECRET_HINT.test(key) });
        made[id] = true;
      }
      fill = false;
    }
    onChange(next);
    setFresh((m) => ({ ...m, ...made }));
    if (blankUsed) setBlank({ id: draftId(), secret: false });
    focusBlank.current = true;
    const n = (k: number) => `${k} variable${k === 1 ? "" : "s"}`;
    const notes = [
      added && updated ? `Updated ${updated}.` : "",
      skip.size ? `Skipped ${skip.size}.` : "",
      p.repeated.length
        ? `${p.repeated.join(", ")} repeated. The last value is used.`
        : "",
      p.invalid ? `Ignored ${p.invalid} line${p.invalid === 1 ? "" : "s"}.` : "",
    ].filter(Boolean);
    toast({
      tone: "success",
      title: added
        ? `Added ${n(added)}`
        : updated
          ? `Updated ${n(updated)}`
          : "Nothing added",
      description: notes.join(" ") || undefined,
    });
  };

  const onPaste =
    (r: DraftEnv, into: "key" | "value") =>
    (e: ClipboardEvent<HTMLInputElement>) => {
      const parsed = envFromPaste(e.clipboardData.getData("text/plain"), into);
      if (!parsed) return;
      e.preventDefault();
      const { entries, repeated } = lastWins(parsed.entries);
      const p: PendingPaste = {
        targetId: r.id,
        entries,
        repeated,
        invalid: parsed.invalid.length,
      };
      const existing = new Set(
        rows.filter((x) => x.id !== r.id || x.key).map((x) => x.key),
      );
      if (entries.some(([k]) => existing.has(k))) setConflict(p);
      else addPasted(p, new Set());
    };

  return (
    <div>
      <div
        ref={list}
        className="space-y-2"
        role="list"
        aria-label="Environment variables"
      >
        <div className="hidden grid-cols-[minmax(0,1fr)_minmax(0,1.75fr)_88px_32px] gap-2 px-0.5 text-[11.5px] font-medium text-muted-foreground sm:grid">
          <span>Key</span>
          <span>Value</span>
          <span>Type</span>
          <span />
        </div>
        {all.map((r, i) => {
          const isBlank = r.id === blank.id;
          const name = r.key || (isBlank ? "new variable" : "variable");
          const valueProps = {
            onPaste: onPaste(r, "value"),
            onKeyDown: (e: KeyboardEvent<HTMLInputElement>) => {
              if (e.key !== "Enter" || e.nativeEvent.isComposing) return;
              e.preventDefault();
              const next = all[i + 1];
              if (next)
                list.current
                  ?.querySelector<HTMLInputElement>(`[data-env-key="${next.id}"]`)
                  ?.focus();
            },
          };
          return (
            <div key={r.id} role="listitem">
              <div className="grid grid-cols-[minmax(0,1fr)_88px_32px] gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.75fr)_88px_32px]">
                <Input
                  mono
                  data-env-key={r.id}
                  value={r.key}
                  onChange={(e) =>
                    setKey(r, e.target.value.toUpperCase().replace(/\s/g, "_"))
                  }
                  onPaste={onPaste(r, "key")}
                  placeholder="KEY"
                  aria-label={isBlank ? "New variable name" : "Variable name"}
                  aria-invalid={errors[`env.${r.id}`] ? true : undefined}
                />
                <div className="order-last col-span-3 sm:order-none sm:col-span-1">
                  {r.secret ? (
                    <SecretInput
                      {...valueProps}
                      value={
                        r.value ||
                        (shown[r.id] ? (revealed[r.id] ?? "") : "")
                      }
                      onChange={(e) => {
                        setRevealed(({ [r.id]: _, ...rest }) => rest);
                        set(r.id, { value: e.target.value });
                      }}
                      placeholder={
                        r.hasValue
                          ? "•••••••• Saved"
                          : "Leave empty to generate one"
                      }
                      aria-label={`${r.hasValue && !r.value && revealed[r.id] === undefined ? "New value" : "Value"} of ${name}`}
                      shown={!!shown[r.id]}
                      busy={revealing === r.id}
                      toggleDisabled={
                        r.hasValue && !r.value && !onReveal
                          ? "Owners and admins only"
                          : undefined
                      }
                      onToggle={async () => {
                        if (shown[r.id]) {
                          forget(r.id);
                          return;
                        }
                        if (r.hasValue && !r.value && onReveal) {
                          setRevealing(r.id);
                          const v = await onReveal(r);
                          setRevealing(null);
                          if (v === null) return;
                          setRevealed((m) => ({ ...m, [r.id]: v }));
                        }
                        setShown((m) => ({ ...m, [r.id]: true }));
                      }}
                    />
                  ) : (
                    <Input
                      {...valueProps}
                      mono
                      value={r.value}
                      onChange={(e) => set(r.id, { value: e.target.value })}
                      placeholder="value"
                      aria-label={`Value of ${name}`}
                    />
                  )}
                </div>
                <Select
                  value={r.secret ? "secret" : "plain"}
                  onChange={(e) => {
                    const secret = e.target.value === "secret";
                    setPicked((m) => ({ ...m, [r.id]: true }));
                    if (isBlank) setBlank((b) => ({ ...b, secret }));
                    else
                      set(r.id, {
                        secret,
                        // A revealed secret made plain keeps its value.
                        ...(!secret && !r.value && revealed[r.id] !== undefined
                          ? { value: revealed[r.id] }
                          : {}),
                      });
                  }}
                  heading="Type"
                  aria-label={`Type of ${name}`}
                >
                  <option value="plain">Variable</option>
                  <option value="secret">Secret</option>
                </Select>
                {isBlank ? (
                  <span />
                ) : (
                  <Button
                    variant="ghost"
                    size="icon"
                    onClick={() => onChange(rows.filter((x) => x.id !== r.id))}
                    aria-label={`Remove ${name}`}
                  >
                    <TrashIcon size={14} />
                  </Button>
                )}
              </div>
              {errors[`env.${r.id}`] ? (
                <p className="mt-1 text-[11.5px] text-destructive">
                  {errors[`env.${r.id}`]}
                </p>
              ) : null}
            </div>
          );
        })}
      </div>
      <PasteConflictDialog
        pending={conflict}
        rows={rows}
        visible={(r) =>
          !r.secret ? r.value : shown[r.id] ? r.value || revealed[r.id] : undefined
        }
        onClose={() => setConflict(null)}
        onConfirm={(skip) => {
          if (conflict) addPasted(conflict, skip);
          setConflict(null);
        }}
      />
    </div>
  );
}

type PendingPaste = {
  /** The row pasted into. */
  targetId: string;
  entries: EnvEntry[];
  /** Keys given more than once in the paste. */
  repeated: string[];
  /** Lines that weren't KEY=value. */
  invalid: number;
};

const MASK = "••••••••";

function PasteConflictDialog({
  pending,
  rows,
  visible,
  onClose,
  onConfirm,
}: {
  pending: PendingPaste | null;
  rows: DraftEnv[];
  /** A row's value if it may be shown; undefined keeps it masked. */
  visible: (r: DraftEnv) => string | undefined;
  onClose: () => void;
  onConfirm: (skip: Set<string>) => void;
}) {
  const [skip, setSkip] = useState<Set<string>>(new Set());
  const [last, setLast] = useState(pending);
  if (pending !== last) {
    setLast(pending);
    setSkip(new Set());
  }
  const byKey = new Map(rows.filter((r) => r.key).map((r) => [r.key, r]));
  const dupes = (pending?.entries ?? []).flatMap(([key, value]) => {
    const r = byKey.get(key);
    return r ? [{ key, value, row: r }] : [];
  });
  const overwrite = dupes.length - skip.size;
  const empty = <span className="italic text-muted-foreground">empty</span>;
  return (
    <Dialog
      open={!!pending}
      onClose={onClose}
      title="Some keys already exist"
      description="Checked keys are overwritten. The rest is added either way."
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => onConfirm(new Set(dupes.map((d) => d.key)))}
          >
            Skip all
          </Button>
          <Button
            size="sm"
            disabled={!overwrite}
            onClick={() => onConfirm(skip)}
          >
            {overwrite === dupes.length ? "Overwrite all" : `Overwrite ${overwrite}`}
          </Button>
        </>
      }
    >
      <ul className="divide-y divide-border" aria-label="Existing keys">
        {dupes.map(({ key, value, row }) => {
          const current = visible(row);
          return (
            <li key={key} className="py-2.5 first:pt-0 last:pb-0">
              <Checkbox
                checked={!skip.has(key)}
                onChange={(on) =>
                  setSkip((s) => {
                    const next = new Set(s);
                    if (on) next.delete(key);
                    else next.add(key);
                    return next;
                  })
                }
                label={<span className="font-mono text-[12px]">{key}</span>}
              />
              <div className="mt-1 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-0.5 pl-6 font-mono text-[11.5px]">
                <span className="font-sans text-muted-foreground">Current</span>
                <span className="truncate">
                  {current === undefined ? MASK : current || empty}
                </span>
                <span className="font-sans text-muted-foreground">New</span>
                <span className="truncate">
                  {row.secret ? MASK : value || empty}
                </span>
              </div>
            </li>
          );
        })}
      </ul>
      {pending?.repeated.length ? (
        <p className="mt-3 text-[12px] text-muted-foreground">
          <span className="font-mono">{pending.repeated.join(", ")}</span>{" "}
          repeated in the paste. The last value is used.
        </p>
      ) : null}
    </Dialog>
  );
}

// ── Ports and exposure ─────────────────────────────────────────────────────

export const EXPOSURE_LABEL: Record<Exposure, string> = {
  internal: "Private",
  http: "Website",
  nodeport: "Port on every server",
  loadbalancer: "Public IP",
};

export function exposureOptions(protocol: PortProtocol, cluster?: Cluster) {
  const lb = cluster?.capabilities.loadBalancer !== "none";
  return [
    {
      value: "internal" as const,
      label: EXPOSURE_LABEL.internal,
      disabled: false,
    },
    {
      value: "http" as const,
      label: "Website (needs a domain)",
      disabled: protocol !== "HTTP",
    },
    {
      value: "nodeport" as const,
      label: EXPOSURE_LABEL.nodeport,
      disabled: false,
    },
    {
      value: "loadbalancer" as const,
      label: lb ? EXPOSURE_LABEL.loadbalancer : "Public IP (unavailable here)",
      disabled: !lb,
    },
  ];
}

/**
 * A public TCP or UDP port's gateway settings: on by default; its
 * options (TCP only) are off until set.
 */
function PortProxyOptions({
  port: p,
  onChange,
}: {
  port: DraftPort;
  onChange: (patch: Partial<DraftPort>) => void;
}) {
  const on = p.proxy !== false;
  const udp = p.protocol === "UDP";
  const [cidrs, setCidrs] = useState(p.ipFilter?.cidrs.join("\n") ?? "");
  const off = {
    ipFilter: null,
    connectionLimit: null,
    sendProxyProtocol: null,
    acceptProxyProtocol: false,
  };
  return (
    <div className="mt-2 space-y-3 rounded-md border border-border bg-subtle px-3 py-3">
      <Switch
        checked={on}
        onChange={(v) => onChange(v ? { proxy: true } : { proxy: false, ...off })}
        label="Through the gateway"
        description={
          on
            ? "Adds analytics, IP rules and limits."
            : "Your service sees visitors' addresses directly. No analytics or rules."
        }
      />
      {on ? (
        <div className={`space-y-3 border-t border-border pt-3 ${udp ? "opacity-60" : ""}`}>
          {udp ? (
            <p className="text-[12px] text-muted-foreground">
              Not available for UDP.
            </p>
          ) : null}
          <div className="grid gap-3 md:grid-cols-[180px_minmax(0,1fr)]">
            <Field label="IP rule">
              {(f) => (
                <Select
                  {...f}
                  disabled={udp}
                  value={p.ipFilter?.mode ?? "off"}
                  onChange={(e) => {
                    const mode = e.target.value;
                    onChange({
                      ipFilter:
                        mode === "off"
                          ? null
                          : {
                              mode: mode as "allow" | "deny",
                              cidrs: p.ipFilter?.cidrs ?? [],
                            },
                    });
                  }}
                >
                  <option value="off">Off</option>
                  <option value="allow">Only these addresses</option>
                  <option value="deny">Everyone but these</option>
                </Select>
              )}
            </Field>
            {p.ipFilter ? (
              <Field
                label="Addresses"
                hint="One per line, e.g. 198.51.100.0/24."
              >
                {(f) => (
                  <Textarea
                    {...f}
                    rows={3}
                    className="font-mono text-[12px]"
                    value={cidrs}
                    placeholder={"203.0.113.7\n198.51.100.0/24"}
                    onChange={(e) => {
                      setCidrs(e.target.value);
                      onChange({
                        ipFilter: {
                          mode: p.ipFilter?.mode ?? "allow",
                          cidrs: e.target.value
                            .split(/[\s,]+/)
                            .map((x) => x.trim())
                            .filter(Boolean),
                        },
                      });
                    }}
                  />
                )}
              </Field>
            ) : (
              <span />
            )}
          </div>
          <div className="grid gap-3 md:grid-cols-[180px_minmax(0,1fr)]">
            <Field label="Connection limit">
              {(f) => (
                <Select
                  {...f}
                  disabled={udp}
                  value={p.connectionLimit ? "on" : "off"}
                  onChange={(e) =>
                    onChange({
                      connectionLimit: e.target.value === "on" ? 1000 : null,
                    })
                  }
                >
                  <option value="off">Off</option>
                  <option value="on">Limit per port</option>
                </Select>
              )}
            </Field>
            {p.connectionLimit ? (
              <Field
                label="Open connections at most"
                hint="For everyone together."
                className="max-w-[260px]"
              >
                {(f) => (
                  <Input
                    {...f}
                    mono
                    type="number"
                    min={1}
                    max={1000000}
                    value={p.connectionLimit ?? ""}
                    onChange={(e) =>
                      onChange({
                        connectionLimit: e.target.value
                          ? Number(e.target.value)
                          : null,
                      })
                    }
                  />
                )}
              </Field>
            ) : (
              <span />
            )}
          </div>
          <Switch
            checked={!!p.acceptProxyProtocol}
            disabled={udp}
            onChange={(v) => onChange({ acceptProxyProtocol: v })}
            label="Accept PROXY protocol from clients"
            description={<>Only if all traffic comes through a load balancer that sends it. <LearnMore page="gateway" /></>}
          />
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0 flex-1">
              <Switch
                checked={!!p.sendProxyProtocol}
                disabled={udp}
                onChange={(v) => onChange({ sendProxyProtocol: v ? "V2" : null })}
                label="Send PROXY protocol to your service"
                description="Your service must expect it, or connections fail."
              />
            </div>
            {p.sendProxyProtocol ? (
              <Select
                className="w-[110px]"
                aria-label="PROXY protocol version"
                value={p.sendProxyProtocol}
                onChange={(e) =>
                  onChange({ sendProxyProtocol: e.target.value as "V1" | "V2" })
                }
              >
                <option value="V2">Version 2</option>
                <option value="V1">Version 1</option>
              </Select>
            ) : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}

export function PortEditor({
  rows,
  onChange,
  errors = {},
  cluster,
  conflicts = {},
}: {
  rows: DraftPort[];
  onChange: (rows: DraftPort[]) => void;
  errors?: Record<string, string>;
  cluster?: Cluster;
  conflicts?: Record<string, string>;
}) {
  const set = (id: string, patch: Partial<DraftPort>) =>
    onChange(rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));
  // A public port only means something for a port exposed on the servers.
  const showPublic = rows.some(
    (r) => r.exposure === "nodeport" || r.exposure === "loadbalancer",
  );
  const cols = showPublic
    ? "md:grid-cols-[minmax(0,1fr)_96px_88px_minmax(0,1.3fr)_96px_32px]"
    : "md:grid-cols-[minmax(0,1fr)_96px_88px_minmax(0,1.3fr)_32px]";
  return (
    <div>
      {rows.length ? (
        <div className="space-y-2">
          <div
            className={`hidden gap-2 px-0.5 text-[11.5px] font-medium text-muted-foreground md:grid ${cols}`}
          >
            <span>Name</span>
            <span>Port</span>
            <span>Protocol</span>
            <span>Exposure</span>
            {showPublic ? <span>Public port</span> : null}
            <span />
          </div>
          {rows.map((r) => {
            const err = errors[`port.${r.id}`];
            const conflict = conflicts[r.id];
            return (
              <div key={r.id}>
                <div className={`grid grid-cols-2 gap-2 ${cols}`}>
                  <Input
                    mono
                    value={r.name}
                    onChange={(e) =>
                      set(r.id, { name: e.target.value.toLowerCase() })
                    }
                    aria-label="Port name"
                    placeholder="http"
                  />
                  <Input
                    mono
                    type="number"
                    min={1}
                    max={65535}
                    value={r.containerPort || ""}
                    onChange={(e) =>
                      set(r.id, { containerPort: Number(e.target.value) })
                    }
                    aria-label="Container port"
                  />
                  <Select
                    value={r.protocol}
                    onChange={(e) => {
                      const protocol = e.target.value as PortProtocol;
                      set(r.id, {
                        protocol,
                        exposure:
                          protocol !== "HTTP" && r.exposure === "http"
                            ? "internal"
                            : r.exposure,
                      });
                    }}
                    aria-label="Protocol"
                  >
                    <option value="HTTP">HTTP</option>
                    <option value="TCP">TCP</option>
                    <option value="UDP">UDP</option>
                  </Select>
                  <Select
                    value={r.exposure}
                    onChange={(e) =>
                      set(r.id, {
                        exposure: e.target.value as Exposure,
                        publicPort: undefined,
                      })
                    }
                    aria-label="Exposure"
                  >
                    {exposureOptions(r.protocol, cluster).map((o) => (
                      <option
                        key={o.value}
                        value={o.value}
                        disabled={o.disabled}
                      >
                        {o.label}
                      </option>
                    ))}
                  </Select>
                  {showPublic ? (
                    <Input
                      mono
                      type="number"
                      value={r.publicPort ?? ""}
                      disabled={
                        r.exposure === "internal" || r.exposure === "http"
                      }
                      placeholder={
                        r.exposure === "nodeport"
                          ? "auto"
                          : r.exposure === "loadbalancer"
                            ? String(r.containerPort)
                            : "—"
                      }
                      onChange={(e) =>
                        set(r.id, {
                          publicPort: e.target.value
                            ? Number(e.target.value)
                            : undefined,
                        })
                      }
                      aria-label="Public port"
                    />
                  ) : null}
                  <Button
                    variant="ghost"
                    size="icon"
                    onClick={() => onChange(rows.filter((x) => x.id !== r.id))}
                    aria-label={`Remove port ${r.name}`}
                  >
                    <TrashIcon size={14} />
                  </Button>
                </div>
                {r.exposure === "loadbalancer" ? (
                  <PortProxyOptions port={r} onChange={(patch) => set(r.id, patch)} />
                ) : null}
                {err ? (
                  <p className="mt-1 text-[11.5px] text-destructive">{err}</p>
                ) : null}
                {!err && conflict ? (
                  <p className="mt-1 flex items-center gap-1 text-[11.5px] text-warning">
                    <WarningIcon size={12} /> {conflict}
                  </p>
                ) : null}
              </div>
            );
          })}
        </div>
      ) : (
        <p className="rounded-md border border-dashed border-border px-3 py-4 text-center text-[12.5px] text-muted-foreground">
          No ports. Nothing can connect to it.
        </p>
      )}
      {cluster?.publicTraffic === false &&
      rows.some((r) => r.exposure !== "internal") ? (
        <p className="mt-2 text-[11.5px] text-muted-foreground">
          Public traffic is off for this cluster.
        </p>
      ) : null}
      <Button
        variant="secondary"
        size="xs"
        className="mt-3"
        onClick={() =>
          onChange([
            ...rows,
            {
              id: draftId(),
              name: rows.length ? `port-${rows.length + 1}` : "http",
              containerPort: 8080,
              protocol: "HTTP",
              exposure: "internal",
            },
          ])
        }
      >
        <PlusIcon size={12} /> Add port
      </Button>
    </div>
  );
}

// ── Resources and health checks ────────────────────────────────────────────

const CPU_PRESETS = [100, 250, 500, 1000, 2000, 4000];
const MEM_PRESETS = [64, 128, 256, 512, 1024, 2048, 4096, 8192];

function Preset({
  value,
  onChange,
  presets,
  fmt,
  label,
  error,
  id,
  ...aria
}: {
  value: number;
  onChange: (v: number) => void;
  presets: number[];
  fmt: (v: number) => string;
  label: string;
  error?: string;
  id: string;
  "aria-invalid"?: boolean;
  "aria-describedby"?: string;
}) {
  const options = presets.includes(value)
    ? presets
    : [...presets, value].sort((a, b) => a - b);
  return (
    <Select
      id={id}
      value={value}
      onChange={(e) => onChange(Number(e.target.value))}
      aria-label={label}
      heading={label}
      {...aria}
      aria-invalid={error ? true : undefined}
    >
      {options.map((p) => (
        <option key={p} value={p}>
          {fmt(p)}
        </option>
      ))}
    </Select>
  );
}

/**
 * Whole NVIDIA GPUs for a service. Shown when its cluster has a GPU server
 * (or the service already asks for some).
 */
export function GpuField({
  value,
  onChange,
  cluster,
  autoscaled = false,
  error,
}: {
  value: number;
  onChange: (n: number) => void;
  cluster: Cluster | undefined;
  autoscaled?: boolean;
  error?: string;
}) {
  const g = cluster?.capabilities.gpu;
  if (!g?.servers && !value) return null;
  const max = g?.maxPerServer ?? 0;
  return (
    <div className="space-y-2">
      <Field
        label="GPUs"
        hint={
          autoscaled
            ? "Turn autoscaling off to use GPUs."
            : max
              ? `Whole NVIDIA GPUs on one server. ${cluster?.name}: ${Math.max(0, (g?.total ?? 0) - (g?.used ?? 0))} free of ${g?.total ?? 0}, up to ${max} on one server.`
              : `${cluster?.name}'s GPU servers aren't ready yet (NVIDIA device plugin: ${g?.devicePlugin ?? "not running"}).`
        }
        error={error}
        className="max-w-[260px]"
      >
        {(p) => (
          <Select
            {...p}
            value={String(value)}
            disabled={autoscaled && !value}
            onChange={(e) => onChange(Number(e.target.value))}
          >
            <option value="0">None</option>
            {Array.from({ length: MAX_GPUS }, (_, i) => i + 1).map((n) => (
              <option key={n} value={n} disabled={n > max && n !== value}>
                {n} GPU{n === 1 ? "" : "s"}
                {n > max ? " (not on one server here)" : ""}
              </option>
            ))}
          </Select>
        )}
      </Field>
      {value > 0 ? (
        <p className="flex gap-1.5 text-[12px] leading-5 text-muted-foreground">
          <WarningIcon size={14} className="mt-0.5 shrink-0 text-warning" />
          <span>
            Deploys cause brief downtime. No autoscaling. <LearnMore page="gpus" />
          </span>
        </p>
      ) : null}
    </div>
  );
}

export function ResourceFields({
  value,
  onChange,
  errors = {},
  cluster,
  autoscaled = false,
}: {
  value: Resources;
  onChange: (r: Resources) => void;
  errors?: Record<string, string>;
  /** Where it runs: offers GPUs when it has GPU servers. */
  cluster?: Cluster;
  autoscaled?: boolean;
}) {
  const set = (patch: Partial<Resources>) => onChange({ ...value, ...patch });
  return (
    <div className="space-y-4">
    <div className="grid gap-4 sm:grid-cols-2">
      <Field
        label="Guaranteed CPU"
        hint="Reserved for each replica."
        error={errors.cpuRequest}
      >
        {(p) => (
          <Preset
            {...p}
            label="Guaranteed CPU"
            value={value.cpuRequest}
            onChange={(v) =>
              set({ cpuRequest: v, cpuLimit: Math.max(v, value.cpuLimit) })
            }
            presets={CPU_PRESETS}
            fmt={cpu}
            error={errors.cpuRequest}
          />
        )}
      </Field>
      <Field
        label="Max CPU"
        hint="Throttled above this."
        error={errors.cpuLimit}
      >
        {(p) => (
          <Preset
            {...p}
            label="Max CPU"
            value={value.cpuLimit}
            onChange={(v) => set({ cpuLimit: v })}
            presets={CPU_PRESETS}
            fmt={cpu}
            error={errors.cpuLimit}
          />
        )}
      </Field>
      <Field
        label="Guaranteed memory"
        hint="Reserved for each replica."
        error={errors.memoryRequest}
      >
        {(p) => (
          <Preset
            {...p}
            label="Guaranteed memory"
            value={value.memoryRequest}
            onChange={(v) =>
              set({
                memoryRequest: v,
                memoryLimit: Math.max(v, value.memoryLimit),
              })
            }
            presets={MEM_PRESETS}
            fmt={mem}
            error={errors.memoryRequest}
          />
        )}
      </Field>
      <Field
        label="Max memory"
        hint="Restarts if it goes over."
        error={errors.memoryLimit}
      >
        {(p) => (
          <Preset
            {...p}
            label="Max memory"
            value={value.memoryLimit}
            onChange={(v) => set({ memoryLimit: v })}
            presets={MEM_PRESETS}
            fmt={mem}
            error={errors.memoryLimit}
          />
        )}
      </Field>
    </div>
      <GpuField
        value={value.gpu ?? 0}
        onChange={(gpu) => set({ gpu })}
        cluster={cluster}
        autoscaled={autoscaled}
        error={errors.gpu}
      />
    </div>
  );
}

export function HealthCheckFields({
  value,
  onChange,
  errors = {},
  label = "Health check",
}: {
  value: HealthCheck;
  onChange: (h: HealthCheck) => void;
  errors?: Record<string, string>;
  label?: string;
}) {
  const set = (patch: Partial<HealthCheck>) => onChange({ ...value, ...patch });
  return (
    <div className="grid gap-4 sm:grid-cols-4">
      <Field label={label} className="sm:col-span-1">
        {(p) => (
          <Select
            {...p}
            value={value.type}
            onChange={(e) =>
              set({ type: e.target.value as HealthCheck["type"] })
            }
          >
            <option value="http">Web request</option>
            <option value="tcp">Port is open</option>
            <option value="exec">Command</option>
            <option value="none">None</option>
          </Select>
        )}
      </Field>
      {value.type === "http" ? (
        <Field label="Path" error={errors.healthPath} className="sm:col-span-2">
          {(p) => (
            <Input
              {...p}
              mono
              value={value.path}
              onChange={(e) => set({ path: e.target.value })}
              placeholder="/healthz"
            />
          )}
        </Field>
      ) : null}
      {value.type === "http" || value.type === "tcp" ? (
        <Field
          label="Port"
          error={errors.healthPort}
          className={value.type === "tcp" ? "sm:col-span-3" : ""}
        >
          {(p) => (
            <Input
              {...p}
              mono
              type="number"
              value={value.port || ""}
              onChange={(e) => set({ port: Number(e.target.value) })}
            />
          )}
        </Field>
      ) : null}
      {value.type === "exec" ? (
        <Field
          label="Command"
          error={errors.healthCommand}
          className="sm:col-span-3"
        >
          {(p) => (
            <Input
              {...p}
              mono
              value={value.command}
              onChange={(e) => set({ command: e.target.value })}
            />
          )}
        </Field>
      ) : null}
      {value.type === "none" ? (
        <p className="self-end pb-2 text-[12px] text-muted-foreground sm:col-span-3">
          Healthy as soon as it starts.
        </p>
      ) : null}
      {value.type !== "none" ? (
        <>
          <Field label="First check after (s)">
            {(p) => (
              <Input
                {...p}
                type="number"
                min={0}
                value={value.initialDelaySeconds}
                onChange={(e) =>
                  set({ initialDelaySeconds: Number(e.target.value) })
                }
              />
            )}
          </Field>
          <Field label="Check every (s)">
            {(p) => (
              <Input
                {...p}
                type="number"
                min={1}
                value={value.periodSeconds}
                onChange={(e) => set({ periodSeconds: Number(e.target.value) })}
              />
            )}
          </Field>
        </>
      ) : null}
    </div>
  );
}
