import {
  ArrowSquareOutIcon,
  CreditCardIcon,
  FlaskIcon,
  QrCodeIcon,
  ReceiptIcon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState, type ReactNode } from "react";
import { IntegrationCard } from "~/components/pyo/IntegrationCard";
import { PolarDialog, type PolarOutcome } from "~/components/pyo/PolarDialog";
import { SnapDialog, type SnapCheckout } from "~/components/pyo/SnapDialog";
import {
  Busy,
  ConfirmNote,
  DialogSection,
  IntegrationDialog,
  Summary,
} from "~/components/pyo/IntegrationDialog";
import { useHosted } from "~/components/pyo/Managed";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import { Badge, Dot, type Tone } from "~/components/ui/Badge";
import { Button, buttonClass } from "~/components/ui/Button";
import { Layered } from "~/components/ui/Layered";
import { EmptyState, KeyValue, Meter, Panel, Skeleton } from "~/components/ui/Panel";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { Tabs } from "~/components/ui/Tabs";
import {
  cloudBilling,
  type BillingInterval,
  type BillingPayment,
  type CloudBilling,
} from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { loadSnap, type SnapOutcome } from "~/lib/pyo/snap";
import { currentTeam, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { seo } from "~/lib/seo";

// A team's Pyo Cloud billing: $5 per server per month or $48 per server per
// year, where a server is each connected machine. Hosted Pyo only, for the
// team's owners and admins. cloud-api (pyo-cloud, billing.rs) serves it.
export const Route = createFileRoute("/_app/$team/settings/billing")({
  head: () => ({ meta: seo({ title: "Billing" }) }),
  component: BillingPage,
});

/** Days data is kept after a team stops paying, until cloud-api says. */
const RETAIN_DAYS = 30;
const DAY_MS = 86_400_000;

function BillingPage() {
  const team = currentTeam(usePyo());
  const hosted = useHosted();
  const admin = useTeamAdmin();
  if (hosted === undefined || !team) return null;
  if (!hosted)
    return (
      <Layered>
        <EmptyState icon={CreditCardIcon} title="No billing" description="Self-hosted Pyo has no billing." />
      </Layered>
    );
  if (!admin)
    return (
      <Layered>
        <EmptyState icon={CreditCardIcon} title="Billing" description="Owners and admins manage billing." />
      </Layered>
    );
  return <BillingLoader key={team.id} team={team.id} />;
}

function BillingLoader({ team }: { team: string }) {
  const q = useQuery(`billing:${team}`, () => cloudBilling.get(team), { refreshMs: 30_000 });
  if (q.status === "loading")
    return (
      <Panel title="Plan">
        <div className="space-y-2 px-4 py-4" aria-busy>
          <Skeleton className="h-5 w-1/3" />
          <Skeleton className="h-5 w-1/2" />
        </div>
      </Panel>
    );
  if (q.status === "error")
    return (
      <Panel title="Plan">
        <p className="px-4 py-4 text-[13px] text-muted-foreground">Couldn't load billing: {q.error}</p>
      </Panel>
    );
  return <Billing b={q.data} team={team} refresh={q.retry} />;
}

// ── Formatting ──────────────────────────────────────────────────────────────

const usd = (n: number) =>
  `$${n.toLocaleString("en-US", { minimumFractionDigits: 0, maximumFractionDigits: 2 })}`;
const idr = (n: number) => `Rp ${n.toLocaleString("id-ID")}`;
const money = (p: BillingPayment) => (p.currency === "idr" ? idr(p.amount) : usd(p.amount / 100));
const date = (ms: number) =>
  new Date(ms).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
const plural = (n: number, one: string) => `${n.toLocaleString("en-US")} ${one}${n === 1 ? "" : "s"}`;
const daysUntil = (ms: number) => Math.max(0, Math.ceil((ms - Date.now()) / DAY_MS));
const errorTitle = (e: unknown) => (e instanceof Error ? e.message : "That didn't work");
type Pair = [ReactNode, ReactNode];

const PROVIDER: Record<NonNullable<CloudBilling["provider"]>, string> = {
  polar: "Card",
  midtrans: "Midtrans",
  test: "Test payment",
};

/** Where the team stands, as one badge. */
function standing(b: CloudBilling): { tone: Tone; label: string } {
  if (b.status === "active" && b.cancelAtPeriodEnd) return { tone: "warning", label: "Cancelled" };
  const all: Record<CloudBilling["status"], { tone: Tone; label: string }> = {
    none: { tone: "neutral", label: "No subscription" },
    active: { tone: "success", label: "Paid" },
    past_due: { tone: "warning", label: "Payment failed" },
    paused: { tone: "danger", label: "Paused" },
    canceled: { tone: "neutral", label: "Ended" },
  };
  return all[b.status];
}

// ── The page ────────────────────────────────────────────────────────────────

function Billing({ b, team, refresh }: { b: CloudBilling; team: string; refresh: () => void }) {
  const [open, setOpen] = useState<"plan" | "midtrans" | null>(null);
  const [checkout, setCheckout] = useState<SnapCheckout | null>(null);
  // Polar's checkout, open over the page.
  const [polarCheckout, setPolarCheckout] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState(false);
  // Bumped after a payment, to load the payments again.
  const [paidAt, setPaidAt] = useState(0);
  const retain = b.retainDays ?? RETAIN_DAYS;
  const subscribed = b.status !== "none" && b.status !== "canceled";
  const polar = b.provider === "polar" && subscribed;
  const midtrans = b.provider === "midtrans" && subscribed;
  // A subscription that renews by itself (Polar, or a test one).
  const renews = (b.provider === "polar" || b.provider === "test") && subscribed;

  /** Runs an action. A URL in its answer is opened (checkout, portal, receipt). */
  const run = async (key: string, f: () => Promise<unknown>, done?: string) => {
    setBusy(key);
    try {
      const r = (await f()) as { url?: string; redirectUrl?: string } | undefined;
      const url = r?.url ?? r?.redirectUrl;
      if (url) return window.location.assign(url);
      if (done) toast({ title: done });
      refresh();
    } catch (e) {
      toast({ tone: "error", title: errorTitle(e) });
    } finally {
      setBusy(null);
    }
  };
  const portal = () => void run("portal", () => cloudBilling.polarPortal(team));
  // A payment made on the page. The provider's notification marks it paid
  // a moment later, so billing loads again now and once more after that.
  const paid = (received: boolean, provider: string) => {
    toast({
      tone: "success",
      title: received ? "Payment received" : "Waiting for your payment",
      description: `It shows here once ${provider} confirms it.`,
    });
    refresh();
    setPaidAt(Date.now());
    setTimeout(() => {
      refresh();
      setPaidAt(Date.now());
    }, 5_000);
  };
  // How a payment in Snap ended. Closed, billing loads again quietly: a QR
  // code may have been paid meanwhile.
  const snapDone = (o: SnapOutcome) => {
    setCheckout(null);
    if (o.kind === "closed") return void refresh();
    if (o.kind === "error") return void toast({ tone: "error", title: o.message });
    paid(o.kind === "success", "Midtrans");
  };
  // How Polar's checkout ended. Closed, billing loads again quietly.
  const polarDone = (o: PolarOutcome) => {
    setPolarCheckout(null);
    if (o === "closed") return void refresh();
    paid(o === "paid", "Polar");
  };
  // Paying what's due: Polar's portal for a card on file, else a plan to pick.
  const pay = () => (polar && b.polar.portal ? portal() : setOpen(midtrans ? "midtrans" : "plan"));

  const s = standing(b);
  const chargeUsd =
    (b.interval === "year" ? b.pricePerServerYearUsd : b.pricePerServerUsd) * Math.max(1, b.servers);
  const usdPrice =
    b.interval === "year"
      ? `${usd(b.pricePerServerYearUsd)} per server a year`
      : `${usd(b.pricePerServerUsd)} per server a month`;
  // Midtrans is paid in rupiah (its prices show only to visitors in Indonesia).
  const idrPrice = b.midtrans
    ? b.interval === "year"
      ? `${idr(b.midtrans.pricePerServerYearIdr)} per server a year`
      : `${idr(b.midtrans.pricePerServerIdr)} per server a month`
    : null;
  const rows: Pair[] = subscribed
    ? [
        [
          "Billed",
          <span className="inline-flex items-center gap-3">
            {b.interval === "year" ? "Yearly" : "Monthly"}
            <Button
              size="xs"
              variant="secondary"
              disabled={busy !== null}
              onClick={() => setOpen(midtrans ? "midtrans" : "plan")}
            >
              Change
            </Button>
          </span>,
        ],
        ...(midtrans ? (idrPrice ? [["Price", idrPrice] as Pair] : []) : [["Price", usdPrice] as Pair]),
      ]
    : [["Price", `${usd(b.pricePerServerUsd)} per server a month or ${usd(b.pricePerServerYearUsd)} a year`]];
  rows.push(["Servers", <span className="tabular-nums">{b.servers}</span>]);
  // A charge to come: only while the subscription is in good standing.
  if (renews && b.periodEnd && b.status === "active")
    rows.push(
      b.cancelAtPeriodEnd
        ? ["Ends", date(b.periodEnd)]
        : [
            "Next charge",
            <span className="tabular-nums">
              <span className="font-semibold">{usd(chargeUsd)}</span> on {date(b.periodEnd)}
            </span>,
          ],
    );
  if (midtrans && b.periodEnd) rows.push(["Paid until", date(b.periodEnd)]);
  if (b.provider && subscribed) rows.push(["Paid with", PROVIDER[b.provider]]);

  return (
    <div className="space-y-4">
      <Notice
        b={b}
        retain={retain}
        busy={busy}
        onPay={pay}
        onSubscribe={() => setOpen("plan")}
        onResume={() => void run("resume", () => cloudBilling.resume(team), "Subscription kept")}
      />

      <Panel
        title="Plan"
        actions={
          <Badge tone={s.tone}>
            <Dot tone={s.tone} />
            {s.label}
          </Badge>
        }
      >
        <KeyValue items={rows} />
      </Panel>

      <Usage b={b} />

      <Layered title="Payment method" bare>
        <div className="grid gap-3 sm:grid-cols-2">
          <IntegrationCard
            id="card"
            name="Card"
            icon={<CreditCardIcon size={18} />}
            description={
              b.polar.configured
                ? "Through Polar. Invoices, receipts and tax ids are in its portal."
                : "Not available on this Pyo Cloud yet."
            }
            status={polar ? { tone: "success", label: "In use" } : { tone: "neutral", label: "Not set up" }}
            detail={polar ? "Card and invoices" : null}
            action={
              polar ? (
                <Button size="xs" variant="secondary" disabled={!b.polar.portal || busy !== null} onClick={portal}>
                  {busy === "portal" ? (
                    <Busy>Opening</Busy>
                  ) : (
                    <>
                      Manage <ArrowSquareOutIcon size={12} aria-hidden />
                    </>
                  )}
                </Button>
              ) : (
                <Button
                  size="xs"
                  variant="secondary"
                  disabled={!b.polar.configured || busy !== null}
                  onClick={() => setOpen("plan")}
                >
                  Set up
                </Button>
              )
            }
          />
          {b.midtrans ? (
            <IntegrationCard
              id="midtrans"
              name="QRIS, bank transfer or e-wallet"
              icon={<QrCodeIcon size={18} />}
              description={
                b.midtrans.configured
                  ? "For Indonesia, through Midtrans. Paid a month or a year at a time."
                  : "Not available on this Pyo Cloud yet."
              }
              status={midtrans ? { tone: "success", label: "In use" } : { tone: "neutral", label: "Not set up" }}
              detail={
                midtrans && b.midtrans.topUpServers
                  ? `${plural(b.midtrans.topUpServers, "new server")} to pay for`
                  : midtrans && b.periodEnd
                    ? `Paid until ${date(b.periodEnd)}`
                    : null
              }
              action={
                <Button
                  size="xs"
                  variant="secondary"
                  disabled={!b.midtrans.configured || busy !== null}
                  onClick={() => setOpen("midtrans")}
                >
                  Pay
                </Button>
              }
            />
          ) : null}
          {b.testPayments ? (
            <IntegrationCard
              id="test"
              name="Test payment"
              icon={<FlaskIcon size={18} />}
              description="Development only. No money moves."
              status={
                b.provider === "test" && subscribed
                  ? { tone: "success", label: "In use" }
                  : { tone: "neutral", label: "Not set up" }
              }
              action={
                <Button
                  size="xs"
                  variant="secondary"
                  disabled={busy !== null || (b.provider === "test" && b.status === "active")}
                  onClick={() => void run("test", () => cloudBilling.testPay(team), "Paid (test)")}
                >
                  {busy === "test" ? <Busy>Paying</Busy> : "Pay (test)"}
                </Button>
              }
            />
          ) : null}
        </div>
      </Layered>

      <Payments
        team={team}
        paidAt={paidAt}
        busy={busy}
        onReceipt={(order) => void run(`receipt:${order}`, () => cloudBilling.receipt(team, order))}
      />

      {renews && b.status === "active" && !b.cancelAtPeriodEnd ? (
        <Panel title="Cancel subscription" tone="danger">
          <div className="space-y-3 px-4 py-3">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <p className="text-[12px] text-muted-foreground">
                {b.periodEnd
                  ? `Stops on ${date(b.periodEnd)}, the end of this period. You can undo it until then.`
                  : "Stops at the end of this period. You can undo it until then."}
              </p>
              {!cancelling ? (
                <Button size="sm" variant="secondary" className="text-destructive" onClick={() => setCancelling(true)}>
                  Cancel subscription
                </Button>
              ) : null}
            </div>
            {cancelling ? (
              <ConfirmNote
                title="Cancel subscription?"
                confirmLabel="Cancel subscription"
                busy={busy === "cancel"}
                onCancel={() => setCancelling(false)}
                onConfirm={() =>
                  void run("cancel", () => cloudBilling.cancel(team), "Subscription cancelled").then(() =>
                    setCancelling(false),
                  )
                }
              >
                {b.periodEnd
                  ? `On ${date(b.periodEnd)} deploys and new servers pause. Services keep running. Data is deleted on ${date(b.periodEnd + retain * DAY_MS)}.`
                  : `When the period ends, deploys and new servers pause. Data is deleted ${retain} days later.`}
              </ConfirmNote>
            ) : null}
          </div>
        </Panel>
      ) : null}

      {open === "plan" ? (
        <PlanDialog
          b={b}
          team={team}
          onClose={() => setOpen(null)}
          onDone={() => {
            setOpen(null);
            refresh();
          }}
          onCheckout={(url) => {
            setOpen(null);
            setPolarCheckout(url);
          }}
        />
      ) : open === "midtrans" && b.midtrans ? (
        <MidtransDialog
          b={b}
          team={team}
          onClose={() => setOpen(null)}
          onPay={(c) => {
            setOpen(null);
            setCheckout(c);
          }}
        />
      ) : null}
      {checkout ? <SnapDialog checkout={checkout} onDone={snapDone} /> : null}
      {polarCheckout ? <PolarDialog url={polarCheckout} onDone={polarDone} /> : null}
    </div>
  );
}

/** What needs doing, above the plan: a failed payment, a pause, an end. */
function Notice({
  b,
  retain,
  busy,
  onPay,
  onSubscribe,
  onResume,
}: {
  b: CloudBilling;
  retain: number;
  busy: string | null;
  onPay: () => void;
  onSubscribe: () => void;
  onResume: () => void;
}) {
  const deleted = b.retainUntil ? date(b.retainUntil) : null;
  const payNow = (
    <Button size="sm" disabled={busy !== null} onClick={onPay}>
      {busy === "portal" ? <Busy>Pay now</Busy> : "Pay now"}
    </Button>
  );
  if (b.status === "past_due")
    return (
      <Note tone="warning" title="Payment failed" action={payNow}>
        {b.graceUntil
          ? `${plural(daysUntil(b.graceUntil), "day")} left to pay. Changes to your servers pause on ${date(b.graceUntil)}. Services keep running.`
          : "Changes to your servers pause 7 days after the payment failed. Services keep running."}
      </Note>
    );
  if (b.status === "paused")
    return (
      <Note tone="danger" title="Paused" action={payNow}>
        {`Your servers get no changes. Services keep running. Paying resumes at once.${deleted ? ` Unpaid, data is deleted on ${deleted}.` : ""}`}
      </Note>
    );
  if (b.status === "canceled")
    return (
      <Note
        tone="neutral"
        title={b.periodEnd ? `Ended on ${date(b.periodEnd)}` : "Subscription ended"}
        action={
          <Button size="sm" disabled={busy !== null} onClick={onSubscribe}>
            Subscribe
          </Button>
        }
      >
        {`Your servers get no changes. Services keep running.${deleted ? ` Data is deleted on ${deleted}.` : ""}`}
      </Note>
    );
  if (b.status === "active" && b.cancelAtPeriodEnd && b.cancelReason === "no_servers")
    return (
      <Note tone="warning" title={b.periodEnd ? `No servers. Ends on ${date(b.periodEnd)}` : "No servers"}>
        {b.periodEnd
          ? `Connect a server before then to keep it. Data is deleted on ${date(b.periodEnd + retain * DAY_MS)}.`
          : "Connect a server to keep it."}
      </Note>
    );
  if (b.status === "active" && b.cancelAtPeriodEnd)
    return (
      <Note
        tone="warning"
        title={b.periodEnd ? `Cancelled. Ends on ${date(b.periodEnd)}` : "Cancelled"}
        action={
          <Button size="sm" variant="secondary" disabled={busy !== null} onClick={onResume}>
            {busy === "resume" ? <Busy>Keep subscription</Busy> : "Keep subscription"}
          </Button>
        }
      >
        {b.periodEnd
          ? `Your servers get no changes after that. Data is deleted on ${date(b.periodEnd + retain * DAY_MS)}.`
          : `Data is deleted ${retain} days after it ends.`}
      </Note>
    );
  if (b.status === "none")
    return (
      <Note
        tone="info"
        title="No subscription"
        action={
          <Button size="sm" disabled={busy !== null} onClick={onSubscribe}>
            Subscribe
          </Button>
        }
      >
        Subscribe before you connect a server.
      </Note>
    );
  return null;
}

function Note({
  tone,
  title,
  action,
  children,
}: {
  tone: "info" | "warning" | "danger" | "neutral";
  title: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  const cls = {
    info: "border-info/25 bg-info/[0.06]",
    warning: "border-warning/30 bg-warning/[0.07]",
    danger: "border-destructive/30 bg-destructive/[0.07]",
    neutral: "border-border bg-muted/40",
  }[tone];
  return (
    <div
      role={tone === "danger" || tone === "warning" ? "alert" : undefined}
      className={`flex flex-wrap items-center gap-x-4 gap-y-2 rounded-xl border px-4 py-3 ${cls}`}
    >
      <div className="min-w-0 flex-1">
        <p className="flex items-center gap-2 text-[13px] font-medium">
          <Dot tone={tone} className="h-2 w-2" />
          {title}
        </p>
        <p className="mt-0.5 pl-4 text-[12.5px] text-muted-foreground">{children}</p>
      </div>
      {action}
    </div>
  );
}

function Usage({ b }: { b: CloudBilling }) {
  const paid = b.seats > 0 && b.status !== "none" && b.status !== "canceled";
  return (
    <Panel
      title="Usage"
      description={paid && b.periodEnd && b.periodEnd > Date.now() ? `This period, until ${date(b.periodEnd)}` : undefined}
    >
      <ul className="list-rows">
        <li className="flex flex-wrap items-center gap-x-6 gap-y-2 px-4 py-3 text-[12.5px]">
          <span className="w-24 shrink-0 text-muted-foreground">Servers</span>
          {paid ? (
            <>
              <Meter
                value={b.servers}
                max={Math.max(b.seats, b.servers, 1)}
                showLabel={false}
                className="min-w-32 flex-1"
                warnAt={1}
                badAt={1}
                label="Servers"
              />
              <span className="shrink-0 tabular-nums">
                {b.servers} <span className="text-muted-foreground">of {plural(b.seats, "paid server")}</span>
              </span>
            </>
          ) : (
            <>
              <span className="min-w-32 flex-1 text-muted-foreground">None paid</span>
              <span className="shrink-0 tabular-nums">
                {b.servers} <span className="text-muted-foreground">connected</span>
              </span>
            </>
          )}
        </li>
      </ul>
    </Panel>
  );
}

const PAYMENT_STATUS: Record<BillingPayment["status"], { tone: Tone; label: string }> = {
  paid: { tone: "success", label: "Paid" },
  refunded: { tone: "neutral", label: "Refunded" },
  partially_refunded: { tone: "neutral", label: "Partly refunded" },
  failed: { tone: "danger", label: "Failed" },
  pending: { tone: "progress", label: "Pending" },
};

function Payments({
  team,
  paidAt,
  busy,
  onReceipt,
}: {
  team: string;
  /** When a payment was last made on this page. */
  paidAt: number;
  busy: string | null;
  onReceipt: (order: string) => void;
}) {
  const q = useQuery(`billing-history:${team}`, () => cloudBilling.history(team), { refreshMs: 60_000 });
  const reload = q.retry;
  useEffect(() => {
    if (paidAt) reload();
  }, [paidAt, reload]);
  return (
    <Panel title="Payments">
      {q.status === "loading" ? (
        <div className="space-y-2 px-4 py-4" aria-busy>
          <Skeleton className="h-4 w-2/3" />
          <Skeleton className="h-4 w-1/2" />
        </div>
      ) : q.status === "error" ? (
        <p className="px-4 py-4 text-[12.5px] text-muted-foreground">Couldn't load payments: {q.error}</p>
      ) : !q.data.payments.length ? (
        <p className="px-4 py-4 text-[12.5px] text-muted-foreground">No payments yet.</p>
      ) : (
        <Table fluid>
          <thead>
            <tr>
              <Th>Date</Th>
              <Th className="max-sm:hidden">For</Th>
              <Th className="text-right">Amount</Th>
              <Th>Status</Th>
              <Th className="w-0">
                <span className="sr-only">Receipt</span>
              </Th>
            </tr>
          </thead>
          <tbody>
            {q.data.payments.map((p) => {
              const st: { tone: Tone; label: string } =
                p.kind === "refund" ? { tone: "info", label: "Refund" } : PAYMENT_STATUS[p.status];
              return (
                <Tr key={p.id}>
                  <Td className="whitespace-nowrap tabular-nums">{date(p.at)}</Td>
                  <Td className="text-muted-foreground max-sm:hidden">{p.description}</Td>
                  <Td className="whitespace-nowrap text-right tabular-nums">
                    {p.kind === "refund" ? `−${money(p)}` : money(p)}
                  </Td>
                  <Td>
                    <Badge tone={st.tone}>{st.label}</Badge>
                  </Td>
                  <Td className="text-right">
                    {p.order ? (
                      <Button
                        size="xs"
                        variant="ghost"
                        disabled={busy !== null}
                        aria-label={`Receipt for ${date(p.at)}`}
                        onClick={() => onReceipt(p.order!)}
                      >
                        {busy === `receipt:${p.order}` ? (
                          <Busy>Receipt</Busy>
                        ) : (
                          <>
                            <ReceiptIcon size={13} aria-hidden /> <span className="max-sm:hidden">Receipt</span>
                          </>
                        )}
                      </Button>
                    ) : null}
                  </Td>
                </Tr>
              );
            })}
          </tbody>
        </Table>
      )}
    </Panel>
  );
}

// ── Dialogs ─────────────────────────────────────────────────────────────────

/** Monthly or yearly by card: a checkout for a new subscription, or a switch.
 * Polar's checkout opens in its own dialog over the page. */
function PlanDialog({
  b,
  team,
  onClose,
  onDone,
  onCheckout,
}: {
  b: CloudBilling;
  team: string;
  onClose: () => void;
  onDone: () => void;
  onCheckout: (url: string) => void;
}) {
  const subscribed =
    (b.provider === "polar" || b.provider === "test") &&
    (b.status === "active" || b.status === "past_due" || b.status === "paused");
  const [interval, setChoice] = useState<BillingInterval>(subscribed ? b.interval : "month");
  const [busy, setBusy] = useState(false);
  const servers = Math.max(1, b.servers);
  const total = (interval === "year" ? b.pricePerServerYearUsd : b.pricePerServerUsd) * servers;
  const same = subscribed && interval === b.interval;
  const submit = async () => {
    setBusy(true);
    try {
      if (subscribed) {
        await cloudBilling.setInterval(team, interval);
        toast({ title: interval === "year" ? "Billed yearly" : "Billed monthly" });
        onDone();
      } else {
        onCheckout((await cloudBilling.polarCheckout(team, interval)).url);
        return;
      }
    } catch (e) {
      toast({ tone: "error", title: errorTitle(e) });
    }
    setBusy(false);
  };
  const items: Pair[] = [
    ["Price", interval === "year" ? `${usd(b.pricePerServerYearUsd)} per server a year` : `${usd(b.pricePerServerUsd)} per server a month`],
    ["Servers", servers],
    [interval === "year" ? "Per year" : "Per month", <span className="font-semibold tabular-nums">{usd(total)}</span>],
  ];
  return (
    <IntegrationDialog
      icon={<CreditCardIcon size={15} />}
      name={subscribed ? "Change plan" : "Subscribe"}
      description="Each connected server counts once."
      onClose={onClose}
      onSubmit={() => {
        if (!same) void submit();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={busy || same}>
            {busy ? (
              <Busy>{subscribed ? "Switching" : "Opening"}</Busy>
            ) : subscribed ? (
              `Switch to ${interval === "year" ? "yearly" : "monthly"}`
            ) : (
              "Continue to payment"
            )}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Tabs
          label="Billed"
          semantics="radio"
          pill
          size="md"
          value={interval}
          onChange={setChoice}
          items={[
            { value: "month", label: "Monthly" },
            ...(b.polar.yearly || b.interval === "year" ? [{ value: "year" as const, label: "Yearly, 20% off" }] : []),
          ]}
        />
        <DialogSection title={same ? "Your plan" : subscribed ? "After the switch" : "Each period"}>
          <Summary items={items} />
        </DialogSection>
        <p className="text-[12px] text-muted-foreground">
          {subscribed && !same ? "Starts now. The rest of this period is credited. " : null}Plus tax where it applies.
        </p>
      </div>
    </IntegrationDialog>
  );
}

/** Indonesia: pay the next month or year, or servers beyond what's paid.
 * Snap opens in its own dialog over the page. Midtrans' payment page is the
 * fallback when snap.js doesn't load. */
function MidtransDialog({
  b,
  team,
  onClose,
  onPay,
}: {
  b: CloudBilling;
  team: string;
  onClose: () => void;
  onPay: (c: SnapCheckout) => void;
}) {
  const m = b.midtrans!;
  const topUp = b.provider === "midtrans" && b.status === "active" && m.topUpServers > 0;
  const [kind, setKind] = useState<BillingInterval>(b.provider === "midtrans" ? b.interval : "month");
  const [busy, setBusy] = useState(false);
  // Snap didn't load here: Midtrans' own payment page.
  const [fallback, setFallback] = useState<string | null>(null);
  const amount = topUp ? m.topUpIdr : kind === "year" ? m.totalYearIdr : m.totalIdr;
  const submit = async () => {
    setBusy(true);
    let checkout: { token: string; redirectUrl: string };
    try {
      checkout = await cloudBilling.midtransCheckout(team, topUp ? "topup" : kind);
    } catch (e) {
      toast({ tone: "error", title: errorTitle(e) });
      setBusy(false);
      return;
    }
    try {
      if (!m.clientKey) throw new Error("No client key");
      onPay({ snap: await loadSnap(m.clientKey, m.sandbox), ...checkout });
    } catch {
      setFallback(checkout.redirectUrl);
      setBusy(false);
    }
  };
  const starts =
    b.provider === "midtrans" && b.periodEnd && b.periodEnd > Date.now() ? date(b.periodEnd) : "Now";
  return (
    <IntegrationDialog
      icon={<QrCodeIcon size={15} />}
      name="Pay with Midtrans"
      description="QRIS, bank transfer or e-wallet."
      onClose={onClose}
      onSubmit={() => {
        if (!fallback) void submit();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          {fallback ? (
            <a href={fallback} className={buttonClass("primary", "sm")}>
              Pay on Midtrans
            </a>
          ) : (
            <Button type="submit" size="sm" disabled={busy || !m.configured}>
              {busy ? <Busy>Opening</Busy> : `Pay ${idr(amount)}`}
            </Button>
          )}
        </>
      }
    >
      {fallback ? (
        <p className="mb-4 text-[12.5px] text-muted-foreground">Payment couldn't open here. Pay on Midtrans' page instead.</p>
      ) : null}
      {topUp ? (
        <DialogSection title="New servers, for the rest of this period">
          <Summary
            items={[
              ["Servers", m.topUpServers],
              ["Until", b.periodEnd ? date(b.periodEnd) : "End of period"],
              ["Amount", <span className="font-semibold tabular-nums">{idr(m.topUpIdr)}</span>],
            ]}
          />
        </DialogSection>
      ) : (
        <div className="space-y-4">
          <Tabs
            label="Pay for"
            semantics="radio"
            pill
            size="md"
            value={kind}
            onChange={setKind}
            items={[
              { value: "month", label: "A month" },
              { value: "year", label: "A year, 20% off" },
            ]}
          />
          <Summary
            items={[
              ["Price", `${idr(kind === "year" ? m.pricePerServerYearIdr : m.pricePerServerIdr)} per server`],
              ["Servers", Math.max(1, b.servers)],
              ["Starts", starts],
              [kind === "year" ? "Per year" : "Per month", <span className="font-semibold tabular-nums">{idr(amount)}</span>],
            ]}
          />
        </div>
      )}
    </IntegrationDialog>
  );
}
