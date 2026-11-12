import { ArrowSquareOutIcon, CreditCardIcon, QrCodeIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { Button } from "~/components/ui/Button";
import { PageHeader, Panel } from "~/components/ui/Panel";
import { cloudBilling, fetchMeta, type CloudBilling } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { currentTeam, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { seo } from "~/lib/seo";

// A team's Pyo Cloud billing: $5 per server per month, where a server is
// each connected machine. Only a hosted build has it (GET /api/v1/meta lists
// "billing"); self-hosted Pyo never links here.
export const Route = createFileRoute("/_app/$team/settings/billing")({
  head: () => ({ meta: seo({ title: "Billing" }) }),
  component: BillingPage,
});

function BillingPage() {
  const s = usePyo();
  const team = currentTeam(s);
  const meta = useQuery("meta", fetchMeta);
  const hosted = meta.status === "success" && meta.data.features.includes("billing");
  const q = useQuery(`billing:${team?.id ?? ""}`, () => cloudBilling.get(team?.id ?? ""), { refreshMs: 30_000 });
  return (
    <div className="space-y-5">
      <PageHeader title="Billing" description="$5 per server per month." />
      {meta.status === "success" && !hosted ? (
        <Panel title="Not part of self-hosted Pyo">
          <p className="px-4 py-4 text-[13px] text-muted-foreground">Self-hosted Pyo has no billing.</p>
        </Panel>
      ) : q.status === "loading" || meta.status === "loading" ? (
        <Panel title="Plan">
          <div className="space-y-2 px-4 py-4" aria-busy>
            <div className="h-5 w-1/3 animate-pulse rounded bg-muted" />
            <div className="h-5 w-1/2 animate-pulse rounded bg-muted" />
          </div>
        </Panel>
      ) : q.status === "error" ? (
        <Panel title="Plan">
          <p className="px-4 py-4 text-[13px] text-muted-foreground">Couldn't load billing: {q.error}</p>
        </Panel>
      ) : team ? (
        <Billing b={q.data} team={team.id} />
      ) : null}
    </div>
  );
}

const usd = (n: number) => `$${n.toLocaleString("en-US", { minimumFractionDigits: 0, maximumFractionDigits: 2 })}`;
const idr = (n: number) => `Rp ${n.toLocaleString("id-ID")}`;
const date = (ms: number) => new Date(ms).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });

function Billing({ b, team }: { b: CloudBilling; team: string }) {
  const [busy, setBusy] = useState<string | null>(null);
  const go = async (key: string, f: () => Promise<{ url?: string; redirectUrl?: string }>) => {
    setBusy(key);
    try {
      const r = await f();
      const url = r.url ?? r.redirectUrl;
      if (url) window.location.assign(url);
    } catch (e) {
      toast({ tone: "error", title: e instanceof Error ? e.message : "That didn't work" });
    } finally {
      setBusy(null);
    }
  };
  const status = STATUS[b.status];
  return (
    <>
      <Panel title="Plan" actions={<StatusLine tone={status.tone}>{status.label}</StatusLine>}>
        <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-6 px-4 py-2 text-[13px]">
          <Row k="Servers" v={`${b.servers}`} hint="Each connected machine counts once." />
          <Row k="Price" v={`${usd(b.pricePerServerUsd)} per server / month`} />
          <Row k="Per month" v={<span className="font-semibold">{usd(b.totalUsd)}</span>} />
          {b.provider ? <Row k="Paid with" v={b.provider === "polar" ? "Card (Polar)" : "Midtrans"} /> : null}
          {b.periodEnd ? <Row k={b.status === "canceled" ? "Ends" : "Renews"} v={date(b.periodEnd)} /> : null}
        </dl>
        {b.status === "past_due" && b.graceUntil ? (
          <p className="border-t border-divider px-4 py-3 text-[12.5px]">
            <span className="font-medium text-warning">A payment didn't go through.</span>{" "}
            <span className="text-muted-foreground">
              Your services keep running. Deploys pause on {date(b.graceUntil)} unless it's paid.
            </span>
          </p>
        ) : b.status === "paused" ? (
          <p className="border-t border-divider px-4 py-3 text-[12.5px]">
            <span className="font-medium text-destructive">Deploys are paused.</span>{" "}
            <span className="text-muted-foreground">Your services keep running. Pay below to deploy again.</span>
          </p>
        ) : b.status === "none" ? (
          <p className="border-t border-divider px-4 py-3 text-[12.5px] text-muted-foreground">
            Add a payment method before you connect a server.
          </p>
        ) : null}
      </Panel>

      <Panel title="Pay">
        <ul className="list-rows">
          <Option
            icon={<CreditCardIcon size={16} />}
            title="Card"
            sub={
              b.polar.configured
                ? `Billed monthly through Polar, ${usd(b.pricePerServerUsd)} per server. Cancel anytime.`
                : "Card payments aren't configured on this Pyo Cloud yet."
            }
            action={
              b.provider === "polar" && b.status !== "none" ? (
                <Button size="sm" variant="secondary" disabled={!b.polar.portal || busy !== null} onClick={() => go("portal", () => cloudBilling.polarPortal(team))}>
                  Manage <ArrowSquareOutIcon size={12} />
                </Button>
              ) : (
                <Button size="sm" disabled={!b.polar.configured || busy !== null} onClick={() => go("polar", () => cloudBilling.polarCheckout(team))}>
                  {busy === "polar" ? "Opening…" : "Pay by card"}
                </Button>
              )
            }
          />
          {b.midtrans ? (
            <Option
              icon={<QrCodeIcon size={16} />}
              title="QRIS, bank transfer or e-wallet"
              sub={
                b.midtrans.configured
                  ? `For Indonesia, through Midtrans: ${idr(b.midtrans.pricePerServerIdr)} per server, paid each month. ${
                      b.midtrans.topUpServers
                        ? `${b.midtrans.topUpServers} new server${b.midtrans.topUpServers === 1 ? "" : "s"} this month: ${idr(b.midtrans.topUpIdr)} for the rest of it.`
                        : `This month: ${idr(b.midtrans.totalIdr)}.`
                    }`
                  : "Midtrans isn't configured on this Pyo Cloud yet."
              }
              action={
                <Button
                  size="sm"
                  variant={b.provider === "midtrans" ? "primary" : "secondary"}
                  disabled={!b.midtrans.configured || busy !== null}
                  onClick={() => go("midtrans", () => cloudBilling.midtransCheckout(team, b.midtrans!.topUpServers ? "topup" : "month"))}
                >
                  {busy === "midtrans" ? "Opening…" : b.midtrans.topUpServers ? "Pay for new servers" : "Pay this month"}
                </Button>
              }
            />
          ) : null}
        </ul>
      </Panel>
    </>
  );
}

const STATUS: Record<CloudBilling["status"], { tone: "ok" | "warn" | "bad" | "muted"; label: string }> = {
  none: { tone: "muted", label: "No payment method" },
  active: { tone: "ok", label: "Active" },
  past_due: { tone: "warn", label: "Payment due" },
  paused: { tone: "bad", label: "Deploys paused" },
  canceled: { tone: "muted", label: "Canceled" },
};

function StatusLine({ tone, children }: { tone: "ok" | "warn" | "bad" | "muted"; children: ReactNode }) {
  const dot = { ok: "bg-success", warn: "bg-warning", bad: "bg-destructive", muted: "bg-muted-foreground/50" }[tone];
  return (
    <span className="inline-flex items-center gap-1.5 text-[12.5px] font-medium">
      <span aria-hidden className={`h-2 w-2 rounded-full ${dot}`} />
      {children}
    </span>
  );
}

function Row({ k, v, hint }: { k: string; v: ReactNode; hint?: string }) {
  return (
    <>
      <dt className="py-2 text-muted-foreground">
        {k}
        {hint ? <span className="block text-[11.5px] text-muted-foreground/80">{hint}</span> : null}
      </dt>
      <dd className="py-2 text-right tabular-nums">{v}</dd>
    </>
  );
}

function Option({ icon, title, sub, action }: { icon: ReactNode; title: string; sub: string; action: ReactNode }) {
  return (
    <li className="flex items-center gap-3 px-4 py-3">
      <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-muted text-foreground/75">{icon}</span>
      <span className="min-w-0 flex-1">
        <span className="block text-[13px] font-medium">{title}</span>
        <span className="block text-[12px] text-muted-foreground">{sub}</span>
      </span>
      {action}
    </li>
  );
}
