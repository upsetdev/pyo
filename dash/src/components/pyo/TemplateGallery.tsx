import {
  BroadcastIcon,
  BrowserIcon,
  CaretDownIcon,
  ChartLineUpIcon,
  DatabaseIcon,
  DotsThreeCircleIcon,
  GlobeSimpleIcon,
  HardDrivesIcon,
  LightningIcon,
  SparkleIcon,
  SquaresFourIcon,
  WrenchIcon,
  type Icon,
  GlobeIcon,
  LockSimpleIcon,
} from "@phosphor-icons/react";
import { useMemo, useState, type ReactNode } from "react";
import { Button } from "~/components/ui/Button";
import { Sheet } from "~/components/ui/Dialog";
import { Layered, TOOLBAR, useInLayer } from "~/components/ui/Layered";
import { Select } from "~/components/ui/Select";
import { KeyValue, NoMatches } from "~/components/ui/Panel";
import { SearchInput } from "~/components/ui/Table";
import { cpu, mem } from "~/lib/pyo/format";
import {
  categoryLabel,
  GALLERY_TEMPLATES as TEMPLATES,
  TEMPLATE_CATEGORIES,
} from "~/lib/pyo/templates";
import type { Template, TemplateCategory } from "~/lib/pyo/types";
import { TemplateLogo } from "./common";
import { CARD_HOVER } from "./ProjectCard";
import { EXPOSURE_LABEL } from "./editors";
import { ENGINE_LABEL } from "./Databases";

export function TemplateCard({
  t,
  onSelect,
  onPreview,
  action = "Deploy",
}: {
  t: Template;
  onSelect: (t: Template) => void;
  onPreview?: (t: Template) => void;
  action?: string;
}) {
  return (
    <article className={`group relative flex flex-col rounded-[14px] border border-card-border bg-card p-4 ${CARD_HOVER} has-[button:focus-visible]:ring-2 has-[button:focus-visible]:ring-primary/40`}>
      {/* The whole card is the target; the Details button sits above it. */}
      <button
        type="button"
        onClick={() => onSelect(t)}
        aria-label={`${action} ${t.name}`}
        className="absolute inset-0 rounded-[14px] focus:outline-none"
      />
      <div className="flex items-center gap-3">
        <TemplateLogo template={t} size="lg" />
        <div className="min-w-0 flex-1">
          <h3 className="truncate text-[14px] font-semibold">{t.name}</h3>
          <p className="flex items-center gap-1.5 truncate text-[12px] text-muted-foreground">
            {categoryLabel(t.category)}
          </p>
        </div>
        {onPreview ? (
          <button
            type="button"
            onClick={() => onPreview(t)}
            className="relative z-10 shrink-0 self-start rounded-full px-2 py-0.5 text-[11.5px] font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            Details
          </button>
        ) : null}
      </div>
      <p className="mt-3 line-clamp-2 text-[12.5px] leading-[1.15rem] text-muted-foreground">
        {t.description}
      </p>
    </article>
  );
}

/**
 * `simple` (inside New deploy) drops recents and shows the most popular
 * templates first, with the rest a click away.
 */
export function TemplateGallery({
  onSelect,
  recentIds = [],
  header,
  action,
  previews = true,
  simple = false,
}: {
  onSelect: (t: Template) => void;
  recentIds?: string[];
  header?: ReactNode;
  action?: string;
  previews?: boolean;
  simple?: boolean;
}) {
  const [q, setQ] = useState("");
  const [cat, setCat] = useState<"all" | TemplateCategory>("all");
  const [preview, setPreview] = useState<Template | null>(null);
  const [expanded, setExpanded] = useState(false);
  const inLayer = useInLayer();
  // The search alone, so each category can say how many it holds.
  const matching = useMemo(() => {
    const ql = q.trim().toLowerCase();
    return TEMPLATES.filter(
      (t) =>
        !ql ||
        t.name.toLowerCase().includes(ql) ||
        t.description.toLowerCase().includes(ql) ||
        t.id.includes(ql) ||
        categoryLabel(t.category).toLowerCase().includes(ql),
    ).sort((a, b) => a.rank - b.rank);
  }, [q]);
  const filtered =
    cat === "all" ? matching : matching.filter((t) => t.category === cat);
  const browsing = !q && cat === "all";
  const limited = simple && browsing && !expanded;
  // Featured ones first when only a few show.
  const shown = limited
    ? [...filtered.filter((t) => t.featured), ...filtered.filter((t) => !t.featured)].slice(0, SIMPLE_LIMIT)
    : filtered;
  const recent = recentIds
    .map((id) => TEMPLATES.find((t) => t.id === id))
    .filter(Boolean) as Template[];
  const counts = Object.fromEntries(
    TEMPLATE_CATEGORIES.map((c) => [
      c.id,
      TEMPLATES.filter((t) => t.category === c.id).length,
    ]),
  );
  // Counts follow the search, so each category says how many match.
  const categories: CategoryOption[] = [
    {
      value: "all",
      label: "All templates",
      icon: SquaresFourIcon,
      n: matching.length,
    },
    ...TEMPLATE_CATEGORIES.filter((c) => counts[c.id]).map((c) => ({
      value: c.id,
      label: c.label,
      icon: CATEGORY_ICON[c.id],
      n: matching.filter((t) => t.category === c.id).length,
    })),
  ];
  const clear = () => {
    setQ("");
    setCat("all");
  };

  const body = (
    <div className="space-y-3">
      {header}
      <div className={TOOLBAR_ROW}>
        <SearchInput
          value={q}
          onChange={setQ}
          placeholder={`Search ${TEMPLATES.length} templates`}
          className="min-w-0 flex-1"
          label="Search templates"
        />
        {/* The page has the category list beside it; narrow, a menu. */}
        <div className={`w-40 sm:w-48 ${simple ? "" : "md:hidden"}`}>
          <Select
            value={cat}
            onChange={(e) => setCat(e.target.value as typeof cat)}
            aria-label="Category"
            heading="Category"
          >
            {categories.map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </Select>
        </div>
      </div>

      {browsing && !simple && recent.length ? (
        <div className="flex flex-wrap items-center gap-2 px-1.5">
          <span className="text-[12.5px] text-muted-foreground">Recent</span>
          {recent.map((t) => (
            <button
              key={t.id}
              type="button"
              onClick={() => onSelect(t)}
              className="inline-flex h-7 items-center gap-1.5 rounded-full border border-border bg-card px-3 text-[12.5px] font-medium transition-colors hover:bg-accent"
            >
              {t.name}
            </button>
          ))}
        </div>
      ) : null}

      <section aria-label="Templates">
        {shown.length ? (
          <div
            className={`grid gap-1.5 sm:grid-cols-2 ${simple ? "" : "xl:grid-cols-3"}`}
          >
            {shown.map((t) => (
              <TemplateCard
                key={t.id}
                t={t}
                onSelect={onSelect}
                onPreview={previews ? setPreview : undefined}
                action={action}
              />
            ))}
          </div>
        ) : (
          <NoMatches
            query={q}
            what="templates"
            hint="Try another search, or deploy any container image."
            onClear={clear}
          />
        )}
        {limited && filtered.length > shown.length ? (
          <button
            type="button"
            onClick={() => setExpanded(true)}
            className="mt-1.5 flex h-10 w-full items-center justify-center gap-1.5 rounded-xl text-[13px] font-medium text-muted-foreground transition-colors hover:bg-card/60 hover:text-foreground"
          >
            Show all {filtered.length} templates
            <CaretDownIcon size={12} weight="bold" />
          </button>
        ) : null}
      </section>
      <TemplatePreview
        t={preview}
        onClose={() => setPreview(null)}
        onSelect={onSelect}
        action={action}
      />
    </div>
  );
  if (simple) return inLayer ? body : <Layered bare>{body}</Layered>;
  return (
    <div className="grid items-start gap-6 md:grid-cols-[188px_minmax(0,1fr)]">
      <CategoryNav value={cat} onChange={setCat} categories={categories} />
      <Layered bare>{body}</Layered>
    </div>
  );
}

const SIMPLE_LIMIT = 8;

const CATEGORY_ICON: Record<TemplateCategory, Icon> = {
  databases: DatabaseIcon,
  caches: LightningIcon,
  storage: HardDrivesIcon,
  networking: GlobeSimpleIcon,
  monitoring: ChartLineUpIcon,
  messaging: BroadcastIcon,
  "developer-tools": WrenchIcon,
  web: BrowserIcon,
  ai: SparkleIcon,
  other: DotsThreeCircleIcon,
};

type CategoryOption = {
  value: "all" | TemplateCategory;
  label: string;
  icon: Icon;
  n: number;
};

/** The categories down the side of the Templates page, with their counts. */
function CategoryNav({
  value,
  onChange,
  categories,
}: {
  value: "all" | TemplateCategory;
  onChange: (v: "all" | TemplateCategory) => void;
  categories: CategoryOption[];
}) {
  return (
    <nav aria-label="Template categories" className="hidden md:block">
      <p className="mb-2 px-2.5 text-[12px] font-medium text-muted-foreground/70">
        Categories
      </p>
      <ul className="space-y-0.5">
        {categories.map((c) => {
          const on = value === c.value;
          const Ico = c.icon;
          return (
            <li key={c.value}>
              <button
                type="button"
                aria-pressed={on}
                onClick={() => onChange(c.value)}
                className={`group flex w-full items-center gap-2.5 rounded-lg px-2.5 py-[7px] text-left text-[13.5px] transition-colors ${
                  on
                    ? "bg-accent text-foreground"
                    : `text-muted-foreground hover:bg-accent/50 hover:text-foreground ${c.n ? "" : "opacity-45"}`
                }`}
              >
                <Ico
                  size={15}
                  className={`shrink-0 ${on ? "text-foreground" : "text-muted-foreground/80 group-hover:text-foreground"}`}
                />
                <span className="min-w-0 flex-1 truncate">{c.label}</span>
                <span className="text-[12px] tabular-nums text-muted-foreground/70">
                  {c.n}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}

const TOOLBAR_ROW = TOOLBAR.replace("mb-1.5 ", "");

/** Configuration preview from the template definition. */
export function TemplatePreview({
  t,
  onClose,
  onSelect,
  action = "Deploy",
}: {
  t: Template | null;
  onClose: () => void;
  onSelect: (t: Template) => void;
  action?: string;
}) {
  return (
    <Sheet
      open={!!t}
      onClose={onClose}
      title={
        t ? (
          <span className="flex items-center gap-2.5">
            <TemplateLogo template={t} size="sm" />
            {t.name}
          </span>
        ) : (
          ""
        )
      }
      description={t?.description}
      footer={
        t ? (
          <>
            <Button variant="ghost" size="sm" onClick={onClose}>
              Close
            </Button>
            <Button
              size="sm"
              onClick={() => {
                onClose();
                onSelect(t);
              }}
            >
              {action} {t.name}
            </Button>
          </>
        ) : null
      }
    >
      {t ? (
        <div className="space-y-5 text-[12.5px]">
          <div className="-mx-5 border-y border-border">
            <KeyValue
              items={[
                [
                  "Image",
                  <span className="font-mono text-[11.5px]">
                    {t.image}:{t.version}
                  </span>,
                ],
                ["Template", `v${t.templateVersion}`],
                [
                  "Runs as",
                  t.stateful
                    ? "One replica with its own data"
                    : "Stateless service",
                ],
                ["Replicas", t.replicas],
                [
                  "Requests",
                  `${cpu(t.resources.cpuRequest)} · ${mem(t.resources.memoryRequest)}`,
                ],
                [
                  "Limits",
                  `${cpu(t.resources.cpuLimit)} · ${mem(t.resources.memoryLimit)}`,
                ],
                [
                  "Storage",
                  t.storage
                    ? `${t.storage.sizeGi} GiB at ${t.storage.mountPath}${t.storage.required ? "" : " (optional)"}`
                    : "None",
                ],
                [
                  "Health check",
                  t.healthCheck.type === "none"
                    ? "None"
                    : t.healthCheck.type === "http"
                      ? `HTTP ${t.healthCheck.path}`
                      : t.healthCheck.type === "tcp"
                        ? `TCP :${t.healthCheck.port}`
                        : t.healthCheck.command,
                ],
                ...(t.databases.length
                  ? [["Databases", t.databases.map((x) => ENGINE_LABEL[x.engine]).join(" and ")] as [string, ReactNode]]
                  : []),
                ...(t.companions.length
                  ? [["Also runs", t.companions.map((c) => c.image.split("/").pop()).join(", ")] as [string, ReactNode]]
                  : []),
              ]}
            />
          </div>
          <div>
            <h4 className="mb-2 font-medium">Ports</h4>
            {t.ports.length ? (
              <ul className="space-y-1">
                {t.ports.map((p) => (
                  <li
                    key={p.name}
                    className="flex items-center justify-between"
                  >
                    <span className="font-mono text-[11.5px]">
                      {p.name} {p.port}/{p.protocol}
                    </span>
                    <span className="inline-flex items-center gap-1 text-muted-foreground">
                      {p.exposure === "internal" ? (
                        <LockSimpleIcon size={11} />
                      ) : (
                        <GlobeIcon size={11} />
                      )}
                      {p.exposure === "http"
                        ? "Internal until you add a domain"
                        : EXPOSURE_LABEL[p.exposure]}
                    </span>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-muted-foreground">No inbound ports.</p>
            )}
          </div>
          <div>
            <h4 className="mb-2 font-medium">Configuration</h4>
            <ul className="space-y-1.5">
              {t.fields.map((f) => (
                <li
                  key={f.key}
                  className="flex items-center justify-between gap-3"
                >
                  <span>
                    {f.label}
                    {f.sensitive ? (
                      <LockSimpleIcon
                        size={11}
                        className="ml-1 inline text-muted-foreground"
                        aria-label="sensitive"
                      />
                    ) : null}
                  </span>
                  <span className="truncate font-mono text-[11.5px] text-muted-foreground">
                    {f.sensitive ? "generated" : String(f.default)}
                  </span>
                </li>
              ))}
            </ul>
          </div>
          {t.hints.length ? (
            <div>
              <h4 className="mb-2 flex items-center gap-1.5 font-medium">
                {t.stateful ? <DatabaseIcon size={13} /> : null} Notes
              </h4>
              <ul className="list-disc space-y-1 pl-4 text-muted-foreground">
                {t.hints.map((h) => (
                  <li key={h}>{h}</li>
                ))}
              </ul>
            </div>
          ) : null}
        </div>
      ) : null}
    </Sheet>
  );
}
