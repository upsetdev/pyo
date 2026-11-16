import { CaretRightIcon, DotsThreeVerticalIcon } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { Badge, Dot, type Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Menu, type MenuItem } from "~/components/ui/Menu";
import { site } from "~/config/site";

// One integration on Integrations: a header with its icon, name and
// category, a body that says what it does, and a footer with how it stands
// (connected) or its setup guide (not connected).

export type IntegrationCardProps = {
  id: string;
  name: string;
  category: string;
  icon: ReactNode;
  description: string;
  connected: boolean;
  /** One real figure, connected only. */
  stat?: { label: string; value: ReactNode } | null;
  /** What it enables, not connected only. */
  tags?: string[];
  /** How it stands, connected only. */
  status?: { tone: Tone; label: string } | null;
  /** The docs page (pyo.dev/docs/<page>). */
  docs?: string;
  /** Not connected: the Connect button (or why there isn't one). */
  action?: ReactNode;
  /** Connected: what can be done with it. */
  menu?: MenuItem[];
  /** Just linked to: a ring for a moment. */
  highlight?: boolean;
};

/** A docs link as a plain anchor. */
export const docsUrl = (page: string) => `${site.docsUrl}/${page}`;

export function IntegrationCard({
  id,
  name,
  category,
  icon,
  description,
  connected,
  stat,
  tags,
  status,
  docs,
  action,
  menu,
  highlight,
}: IntegrationCardProps) {
  return (
    <section
      id={id}
      aria-labelledby={`${id}-name`}
      className={`flex min-w-0 scroll-mt-4 flex-col rounded-[20px] border border-frame-border bg-muted p-1.5 transition-shadow duration-500 ${highlight ? "ring-2 ring-ring ring-offset-2 ring-offset-canvas" : ""}`}
    >
      <header className="flex items-center gap-3 px-2 pb-2 pt-1.5">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-[10px] border border-card-border bg-card text-muted-foreground shadow-card">
          {icon}
        </span>
        <div className="min-w-0 flex-1">
          <h2
            id={`${id}-name`}
            className="truncate text-[13.5px] font-semibold leading-5"
          >
            {name}
          </h2>
          <p className="truncate text-[12px] leading-4 text-muted-foreground">
            {category}
          </p>
        </div>
        {connected ? (
          <>
            <Badge tone="success">
              <Dot tone="success" />
              Connected
            </Badge>
            {menu?.length ? (
              <Menu
                items={menu}
                label={`${name} actions`}
                trigger={(t) => (
                  <Button
                    {...t}
                    aria-label={`${name} actions`}
                    variant="ghost"
                    size="icon"
                    className="-mr-1 h-7 w-7"
                  >
                    <DotsThreeVerticalIcon size={16} weight="bold" />
                  </Button>
                )}
              />
            ) : null}
          </>
        ) : (
          action
        )}
      </header>
      <div className="flex flex-1 flex-col rounded-[14px] border border-card-border bg-card px-4 shadow-card">
        <div className="flex-1 space-y-3 py-3.5">
          <p className="text-[13px] leading-5 text-foreground">
            {description}
          </p>
          {connected && stat ? (
            <p className="flex items-baseline justify-between gap-3 text-[13px]">
              <span className="text-muted-foreground">{stat.label}</span>
              <span className="min-w-0 truncate font-medium tabular-nums">
                {stat.value}
              </span>
            </p>
          ) : null}
          {!connected && tags?.length ? (
            <ul className="flex flex-wrap gap-1.5">
              {tags.map((t) => (
                <li
                  key={t}
                  className="rounded-md bg-secondary px-2 py-0.5 text-[12px] text-muted-foreground"
                >
                  {t}
                </li>
              ))}
            </ul>
          ) : null}
        </div>
        {connected && status ? (
          <footer className="flex min-h-11 items-center gap-2 border-t border-border text-[12px] text-muted-foreground">
            <Dot tone={status.tone} />
            <span className="truncate">{status.label}</span>
          </footer>
        ) : !connected && docs ? (
          <footer className="flex min-h-11 items-center justify-end border-t border-border text-[12px]">
            <a
              href={docsUrl(docs)}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-0.5 font-medium text-muted-foreground hover:text-foreground"
            >
              Setup guide
              <CaretRightIcon size={11} weight="bold" />
            </a>
          </footer>
        ) : null}
      </div>
    </section>
  );
}
