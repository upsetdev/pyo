import { Link, type LinkProps } from "@tanstack/react-router";
import type { AnchorHTMLAttributes } from "react";
import { teamPath, useTeamSlug } from "~/lib/pyo/paths";

type Props = Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "href"> & {
  href: string;
  preload?: false | "intent" | "viewport" | "render";
  activeProps?: AnchorHTMLAttributes<HTMLAnchorElement> &
    Record<`data-${string}`, unknown>;
  activeOptions?: LinkProps["activeOptions"];
};

/**
 * Link for hrefs that come from config strings. Internal paths go through the
 * router (client navigation, preloading), "/#section" splits into path and
 * hash. External links and server routes (/api/*, /logout) render a plain
 * anchor so the browser does a full request. Dashboard paths are written
 * relative to the team ("/projects") and get the team slug added here.
 *
 * Router paths are typed against the route tree; config hrefs are plain
 * strings, so they're passed through untyped here. A typo'd path renders the
 * root 404 page.
 */
export function AppLink({ href, preload, activeProps, ...props }: Props) {
  const slug = useTeamSlug();
  if (/^(https?:|mailto:|tel:)/.test(href) || /^\/(api\/|logout\b)/.test(href))
    return <a href={href} {...props} />;
  const [path, hash] = teamPath(slug, href).split("#");
  return (
    <Link
      to={(path || "/") as string}
      hash={hash}
      preload={preload}
      activeProps={activeProps}
      {...props}
    />
  );
}
