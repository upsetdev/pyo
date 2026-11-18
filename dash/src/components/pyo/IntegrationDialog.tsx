import {
  ArrowSquareOutIcon,
  CheckIcon,
  CircleNotchIcon,
} from "@phosphor-icons/react";
import { useEffect, useRef, type FormEvent, type ReactNode } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { site } from "~/config/site";

// One pattern for every integration's dialog: its logo and name, one short
// sentence with the guide, the fields or what's connected, then Cancel and
// one primary action. Disconnecting asks inline, in the dialog itself.

/** A docs link as a plain anchor. */
export const docsUrl = (page: string) => `${site.docsUrl}/${page}`;

/** The logo on a small tile, as on the card. */
export function LogoTile({ icon, size = "md" }: { icon: ReactNode; size?: "sm" | "md" }) {
  return (
    <span
      className={`flex shrink-0 items-center justify-center rounded-lg border border-frame-border bg-card text-foreground ${size === "sm" ? "h-7 w-7" : "h-9 w-9"}`}
    >
      {icon}
    </span>
  );
}

export function IntegrationDialog({
  icon,
  name,
  description,
  docs,
  onClose,
  onSubmit,
  footer,
  children,
}: {
  icon: ReactNode;
  name: string;
  /** One short sentence. */
  description?: ReactNode;
  /** The docs page, linked after the description. */
  docs?: string;
  onClose: () => void;
  /** The body is a form: Enter submits. */
  onSubmit?: (e: FormEvent) => void;
  footer?: ReactNode;
  children: ReactNode;
}) {
  return (
    <Dialog
      open
      onClose={onClose}
      onSubmit={onSubmit}
      title={
        <span className="flex items-center gap-2.5">
          <LogoTile icon={icon} size="sm" />
          {name}
        </span>
      }
      description={
        description || docs ? (
          <>
            {description}
            {description && docs ? " " : null}
            {docs ? <ExternalLink href={docsUrl(docs)}>Guide</ExternalLink> : null}
          </>
        ) : undefined
      }
      footer={footer}
    >
      {children}
    </Dialog>
  );
}

/** A link out of Pyo, marked as one. */
export function ExternalLink({
  href,
  children,
  className = "",
}: {
  href: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className={`inline-flex items-center gap-1 whitespace-nowrap font-medium text-primary hover:text-primary/80 ${className}`}
    >
      {children}
      <ArrowSquareOutIcon size={12} aria-hidden />
      <span className="sr-only">(opens in a new tab)</span>
    </a>
  );
}

/** A small text button, for a second way in ("Use an OAuth client instead"). */
export function TextButton({ onClick, children }: { onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="rounded text-[12px] font-medium text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
    >
      {children}
    </button>
  );
}

/** A button's label while it works: a spinner and the label. */
export function Busy({ children }: { children: ReactNode }) {
  return (
    <>
      <CircleNotchIcon size={12} className="animate-spin motion-reduce:animate-none" aria-hidden />
      {children}
    </>
  );
}

/** Where a flow is: done steps get a check, the current one is marked. */
export function Steps({ steps, at }: { steps: string[]; at: number }) {
  return (
    <ol aria-label="Steps" className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[12px]">
      {steps.map((label, i) => (
        <li
          key={label}
          aria-current={i === at ? "step" : undefined}
          className="flex items-center gap-2"
        >
          <span
            className={`flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-[11px] font-semibold ${
              i < at
                ? "bg-success/15 text-success"
                : i === at
                  ? "bg-primary/12 text-primary dark:text-info"
                  : "bg-secondary text-muted-foreground"
            }`}
          >
            {i < at ? <CheckIcon size={11} weight="bold" aria-label="Done" /> : i + 1}
          </span>
          <span className={i === at ? "font-medium text-foreground" : "text-muted-foreground"}>
            {label}
          </span>
          {i < steps.length - 1 ? <span aria-hidden className="h-px w-5 bg-border" /> : null}
        </li>
      ))}
    </ol>
  );
}

/** A row in a dialog's list: icon, name, one line about it, and its actions. */
export function DialogRow({
  icon,
  title,
  meta,
  badge,
  actions,
  children,
}: {
  icon?: ReactNode;
  title: ReactNode;
  meta?: ReactNode;
  badge?: ReactNode;
  actions?: ReactNode;
  /** Under the row: an inline confirm, an error. */
  children?: ReactNode;
}) {
  return (
    <li className="space-y-3 px-5 py-3">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        {icon ? (
          <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground">
            {icon}
          </span>
        ) : null}
        <div className="min-w-0 flex-1">
          <p className="truncate text-[13px] font-medium">{title}</p>
          {meta ? <p className="truncate text-[12px] text-muted-foreground">{meta}</p> : null}
        </div>
        {badge}
        {actions ? <span className="flex shrink-0 items-center gap-1">{actions}</span> : null}
      </div>
      {children}
    </li>
  );
}

/** Rows that run edge to edge in the dialog's card. */
export function DialogRows({ children }: { children: ReactNode }) {
  return <ul className="list-rows -mx-5 -my-4">{children}</ul>;
}

/**
 * Confirm a destructive action in place (no second dialog): what happens,
 * then Cancel and the action. Focus moves to Cancel when it shows.
 */
export function ConfirmNote({
  title,
  children,
  confirmLabel,
  busy,
  onConfirm,
  onCancel,
}: {
  title: string;
  children?: ReactNode;
  confirmLabel: string;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    cancel.current?.focus();
    cancel.current?.scrollIntoView({ block: "nearest" });
  }, []);
  return (
    <div
      role="group"
      aria-label={title}
      className="rounded-lg border border-destructive/30 bg-destructive/[0.07] px-3 py-2.5 text-[12.5px] leading-5"
    >
      <p className="font-medium text-foreground">{title}</p>
      {children ? <p className="text-muted-foreground">{children}</p> : null}
      <div className="mt-2.5 flex flex-wrap justify-end gap-2">
        <Button ref={cancel} variant="ghost" size="xs" disabled={busy} onClick={onCancel}>
          Cancel
        </Button>
        <Button variant="danger" size="xs" disabled={busy} onClick={onConfirm}>
          {busy ? <Busy>{confirmLabel}</Busy> : confirmLabel}
        </Button>
      </div>
    </div>
  );
}

/** What's connected, as label and value rows in a box. */
export function Summary({ items }: { items: [ReactNode, ReactNode][] }) {
  return (
    <dl className="list-rows rounded-lg border border-border [--row-min:0]">
      {items.map(([k, v], i) => (
        <div key={i} className="flex items-center justify-between gap-4 px-3 py-2 text-[12.5px]">
          <dt className="shrink-0 text-muted-foreground">{k}</dt>
          <dd className="flex min-w-0 items-center justify-end gap-2 text-right text-foreground">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

/** A section's small heading inside a dialog. */
export function DialogSection({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-2">
      <h3 className="text-[12px] font-medium text-muted-foreground">{title}</h3>
      {children}
    </section>
  );
}
