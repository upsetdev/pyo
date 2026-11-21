import { QrCodeIcon } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";
import { PayDialog } from "~/components/pyo/PayDialog";
import { snapEmbed, snapHide, type Snap, type SnapOutcome } from "~/lib/pyo/snap";

/** A Midtrans payment to show: its Snap token and its payment page. */
export type SnapCheckout = { snap: Snap; token: string; redirectUrl: string };

const EMBED_ID = "midtrans-snap";

/** Midtrans' payment in a dialog over the page. When Snap can't show here,
 * Midtrans' payment page shows in its place. */
export function SnapDialog({
  checkout,
  onDone,
}: {
  checkout: SnapCheckout;
  onDone: (o: SnapOutcome) => void;
}) {
  const { snap, token, redirectUrl } = checkout;
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
    <PayDialog title="Pay with Midtrans" icon={<QrCodeIcon size={15} />} onClose={close}>
      {frame ? (
        <iframe src={frame} title="Midtrans" allow="payment *" className="relative block h-full w-full border-0" />
      ) : (
        <div id={EMBED_ID} className="relative h-full w-full overflow-y-auto" />
      )}
    </PayDialog>
  );
}
