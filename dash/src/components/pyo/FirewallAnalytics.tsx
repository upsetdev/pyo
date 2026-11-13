import { ObservabilityOff, useObservabilityOn } from "~/components/pyo/ObservabilityOff";
import { useRef, useState, type ReactNode } from "react";
import { BarList, Legend, TimeSeriesChart, type Series } from "~/components/charts/Charts";
import { TimeAgo } from "~/components/pyo/common";
import { ruleLabel } from "~/components/pyo/Firewall";
import { CUSTOM_FROM, wafMode } from "~/components/pyo/Waf";
import { Badge } from "~/components/ui/Badge";
import { Segmented } from "~/components/ui/Field";
import { Flag } from "~/components/ui/Flag";
import { Layered } from "~/components/ui/Layered";
import { Skeleton } from "~/components/ui/Panel";
import { Reveal, usePresence } from "~/components/ui/Reveal";
import { fetchFirewallBlocked, fetchWafHits } from "~/lib/pyo/backend";
import { compact } from "~/lib/pyo/format";
import { countryName } from "~/lib/pyo/location";
import { useQuery } from "~/lib/pyo/query";
import { usePyo } from "~/lib/pyo/store";
import type { Application } from "~/lib/pyo/types";

// What a service's firewall did: requests and connections its access rules
// turned away, and what its WAF blocked or only logged, over a range.

const RANGES = ["24h", "7d", "30d"] as const;
const SHOWN = 6;

type Row = { key: string; label: ReactNode; value: number };

/** Rows from both sources summed by key, largest first. */
function merge(rows: { key: string; value: number }[], label: (key: string) => ReactNode): Row[] {
  const sum = new Map<string, number>();
  for (const r of rows) if (r.value > 0) sum.set(r.key, (sum.get(r.key) ?? 0) + r.value);
  return [...sum].sort((a, b) => b[1] - a[1]).map(([key, value]) => ({ key, label: label(key), value }));
}

/** A ranked list in the card: a small heading (or tabs), then bars. */
function Top({ title, tabs }: { title: string; tabs: { id: string; label: string; rows: Row[]; mono?: boolean }[] }) {
  const shown = tabs.filter((t) => t.rows.length);
  const [id, setId] = useState(shown[0]?.id);
  const tab = shown.find((t) => t.id === id) ?? shown[0];
  return (
    <section aria-label={title} className="min-w-0">
      <div className="flex h-11 items-center justify-between gap-3 px-4 pt-2">
        <h3 className="text-[12px] font-medium text-muted-foreground">{title}</h3>
        {shown.length > 1 ? (
          <Segmented label={title} size="xs" value={tab.id} onChange={setId} options={shown.map((t) => ({ value: t.id, label: t.label }))} />
        ) : null}
      </div>
      <BarList rows={(tab?.rows ?? []).slice(0, SHOWN)} format={compact} mono={tab?.mono} empty="Nothing yet" />
    </section>
  );
}

const country = (code: string) => (
  <span className="inline-flex items-center gap-1.5">
    <Flag country={code} size={14} />
    {(code && countryName(code)) || code || "Unknown"}
  </span>
);

/** What the service's firewall did: "Analytics" on its Firewall tab. */
export function FirewallAnalytics({ app }: { app: Application }) {
  return useObservabilityOn() ? (
    <Analytics app={app} />
  ) : (
    <Layered bare>
      <section aria-label="Analytics" className="overflow-hidden rounded-[14px] border border-card-border bg-card">
        <ObservabilityOff compact />
      </section>
    </Layered>
  );
}

function Analytics({ app }: { app: Application }) {
  const s = usePyo();
  const [range, setRange] = useState<(typeof RANGES)[number]>("24h");
  const waf = wafMode(app) !== "off";
  const fwQ = useQuery(`firewall-blocked:${app.id}:${range}`, () => fetchFirewallBlocked(app.id, range), { refreshMs: 30_000 });
  const wafQ = useQuery(`waf-hits:${app.id}:${range}:${waf}`, () => (waf ? fetchWafHits(app.id, range) : Promise.resolve(null)), {
    refreshMs: 30_000,
  });
  const fw = fwQ.status === "success" ? fwQ.data : null;
  // The WAF's parts (its series, Logged, its rules and events) show only
  // while it's on, fading in and out; the access rules' always show.
  const live = waf && wafQ.status === "success" ? wafQ.data : null;
  const last = useRef(live);
  if (live) last.current = live;
  const wafShown = usePresence(!!live);
  const hits = wafShown.mounted ? (live ?? last.current) : null;
  const fade = wafShown.visible ? 1 : 0;
  // Turned on after loading, the card stays put and the WAF's parts fade in.
  const loadedWith = useRef({ range, waf });
  if (loadedWith.current.range !== range) loadedWith.current = { range, waf };
  const loading = fwQ.status === "loading" || (loadedWith.current.waf && waf && wafQ.status === "loading");
  const error = fwQ.status === "error" ? fwQ.error : wafQ.status === "error" ? wafQ.error : null;

  const fwRules = s.firewallRules.filter((r) => r.serviceId === app.id);
  const byNumber = new Map(s.wafRules.map((r) => [r.number, r]));
  // Custom rules by name, the Core Rule Set's by number.
  const wafLabel = (rule: number, message?: string) =>
    rule >= CUSTOM_FROM ? (byNumber.get(rule)?.name ?? message ?? String(rule)) : String(rule);

  const blocked = (fw?.total ?? 0) + (hits?.blocked ?? 0);
  const logged = hits?.logged ?? 0;
  const times = (fw?.series ?? hits?.series ?? []).map((p) => p.at);
  const series: Series[] = [
    { key: "rules", label: "Access rules", color: "var(--series-1)", values: (fw?.series ?? []).map((p) => p.count), opacity: 1 },
    ...(hits
      ? [
          { key: "waf", label: "WAF", color: "var(--series-2)", values: hits.series.map((p) => p.blocked), opacity: fade },
          { key: "logged", label: "Logged", color: "var(--series-3)", values: hits.series.map((p) => p.logged), opacity: fade },
        ]
      : []),
  ];
  const totals: Record<string, number> = { rules: fw?.total ?? 0, waf: hits?.blocked ?? 0, logged };

  const ruleRows = [
    ...(fw?.rules ?? []).map((r) => ({ key: `fw:${r.rule}`, value: r.count })),
    ...(hits?.rules ?? []).map((r) => ({ key: `waf:${r.rule}`, value: r.count })),
  ];
  const wafMessages = new Map((hits?.rules ?? []).map((r) => [String(r.rule), r.message]));
  const rules = merge(ruleRows, (k) => {
    const [kind, id] = [k.slice(0, k.indexOf(":")), k.slice(k.indexOf(":") + 1)];
    if (kind === "fw") return ruleLabel(id, fwRules);
    const n = Number(id);
    return n >= CUSTOM_FROM ? (
      wafLabel(n, wafMessages.get(id))
    ) : (
      <>
        <span className="font-mono">{n}</span> <span className="text-muted-foreground">{wafMessages.get(id)}</span>
      </>
    );
  });
  const countries = merge(
    [
      ...(fw?.countries ?? []).map((c) => ({ key: c.country, value: c.count })),
      ...(hits?.countries ?? []).map((c) => ({ key: c.country, value: c.blocked + c.logged })),
    ],
    country,
  );
  const ips = merge(
    [
      ...(fw?.sources ?? []).filter((x) => x.ip).map((x) => ({ key: x.ip, value: x.count })),
      ...(hits?.sources ?? []).filter((x) => x.ip).map((x) => ({ key: x.ip, value: x.blocked + x.logged })),
    ],
    (ip) => ip,
  );
  const paths = merge(
    [
      ...(fw?.paths ?? []).map((p) => ({ key: p.path, value: p.count })),
      ...(hits?.paths ?? []).map((p) => ({ key: p.path, value: p.blocked + p.logged })),
    ],
    (p) => p,
  );

  type Event = { at: number; tone: "danger" | "warning" | "success"; outcome: string; ip: string; country: string; what: string; why: ReactNode };
  const events: Event[] = [
    ...(fw?.recent ?? []).map(
      (r): Event => ({
        at: r.at,
        tone: "danger",
        outcome: "Blocked",
        ip: r.ip,
        country: r.country,
        what: r.kind === "website" ? r.target : `${r.kind.toUpperCase()} ${r.target}`,
        why: ruleLabel(r.rule, fwRules),
      }),
    ),
    ...(hits?.recent ?? []).map((r): Event => {
      const allowed = !r.blocked && r.rules.some((n) => byNumber.get(n)?.action === "allow");
      const shown = r.rules
        .map((n, j) => ({ n, m: r.messages[j] }))
        .filter((x) => x.n < 949000 || x.n >= CUSTOM_FROM)
        .slice(0, 3);
      return {
        at: r.at,
        tone: r.blocked ? "danger" : allowed ? "success" : "warning",
        outcome: r.blocked ? "Blocked" : allowed ? "Allowed" : "Logged",
        ip: r.ip,
        country: r.country,
        what: `${r.method} ${r.host}${r.url}`,
        why: shown.map((x, j) => (
          <span key={x.n} className={x.n >= CUSTOM_FROM ? "" : "font-mono"}>
            {j ? ", " : ""}
            {wafLabel(x.n, x.m)}
          </span>
        )),
      };
    }),
  ]
    .sort((a, b) => b.at - a.at)
    .slice(0, 25);

  const stat = (label: string, value: number) => (
    <div className="whitespace-nowrap px-4 py-3">
      <span className="block text-[12px] text-muted-foreground">{label}</span>
      <span className="mt-0.5 block text-[20px] font-semibold tabular-nums tracking-tight">
        {loading ? <Skeleton className="mt-1 h-6 w-14" /> : compact(value)}
      </span>
    </div>
  );

  return (
    <Layered bare>
      <section
        id="analytics"
        aria-labelledby="analytics-title"
        className="overflow-hidden rounded-[14px] border border-card-border bg-card"
      >
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-border px-4 py-3">
          <h2 id="analytics-title" className="text-[14px] font-medium tracking-tight">
            Analytics
          </h2>
          <Segmented label="Time range" size="xs" value={range} onChange={setRange} options={RANGES.map((r) => ({ value: r, label: r }))} />
        </div>
        {error && !fw && !hits ? (
          <p className="px-4 py-6 text-center text-[12.5px] text-muted-foreground">{error}</p>
        ) : (
          <>
            <div className="flex border-b border-border">
              <div className="min-w-0 flex-1 basis-0">{stat("Blocked", blocked)}</div>
              {hits ? (
                // Logged opens beside Blocked as the WAF comes on.
                <div
                  aria-hidden={!fade || undefined}
                  className="min-w-0 basis-0 overflow-hidden border-l border-border transition-[flex-grow,opacity] duration-200 ease-out motion-reduce:transition-none"
                  style={{ flexGrow: fade, opacity: fade }}
                >
                  {stat("Logged", logged)}
                </div>
              ) : null}
            </div>
            <div className="px-3 pb-3 pt-4 sm:px-4">
              {loading ? (
                <Skeleton className="h-[160px] w-full" />
              ) : (
                <>
                  <TimeSeriesChart label={`Blocked and logged over the last ${range}`} stacked times={times} series={series} format={compact} height={160} />
                  <Reveal open={series.length > 1 && !!fade}>
                    <div className="pl-11 pt-3">
                      <Legend items={series.map((x) => ({ label: x.label, color: x.color, value: compact(totals[x.key]), opacity: x.opacity }))} />
                    </div>
                  </Reveal>
                </>
              )}
            </div>
            {!loading && blocked + logged > 0 ? (
              <>
                <div className="grid divide-y divide-border border-t border-border md:grid-cols-3 md:divide-x md:divide-y-0">
                  <Top title="Rules" tabs={[{ id: "rules", label: "Rules", rows: rules }]} />
                  <Top
                    title="Sources"
                    tabs={[
                      { id: "countries", label: "Countries", rows: countries },
                      { id: "ips", label: "IPs", rows: ips, mono: true },
                    ]}
                  />
                  <Top title="Paths" tabs={[{ id: "paths", label: "Paths", rows: paths, mono: true }]} />
                </div>
                <div className="border-t border-border">
                  <h3 className="px-4 pb-1 pt-3 text-[12px] font-medium text-muted-foreground">Recent</h3>
                  <ul className="list-rows">
                    {events.map((r, i) => (
                      <li key={`${r.at}-${i}`} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2 text-[12.5px]">
                        <span className="w-20 shrink-0 text-muted-foreground">
                          <TimeAgo at={r.at} />
                        </span>
                        <Badge tone={r.tone}>{r.outcome}</Badge>
                        <span className="inline-flex min-w-0 items-center gap-1.5">
                          <Flag country={r.country} size={14} />
                          {r.ip ? <span className="font-mono">{r.ip}</span> : null}
                        </span>
                        <span className="min-w-0 truncate text-muted-foreground">{r.what}</span>
                        <span className="ml-auto text-muted-foreground">{r.why}</span>
                      </li>
                    ))}
                  </ul>
                </div>
              </>
            ) : !loading ? (
              <p className="border-t border-border px-4 py-6 text-center text-[12.5px] text-muted-foreground">Nothing blocked in the last {range}.</p>
            ) : null}
          </>
        )}
      </section>
    </Layered>
  );
}
