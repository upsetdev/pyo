// The charts' y-axis ticks (Charts.tsx yTicks): whole numbers for counts,
// no repeated labels, even with no data.
// bun test dash/scripts/chart-ticks.test.ts
import { expect, test } from "bun:test";
import { yTicks } from "../src/components/charts/Charts";
import { compact, pct } from "../src/lib/pyo/format";

const count = (v: number) => String(Math.round(v));

test("no data shows 0 and 1", () => {
  // The chart's maximum with all values 0 (niceMax).
  expect(yTicks(1, compact)).toEqual([0, 1]);
  expect(yTicks(1, count)).toEqual([0, 1]);
});

test("small counts stay whole", () => {
  expect(yTicks(2, compact)).toEqual([0, 1, 2]);
  expect(yTicks(4, compact)).toEqual([0, 1, 2, 3, 4]);
  expect(yTicks(10, compact)).toEqual([0, 5, 10]);
});

test("larger counts keep quarters", () => {
  expect(yTicks(400, compact)).toEqual([0, 100, 200, 300, 400]);
});

test("formats with fractions keep quarters", () => {
  expect(yTicks(1, (v) => pct(v))).toEqual([0, 0.25, 0.5, 0.75, 1]);
  expect(yTicks(1, (v) => `${+v.toFixed(2)}/s`)).toEqual([0, 0.25, 0.5, 0.75, 1]);
});
