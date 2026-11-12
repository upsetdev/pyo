import { WarningIcon, XIcon } from "@phosphor-icons/react";
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type FormEvent,
  type ReactNode,
} from "react";
import { Button } from "./Button";
import { Field, Input } from "./Field";

// Modal dialog and side sheet on the native <dialog> element: the browser
// provides the top layer, focus containment, Escape to close and inert
// background. Focus handling is below.
//
// Layered like the dashboard's sections (ui/Layered): title and buttons on
// a tinted frame, the content on a card inside it. The frame's tint is laid
// over the canvas color so the page doesn't show through.

const FRAME = "bg-muted";
const CARD =
  "min-h-0 flex-1 overflow-y-auto rounded-[14px] border border-card-border bg-card px-5 py-4 shadow-card";
const CLOSE =
  "-mr-1 rounded-md p-1 text-muted-foreground hover:bg-foreground/[0.06] hover:text-foreground";

// On open, focus goes to the dialog itself, not into it: screen readers
// announce it, Escape and the focus trap work, and the first Tab reaches
// its first control. Fields don't take focus on their own (no autoFocus in
// dialogs). On close, focus returns to the element that opened it.
function useNativeDialog(open: boolean, onClose: () => void) {
  const ref = useRef<HTMLDialogElement>(null);
  const opener = useRef<HTMLElement | null>(null);
  const restore = () => {
    const o = opener.current;
    opener.current = null;
    if (o?.isConnected) o.focus({ preventScroll: true });
  };
  useLayoutEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) {
      opener.current =
        document.activeElement instanceof HTMLElement ? document.activeElement : null;
      d.showModal();
      d.focus({ preventScroll: true });
    }
    if (!open && d.open) {
      d.close();
      restore();
    }
  }, [open]);
  // Unmounted while open (a parent that remounts it with a new key).
  useLayoutEffect(() => {
    const d = ref.current;
    return () => {
      if (!d?.open) return;
      d.close();
      restore();
    };
  }, []);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    const onCancel = (e: Event) => {
      e.preventDefault();
      onClose();
    };
    d.addEventListener("cancel", onCancel);
    return () => d.removeEventListener("cancel", onCancel);
  }, [onClose]);
  return ref;
}

export function Dialog({
  open,
  onClose,
  title,
  description,
  children,
  footer,
  onSubmit,
  width = "max-w-lg",
  fullOnPhone = false,
}: {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  /** Renders the body as a form; Enter submits. */
  onSubmit?: (e: FormEvent) => void;
  width?: string;
  /** A full-screen sheet on phones. */
  fullOnPhone?: boolean;
}) {
  const ref = useNativeDialog(open, onClose);
  const Body = onSubmit ? "form" : "div";
  const phone = fullOnPhone ? "max-sm:m-0 max-sm:h-dvh max-sm:max-h-dvh max-sm:w-full max-sm:max-w-none max-sm:rounded-none max-sm:border-0" : "";
  return (
    <dialog
      ref={ref}
      tabIndex={-1}
      aria-labelledby="dialog-title"
      onClick={(e) => e.target === e.currentTarget && onClose()}
      className={`m-auto w-[calc(100%-2rem)] ${width} overflow-visible rounded-[22px] outline-none border border-border-strong bg-muted p-0 text-foreground shadow-pop ${phone}`}
    >
      {open ? (
        <Body
          onSubmit={
            onSubmit
              ? (e: FormEvent) => {
                  e.preventDefault();
                  onSubmit(e);
                }
              : undefined
          }
          noValidate
          className={`flex max-h-[85dvh] flex-col rounded-[21px] p-1.5 ${FRAME} ${fullOnPhone ? "max-sm:h-full max-sm:max-h-none max-sm:rounded-none" : ""}`}
        >
          <div className="flex items-start justify-between gap-4 px-3.5 pb-3 pt-2.5">
            <div className="min-w-0">
              <h2
                id="dialog-title"
                className="text-[15px] font-semibold tracking-tight"
              >
                {title}
              </h2>
              {description ? (
                <p className="mt-1 text-[12.5px] leading-5 text-muted-foreground">
                  {description}
                </p>
              ) : null}
            </div>
            <button
              type="button"
              onClick={onClose}
              aria-label="Close"
              className={CLOSE}
            >
              <XIcon size={15} />
            </button>
          </div>
          {children ? <div className={CARD}>{children}</div> : null}
          {footer ? (
            <div className="flex flex-wrap items-center justify-end gap-2 px-2 pb-1 pt-2.5">
              {footer}
            </div>
          ) : null}
        </Body>
      ) : null}
    </dialog>
  );
}

export function Sheet({
  open,
  onClose,
  title,
  description,
  children,
  footer,
}: {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  description?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
}) {
  const ref = useNativeDialog(open, onClose);
  return (
    <dialog
      ref={ref}
      tabIndex={-1}
      aria-labelledby="sheet-title"
      onClick={(e) => e.target === e.currentTarget && onClose()}
      className="sheet fixed outline-none inset-y-0 left-auto right-0 m-0 h-dvh max-h-dvh w-full max-w-md border-l border-border-strong bg-canvas p-0 text-foreground shadow-pop"
    >
      {open ? (
        <div className={`flex h-full flex-col p-1.5 ${FRAME}`}>
          <div className="flex items-start justify-between gap-4 px-3.5 pb-3 pt-2.5">
            <div className="min-w-0">
              <h2
                id="sheet-title"
                className="text-[15px] font-semibold tracking-tight"
              >
                {title}
              </h2>
              {description ? (
                <p className="mt-1 text-[12.5px] text-muted-foreground">
                  {description}
                </p>
              ) : null}
            </div>
            <button
              type="button"
              onClick={onClose}
              aria-label="Close"
              className={CLOSE}
            >
              <XIcon size={15} />
            </button>
          </div>
          <div className={CARD}>{children}</div>
          {footer ? (
            <div className="flex items-center justify-end gap-2 px-2 pb-1 pt-2.5">
              {footer}
            </div>
          ) : null}
        </div>
      ) : null}
    </dialog>
  );
}

/**
 * Confirmation for destructive or disruptive actions. With `confirmText`, the
 * user types it to enable the button.
 */
export function ConfirmDialog({
  open,
  onClose,
  onConfirm,
  title,
  description,
  confirmLabel,
  confirmText,
  tone = "danger",
  children,
}: {
  open: boolean;
  onClose: () => void;
  onConfirm: () => void;
  title: string;
  description: ReactNode;
  confirmLabel: string;
  confirmText?: string;
  tone?: "danger" | "primary";
  children?: ReactNode;
}) {
  const [typed, setTyped] = useState("");
  useEffect(() => {
    if (open) setTyped("");
  }, [open]);
  const ok = !confirmText || typed.trim() === confirmText;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={
        <span className="flex items-center gap-2">
          {tone === "danger" ? (
            <WarningIcon size={16} weight="fill" className="text-destructive" />
          ) : null}
          {title}
        </span>
      }
      onSubmit={() => {
        if (!ok) return;
        onConfirm();
        onClose();
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant={tone} size="sm" disabled={!ok}>
            {confirmLabel}
          </Button>
        </>
      }
    >
      <div className="space-y-3 text-[13px] leading-5 text-muted-foreground">
        <div>{description}</div>
        {children}
        {confirmText ? (
          <Field
            label={
              <span>
                Type{" "}
                <span className="font-mono text-foreground">{confirmText}</span>{" "}
                to confirm
              </span>
            }
          >
            {(p) => (
              <Input
                {...p}
                value={typed}
                onChange={(e) => setTyped(e.target.value)}
                mono
                autoComplete="off"
              />
            )}
          </Field>
        ) : null}
      </div>
    </Dialog>
  );
}
