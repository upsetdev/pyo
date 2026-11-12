import type { ReactNode } from "react";

export type NoteTone =
  "success" | "progress" | "warning" | "danger" | "neutral";

// Exactly Callout's look (the server page's status card), plus a success and
// an in-progress tone and room for an action.
const BOX: Record<NoteTone, string> = {
  success: "border-success/30 bg-success/[0.07]",
  progress: "border-info/25 bg-info/[0.06]",
  warning: "border-warning/30 bg-warning/[0.07]",
  danger: "border-destructive/30 bg-destructive/[0.07]",
  neutral: "border-border bg-muted/40",
};
const ICON: Record<NoteTone, string> = {
  success: "text-success",
  progress: "text-info",
  warning: "text-warning",
  danger: "text-destructive",
  neutral: "text-muted-foreground",
};

/** A state in one line or two: an icon, a short title and one sentence. */
export function StatusNote({
  tone,
  icon,
  title,
  children,
  action,
  label = "Status",
}: {
  tone: NoteTone;
  icon: ReactNode;
  title: ReactNode;
  children?: ReactNode;
  action?: ReactNode;
  label?: string;
}) {
  return (
    <section
      aria-label={label}
      aria-live="polite"
      role={tone === "danger" ? "alert" : undefined}
      className={`flex gap-2.5 rounded-lg border px-3 py-2.5 text-[13px] leading-5 ${BOX[tone]}`}
    >
      <span
        className={`mt-0.5 flex h-[15px] w-[15px] shrink-0 items-center justify-center ${ICON[tone]}`}
      >
        {icon}
      </span>
      <div className="min-w-0 text-foreground/90">
        <p className="font-medium text-foreground">{title}</p>
        {children ? (
          <div className="mt-0.5 text-muted-foreground">{children}</div>
        ) : null}
      </div>
      {action ? (
        <div className="ml-auto shrink-0 self-center">{action}</div>
      ) : null}
    </section>
  );
}
