import { CircleNotchIcon, QrCodeIcon, XIcon } from "@phosphor-icons/react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { LogoTile } from "~/components/pyo/IntegrationDialog";
import { snapEmbed, snapHide, type Snap, type SnapOutcome } from "~/lib/pyo/snap";

/** A Midtrans payment to show: its Snap token and its payment page. */
export type SnapCheckout = { snap: Snap; token: string; redirectUrl: string };

const EMBED_ID = "midtrans-snap";

/** Midtrans' payment in a dialog over the page, which shows blurred behind
 * it. Only the close button or Escape closes it, never a click beside it, so
 * a payment isn't lost by accident. When Snap can't show here, Midtrans'
 * payment page shows in its place. */
export function SnapDialog({
  checkout,
  onDone,
}: {
  checkout: SnapCheckout;
  onDone: (o: SnapOutcome) => void;
}) {
  const { snap, token, redirectUrl } = checkout;
  const ref = useRef<HTMLDialogElement>(null);
  const [frame, setFrame] = useState<string | null>(null);
  const done = useRef(onDone);
  done.current = onDone;
  const settled = useRef(false);
  const finish = (o: SnapOutcome) => {
    if (settled.current) return;
    settled.current = true;
    done.current(o);
  };
  const close = () => {
    snapHide(snap);
    finish({ kind: "closed" });
  };
  const closeRef = useRef(close);
  closeRef.current = close;

  // A modal <dialog>: the top layer, focus kept inside, the page inert.
  useLayoutEffect(() => {
    const d = ref.current;
    if (!d) return;
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    d.showModal();
    d.focus({ preventScroll: true });
    const onCancel = (e: Event) => {
      e.preventDefault();
      closeRef.current();
    };
    d.addEventListener("cancel", onCancel);
    // Snap scrolls the page to the top when it shows. Keep it where it was.
    const y = window.scrollY;
    const keep = () => {
      if (window.scrollY !== y) window.scrollTo({ top: y, behavior: "instant" });
    };
    window.addEventListener("scroll", keep);
    return () => {
      d.removeEventListener("cancel", onCancel);
      window.removeEventListener("scroll", keep);
      if (d.open) d.close();
      if (opener?.isConnected) opener.focus({ preventScroll: true });
    };
  }, []);

  useEffect(() => {
    try {
      snapEmbed(snap, token, EMBED_ID, finish);
    } catch {
      setFrame(redirectUrl);
      return;
    }
    return () => snapHide(snap);
    // Once per payment.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [snap, token]);

  return (
    <dialog
      ref={ref}
      tabIndex={-1}
      aria-labelledby="snap-title"
      className="snap-pay m-auto max-h-none w-[calc(100%-1.5rem)] max-w-[440px] overflow-visible rounded-[22px] border border-border-strong bg-muted p-0 text-foreground shadow-pop outline-none"
    >
      <div className="flex flex-col rounded-[21px] bg-muted p-1.5">
        <div className="flex items-center justify-between gap-4 px-3.5 pb-2.5 pt-2">
          <h2 id="snap-title" className="flex items-center gap-2.5 text-[15px] font-semibold tracking-tight">
            <LogoTile icon={<QrCodeIcon size={15} />} size="sm" />
            Pay with Midtrans
          </h2>
          <button
            type="button"
            onClick={close}
            aria-label="Close"
            className="-mr-1 rounded-md p-1 text-muted-foreground hover:bg-foreground/[0.06] hover:text-foreground"
          >
            <XIcon size={15} />
          </button>
        </div>
        <div className="relative h-[min(680px,calc(100dvh-5.5rem))] overflow-hidden rounded-[14px] border border-card-border bg-white shadow-card">
          <CircleNotchIcon
            size={18}
            aria-hidden
            className="absolute left-1/2 top-1/2 -ml-[9px] -mt-[9px] animate-spin text-neutral-400 motion-reduce:animate-none"
          />
          {frame ? (
            <iframe src={frame} title="Midtrans" allow="payment *" className="relative block h-full w-full border-0" />
          ) : (
            <div id={EMBED_ID} className="relative h-full w-full overflow-y-auto" />
          )}
        </div>
      </div>
    </dialog>
  );
}
