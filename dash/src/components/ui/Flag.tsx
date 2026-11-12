import { GlobeHemisphereEastIcon } from "@phosphor-icons/react";
import { useMemo } from "react";
import { countryName, locationLabel, parseLocation } from "~/lib/pyo/location";
import { Select } from "./Field";
import { Tooltip } from "./Tooltip";

// Country flags (flag-icons, SVG). Each is its own small asset, loaded only
// where it's shown.
const FLAGS = import.meta.glob("/node_modules/flag-icons/flags/4x3/*.svg", {
  query: "?url",
  import: "default",
  eager: true,
}) as Record<string, string>;

const flagUrl = (code: string) =>
  FLAGS[`/node_modules/flag-icons/flags/4x3/${code.toLowerCase()}.svg`];

// Square versions, cropped round for rows of places.
const SQUARE = import.meta.glob("/node_modules/flag-icons/flags/1x1/*.svg", {
  query: "?url",
  import: "default",
  eager: true,
}) as Record<string, string>;
const squareUrl = (code: string) =>
  SQUARE[`/node_modules/flag-icons/flags/1x1/${code.toLowerCase()}.svg`];

/**
 * Places as overlapping round flags, like the service logos on project
 * cards; each names its place on hover.
 */
/** `detail`: more to say in each tooltip, e.g. how the place was found. */
export function PlaceFlags({
  places,
  size = 22,
  detail,
}: {
  places: string[];
  size?: number;
  detail?: string;
}) {
  return (
    <span className="flex items-center">
      {places.map((loc, i) => {
        const cc = parseLocation(loc).country;
        const url = cc ? squareUrl(cc) : undefined;
        return (
          <span key={loc} className={i ? "-ml-1" : ""}>
            <Tooltip
              content={
                detail
                  ? `${locationLabel(loc)} · ${detail}`
                  : locationLabel(loc)
              }
            >
              <span
                className="relative z-10 flex items-center justify-center rounded-full border border-border bg-accent ring-2 ring-card transition-transform hover:z-20 hover:-translate-y-0.5 dark:bg-popover"
                style={{ width: size, height: size }}
              >
                {url ? (
                  <img
                    src={url}
                    alt={locationLabel(loc)}
                    className="rounded-full object-cover ring-1 ring-border"
                    style={{
                      width: Math.round(size * 0.56),
                      height: Math.round(size * 0.56),
                    }}
                  />
                ) : (
                  <GlobeHemisphereEastIcon
                    size={Math.round(size * 0.56)}
                    className="text-muted-foreground"
                    aria-label={locationLabel(loc)}
                  />
                )}
              </span>
            </Tooltip>
          </span>
        );
      })}
    </span>
  );
}

export function Flag({
  country,
  size = 16,
  className = "",
}: {
  country: string;
  size?: number;
  className?: string;
}) {
  const url = country ? flagUrl(country) : undefined;
  if (!url)
    return (
      <GlobeHemisphereEastIcon
        size={size}
        className={`shrink-0 text-muted-foreground ${className}`}
        aria-hidden
      />
    );
  return (
    <img
      src={url}
      alt=""
      width={size}
      height={Math.round(size * 0.75)}
      className={`shrink-0 rounded-[2px] object-cover ring-1 ring-border ${className}`}
      style={{ width: size, height: Math.round(size * 0.75) }}
    />
  );
}

/** A cluster's location: its flag and "City, Country". */
export function Location({
  value,
  className = "",
}: {
  value: string;
  className?: string;
}) {
  return (
    <span className={`inline-flex min-w-0 items-center gap-1.5 ${className}`}>
      <Flag country={parseLocation(value).country} size={14} />
      <span className="truncate">{locationLabel(value)}</span>
    </span>
  );
}

/** Countries that have a flag, by name. */
function useCountries() {
  return useMemo(
    () =>
      Object.keys(FLAGS)
        .map((k) => k.slice(k.lastIndexOf("/") + 1, -4).toUpperCase())
        .filter((c) => c.length === 2 && countryName(c) !== c)
        .map((code) => ({ code, name: countryName(code) }))
        .sort((a, b) => a.name.localeCompare(b.name)),
    [],
  );
}

export function CountrySelect({
  value,
  onChange,
  ...aria
}: {
  value: string;
  onChange: (code: string) => void;
  id?: string;
  "aria-label"?: string;
  "aria-describedby"?: string;
}) {
  const countries = useCountries();
  return (
    <Select {...aria} value={value} onChange={(e) => onChange(e.target.value)}>
      <option value="" hidden>
        Pick a country
      </option>
      {countries.map((c) => (
        <option key={c.code} value={c.code}>
          <span className="inline-flex items-center gap-2">
            <Flag country={c.code} size={16} />
            {c.name}
          </span>
        </option>
      ))}
    </Select>
  );
}
