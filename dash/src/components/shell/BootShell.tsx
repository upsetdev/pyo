import {
  BellIcon,
  ListIcon,
  PlusIcon,
  SidebarSimpleIcon,
} from "@phosphor-icons/react";
import { Logo } from "~/components/site/Logo";
import { Button } from "~/components/ui/Button";
import { NAV_GROUPS, NAV_LABEL, navIconClass, navItemClass } from "./nav";

// Marks the current page's link. It runs as soon as the links exist, before
// the first paint, because the app itself hasn't loaded yet. It rewrites the
// link's and icon's class before React hydrates, so those two elements
// suppress the expected hydration mismatch.
const markCurrent = `(function(){var p="/"+location.pathname.split("/").slice(2).join("/");p=p.replace(/\\/$/,"")||"/";document.querySelectorAll("[data-boot-href]").forEach(function(a){var h=a.dataset.bootHref,hs=[h].concat((a.dataset.match||"").split(",").filter(Boolean));if(a.hasAttribute("data-exact")?p===h:hs.some(function(x){return p===x||p.indexOf(x+"/")===0})){a.className=a.dataset.on;var i=a.querySelector("svg");if(i)i.setAttribute("class",a.dataset.iconOn)}})})()`;

/**
 * The dashboard's frame, in the static HTML: the real sidebar and top bar from
 * the first paint, before any JavaScript, in the same places as AppShell so
 * nothing moves when it takes over. Only what depends on data is a placeholder.
 */
export function BootShell() {
  const bar = "rounded-md bg-muted/70";
  return (
    <div className="boot-app h-full bg-sidebar">
      <aside className="hidden lg:fixed lg:inset-y-0 lg:left-0 lg:flex lg:w-60 lg:flex-col">
        <div className="flex h-14 shrink-0 items-center px-3">
          <span className="flex h-8 shrink-0 items-center px-[7px]">
            <Logo showName={false} />
          </span>
          <svg
            aria-hidden
            viewBox="0 0 12 20"
            className="h-5 w-3 shrink-0 text-border"
          >
            <path
              d="M9 2 3 18"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
            />
          </svg>
          <div className="ml-1 flex h-8 min-w-0 flex-1 items-center">
            <div className={`h-4 w-28 animate-pulse ${bar}`} />
          </div>
        </div>
        <div className="mt-4 flex flex-1 flex-col overflow-hidden px-3 pb-4">
          <div className="space-y-8">
            {NAV_GROUPS.map((g, i) => (
              <div key={i}>
                {g.label ? <p className={NAV_LABEL}>{g.label}</p> : null}
                <ul className="flex flex-col gap-1">
                  {g.items
                    .filter((item) => !item.feature)
                    .map((item) => {
                      const Icon = item.icon;
                      return (
                        <li key={item.href}>
                          <div
                            data-boot-href={item.href}
                            data-exact={item.exact ? "" : undefined}
                            data-match={item.match?.join(",")}
                            data-on={navItemClass(true)}
                            data-icon-on={navIconClass(true)}
                            className={navItemClass(false)}
                            suppressHydrationWarning
                          >
                            <Icon
                              size={16}
                              className={navIconClass(false)}
                              suppressHydrationWarning
                            />
                            {item.label}
                          </div>
                        </li>
                      );
                    })}
                </ul>
              </div>
            ))}
          </div>
        </div>
      </aside>
      <script dangerouslySetInnerHTML={{ __html: markCurrent }} />
      <div className="flex h-full min-w-0 flex-col lg:pl-60">
        <div className="flex min-h-0 flex-1 flex-col bg-canvas">
          <div className="flex h-14 shrink-0 items-center gap-2 border-b border-border bg-canvas/85 px-4 md:px-8">
            <span className="-ml-1.5 inline-flex h-8 w-8 items-center justify-center rounded-full text-muted-foreground lg:hidden">
              <ListIcon size={16} />
            </span>
            <span className="-ml-1.5 hidden h-8 w-8 items-center justify-center rounded-full text-muted-foreground lg:inline-flex">
              <SidebarSimpleIcon size={16} />
            </span>
            <div className="min-w-0 flex-1" />
            <div className="flex shrink-0 items-center gap-3">
              <Button size="sm" tabIndex={-1}>
                <PlusIcon size={13} weight="bold" />
                <span className="hidden sm:inline">New</span>
              </Button>
              <span className="inline-flex h-8 w-8 items-center justify-center text-muted-foreground">
                <BellIcon size={16} />
              </span>
              <div className="h-8 w-8 animate-pulse rounded-full bg-muted" />
            </div>
          </div>
          <div className="mx-auto w-full max-w-6xl animate-pulse space-y-3 px-4 py-8 md:px-8">
            <div className={`h-7 w-48 ${bar}`} />
            <div className={`h-4 w-72 ${bar} opacity-60`} />
            <div className="grid gap-4 pt-5 sm:grid-cols-3">
              <div className="h-28 rounded-[14px] border border-card-border bg-card" />
              <div className="h-28 rounded-[14px] border border-card-border bg-card" />
              <div className="h-28 rounded-[14px] border border-card-border bg-card" />
            </div>
            <div className="h-64 rounded-[14px] border border-card-border bg-card" />
          </div>
        </div>
      </div>
    </div>
  );
}
