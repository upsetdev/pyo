// A small Markdown renderer for release notes.
//
// It covers what our release notes use: headings, bullet lists with one
// nesting level, paragraphs, bold, inline code, links and fenced code.
// Anything else stays as plain text.
//
// It builds React elements only, so raw HTML in the notes shows as text.
// Images render as links, never as <img>. Only https:// links are live.
import { Fragment, type ReactNode } from "react";

export type Inline =
  | { t: "text"; v: string }
  | { t: "code"; v: string }
  | { t: "strong"; c: Inline[] }
  | { t: "link"; href: string; c: Inline[] };

export type Item = { c: Inline[]; items: Item[] };

export type Block =
  | { t: "heading"; level: 1 | 2 | 3; c: Inline[] }
  | { t: "paragraph"; c: Inline[] }
  | { t: "list"; items: Item[] }
  | { t: "code"; v: string }
  | { t: "rule" };

// How far ahead the inline parser looks for a closing `]`, `)`, `**` or
// backtick run. Keeps a long line of unclosed brackets from going quadratic.
const LOOKAHEAD = 2000;

/** The URL as a live link target, or null when it must stay text. */
export function safeHref(raw: string): string | null {
  const s = raw.trim();
  if (!/^https:\/\//i.test(s)) return null;
  try {
    const u = new URL(s);
    return u.protocol === "https:" && u.hostname ? u.href : null;
  } catch {
    return null;
  }
}

function pushText(out: Inline[], v: string) {
  if (!v) return;
  const last = out[out.length - 1];
  if (last?.t === "text") last.v += v;
  else out.push({ t: "text", v });
}

// `[label](url)` at s[i] === "[". Returns the label, url and end index, or
// null when it isn't a well-formed link.
function linkAt(s: string, i: number) {
  let depth = 0;
  const stop = Math.min(s.length, i + LOOKAHEAD);
  let close = -1;
  for (let j = i; j < stop; j++) {
    const ch = s[j];
    if (ch === "\\") {
      j++;
      continue;
    }
    if (ch === "`") {
      // A code span inside the label hides its brackets.
      const end = codeSpanEnd(s, j);
      if (end > 0) {
        j = end - 1;
        continue;
      }
    }
    if (ch === "[") depth++;
    else if (ch === "]" && --depth === 0) {
      close = j;
      break;
    }
  }
  if (close < 0 || s[close + 1] !== "(") return null;
  // The URL runs to the matching ")", allowing one level of parentheses.
  let paren = 0;
  let end = -1;
  for (let j = close + 2; j < stop; j++) {
    const ch = s[j];
    if (ch === " " || ch === "\t" || ch === "\n") break;
    if (ch === "(") paren++;
    else if (ch === ")") {
      if (paren === 0) {
        end = j;
        break;
      }
      paren--;
    }
  }
  if (end < 0) return null;
  return {
    label: s.slice(i + 1, close),
    url: s.slice(close + 2, end),
    end: end + 1,
  };
}

// The index after a code span starting at s[i] === "`", or -1.
function codeSpanEnd(s: string, i: number): number {
  let n = 0;
  while (s[i + n] === "`") n++;
  const fence = "`".repeat(n);
  const stop = Math.min(s.length, i + n + LOOKAHEAD);
  let j = s.indexOf(fence, i + n);
  while (j >= 0 && j < stop) {
    // The closing run must be exactly n backticks long.
    if (s[j + n] !== "`") return j + n;
    while (s[j] === "`") j++;
    j = s.indexOf(fence, j);
  }
  return -1;
}

/** Parses one block's text into inline nodes. */
export function parseInline(s: string, inLink = false, inStrong = false): Inline[] {
  const out: Inline[] = [];
  let i = 0;
  let text = "";
  const flush = () => {
    pushText(out, text);
    text = "";
  };
  while (i < s.length) {
    const ch = s[i];
    if (ch === "\\" && i + 1 < s.length && /[!-/:-@[-`{-~]/.test(s[i + 1])) {
      text += s[i + 1];
      i += 2;
      continue;
    }
    if (ch === "`") {
      const end = codeSpanEnd(s, i);
      if (end > 0) {
        let n = 0;
        while (s[i + n] === "`") n++;
        let v = s.slice(i + n, end - n).replace(/\n/g, " ");
        if (/^ .*[^ ].* $/.test(v)) v = v.slice(1, -1);
        flush();
        out.push({ t: "code", v });
        i = end;
        continue;
      }
      // An unclosed run stays text as a whole.
      while (s[i] === "`") text += s[i++];
      continue;
    }
    if (ch === "*" && s[i + 1] === "*" && !inStrong) {
      const close = s.indexOf("**", i + 2);
      if (close > i + 2 && close - i < LOOKAHEAD && !/\s/.test(s[i + 2]) && !/\s/.test(s[close - 1])) {
        flush();
        out.push({ t: "strong", c: parseInline(s.slice(i + 2, close), inLink, true) });
        i = close + 2;
        continue;
      }
    }
    const image = ch === "!" && s[i + 1] === "[";
    if ((ch === "[" || image) && !inLink) {
      const at = image ? i + 1 : i;
      const link = linkAt(s, at);
      if (link) {
        flush();
        const href = safeHref(link.url);
        // An image's alt text is plain text. Fall back to the URL when empty.
        const label = image
          ? [{ t: "text", v: link.label || link.url } as Inline]
          : parseInline(link.label, true, inStrong);
        if (href) out.push({ t: "link", href, c: label });
        else for (const n of label) out.push(n);
        i = link.end;
        continue;
      }
    }
    text += ch;
    i++;
  }
  flush();
  return out;
}

const HEADING = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/;
const BULLET = /^( *)[-*+][ \t]+(.*)$/;
const RULE = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const FENCE = /^ {0,3}(`{3,}|~{3,})/;

/** Parses release notes into blocks. Never throws on odd input. */
export function parseMarkdown(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, "\n").replace(/\t/g, "    ").split("\n");
  const blocks: Block[] = [];
  let para: string[] = [];
  let list: Item[] | null = null;
  // The last item at each level, and the text lines that belong to it.
  let top: { item: Item; lines: string[] } | null = null;
  let sub: { item: Item; lines: string[] } | null = null;

  const closeItem = () => {
    if (sub) sub.item.c = parseInline(sub.lines.join("\n"));
    if (top) top.item.c = parseInline(top.lines.join("\n"));
    sub = null;
  };
  const closeList = () => {
    closeItem();
    top = null;
    if (list) blocks.push({ t: "list", items: list });
    list = null;
  };
  const closePara = () => {
    if (para.length) blocks.push({ t: "paragraph", c: parseInline(para.join("\n")) });
    para = [];
  };

  for (let n = 0; n < lines.length; n++) {
    const line = lines[n];
    if (!line.trim()) {
      closePara();
      // A blank line inside a list ends the item's text but not the list.
      if (list) {
        let next = n + 1;
        while (next < lines.length && !lines[next].trim()) next++;
        if (next >= lines.length || !BULLET.test(lines[next])) closeList();
      }
      continue;
    }
    const fence = FENCE.exec(line);
    if (fence) {
      closePara();
      closeList();
      const mark = fence[1];
      const body: string[] = [];
      n++;
      while (n < lines.length && !lines[n].trimStart().startsWith(mark)) body.push(lines[n++]);
      blocks.push({ t: "code", v: body.join("\n") });
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      closePara();
      closeList();
      const level = Math.min(heading[1].length, 3) as 1 | 2 | 3;
      blocks.push({ t: "heading", level, c: parseInline(heading[2] ?? "") });
      continue;
    }
    if (RULE.test(line)) {
      closePara();
      closeList();
      blocks.push({ t: "rule" });
      continue;
    }
    const bullet = BULLET.exec(line);
    if (bullet) {
      closePara();
      const nested = bullet[1].length >= 2 && top !== null;
      if (nested && top) {
        if (sub) sub.item.c = parseInline(sub.lines.join("\n"));
        const item: Item = { c: [], items: [] };
        top.item.items.push(item);
        sub = { item, lines: [bullet[2]] };
      } else {
        closeItem();
        const item: Item = { c: [], items: [] };
        list ??= [];
        list.push(item);
        top = { item, lines: [bullet[2]] };
      }
      continue;
    }
    // Any other line continues the open item or paragraph.
    const target = sub ?? top;
    if (list && target) target.lines.push(line.trim());
    else para.push(line.trim());
  }
  closePara();
  closeList();
  return blocks;
}

const LINK = "font-medium text-primary hover:text-primary/80";
const CODE = "rounded bg-muted px-1 py-px font-mono text-[11.5px]";

function Inlines({ nodes, inLink }: { nodes: Inline[]; inLink?: boolean }) {
  return (
    <>
      {nodes.map((n, i) => {
        switch (n.t) {
          case "text":
            return <Fragment key={i}>{n.v}</Fragment>;
          case "code":
            return (
              // Inside a link, code takes the link's colour.
              <code
                key={i}
                className={inLink ? CODE : `${CODE} text-foreground/85`}
              >
                {n.v}
              </code>
            );
          case "strong":
            return (
              <strong key={i} className="font-semibold">
                <Inlines nodes={n.c} inLink={inLink} />
              </strong>
            );
          case "link":
            return (
              <a
                key={i}
                href={n.href}
                target="_blank"
                rel="noreferrer noopener"
                className={LINK}
              >
                <Inlines nodes={n.c} inLink />
              </a>
            );
        }
      })}
    </>
  );
}

function Items({ items, nested }: { items: Item[]; nested?: boolean }) {
  return (
    <ul
      className={
        nested
          ? "mt-0.5 list-[circle] space-y-0.5 pl-4"
          : "list-disc space-y-0.5 pl-4 marker:text-muted-foreground"
      }
    >
      {items.map((it, i) => (
        <li key={i}>
          <Inlines nodes={it.c} />
          {it.items.length ? <Items items={it.items} nested /> : null}
        </li>
      ))}
    </ul>
  );
}

const HEADING_CLASS = {
  1: "text-[14px] font-semibold",
  2: "text-[13.5px] font-semibold",
  3: "text-[13px] font-semibold",
} as const;

function render(b: Block, i: number): ReactNode {
  switch (b.t) {
    case "heading": {
      // The dialog's "Release notes" title is an h3, so the notes start at h4.
      const H = `h${b.level + 3}` as "h4" | "h5" | "h6";
      return (
        <H key={i} className={`${HEADING_CLASS[b.level]} mt-4 first:mt-0`}>
          <Inlines nodes={b.c} />
        </H>
      );
    }
    case "paragraph":
      return (
        <p key={i} className="mt-2 first:mt-0">
          <Inlines nodes={b.c} />
        </p>
      );
    case "list":
      return (
        <div key={i} className="mt-1.5 first:mt-0">
          <Items items={b.items} />
        </div>
      );
    case "code":
      return (
        <pre
          key={i}
          className="mt-2 overflow-x-auto rounded-md bg-muted px-2 py-1.5 font-mono text-[11.5px] leading-[18px] first:mt-0"
        >
          {b.v}
        </pre>
      );
    case "rule":
      return <hr key={i} className="my-3 border-border" />;
  }
}

/** Release notes as React elements. */
export function Markdown({ source, className }: { source: string; className?: string }) {
  return <div className={className}>{parseMarkdown(source).map(render)}</div>;
}
