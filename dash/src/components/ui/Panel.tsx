import {
  ArrowClockwiseIcon,
  MagnifyingGlassIcon,
  WarningCircleIcon,
  type Icon,
} from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { Button } from "./Button";
import { Layered, useInLayer } from "./Layered";

// Layout primitives. Panels are layered: a tinted surround holding the title,
// a card with the content; content inside uses dividers, not more cards.

export function PageHeader({
  title,
  description,
  actions,
  meta,
  icon,
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  meta?: ReactNode;
  icon?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
      <div className="flex min-w-0 items-start gap-3">
        {icon}
        <div className="min-w-0">
          <h1 className="truncate text-[24px] font-semibold tracking-tight text-foreground">
            {title}
          </h1>
          {description ? (
            <p className="mt-0.5 text-[13px] text-muted-foreground">
              {description}
            </p>
          ) : null}
          {meta ? (
            <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1.5 text-[12px] text-muted-foreground">
              {meta}
            </div>
          ) : null}
        </div>
      </div>
      {actions ? (
        <div className="flex flex-wrap items-center gap-2">{actions}</div>
      ) : null}
    </div>
  );
}

export function Page({
  children,
  width = "max-w-6xl",
}: {
  children: ReactNode;
  width?: string;
}) {
  return (
    <div
      className={`mx-auto w-full ${width} space-y-8 px-4 py-8 md:px-8 md:py-10`}
    >
      {children}
    </div>
  );
}

/** A titled section: the layered card (see Layered), with the old Panel API. */
export function Panel({
  title,
  description,
  actions,
  footer,
  children,
  className = "",
  bodyClassName = "",
  tone,
  id,
}: {
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  footer?: ReactNode;
  children: ReactNode;
  className?: string;
  bodyClassName?: string;
  tone?: "danger";
  id?: string;
}) {
  const nested = useInLayer();
  if (nested)
    // Already in a layer: one card, its title in a divided header.
    return (
      <section
        id={id}
        className={`min-w-0 overflow-hidden rounded-[14px] border bg-card ${tone === "danger" ? "border-destructive/25" : "border-card-border"} ${className}`}
      >
        {title || actions ? (
          <header className="flex min-h-12 flex-wrap items-center justify-between gap-x-3 gap-y-1 border-b border-border px-4 py-3">
            <div className="min-w-0">
              {title ? (
                <h2 className="text-[13.5px] font-semibold">{title}</h2>
              ) : null}
              {description ? (
                <p className="text-[12px] text-muted-foreground">
                  {description}
                </p>
              ) : null}
            </div>
            {actions ? (
              <div className="flex items-center gap-2">{actions}</div>
            ) : null}
          </header>
        ) : null}
        <div className={bodyClassName}>{children}</div>
        {footer ? (
          <div className="border-t border-border px-4 py-2.5 text-[12.5px] text-muted-foreground">
            {footer}
          </div>
        ) : null}
      </section>
    );
  return (
    <Layered
      id={id}
      title={title}
      description={description}
      action={actions}
      footer={footer}
      tone={tone}
      className={className}
      bodyClassName={bodyClassName}
    >
      {children}
    </Layered>
  );
}

/** Label/value rows for metadata. */
export function KeyValue({
  items,
  className = "",
}: {
  items: [ReactNode, ReactNode][];
  className?: string;
}) {
  return (
    <dl className={`list-rows [--row-min:0] ${className}`}>
      {items.map(([k, v], i) => (
        <div
          key={i}
          className="flex items-start justify-between gap-4 px-4 py-2.5 text-[12.5px]"
        >
          <dt className="shrink-0 text-muted-foreground">{k}</dt>
          <dd className="min-w-0 text-right text-foreground">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export function EmptyState({
  icon: IconCmp,
  title,
  description,
  action,
  compact,
}: {
  icon?: Icon;
  /** None: just the action, centered. */
  title?: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  compact?: boolean;
}) {
  return (
    <div
      className={`flex flex-col items-center justify-center text-center ${compact ? "px-4 py-8" : "px-6 py-14"}`}
    >
      {IconCmp ? (
        <div className="mb-3 flex h-10 w-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
          <IconCmp size={18} />
        </div>
      ) : null}
      {title ? <p className="text-[13.5px] font-medium text-foreground">{title}</p> : null}
      {description ? (
        <p className="mt-1 max-w-sm text-[12.5px] leading-5 text-muted-foreground">
          {description}
        </p>
      ) : null}
      {action ? (
        <div className={`flex flex-wrap justify-center gap-2 ${title || description ? "mt-4" : ""}`}>{action}</div>
      ) : null}
    </div>
  );
}

export function ErrorState({
  title,
  description,
  onRetry,
}: {
  title: string;
  description?: ReactNode;
  onRetry?: () => void;
}) {
  return (
    <div
      role="alert"
      className="flex flex-col items-center justify-center px-6 py-10 text-center"
    >
      <WarningCircleIcon size={22} className="mb-2 text-destructive" />
      {title ? <p className="text-[13.5px] font-medium text-foreground">{title}</p> : null}
      {description ? (
        <p className="mt-1 max-w-sm text-[12.5px] leading-5 text-muted-foreground">
          {description}
        </p>
      ) : null}
      {onRetry ? (
        <Button
          variant="secondary"
          size="sm"
          className="mt-4"
          onClick={onRetry}
        >
          <ArrowClockwiseIcon size={13} /> Retry
        </Button>
      ) : null}
    </div>
  );
}

export function Skeleton({ className = "" }: { className?: string }) {
  return (
    <div
      aria-hidden
      className={`animate-pulse rounded bg-muted ${className}`}
    />
  );
}

/** Compact metric for summary rows: label, value, optional context. */
export function Metric({
  label,
  value,
  sub,
  tone,
}: {
  label: ReactNode;
  value: ReactNode;
  sub?: ReactNode;
  tone?: "danger" | "warning" | "success";
}) {
  const color =
    tone === "danger"
      ? "text-destructive"
      : tone === "warning"
        ? "text-warning"
        : tone === "success"
          ? "text-success"
          : "text-foreground";
  return (
    <div className="min-w-0 px-5 py-4">
      <p className="truncate text-[12.5px] text-muted-foreground">{label}</p>
      <p
        className={`mt-1.5 text-[24px] font-semibold tabular-nums tracking-tight ${color}`}
      >
        {value}
      </p>
      {sub ? (
        <p className="truncate text-[11.5px] text-muted-foreground">{sub}</p>
      ) : null}
    </div>
  );
}

/** Row of metrics separated by dividers inside one panel. */
export function MetricStrip({
  children,
  cols = "grid-cols-2 sm:grid-cols-4",
}: {
  children: ReactNode;
  cols?: string;
}) {
  const strip = (
    <div
      className={`grid ${cols} gap-px overflow-hidden rounded-[14px] border border-card-border bg-border [&>*]:bg-card`}
    >
      {children}
    </div>
  );
  return useInLayer() ? strip : <Layered bare>{strip}</Layered>;
}

/** Utilization bar: used / capacity, turning amber then red. */
export function Meter({
  value,
  max,
  label,
  showLabel = true,
  className = "",
  warnAt = 0.75,
  badAt = 0.9,
}: {
  value: number;
  max: number;
  label?: string;
  showLabel?: boolean;
  className?: string;
  /** Where the bar turns warning, then destructive (disks: 0.85 and 0.95). */
  warnAt?: number;
  badAt?: number;
}) {
  const ratio = max > 0 ? Math.min(1, value / max) : 0;
  const color =
    ratio > badAt ? "bg-destructive" : ratio > warnAt ? "bg-warning" : "bg-primary";
  return (
    <div className={`flex min-w-0 items-center gap-2 ${className}`}>
      <div
        className="h-1.5 min-w-10 flex-1 overflow-hidden rounded-full bg-foreground/10"
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={max}
        aria-valuenow={value}
      >
        <div
          className={`h-full rounded-full ${color}`}
          style={{ width: `${Math.max(ratio * 100, ratio > 0 ? 2 : 0)}%` }}
        />
      </div>
      {showLabel ? (
        <span className="w-9 shrink-0 text-right text-[11.5px] tabular-nums text-muted-foreground">
          {Math.round(ratio * 100)}%
        </span>
      ) : null}
    </div>
  );
}

export function Callout({
  tone = "info",
  icon: IconCmp,
  title,
  children,
}: {
  tone?: "info" | "warning" | "danger" | "neutral";
  icon?: Icon;
  title?: ReactNode;
  children?: ReactNode;
}) {
  const cls = {
    info: "border-info/25 bg-info/[0.06]",
    warning: "border-warning/30 bg-warning/[0.07]",
    danger: "border-destructive/30 bg-destructive/[0.07]",
    neutral: "border-border bg-muted/40",
  }[tone];
  const iconCls = {
    info: "text-info",
    warning: "text-warning",
    danger: "text-destructive",
    neutral: "text-muted-foreground",
  }[tone];
  return (
    <div
      className={`flex gap-2.5 rounded-lg border px-3 py-2.5 text-[13px] leading-5 ${cls}`}
      role={tone === "danger" ? "alert" : undefined}
    >
      {IconCmp ? (
        <IconCmp size={15} className={`mt-0.5 shrink-0 ${iconCls}`} />
      ) : null}
      <div className="min-w-0 text-foreground/90">
        {title ? <p className="font-medium text-foreground">{title}</p> : null}
        {children ? (
          <div className={title ? "mt-0.5 text-muted-foreground" : ""}>
            {children}
          </div>
        ) : null}
      </div>
    </div>
  );
}

export function Mono({
  children,
  className = "",
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <span className={`font-mono text-[11.5px] ${className}`}>{children}</span>
  );
}

/** A search or filter that matched nothing, with a way back. */
export function NoMatches({
  query,
  what = "results",
  hint = "Try another search or filter.",
  onClear,
}: {
  query?: string;
  what?: string;
  hint?: ReactNode;
  onClear?: () => void;
}) {
  const q = query?.trim();
  return (
    <div className="flex flex-col items-center rounded-[14px] border border-card-border bg-card px-6 py-12 text-center">
      <span className="flex h-10 w-10 items-center justify-center rounded-full border border-border bg-muted/60 text-muted-foreground">
        <MagnifyingGlassIcon size={16} />
      </span>
      <p className="mt-4 text-[14px] font-medium">
        {q ? (
          <>
            No {what} match “{q}”
          </>
        ) : (
          <>No {what} match these filters</>
        )}
      </p>
      <p className="mt-1 text-[13px] text-muted-foreground">{hint}</p>
      {onClear ? (
        <Button
          variant="secondary"
          size="sm"
          className="mt-4"
          onClick={onClear}
        >
          Clear {q ? "search" : "filters"}
        </Button>
      ) : null}
    </div>
  );
}
