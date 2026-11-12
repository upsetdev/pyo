import {
  createFileRoute,
  Link,
  Outlet,
  redirect,
} from "@tanstack/react-router";
import { Logo } from "~/components/site/Logo";
import { ThemeToggle } from "~/components/site/ThemeToggle";
import { site } from "~/config/site";
import { auth } from "~/lib/pyo/backend";
import { safeNextPath } from "~/lib/validation";

export const Route = createFileRoute("/_auth")({
  ssr: false,
  // Signed-in visitors skip the auth pages, honoring a same-origin ?next=.
  beforeLoad: async ({ location }) => {
    const user = await auth.me().catch(() => null);
    const setup = location.pathname === "/setup";
    // /setup is for the printed sign-in, and until it's replaced nothing else is.
    if (user?.mustSetUp) {
      if (!setup) throw redirect({ to: "/setup" });
      return;
    }
    if (!user && setup) throw redirect({ to: "/login" });
    if (user) {
      // An invite link while signed in: accept it there.
      const invite = (location.search as { invite?: unknown }).invite;
      if (location.pathname === "/signup" && typeof invite === "string")
        throw redirect({ href: `/invite?${new URLSearchParams({ invite })}` });
      const next = (location.search as { next?: unknown }).next;
      throw redirect({
        href: safeNextPath(typeof next === "string" ? next : null),
      });
    }
  },
  component: AuthLayout,
});

/**
 * One centered column: the mark, the form, a quiet footer. As a layout,
 * switching login <-> signup only swaps the form.
 */
function AuthLayout() {
  return (
    <main
      data-ready
      className="relative flex min-h-dvh flex-col bg-sidebar px-4 text-foreground"
    >
      <div className="absolute right-4 top-4">
        <ThemeToggle />
      </div>
      <div className="flex flex-1 items-center justify-center py-16">
        <div className="w-full max-w-[360px] animate-fade-up">
          <Link
            to="/"
            aria-label={`${site.name} home`}
            className="mx-auto mb-8 flex w-fit"
          >
            <Logo showName={false} className="[&_svg]:h-9 [&_svg]:w-9" />
          </Link>
          <Outlet />
        </div>
      </div>
      <footer className="flex items-center justify-center gap-4 pb-6 text-[12px] text-muted-foreground">
        <span>
          © {new Date().getFullYear()} {site.name}
        </span>
        <a
          href={site.docsUrl}
          target="_blank"
          rel="noreferrer"
          className="transition hover:text-foreground"
        >
          Docs
        </a>
      </footer>
    </main>
  );
}
