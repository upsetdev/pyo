// The dashboard's rule test agrees with the WAF's processor (waf/): both
// pass the same cases.
// bun test scripts/waf-match.test.ts
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { matches, type TestRequest, type TestRule } from "../src/lib/pyo/wafMatch";

const here = new URL("../src/lib/pyo/waf-vectors.json", import.meta.url).pathname;
const vectors: { name: string; rule: TestRule; request: TestRequest; matches: boolean }[] = JSON.parse(readFileSync(here, "utf8"));

for (const v of vectors) {
  test(v.name, () => {
    expect(matches(v.rule, v.request)).toBe(v.matches);
  });
}

// The processor's own copy (waf/).
const theirs = new URL("../../waf/cmd/pyo-waf/testdata/vectors.json", import.meta.url).pathname;
test("the same cases as the WAF's processor", () => {
  expect(readFileSync(theirs, "utf8")).toBe(readFileSync(here, "utf8"));
});
