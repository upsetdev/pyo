// The dashboard's ANSI parser for log lines.
// bun test scripts/ansi.test.ts
import { expect, test } from "bun:test";
import { ansiClass, nearestBasic, parseAnsi, stripAnsi } from "../src/lib/pyo/ansi";

const E = "\x1b";
const GHOST = `[2026-11-10 06:47:15] ${E}[36mINFO${E}[39m "GET /ghost/api/admin/labels/?limit=100&page=1" ${E}[32m200${E}[39m 36ms`;

test("the Ghost line", () => {
  expect(parseAnsi(GHOST)).toEqual([
    { text: "[2026-11-10 06:47:15] ", style: {} },
    { text: "INFO", style: { fg: 6 } },
    { text: ' "GET /ghost/api/admin/labels/?limit=100&page=1" ', style: {} },
    { text: "200", style: { fg: 2 } },
    { text: " 36ms", style: {} },
  ]);
  expect(stripAnsi(GHOST)).toBe(
    '[2026-11-10 06:47:15] INFO "GET /ghost/api/admin/labels/?limit=100&page=1" 200 36ms',
  );
  expect(stripAnsi(GHOST)).not.toContain("␛");
});

test("plain text untouched", () => {
  const s = "plain line, tab\tand ünïcødé ✓ [36m not an escape";
  expect(stripAnsi(s)).toBe(s);
  expect(parseAnsi(s)).toEqual([{ text: s, style: {} }]);
  expect(parseAnsi("")).toEqual([]);
});

test("nested styles and resets", () => {
  const s = `${E}[1mbold ${E}[31mred ${E}[2;3;4mall${E}[22m no-weight${E}[23;24m plain-red${E}[39m default${E}[0m end`;
  expect(parseAnsi(s)).toEqual([
    { text: "bold ", style: { bold: true } },
    { text: "red ", style: { bold: true, fg: 1 } },
    { text: "all", style: { bold: true, fg: 1, dim: true, italic: true, underline: true } },
    { text: " no-weight", style: { fg: 1, italic: true, underline: true } },
    { text: " plain-red", style: { fg: 1 } },
    // Adjacent segments of one style merge.
    { text: " default end", style: {} },
  ]);
  // Backgrounds, bright colors, 49 and a bare reset.
  expect(parseAnsi(`${E}[97;41m FAIL ${E}[49m x${E}[m y`)).toEqual([
    { text: " FAIL ", style: { fg: 15, bg: 1 } },
    { text: " x", style: { fg: 15 } },
    { text: " y", style: {} },
  ]);
  expect(parseAnsi(`${E}[92mok${E}[104mblue`)).toEqual([
    { text: "ok", style: { fg: 10 } },
    { text: "blue", style: { fg: 10, bg: 12 } },
  ]);
});

test("256 colors and truecolor map to the nearest basic one", () => {
  expect(parseAnsi(`${E}[38;5;9mx`)[0].style).toEqual({ fg: 9 });
  expect(parseAnsi(`${E}[38;5;196mx`)[0].style).toEqual({ fg: 1 }); // red
  expect(parseAnsi(`${E}[38;5;214mx`)[0].style).toEqual({ fg: 3 }); // orange
  expect(parseAnsi(`${E}[38;5;45mx`)[0].style).toEqual({ fg: 6 }); // cyan
  expect(parseAnsi(`${E}[38;5;244mx`)[0].style).toEqual({ fg: 8 }); // grey
  expect(parseAnsi(`${E}[48;5;22mx`)[0].style).toEqual({ bg: 2 }); // dark green
  expect(parseAnsi(`${E}[38;2;255;0;0mx`)[0].style).toEqual({ fg: 1 });
  expect(parseAnsi(`${E}[38;2;120;200;255mx`)[0].style).toEqual({ fg: 12 }); // light blue
  expect(parseAnsi(`${E}[48;2;20;20;20;1mx`)[0].style).toEqual({ bg: 0, bold: true });
  // The colon forms.
  expect(parseAnsi(`${E}[38:5:2mx`)[0].style).toEqual({ fg: 2 });
  expect(parseAnsi(`${E}[38:2::0:200:0mx`)[0].style).toEqual({ fg: 2 });
  expect(parseAnsi(`${E}[38:2:0:0:255mx`)[0].style).toEqual({ fg: 4 });
  // Out of range: ignored.
  expect(parseAnsi(`${E}[38;5;300mx`)[0].style).toEqual({});
  expect(nearestBasic(255, 255, 255)).toBe(15);
  expect(nearestBasic(0, 0, 0)).toBe(0);
  expect(nearestBasic(200, 0, 200)).toBe(5);
});

test("non-SGR sequences are dropped", () => {
  // Cursor moves, clear line, private modes, a charset, save/restore.
  expect(stripAnsi(`${E}[2K${E}[1Gdone ${E}[?25h${E}[3Aok${E}(B${E}7${E}8`)).toBe("done ok");
  expect(parseAnsi(`${E}[2K${E}[32mdone${E}[0m${E}[1A`)).toEqual([
    { text: "done", style: { fg: 2 } },
  ]);
  // Window titles; a hyperlink keeps its text.
  expect(stripAnsi(`${E}]0;title\x07after`)).toBe("after");
  expect(stripAnsi(`see ${E}]8;;https://pyo.dev${E}\\the docs${E}]8;;${E}\\.`)).toBe("see the docs.");
  // C1 forms and stray control characters.
  expect(parseAnsi("\u009b31mred\u009b2J\r\b\x00")).toEqual([{ text: "red", style: { fg: 1 } }]);
  expect(stripAnsi("a\u0085b\x7fc")).toBe("abc");
  // Newlines and tabs stay (cron output is several lines).
  expect(stripAnsi(`a\n${E}[1mb\tc`)).toBe("a\nb\tc");
});

test("a truncated or malformed sequence at the end of a chunk", () => {
  expect(parseAnsi(`ok ${E}[31mred${E}[38;5;2`)).toEqual([
    { text: "ok ", style: {} },
    { text: "red", style: { fg: 1 } },
  ]);
  expect(stripAnsi(`ok ${E}`)).toBe("ok ");
  expect(stripAnsi(`ok ${E}[`)).toBe("ok ");
  expect(stripAnsi(`ok ${E}]8;;http://x`)).toBe("ok ");
  // Broken off by another character: that character stays.
  expect(stripAnsi(`a${E}[3é`)).toBe("aé");
  expect(stripAnsi(`a${E}[31\x07b`)).toBe("ab");
});

test("classes are theme tokens, never the purple series", () => {
  expect(ansiClass({})).toBe("");
  expect(ansiClass({ fg: 1, bold: true })).toBe("text-destructive font-bold");
  expect(ansiClass({ fg: 6 })).toContain("cyan");
  for (let i = 0; i < 16; i++) {
    const c = ansiClass({ fg: i, bg: i });
    expect(c).not.toMatch(/violet|purple|fuchsia|pink|series/);
  }
  expect(ansiClass({ fg: 5 })).toBe("text-foreground");
  expect(ansiClass({ dim: true, italic: true, underline: true })).toBe("opacity-60 italic underline");
});
