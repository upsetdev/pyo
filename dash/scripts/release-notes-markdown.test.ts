// The release notes renderer in Settings › Updates.
// bun test scripts/release-notes-markdown.test.ts
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { Markdown, parseInline, parseMarkdown, safeHref } from "../src/lib/pyo/markdown";

const html = (source: string) => renderToStaticMarkup(createElement(Markdown, { source }));
const NOTES = readFileSync(new URL("./fixtures/release-notes-0.52.0.md", import.meta.url), "utf8");

test("headings one to three, deeper ones as three, emoji kept", () => {
  expect(parseMarkdown("# One\n## Two\n### 🚀 Features\n#### Four")).toEqual([
    { t: "heading", level: 1, c: [{ t: "text", v: "One" }] },
    { t: "heading", level: 2, c: [{ t: "text", v: "Two" }] },
    { t: "heading", level: 3, c: [{ t: "text", v: "🚀 Features" }] },
    { t: "heading", level: 3, c: [{ t: "text", v: "Four" }] },
  ]);
  expect(html("### 🐞 Bug Fixes")).toContain(">🐞 Bug Fixes</h6>");
  // No space after the hashes is a paragraph.
  expect(parseMarkdown("#hashtag")[0].t).toBe("paragraph");
});

test("bullet lists with - and *, one nesting level", () => {
  expect(parseMarkdown("- one\n* two\n  - inner\n- three")).toEqual([
    {
      t: "list",
      items: [
        { c: [{ t: "text", v: "one" }], items: [] },
        { c: [{ t: "text", v: "two" }], items: [{ c: [{ t: "text", v: "inner" }], items: [] }] },
        { c: [{ t: "text", v: "three" }], items: [] },
      ],
    },
  ]);
  const out = html("- a\n  - b");
  expect(out.match(/<ul/g)?.length).toBe(2);
  expect(out.match(/<li>/g)?.length).toBe(2);
});

test("a wrapped item line joins its item", () => {
  const [list] = parseMarkdown("- first line\n  goes on\n- next");
  expect(list).toEqual({
    t: "list",
    items: [
      { c: [{ t: "text", v: "first line\ngoes on" }], items: [] },
      { c: [{ t: "text", v: "next" }], items: [] },
    ],
  });
});

test("paragraphs split on blank lines", () => {
  expect(parseMarkdown("one\ntwo\n\nthree")).toEqual([
    { t: "paragraph", c: [{ t: "text", v: "one\ntwo" }] },
    { t: "paragraph", c: [{ t: "text", v: "three" }] },
  ]);
  expect(html("a\n\nb")).toBe('<div><p class="mt-2 first:mt-0">a</p><p class="mt-2 first:mt-0">b</p></div>');
});

test("bold, inline code and links", () => {
  expect(parseInline("Under **Settings › Updates** now")).toEqual([
    { t: "text", v: "Under " },
    { t: "strong", c: [{ t: "text", v: "Settings › Updates" }] },
    { t: "text", v: " now" },
  ]);
  expect(parseInline("set `engines.node` and ``a ` b``")).toEqual([
    { t: "text", v: "set " },
    { t: "code", v: "engines.node" },
    { t: "text", v: " and " },
    { t: "code", v: "a ` b" },
  ]);
  expect(parseInline("see [the docs](https://example.com/a_(b)) here")).toEqual([
    { t: "text", v: "see " },
    { t: "link", href: "https://example.com/a_(b)", c: [{ t: "text", v: "the docs" }] },
    { t: "text", v: " here" },
  ]);
});

test("a commit link with code in its label", () => {
  const url = "https://github.com/upsetdev/pyo/commit/00ddcd8a04b648d540e0ffdae02effb2ac3c18cc";
  expect(parseInline(`[\`00ddcd8\`](${url}) feat: x`)).toEqual([
    { t: "link", href: url, c: [{ t: "code", v: "00ddcd8" }] },
    { t: "text", v: " feat: x" },
  ]);
  const out = html(`- [\`00ddcd8\`](${url}) feat: x`);
  expect(out).toContain(`<a href="${url}" target="_blank" rel="noreferrer noopener"`);
  expect(out).toContain("<code");
});

test("fenced code and rules", () => {
  expect(parseMarkdown("```sh\ncurl x | sh\n```\n---")).toEqual([
    { t: "code", v: "curl x | sh" },
    { t: "rule" },
  ]);
});

test("javascript:, data:, http: and relative links stay text", () => {
  for (const url of [
    "javascript:alert(1)",
    "JavaScript:alert(1)",
    " javascript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "http://example.com",
    "/settings",
    "//evil.example",
    "vbscript:x",
    "https://",
  ]) {
    expect(safeHref(url)).toBeNull();
    const out = html(`[click](${url})`);
    expect(out).not.toContain("<a");
    expect(out).toContain("click");
  }
  expect(safeHref("https://example.com/x")).toBe("https://example.com/x");
});

test("raw HTML shows as text", () => {
  const out = html('<script>alert("x")</script>\n\n<img src="https://t.example/p.gif" onerror="x()">');
  expect(out).not.toContain("<script");
  expect(out).not.toContain("<img");
  expect(out).toContain("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;");
  expect(out).toContain("&lt;img src=");
});

test("images render as a link or their alt text, never <img>", () => {
  const pixel = html("![pixel](https://t.example/p.gif)");
  expect(pixel).not.toContain("<img");
  expect(pixel).toContain('<a href="https://t.example/p.gif"');
  expect(pixel).toContain(">pixel</a>");

  const bad = html("![alt](javascript:alert(1))");
  expect(bad).not.toContain("<img");
  expect(bad).not.toContain("<a");
  expect(bad).toContain("alt");

  // An empty alt shows the address.
  expect(html("![](https://t.example/p.gif)")).toContain(">https://t.example/p.gif</a>");
  // Markup in alt text stays text.
  expect(html("![**x**](https://t.example/p.gif)")).not.toContain("<strong");
});

test("odd syntax stays text and never throws", () => {
  for (const s of [
    "",
    "**unclosed",
    "`unclosed",
    "[no url]",
    "[a](unclosed",
    "> quote",
    "| a | b |\n|---|---|",
    "1. ordered",
    "*italic*",
    "```\nunclosed fence",
    "\\*escaped\\*",
    "-\n*\n#",
    "[".repeat(5000) + "](".repeat(5000),
    "`".repeat(3000) + "x",
    "**".repeat(5000),
  ]) {
    expect(() => html(s)).not.toThrow();
  }
  expect(html("**unclosed")).toContain("**unclosed");
  expect(html("*italic*")).toContain("*italic*");
  expect(html("\\*escaped\\*")).toContain("*escaped*");
});

test("20,000 characters of brackets render quickly", () => {
  const start = performance.now();
  html("[`".repeat(10_000));
  expect(performance.now() - start).toBeLessThan(1000);
});

test("the 0.52.0 release notes", () => {
  const out = html(NOTES);
  const links = out.match(/<a /g) ?? [];
  expect(links.length).toBe(107);
  expect(out.match(/href="https:\/\/github\.com\/upsetdev\/pyo\/commit\//g)?.length).toBe(107);
  expect(out.match(/rel="noreferrer noopener"/g)?.length).toBe(107);
  expect(out).toContain(">🚀 Features</h6>");
  expect(out).toContain(">⬆️ Security Fixes</h6>");
  expect(out).toContain("<strong");
  expect(out).not.toContain("**");
  expect(out).not.toContain("](");
  const blocks = parseMarkdown(NOTES);
  expect(blocks.filter((b) => b.t === "heading").length).toBe(5);
  expect(blocks.filter((b) => b.t === "list").length).toBe(5);
});
