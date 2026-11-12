import {
  CubeIcon,
  LightningIcon,
  StackIcon,
  type Icon,
  type IconProps,
} from "@phosphor-icons/react";
import { forwardRef, useId, type ReactNode } from "react";
import { siMysql } from "simple-icons";
import type { Tone } from "~/components/ui/Badge";
import { PostgresLogo } from "~/components/ui/EngineLogo";
import { Field, Input, optionClass, Select } from "~/components/ui/Field";
import type { Database } from "~/lib/pyo/types";

// Shared by the Databases pages: labels for engines, sizes and states.

export const ENGINE_LABEL: Record<Database["engine"], string> = {
  postgres: "Postgres",
  mysql: "MySQL",
  valkey: "KV",
};

/** "Postgres 18" or "KV · Valkey 8.1". */
export const engineDetail = (d: Pick<Database, "engine" | "version">) =>
  d.engine === "valkey"
    ? `KV · Valkey ${d.version}`
    : `${ENGINE_LABEL[d.engine]} ${d.version}`;

export const SIZES = [
  { value: "small", label: "Small", detail: "0.25 CPU · 512 MiB · 5 GiB" },
  { value: "medium", label: "Medium", detail: "0.5 CPU · 1 GiB · 10 GiB" },
  { value: "large", label: "Large", detail: "1 CPU · 2 GiB · 20 GiB" },
  { value: "xlarge", label: "X-Large", detail: "2 CPU · 4 GiB · 50 GiB" },
] as const;

export const sizeLabel = (d: Database) =>
  `${SIZES.find((s) => s.value === d.size)?.label ?? d.size} · ${d.storageGi} GiB${d.instances > 1 ? " · high availability" : ""}`;

/** Its state as a dot and a word. */
export function dbState(d: Database): { tone: Tone; label: string } {
  if (d.state === "deleting") return { tone: "neutral", label: "Deleting" };
  if (d.import?.state === "pending" || d.import?.state === "running") return { tone: "progress", label: "Importing…" };
  if (d.import?.state === "failed" && d.import.newDatabase) return { tone: "danger", label: "Import failed" };
  if (!d.status?.phase && d.setup) {
    return d.setup.state === "failed"
      ? { tone: "danger", label: d.setup.message ? `Couldn't set up databases on this cluster: ${d.setup.message}` : "Couldn't set up databases on this cluster" }
      : { tone: "progress", label: "Setting up databases on this cluster…" };
  }
  switch (d.status?.phase) {
    case "Ready":
      return { tone: "success", label: "Ready" };
    case "Degraded":
      return { tone: "warning", label: "Degraded" };
    case "Failed":
      return { tone: "danger", label: "Failed" };
    default:
      return { tone: "progress", label: "Creating" };
  }
}

export const MIN_REPLICAS = 2;
/** Postgres up to 8 replicas; KV and MySQL up to 4. */
export const maxReplicas = (engine: Database["engine"]) =>
  engine === "postgres" ? 8 : 4;

/** MySQL's replica counts with high availability (instances 3 or 5, as the API takes them). */
export const MYSQL_REPLICAS = [2, 4];

/** The replica counts high availability can have. */
export const replicaOptions = (engine: Database["engine"]) =>
  engine === "mysql"
    ? MYSQL_REPLICAS
    : Array.from({ length: maxReplicas(engine) - MIN_REPLICAS + 1 }, (_, i) => MIN_REPLICAS + i);

/** The nearest count high availability can have. */
export const fitReplicas = (engine: Database["engine"], n: number) => {
  const o = replicaOptions(engine);
  return o.includes(n) ? n : (o.find((x) => x >= n) ?? o[o.length - 1]);
};

/** Dedicated read replicas at most: Postgres 5, KV 4, MySQL none. */
export const maxReadReplicas = (engine: Database["engine"]) =>
  engine === "postgres" ? 5 : engine === "mysql" ? 0 : 4;

/** "Single server", or "High availability · primary main-1 · 2 replicas". */
export const availabilityLabel = (d: Database) =>
  d.instances > 1
    ? `High availability${d.status?.primary ? ` · primary ${d.status.primary}` : ""} · ${d.instances - 1} replicas`
    : "Single server";

/** "0.25 CPU · 512 MiB", from what the API reports per instance. */
export const sizeSpec = (d: Pick<Database, "cpu" | "memory">) => {
  const cpu = d.cpu.endsWith("m")
    ? Number(d.cpu.slice(0, -1)) / 1000
    : Number(d.cpu);
  const mem = d.memory.replace(/^(\d+(?:\.\d+)?)(Ki|Mi|Gi|Ti)$/, "$1 $2B");
  return `${Number.isFinite(cpu) ? +cpu.toFixed(2) : d.cpu} CPU · ${mem}`;
};

/** MySQL's own logo (Simple Icons), sized like the other engines' icons. */
const MysqlIcon = forwardRef<SVGSVGElement, IconProps>(function MysqlIcon({ size = 16 }, ref) {
  return (
    <svg ref={ref} viewBox="0 0 24 24" width={size} height={size} aria-hidden fill={`#${siMysql.hex}`} className="shrink-0">
      <path d={siMysql.path} />
    </svg>
  );
}) as unknown as Icon;

export const ENGINE_ICON: Record<Database["engine"], Icon> = {
  postgres: PostgresLogo,
  mysql: MysqlIcon,
  valkey: LightningIcon,
};

/** A small label over a group of choices, like a field's. */
export function GroupLabel({ children }: { children: ReactNode }) {
  return (
    <p className="mb-1.5 text-[12px] font-medium text-muted-foreground">
      {children}
    </p>
  );
}

export function Choices({
  label,
  cols = "sm:grid-cols-2",
  children,
}: {
  label: string;
  cols?: string;
  children: ReactNode;
}) {
  return (
    <div role="radiogroup" aria-label={label} className={`grid gap-2 ${cols}`}>
      {children}
    </div>
  );
}

/** One pick-one card: an optional icon, a name and a line under it. */
export function Choice({
  icon: IconCmp,
  label,
  detail,
  selected,
  onSelect,
  disabled = false,
}: {
  icon?: Icon;
  label: string;
  detail: ReactNode;
  selected: boolean;
  onSelect: () => void;
  disabled?: boolean;
}) {
  const id = useId();
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-describedby={`${id}-d`}
      disabled={disabled}
      onClick={onSelect}
      className={`${optionClass({ selected })} flex w-full min-w-0 items-center gap-3 px-3 py-2.5 disabled:cursor-not-allowed disabled:opacity-50`}
    >
      {IconCmp ? (
        <span
          aria-hidden
          className={`flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border ${selected ? "border-primary/30 bg-primary/10 text-primary" : "border-border bg-muted text-muted-foreground"}`}
        >
          <IconCmp size={16} />
        </span>
      ) : null}
      <span className="min-w-0 flex-1">
        <span className="block text-[13px] font-medium text-foreground">
          {label}
        </span>
        <span
          id={`${id}-d`}
          className="mt-0.5 block text-[12px] leading-[1.15rem] text-muted-foreground"
        >
          {detail}
        </span>
      </span>
    </button>
  );
}

/** Single server or high availability as two cards; the replica count under them. */
export function AvailabilityField({
  engine,
  ha,
  replicas,
  onChange,
  atLeast = 1,
}: {
  engine: Database["engine"];
  ha: boolean;
  replicas: number;
  onChange: (ha: boolean, replicas: number) => void;
  /** Instances it can't go below (MySQL only grows). */
  atLeast?: number;
}) {
  const options = replicaOptions(engine).filter((n) => n + 1 >= atLeast);
  const shown = options.includes(replicas) ? replicas : options[0];
  return (
    <div>
      <GroupLabel>Availability</GroupLabel>
      <Choices label="Availability">
        <Choice
          icon={CubeIcon}
          label="Single server"
          detail={atLeast > 1 ? "It can't go back to one." : "No automatic failover."}
          selected={!ha}
          disabled={atLeast > 1}
          onSelect={() => onChange(false, replicas)}
        />
        <Choice
          icon={StackIcon}
          label="High availability"
          detail="Fails over to a replica."
          selected={ha}
          onSelect={() => onChange(true, replicas)}
        />
      </Choices>
      {ha ? (
        <Field
          label="Replicas"
          hint={`A primary and ${shown} replicas: ${shown + 1} instances.`}
          className="mt-3 sm:w-1/2 sm:pr-1"
        >
          {(p) => (
            <Select
              {...p}
              value={shown}
              onChange={(e) => onChange(true, Number(e.target.value))}
            >
              {options.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </Select>
          )}
        </Field>
      ) : null}
    </div>
  );
}

/** Instances for the API: 1, or a primary and its replicas. */
export const instancesOf = (ha: boolean, replicas: number) =>
  ha ? replicas + 1 : 1;
export const replicasValid = (
  engine: Database["engine"],
  ha: boolean,
  replicas: number,
) => !ha || replicaOptions(engine).includes(replicas);

/** CPU (millicores), memory (MiB) and storage (GiB) per instance of each preset. */
export const SIZE_SPECS: Record<string, { cpu: number; mem: number; storage: number }> = {
  small: { cpu: 250, mem: 512, storage: 5 },
  medium: { cpu: 500, mem: 1024, storage: 10 },
  large: { cpu: 1000, mem: 2048, storage: 20 },
  xlarge: { cpu: 2000, mem: 4096, storage: 50 },
};

/** Custom sizes' bounds, as the API checks them. */
export const minMemory = (engine: Database["engine"]) =>
  engine === "mysql" ? 1024 : engine === "postgres" ? 512 : 256;

/** The presets an engine can run on: MySQL needs 1 GiB, so no Small. */
export const sizesFor = (engine: Database["engine"]) =>
  SIZES.filter((x) => SIZE_SPECS[x.value].mem >= minMemory(engine));

/** The size to keep when switching engine: the smallest it allows when it's below. */
export const fitSize = (engine: Database["engine"], size: string) =>
  size === "custom" || sizesFor(engine).some((x) => x.value === size) ? size : sizesFor(engine)[0].value;
export function customError(engine: Database["engine"], cpu: number, mem: number) {
  if (!(cpu >= 125 && cpu <= 8000) || cpu % 125) return "CPU is 0.125 to 8, in steps of 0.125.";
  if (!(mem >= minMemory(engine) && mem <= 32768) || mem % 256) return `Memory is ${minMemory(engine)} to 32768 MiB, in steps of 256.`;
  return "";
}

/** "0.25 vCPU", from millicores. */
export const vcpu = (m: number) => `${+(m / 1000).toFixed(3)} vCPU`;
/** "512 MiB" or "1.5 GiB", from MiB, like the rest of the dashboard. */
export const memText = (mi: number) => (mi < 1024 ? `${mi} MiB` : `${+(mi / 1024).toFixed(1)} GiB`);

/** A preset or Custom, with CPU and memory (and storage) for Custom. */
export function SizeFields({
  engine,
  size,
  cpu,
  mem,
  onChange,
}: {
  engine: Database["engine"];
  size: string;
  cpu: number;
  mem: number;
  onChange: (v: { size: string; cpu: number; mem: number }) => void;
}) {
  const err = size === "custom" ? customError(engine, cpu, mem) : "";
  return (
    <div className="space-y-4">
      <Field label="Size" hint="Per instance.">
        {(p) => (
          <Select
            {...p}
            value={size}
            onChange={(e) => {
              const v = e.target.value;
              // Custom starts from the preset it was.
              const from = SIZE_SPECS[size];
              onChange(v === "custom" && from ? { size: v, cpu: from.cpu, mem: Math.max(from.mem, minMemory(engine)) } : { size: v, cpu, mem });
            }}
          >
            {sizesFor(engine).map((x) => (
              <option key={x.value} value={x.value}>
                {x.label} · {x.detail}
              </option>
            ))}
            <option value="custom">Custom</option>
          </Select>
        )}
      </Field>
      {size === "custom" ? (
        <div className="grid items-start gap-4 sm:grid-cols-2">
          <Field label="CPU (vCPU)" error={err.startsWith("CPU") ? err : undefined}>
            {(p) => (
              <Input {...p} type="number" min={0.125} max={8} step={0.125} value={cpu / 1000} onChange={(e) => onChange({ size, cpu: Math.round(Number(e.target.value) * 1000), mem })} />
            )}
          </Field>
          <Field label="Memory (MiB)" error={err.startsWith("Memory") ? err : undefined}>
            {(p) => (
              <Input {...p} type="number" min={minMemory(engine)} max={32768} step={256} value={mem} onChange={(e) => onChange({ size, cpu, mem: Number(e.target.value) })} />
            )}
          </Field>
        </div>
      ) : null}
    </div>
  );
}

type Row = { label: string; plural?: string; title?: string; count: number; cpu: number | null; mem: number | null; storage: number | null };
type ResourceInput = {
  engine: Database["engine"];
  cpu: number;
  mem: number;
  storageGi: number;
  instances: number;
  readReplicas: number;
  pooler: boolean;
  rest: boolean;
};

/**
 * Everything Pyo runs for a database and what each part reserves, as the
 * controller asks: its instances at its size (MySQL: nothing else); PgBouncer poolers reserve
 * nothing of their own; sentinels 0.02 vCPU and 32 MiB each; KV's endpoint
 * (pyo-kv: Redis, and HTTP with the REST API) always two, 0.05 vCPU and
 * 32 MiB each, using up to 512 MiB. One calculation for the form, Settings and
 * Overview.
 */
export function resourceRows({ engine, cpu, mem, storageGi, instances, readReplicas, pooler, rest }: ResourceInput): Row[] {
  const ha = instances > 1;
  return [
    { label: "Primary", count: 1, cpu, mem, storage: storageGi },
    ...(ha ? [{ label: "Replica", count: instances - 1, cpu, mem, storage: storageGi }] : []),
    ...(readReplicas ? [{ label: "Read replica", count: readReplicas, cpu, mem, storage: storageGi }] : []),
    ...(engine === "mysql"
      ? []
      : engine === "postgres"
      ? [
          ...(pooler ? [{ label: "Pooler", count: ha ? 2 : 1, cpu: null, mem: null, storage: null }] : []),
          ...(readReplicas ? [{ label: "Read pooler", count: readReplicas > 1 ? 2 : 1, cpu: null, mem: null, storage: null }] : []),
        ]
      : [
          ...(ha ? [{ label: "Sentinel", count: 3, cpu: 20, mem: 32, storage: null }] : []),
          {
            label: rest ? "Endpoint (Redis, HTTP)" : "Endpoint (Redis)",
            plural: rest ? "Endpoints (Redis, HTTP)" : "Endpoints (Redis)",
            title: "Reserves 0.05 vCPU and 32 MiB each, and can use up to 512 MiB.",
            count: 2,
            cpu: 50,
            mem: 32,
            storage: null,
          },
        ]),
  ];
}

export function resourceTotals(rows: Row[]) {
  const sum = (k: "cpu" | "mem" | "storage") => rows.reduce((a, r) => a + (r[k] ?? 0) * r.count, 0);
  return { count: rows.reduce((a, r) => a + r.count, 0), cpu: sum("cpu"), mem: sum("mem"), storage: sum("storage") };
}

/** A database's own input to the calculation, as saved. */
export const resourcesOf = (d: Database): ResourceInput => ({
  engine: d.engine,
  cpu: d.cpuMillis ?? SIZE_SPECS[d.size]?.cpu ?? 0,
  mem: d.memoryMi ?? SIZE_SPECS[d.size]?.mem ?? 0,
  storageGi: d.storageGi,
  instances: d.instances,
  readReplicas: d.readReplicas?.instances ?? 0,
  pooler: d.pooler,
  rest: d.rest,
});

/** Its totals for Details: "2.5 vCPU · 5 GiB" and "100 GiB". */
export const totals = (d: Database) => {
  const t = resourceTotals(resourceRows(resourcesOf(d)));
  return { compute: `${vcpu(t.cpu)} · ${memText(t.mem)}`, storage: `${t.storage} GiB` };
};

/** The breakdown and total, as a table. */
export function ResourceSummary(input: ResourceInput) {
  const rows = resourceRows(input);
  const t = resourceTotals(rows);
  const cell = "whitespace-nowrap px-3 py-2 text-right tabular-nums";
  return (
    <div className="overflow-x-auto rounded-lg border border-border">
      <table className="w-full text-[12.5px]">
        <thead>
          <tr className="border-b border-border bg-muted/40 text-muted-foreground">
            <th className="px-3 py-2 text-left font-medium">Includes {t.count} instances</th>
            <th className={`${cell} font-medium`}>CPU</th>
            <th className={`${cell} font-medium`}>Memory</th>
            <th className={`${cell} font-medium`}>Storage</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.label} className="border-b border-border last:border-0">
              <td className="px-3 py-2">
                <span title={r.title}>
                  {r.count}× {r.plural ? (r.count > 1 ? r.plural : r.label).replace(/^E/, "e") : `${r.label.toLowerCase()}${r.count > 1 ? "s" : ""}`}
                </span>
              </td>
              <td className={cell}>{r.cpu === null ? "—" : vcpu(r.cpu * r.count)}</td>
              <td className={cell}>{r.mem === null ? "—" : memText(r.mem * r.count)}</td>
              <td className={cell}>{r.storage === null ? "—" : `${r.storage * r.count} GiB`}</td>
            </tr>
          ))}
        </tbody>
        <tfoot>
          <tr className="border-t border-border-strong font-semibold">
            <td className="px-3 py-2">Total resources</td>
            <td className={cell}>{vcpu(t.cpu)}</td>
            <td className={cell}>{memText(t.mem)}</td>
            <td className={cell}>{t.storage} GiB</td>
          </tr>
        </tfoot>
      </table>
    </div>
  );
}
