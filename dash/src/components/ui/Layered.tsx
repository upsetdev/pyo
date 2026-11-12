import { createContext, useContext, type ReactNode } from "react";

const InLayer = createContext(false);
/** True inside a Layered section: nested panels render as plain cards. */
export const useInLayer = () => useContext(InLayer);

/**
 * Two layers: content on a card, set in a tinted one that can carry a title
 * (with a description and an action) above it and a short note under it.
 * `bare` when the content brings its own cards (a grid of them, or a list
 * that's one). `danger` tints it for destructive settings.
 */
export function Layered({
  title,
  description,
  action,
  footer,
  bare = false,
  tone,
  id,
  className = "",
  bodyClassName = "",
  children,
}: {
  title?: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  footer?: ReactNode;
  bare?: boolean;
  tone?: "danger";
  id?: string;
  className?: string;
  bodyClassName?: string;
  children: ReactNode;
}) {
  const danger = tone === "danger";
  return (
    <section
      id={id}
      aria-labelledby={title && id ? `${id}-title` : undefined}
      className={`min-w-0 rounded-[20px] border p-1.5 ${danger ? "border-destructive/15 bg-destructive/[0.06] dark:bg-destructive/[0.08]" : "border-frame-border bg-muted"} ${className}`}
    >
      {title || action ? (
        <div className="flex min-h-9 flex-wrap items-center justify-between gap-x-3 gap-y-1 px-3 pb-1.5 pt-1">
          <div className="min-w-0">
            {title ? (
              <h2
                id={id ? `${id}-title` : undefined}
                className={`text-[13.5px] font-semibold ${danger ? "text-destructive" : ""}`}
              >
                {title}
              </h2>
            ) : null}
            {description ? (
              <p className="text-[12.5px] text-muted-foreground">
                {description}
              </p>
            ) : null}
          </div>
          {action ? (
            <div className="flex shrink-0 items-center gap-2">{action}</div>
          ) : null}
        </div>
      ) : null}
      <InLayer.Provider value>
        {bare ? (
          children
        ) : (
          <div
            className={`min-w-0 overflow-hidden rounded-[14px] border bg-card shadow-card ${danger ? "border-destructive/25" : "border-card-border"} ${bodyClassName}`}
          >
            {children}
          </div>
        )}
      </InLayer.Provider>
      {footer ? (
        <div className="px-3 pb-1.5 pt-2.5 text-[12.5px] text-muted-foreground">
          {footer}
        </div>
      ) : null}
    </section>
  );
}

/**
 * A list page's body: the toolbar (search, filters, view) set in the tinted
 * layer, then the content's own card, table or grid.
 */
export function ListSection({
  toolbar,
  title,
  action,
  footer,
  id,
  children,
}: {
  toolbar?: ReactNode;
  title?: ReactNode;
  action?: ReactNode;
  footer?: ReactNode;
  id?: string;
  children: ReactNode;
}) {
  return (
    <Layered bare id={id} title={title} action={action} footer={footer}>
      {toolbar ? <div className={TOOLBAR}>{toolbar}</div> : null}
      {children}
    </Layered>
  );
}

/** Controls in a layered toolbar: the fields' 36px height, radius and surface. */
export const TOOLBAR =
  "mb-1.5 flex flex-wrap items-center gap-1.5 [&_input[type=search]]:rounded-[10px] [&_input[type=search]]:pl-9 [&_svg.absolute]:left-3.5";

/** A count beside a section title. */
export function Count({ n, tone }: { n: number; tone?: "danger" }) {
  return (
    <span
      className={`rounded-full px-1.5 text-[11.5px] font-medium tabular-nums ${tone === "danger" ? "bg-destructive/10 text-destructive" : "bg-foreground/[0.06] text-muted-foreground"}`}
    >
      {n}
    </span>
  );
}
