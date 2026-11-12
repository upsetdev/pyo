import {
  BrowserIcon,
  ChartLineUpIcon,
  DatabaseIcon,
  DotsThreeCircleIcon,
  GitBranchIcon,
  GlobeSimpleIcon,
  HardDrivesIcon,
  LightningIcon,
  PackageIcon,
  QueueIcon,
  SparkleIcon,
  SquaresFourIcon,
  WrenchIcon,
  type Icon,
} from "@phosphor-icons/react";
import { AppLink } from "~/components/ui/AppLink";
import { siDocker, siGit, siGithub, type SimpleIcon } from "simple-icons";
import type { ReactNode } from "react";
import { dateTime, shortSha, timeAgo } from "~/lib/pyo/format";
import { getTemplate } from "~/lib/pyo/templates";
import type {
  Application,
  ServiceSource,
  Template,
  TemplateCategory,
} from "~/lib/pyo/types";

/** A template's icon when it has no logo: its category's. */
export const CATEGORY_ICONS: Record<TemplateCategory, Icon> = {
  databases: DatabaseIcon,
  caches: LightningIcon,
  storage: HardDrivesIcon,
  networking: GlobeSimpleIcon,
  monitoring: ChartLineUpIcon,
  messaging: QueueIcon,
  "developer-tools": WrenchIcon,
  web: BrowserIcon,
  ai: SparkleIcon,
  other: DotsThreeCircleIcon,
};

export function IconTile({
  icon: IconCmp,
  size = "md",
}: {
  icon: Icon;
  size?: "sm" | "md" | "lg";
}) {
  const dims =
    size === "sm" ? "h-6 w-6" : size === "lg" ? "h-10 w-10" : "h-8 w-8";
  const px = size === "sm" ? 13 : size === "lg" ? 20 : 16;
  return (
    <span
      aria-hidden
      className={`flex ${dims} shrink-0 items-center justify-center rounded-md border border-border bg-muted/60 text-foreground/85`}
    >
      <IconCmp size={px} />
    </span>
  );
}

export const GIT_LOGO = siGit;
export const GITHUB_LOGO = siGithub;
export const DOCKER_LOGO = siDocker;

/** A brand logo in its own color, or the text color when the brand color is too dark to read. */
export function BrandMark({
  icon,
  size = 16,
}: {
  /** A Simple Icon, or a template's logo: one path and its colour. */
  icon: Pick<SimpleIcon, "hex" | "path">;
  size?: number;
}) {
  const [r, g, b] = [0, 2, 4].map(
    (i) => parseInt(icon.hex.slice(i, i + 2), 16) / 255,
  );
  const luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return (
    <svg
      viewBox="0 0 24 24"
      width={size}
      height={size}
      aria-hidden
      fill={luminance < 0.35 ? "currentColor" : `#${icon.hex}`}
      className="shrink-0"
    >
      <path d={icon.path} />
    </svg>
  );
}

const TILE = {
  sm: ["h-6 w-6", 13],
  md: ["h-8 w-8", 16],
  lg: ["h-10 w-10", 20],
} as const;

/** Square tile with the template's logo, or its category's icon. */
export function TemplateLogo({
  template: t,
  size = "md",
}: {
  template: Pick<Template, "logo" | "category">;
  size?: "sm" | "md" | "lg";
}) {
  if (!t.logo)
    return <IconTile icon={CATEGORY_ICONS[t.category]} size={size} />;
  const [dims, px] = TILE[size];
  return (
    <span
      aria-hidden
      className={`flex ${dims} shrink-0 items-center justify-center rounded-md border border-border bg-muted/60 text-foreground/85`}
    >
      <BrandMark icon={t.logo} size={px} />
    </span>
  );
}

export function sourceIcon(source: ServiceSource): Icon {
  if (source.type === "git") return GitBranchIcon;
  if (source.type === "image") return PackageIcon;
  const t = getTemplate(source.templateId);
  return t ? CATEGORY_ICONS[t.category] : SquaresFourIcon;
}

export function ServiceIcon({
  app,
  size = "md",
}: {
  app: Pick<Application, "source">;
  size?: "sm" | "md" | "lg";
}) {
  const t =
    app.source.type === "template"
      ? getTemplate(app.source.templateId)
      : undefined;
  return t ? (
    <TemplateLogo template={t} size={size} />
  ) : (
    <IconTile icon={sourceIcon(app.source)} size={size} />
  );
}

/** One-line description of where a service's code or image comes from. */
export function SourceSummary({
  source,
  revision,
}: {
  source: ServiceSource;
  revision?: string;
}) {
  if (source.type === "git") {
    const repo = source.repoUrl
      .replace(/^https:\/\/(www\.)?github\.com\//, "")
      .replace(/\.git$/, "");
    return (
      <span className="flex min-w-0 max-w-full items-center gap-1.5">
        <GitBranchIcon size={12} className="shrink-0 text-muted-foreground" />
        <span className="truncate font-mono text-[11.5px]">
          {repo}
          <span className="text-muted-foreground">@{source.branch}</span>
          {revision ? (
            <span className="text-muted-foreground">
              {" "}
              · {shortSha(revision)}
            </span>
          ) : null}
        </span>
      </span>
    );
  }
  const t =
    source.type === "template" ? getTemplate(source.templateId) : undefined;
  return (
    <span className="flex min-w-0 max-w-full items-center gap-1.5">
      <PackageIcon size={12} className="shrink-0 text-muted-foreground" />
      <span className="truncate font-mono text-[11.5px]">
        {t ? `${t.name} ` : ""}
        <span className={t ? "text-muted-foreground" : ""}>
          {t ? "" : source.image.replace(/^docker\.io\/(library\/)?/, "")}
          {source.reference.startsWith("sha256:")
            ? `@${shortSha(source.reference)}`
            : `:${source.reference}`}
        </span>
      </span>
    </span>
  );
}

export function TimeAgo({
  at,
  className = "",
}: {
  at: number;
  className?: string;
}) {
  // A record without a time (older data) shows a dash rather than throwing.
  if (!Number.isFinite(at))
    return (
      <span className={`tabular-nums text-muted-foreground ${className}`}>
        —
      </span>
    );
  return (
    <time
      dateTime={new Date(at).toISOString()}
      title={dateTime(at)}
      className={`whitespace-nowrap tabular-nums ${className}`}
    >
      {timeAgo(at)}
    </time>
  );
}

export function EntityLink({
  to,
  children,
  className = "",
}: {
  to: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <AppLink
      href={to}
      className={`truncate font-medium text-foreground hover:text-primary ${className}`}
      onClick={(e) => e.stopPropagation()}
    >
      {children}
    </AppLink>
  );
}

export function KindLabel({
  app,
}: {
  app: Pick<Application, "kind" | "workloadKind">;
}) {
  return (
    <span className="text-[11.5px] text-muted-foreground">
      {app.kind === "service" ? "Template" : "Custom"} · {app.workloadKind}
    </span>
  );
}

/** Text with `backticked` parts (a command, say) set in code. */
export function CodeText({ text }: { text: string }) {
  return (
    <>
      {text.split("`").map((part, i) =>
        i % 2 ? (
          <code key={i} className="break-all font-mono text-[0.92em]">
            {part}
          </code>
        ) : (
          part
        ),
      )}
    </>
  );
}
