import { site } from "~/config/site";

/** <head> meta for a route, with the site-wide title template. */
export function seo({
  title,
  description,
}: { title?: string; description?: string } = {}) {
  const fullTitle = title
    ? `${title} | ${site.name}`
    : `${site.name}: ${site.tagline}`;
  const desc = description ?? site.description;
  return [
    { title: fullTitle },
    { name: "description", content: desc },
    { property: "og:title", content: fullTitle },
    { property: "og:description", content: desc },
    { property: "og:site_name", content: site.name },
    { property: "og:type", content: "website" },
    { name: "twitter:card", content: "summary_large_image" },
  ];
}
