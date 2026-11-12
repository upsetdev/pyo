/// <reference types="vite/client" />
import {
  createRootRoute,
  type ErrorComponentProps,
  HeadContent,
  Outlet,
  Scripts,
} from "@tanstack/react-router";
import type { ReactNode } from "react";
import { BootShell } from "~/components/shell/BootShell";
import { ThemeProvider, themeScript } from "~/components/site/ThemeProvider";
import { Button, ButtonLink } from "~/components/ui/Button";
import { seo } from "~/lib/seo";
import appCss from "~/styles.css?url";

export const Route = createRootRoute({
  // The dashboard is a static SPA: the prerendered shell is just <html> and
  // <head>. Rendering a route into it would mismatch every other path.
  ssr: false,
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      ...seo(),
    ],
    links: [
      { rel: "stylesheet", href: appCss },
      { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" },
    ],
  }),
  shellComponent: RootDocument,
  component: () => <Outlet />,
  notFoundComponent: NotFound,
  errorComponent: RouteError,
});

function RootDocument({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        {/* Applies the saved theme before first paint (no flash). */}
        <script dangerouslySetInnerHTML={{ __html: themeScript }} />
        {/* Which skeleton the static page shows until the app draws (see BootSkeleton). */}
        <script dangerouslySetInnerHTML={{ __html: bootScript }} />
        <HeadContent />
      </head>
      <body>
        <BootSkeleton />
        <ThemeProvider>{children}</ThemeProvider>
        <Scripts />
      </body>
    </html>
  );
}

const bootScript = `document.documentElement.dataset.boot=/^\\/(login|signup|setup)(\\/|$)/.test(location.pathname)?"auth":"app"`;

/**
 * What shows from the first paint, before any JavaScript, until a page marked
 * data-ready renders (styles.css): the dashboard's frame, or the sign-in form's
 * outline, by path.
 */
function BootSkeleton() {
  const bar = "rounded-md bg-muted/70";
  return (
    <div id="boot" aria-hidden="true" className="fixed inset-0 -z-10 bg-sidebar">
      <BootShell />
      <div className="boot-auth flex h-full animate-pulse items-center justify-center px-4">
        <div className="w-full max-w-[360px] space-y-3">
          <div className="mx-auto mb-8 h-9 w-9 rounded-lg bg-muted" />
          <div className={`mx-auto h-5 w-40 ${bar}`} />
          <div className="h-9 rounded-lg border border-border bg-card" />
          <div className="h-9 rounded-lg border border-border bg-card" />
          <div className="h-9 rounded-lg bg-muted" />
        </div>
      </div>
    </div>
  );
}

function NotFound() {
  return (
    <main
      data-ready
      className="flex min-h-dvh flex-col items-center justify-center bg-canvas px-6 py-24 text-center"
    >
      <p className="font-mono text-[12px] text-muted-foreground">404</p>
      <h1 className="mt-3 text-2xl font-semibold tracking-tight text-foreground">
        Page not found
      </h1>
      <p className="mt-2 max-w-md text-[13px] leading-6 text-muted-foreground">
        This page doesn't exist.
      </p>
      <div className="mt-6 flex flex-wrap justify-center gap-2">
        <ButtonLink href="/">Go to overview</ButtonLink>
        <ButtonLink href="/projects" variant="secondary">
          Projects
        </ButtonLink>
      </div>
    </main>
  );
}

/** Shown when a loader or component throws. Logged to the console in dev. */
function RouteError({ error, reset }: ErrorComponentProps) {
  if (import.meta.env.DEV) console.error(error);
  return (
    <main
      data-ready
      className="flex min-h-dvh flex-col items-center justify-center bg-canvas px-6 py-24 text-center"
    >
      <p className="font-mono text-[12px] text-muted-foreground">Error</p>
      <h1 className="mt-3 text-2xl font-semibold tracking-tight text-foreground">
        This page failed to load
      </h1>
      <p className="mt-2 max-w-md text-[13px] leading-6 text-muted-foreground">
        {error instanceof Error && error.message
          ? error.message
          : "An unexpected error occurred."}
      </p>
      <div className="mt-6 flex flex-wrap justify-center gap-2">
        <Button onClick={reset}>Try again</Button>
        <ButtonLink href="/" variant="secondary">
          Go to overview
        </ButtonLink>
      </div>
    </main>
  );
}
