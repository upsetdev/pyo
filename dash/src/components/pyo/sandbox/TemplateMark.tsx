import { CubeIcon, MonitorIcon } from "@phosphor-icons/react";
import {
  siAlmalinux,
  siAlpinelinux,
  siArchlinux,
  siBun,
  siCentos,
  siDebian,
  siDeno,
  siDocker,
  siFedora,
  siGo,
  siNodedotjs,
  siOpenjdk,
  siPhp,
  siPython,
  siRockylinux,
  siRuby,
  siRust,
  siUbuntu,
  type SimpleIcon,
} from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import {
  BASE_TEMPLATE,
  DESKTOP_TEMPLATE,
  isBase,
  isDesktop,
  type Sandbox,
  type SandboxTemplate,
} from "~/lib/pyo/sandbox";

// A sandbox template's logo, by what its image runs: languages first, since
// node:22 is Debian underneath but reads as Node. Pyo's base is Debian, its
// desktop a screen.

type TemplateLike = Pick<SandboxTemplate, "name"> &
  Partial<Pick<SandboxTemplate, "builtin" | "source">>;

const LOGOS: [RegExp, SimpleIcon][] = [
  [/\bnode(js)?\b/, siNodedotjs],
  [/\bpython\b/, siPython],
  [/\bbun\b/, siBun],
  [/\bdeno\b/, siDeno],
  [/\bgolang\b|(^|\/)go(:|$)/, siGo],
  [/\brust\b/, siRust],
  [/\bruby\b/, siRuby],
  [/\bphp\b/, siPhp],
  [/\b(openjdk|java|temurin)\b/, siOpenjdk],
  [/\bubuntu\b/, siUbuntu],
  [/\bdebian\b/, siDebian],
  [/\balpine\b/, siAlpinelinux],
  [/\bfedora\b/, siFedora],
  [/\barch(linux)?\b/, siArchlinux],
  [/\bcentos\b/, siCentos],
  [/\brocky(linux)?\b/, siRockylinux],
  [/\balma(linux)?\b/, siAlmalinux],
  [/\bdocker\b/, siDocker],
];

export function templateLogo(t: TemplateLike) {
  if (isBase(t)) return siDebian;
  const text = `${t.source?.image ?? ""} ${t.source?.os ?? ""} ${t.name}`.toLowerCase();
  return LOGOS.find(([re]) => re.test(text))?.[1];
}

/** A sandbox's template, from the team's templates when it's there. */
export function templateOf(x: Pick<Sandbox, "template" | "templateId">, templates: SandboxTemplate[]): TemplateLike {
  return (
    templates.find((t) => t.id === x.templateId) ??
    templates.find((t) => t.name === x.template) ?? {
      name: x.template,
      builtin: x.template === BASE_TEMPLATE || x.template === DESKTOP_TEMPLATE,
    }
  );
}

const SIZES = {
  sm: { box: "h-6 w-6 rounded-md", icon: 13 },
  md: { box: "h-8 w-8 rounded-lg", icon: 16 },
  lg: { box: "h-9 w-9 rounded-lg", icon: 18 },
};

/** The template's logo in a tile, or a cube when nothing matches. */
export function TemplateMark({
  template,
  size = "md",
  className = "",
}: {
  template: TemplateLike;
  size?: keyof typeof SIZES;
  className?: string;
}) {
  const logo = isDesktop(template) ? undefined : templateLogo(template);
  const { box, icon } = SIZES[size];
  return (
    <span
      className={`flex ${box} shrink-0 items-center justify-center border border-frame-border bg-card text-muted-foreground ${className}`}
    >
      {isDesktop(template) ? (
        <MonitorIcon size={icon} weight="duotone" className="text-primary" />
      ) : logo ? (
        <BrandMark icon={logo} size={icon} tone={logo === siDebian ? "brand" : "auto"} />
      ) : (
        <CubeIcon size={icon} />
      )}
    </span>
  );
}
