import { IconBase, type Icon, type IconWeight } from "@phosphor-icons/react";
import { forwardRef, type ReactElement } from "react";

// The project mark: three pointy-top hexagons in a honeycomb cluster, two
// side by side with one nested on top. Built on Phosphor's IconBase, so it
// takes the same props (size, weight, color, mirrored, IconContext) and is an
// `Icon` wherever a Phosphor icon is.
//
// Pointy-top, because only that orientation stacks two side by side with one
// centred above (flat-top neighbours sit at 30° angles), and its vertical
// sides land on whole pixels at 16px: every vertical edge is at 8 mod 16, so
// each 1px stroke and the 1px gaps between hexagons render sharp.

const SQRT3 = Math.sqrt(3);
/** Hexagon width (flat side to flat side) at the regular stroke. */
const WIDTH = 80;
/** Between neighbouring centres: the width plus a 32-unit gap (1px at 16px with the stroke). */
const STEP = 112;
const TOP_X = 120;
const ROW = (STEP * SQRT3) / 2;
// Centre the cluster's ink vertically: two rows plus a hexagon's height and the stroke.
const TOP_Y = 128 - (ROW + (2 * WIDTH) / SQRT3 + 16) / 2 + 8 + WIDTH / SQRT3;
const CENTRES: [number, number][] = [
  [TOP_X, TOP_Y],
  [TOP_X - STEP / 2, TOP_Y + ROW],
  [TOP_X + STEP / 2, TOP_Y + ROW],
];

/** A pointy-top hexagon as path data, `width` across its flat sides. */
function hexagon([cx, cy]: [number, number], width: number) {
  const r = width / SQRT3;
  const points = [-90, -30, 30, 90, 150, 210].map((deg) => {
    const a = (deg * Math.PI) / 180;
    return `${+(cx + r * Math.cos(a)).toFixed(2)},${+(cy + r * Math.sin(a)).toFixed(2)}`;
  });
  return `M${points.join("L")}Z`;
}

/** The cluster at this stroke width. Heavier strokes shrink each hexagon so
 *  the outer edge and the gaps stay put, and the gaps never close up. */
const cluster = (stroke: number) =>
  CENTRES.map((c) => hexagon(c, WIDTH + 16 - stroke)).join("");

const outline = (stroke: number, d = cluster(stroke)) => (
  <path
    d={d}
    fill="none"
    stroke="currentColor"
    strokeWidth={stroke}
    strokeLinecap="round"
    strokeLinejoin="round"
  />
);

const REGULAR = cluster(16);

const weights = new Map<IconWeight, ReactElement>([
  ["thin", outline(8)],
  ["light", outline(12)],
  ["regular", outline(16, REGULAR)],
  ["bold", outline(24)],
  [
    "fill",
    <path
      d={REGULAR}
      stroke="currentColor"
      strokeWidth={16}
      strokeLinejoin="round"
    />,
  ],
  [
    "duotone",
    <>
      <path d={REGULAR} opacity={0.2} />
      {outline(16, REGULAR)}
    </>,
  ],
]);

export const ProjectIcon: Icon = forwardRef((props, ref) => (
  <IconBase
    ref={ref}
    aria-hidden={props.alt ? undefined : true}
    {...props}
    weights={weights}
  />
));
ProjectIcon.displayName = "ProjectIcon";
