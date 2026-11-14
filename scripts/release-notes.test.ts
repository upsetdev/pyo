import { describe, expect, test } from "bun:test";
import { bullet, classify, compareTags, dedupe, render } from "./release-notes";

const sha = "abc1234def5678abc1234def5678abc1234def56";
const c = (subject: string, body = "", s = sha) => ({ sha: s, subject, body });

describe("sections", () => {
  test.each([
    ["feat(api): add a thing", "feat"],
    ["fix: mend a thing", "fix"],
    ["fix(dash)!: mend a thing", "fix"],
    ["perf(api): cut memory", "perf"],
    ["docs: explain a thing", "docs"],
    ["refactor(api): move a thing", "other"],
    ["build: bump rust", "other"],
    ["revert: feat: add a thing", "other"],
    ["init", "other"],
    ["chore(release): 0.2.2", null],
    ["ci: run on tags", null],
    ["test(api): cover a thing", null],
    ["style(host): format", null],
  ])("%s", (subject, section) => expect(classify(c(subject))).toBe(section as never));
});

describe("security fixes", () => {
  test("scope security", () => expect(classify(c("fix(security): check the token"))).toBe("security"));
  test("a Security: footer", () =>
    expect(classify(c("fix(api): check the token", "Body.\n\nSecurity: GHSA-4c2f-9xq8-vhmr"))).toBe("security"));
  test("a footer with words", () => expect(classify(c("chore: bump a dependency", "Security: CVE-2026-1234"))).toBe("security"));
  test("the word alone isn't enough", () =>
    expect(classify(c("fix(gateway): keep the SecurityPolicy", "The security policy denies a request."))).toBe("fix"));
  test("links the advisory", () =>
    expect(bullet(c("fix(api): check the token", "Security: GHSA-4c2f-9xq8-vhmr"))).toBe(
      "- [GHSA-4c2f-9xq8-vhmr](https://github.com/upsetdev/pyo/security/advisories/GHSA-4c2f-9xq8-vhmr) fix(api): check the token",
    ));
});

describe("bullets", () => {
  test("links the commit", () =>
    expect(bullet(c("fix: mend a thing"))).toBe(`- [\`abc1234\`](https://github.com/upsetdev/pyo/commit/${sha}) fix: mend a thing`));
  test("links the pull request", () =>
    expect(bullet(c("feat: add a thing (#42)"))).toBe("- [#42](https://github.com/upsetdev/pyo/pull/42) feat: add a thing"));
  test("thanks an outside contributor", () => expect(bullet(c("fix: mend"), "someone")).toEndWith(" fix: mend (Thanks @someone)"));
  test("a maintainer, in any case", () => expect(bullet(c("fix: mend"), "FransAllen")).toEndWith(" fix: mend"));
  test("a configured maintainer", () => expect(bullet(c("fix: mend"), "someone", ["someone"])).toEndWith(" fix: mend"));
  test("no login", () => expect(bullet(c("fix: mend"), null)).toEndWith(" fix: mend"));
});

describe("cherry-picks", () => {
  test("listed once, and not when already released", () => {
    const commits = [c("fix: a", "", "1"), c("fix: b", "", "2"), c("fix: a", "", "3"), c("fix: c", "", "4")];
    expect(dedupe(commits, ["fix: c"]).map((x) => x.sha)).toEqual(["1", "2"]);
  });
});

describe("notes", () => {
  test("sections in order, empty ones left out", () => {
    const notes = render({
      from: "v0.2.1",
      to: "v0.2.2",
      commits: [c("docs: d", "", "1"), c("chore: x", "", "2"), c("fix: f", "", "3"), c("feat: n", "", "4"), c("fix(security): s", "", "5")],
      logins: new Map([["3", "someone"]]),
    });
    const order = ["🚀 Features", "🐞 Bug Fixes", "⬆️ Security Fixes", "📝 Docs"].map((h) => notes.indexOf(`### ${h}`));
    expect(order.every((i, n) => i >= 0 && (n === 0 || i > order[n - 1]))).toBe(true);
    expect(notes).not.toContain("Performance");
    expect(notes).not.toContain("chore: x");
    expect(notes).toContain("fix: f (Thanks @someone)");
    expect(notes).toEndWith("**Full Changelog**: https://github.com/upsetdev/pyo/compare/v0.2.1...v0.2.2\n");
  });
  test("notices first", () => {
    const notes = render({
      from: "v0.2.1",
      to: "v0.2.2",
      commits: [c("fix: f")],
      notices: { version: "0.2.2", minFrom: "0.2.0", manualOnly: true, beforeYouUpdate: ["Postgres restarts once."] },
    });
    expect(notes.startsWith("### ⚠️ Before you update\n\n- Postgres restarts once.\n- Update to 0.2.0 first")).toBe(true);
    expect(notes).toContain("- Automatic updates skip this release.");
  });
  test("the first release", () => {
    const notes = render({ from: null, to: "v0.1.0", commits: [] });
    expect(notes).toBe("Initial release.\n\n**Full Changelog**: https://github.com/upsetdev/pyo/commits/v0.1.0\n");
  });
});

test("tags compare as versions", () => {
  expect(["v0.10.0", "v0.2.0", "v0.9.1"].sort(compareTags)).toEqual(["v0.2.0", "v0.9.1", "v0.10.0"]);
});
