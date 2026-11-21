import { CreditCardIcon } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";
import { PayDialog } from "~/components/pyo/PayDialog";
import { buttonClass } from "~/components/ui/Button";

// Polar's checkout on the page: its embedded checkout in an iframe of ours,
// inside the same dialog as Midtrans. cloud-api creates the checkout with
// this dashboard as its embed origin, so Polar lets the page frame it and
// tells it how the payment goes through postMessage, as Polar's own embed
// script (@polar-sh/checkout) listens for it. That script isn't loaded: it
// would draw its own full-screen frame.

/** Where Polar's checkout runs, live and sandbox. Only these may talk to the
 * dialog. */
const POLAR_ORIGINS = ["https://polar.sh", "https://sandbox.polar.sh"];
const ALLOW = POLAR_ORIGINS.join(" ");
/** How long Polar has to say it loaded before the fallback link shows. */
const LOAD_MS = 15_000;

/** How a Polar checkout ended: paid, sent but not yet through, or closed. */
export type PolarOutcome = "paid" | "pending" | "closed";

/** What Polar's checkout posts to the page that embeds it. */
type PolarMessage = { type: "POLAR_CHECKOUT"; event: "loaded" | "confirmed" | "success" | "close" | string };

const isPolarMessage = (d: unknown): d is PolarMessage =>
  typeof d === "object" && d !== null && (d as { type?: unknown }).type === "POLAR_CHECKOUT";

/** The checkout in its embedded form, which talks to this page. */
function embedded(url: string): string | null {
  try {
    const u = new URL(url);
    if (!POLAR_ORIGINS.includes(u.origin)) return null;
    u.searchParams.set("embed", "true");
    u.searchParams.set("embed_origin", window.location.origin);
    u.searchParams.set("theme", "light");
    return u.toString();
  } catch {
    return null;
  }
}

/** Polar's card checkout in a dialog over the page. When it doesn't open
 * here, a link to Polar's checkout page shows instead. */
export function PolarDialog({ url, onDone }: { url: string; onDone: (o: PolarOutcome) => void }) {
  const src = useRef(embedded(url)).current;
  const frame = useRef<HTMLIFrameElement>(null);
  const [failed, setFailed] = useState(src === null);
  const loaded = useRef(false);
  // Polar's page may be see-through: once it shows, it hides the spinner.
  const [ready, setReady] = useState(false);
  // The card was sent: closing now waits for Polar instead of cancelling.
  const confirmed = useRef(false);
  const done = useRef(onDone);
  done.current = onDone;
  const settled = useRef(false);
  const finish = (o: PolarOutcome) => {
    if (settled.current) return;
    settled.current = true;
    done.current(o);
  };
  const close = () => finish(confirmed.current ? "pending" : "closed");

  useEffect(() => {
    if (!src) return;
    const onMessage = (e: MessageEvent) => {
      if (!POLAR_ORIGINS.includes(e.origin) || e.source !== frame.current?.contentWindow) return;
      if (!isPolarMessage(e.data)) return;
      switch (e.data.event) {
        case "loaded":
          loaded.current = true;
          setReady(true);
          setFailed(false);
          break;
        case "confirmed":
          confirmed.current = true;
          break;
        case "success":
          finish("paid");
          break;
        case "close":
          // As Polar's script: no closing while a payment goes through.
          if (!confirmed.current) finish("closed");
          break;
      }
    };
    window.addEventListener("message", onMessage);
    const timer = setTimeout(() => {
      if (!loaded.current) setFailed(true);
    }, LOAD_MS);
    return () => {
      window.removeEventListener("message", onMessage);
      clearTimeout(timer);
    };
    // Once per checkout.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [src]);

  return (
    <PayDialog title="Pay by card" icon={<CreditCardIcon size={15} />} onClose={close}>
      {src ? (
        <iframe
          ref={frame}
          src={src}
          title="Polar checkout"
          allow={`payment 'self' ${ALLOW}; publickey-credentials-get 'self' ${ALLOW}`}
          className={`relative block h-full w-full border-0 ${ready ? "bg-white" : ""}`}
        />
      ) : null}
      {failed ? (
        <div className="absolute inset-0 flex flex-col items-center justify-center gap-4 bg-muted px-6 text-center">
          <p className="text-[12.5px] text-muted-foreground">Payment couldn't open here. Pay on Polar's page instead.</p>
          <a href={url} className={buttonClass("primary", "sm")}>
            Pay on Polar
          </a>
        </div>
      ) : null}
    </PayDialog>
  );
}
