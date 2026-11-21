import { CircleNotchIcon, XIcon } from "@phosphor-icons/react";
import { useLayoutEffect, useRef, type ReactNode } from "react";
import { LogoTile } from "~/components/pyo/IntegrationDialog";

/** A payment provider's checkout in a dialog over the page, which shows
 * blurred behind it. Only the close button or Escape closes it, never a
 * click beside it, so a payment isn't lost by accident. The checkout goes in
 * `children`, over a spinner that shows until it paints. */
export function PayDialog({
  title,
  icon,
  onClose,
  children,
}: {
  title: string;
  icon: ReactNode;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

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

  return (
    <dialog
      ref={ref}
      tabIndex={-1}
      aria-labelledby="pay-title"
      className="pay-dialog m-auto max-h-none w-[calc(100%-1.5rem)] max-w-[440px] overflow-visible rounded-[22px] border border-border-strong bg-muted p-0 text-foreground shadow-pop outline-none"
    >
      <div className="flex flex-col rounded-[21px] bg-muted p-1.5">
        <div className="flex items-center justify-between gap-4 px-3.5 pb-2.5 pt-2">
          <h2 id="pay-title" className="flex items-center gap-2.5 text-[15px] font-semibold tracking-tight">
            <LogoTile icon={icon} size="sm" />
            {title}
          </h2>
          <button
            type="button"
            onClick={() => closeRef.current()}
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
          {children}
        </div>
      </div>
    </dialog>
  );
}
