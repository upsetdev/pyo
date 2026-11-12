/**
 * ANSI escape sequences in container output: colors and text styles (SGR,
 * `ESC [ … m`) are kept as styled segments; every other sequence (cursor
 * moves, clearing, titles, hyperlinks, whose text stays) and every control
 * character but tab and newline is dropped, so none reaches the page.
 * Search, filters, copying and levels read `stripAnsi`'s plain text.
 *
 * The same rules as pyo-proto's `ansi` module.
 */

/** One of the 16 basic colors: 0–7, then their bright forms 8–15. */
export type AnsiColor = number;

export type AnsiStyle = {
  fg?: AnsiColor;
  bg?: AnsiColor;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
  underline?: boolean;
};

export type AnsiSegment = { text: string; style: AnsiStyle };

const PLAIN: AnsiStyle = Object.freeze({});

/** Anything worth a look: ESC, C0 but tab and newline, DEL and C1. */
const SPECIAL = /[\x00-\x08\x0b-\x1f\x7f-\x9f]/;

type Token = { end: number; sgr?: string };

/**
 * The sequence or control character at `i`, and where it ends. A sequence
 * cut off by the end of the text runs to the end; a malformed one ends
 * where it went wrong. `sgr` is set, to its parameters, for a color one.
 */
function scan(s: string, i: number): Token {
  const c = s.charCodeAt(i);
  let intro: number;
  let j: number;
  if (c === 0x1b) {
    const n = s.charCodeAt(i + 1); // NaN at the end
    if (n >= 0x40 && n <= 0x5f) {
      intro = n;
      j = i + 2;
    } else if (n >= 0x20 && n <= 0x2f) {
      // nF: ESC, intermediates, one final byte (ESC ( B).
      j = i + 1;
      while (j < s.length && s.charCodeAt(j) >= 0x20 && s.charCodeAt(j) <= 0x2f) j++;
      if (j >= s.length) return { end: s.length };
      const f = s.charCodeAt(j);
      return { end: f >= 0x30 && f <= 0x7e ? j + 1 : j };
    } else if (n >= 0x30 && n <= 0x7e) {
      // Fp and Fs: ESC 7, ESC 8, ESC c, ESC =.
      return { end: i + 2 };
    } else {
      // A lone ESC.
      return { end: i + 1 };
    }
  } else if (c >= 0x80 && c <= 0x9f) {
    // C1: the one-character forms of ESC @ … ESC _.
    intro = c - 0x40;
    j = i + 1;
  } else {
    return { end: i + 1 };
  }
  // CSI: parameters, intermediates, one final byte.
  if (intro === 0x5b) {
    const start = j;
    while (j < s.length && s.charCodeAt(j) >= 0x30 && s.charCodeAt(j) <= 0x3f) j++;
    const params = s.slice(start, j);
    const inter = j;
    while (j < s.length && s.charCodeAt(j) >= 0x20 && s.charCodeAt(j) <= 0x2f) j++;
    if (j >= s.length) return { end: s.length };
    const f = s.charCodeAt(j);
    if (f === 0x6d && inter === j && /^[\d;:]*$/.test(params))
      return { end: j + 1, sgr: params };
    // Broken off by something else: drop what's read, keep what follows.
    return { end: f >= 0x40 && f <= 0x7e ? j + 1 : j };
  }
  // OSC, DCS, SOS, PM, APC: a string up to ST (ESC \ or U+009C) or BEL.
  if (intro === 0x5d || intro === 0x50 || intro === 0x58 || intro === 0x5e || intro === 0x5f) {
    for (; j < s.length; j++) {
      const d = s.charCodeAt(j);
      if (d === 0x07 || d === 0x9c) return { end: j + 1 };
      if (d === 0x1b && s.charCodeAt(j + 1) === 0x5c) return { end: j + 2 };
    }
    return { end: s.length };
  }
  // Any other two-character escape (ESC D, ESC M, …).
  return { end: j };
}

/** The xterm 256-color palette's index `n` as RGB (16 and up). */
function rgb256(n: number): [number, number, number] {
  if (n >= 232) {
    const v = 8 + 10 * (n - 232);
    return [v, v, v];
  }
  const k = n - 16;
  const step = (x: number) => (x ? 55 + 40 * x : 0);
  return [step(Math.floor(k / 36)), step(Math.floor(k / 6) % 6), step(k % 6)];
}

/**
 * The basic color nearest an RGB one, by hue: greys by lightness, then red,
 * yellow (orange too), green, cyan, blue and magenta; light ones bright.
 */
export function nearestBasic(r: number, g: number, b: number): AnsiColor {
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const light = (max + min) / 510;
  if (max - min < 40) return light < 0.2 ? 0 : light < 0.55 ? 8 : light < 0.85 ? 7 : 15;
  const d = max - min;
  let h =
    max === r ? ((g - b) / d) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  h = (h * 60 + 360) % 360;
  const base =
    h < 25 || h >= 330 ? 1 : h < 75 ? 3 : h < 165 ? 2 : h < 200 ? 6 : h < 260 ? 4 : 5;
  return light > 0.6 ? base + 8 : base;
}

const color256 = (n: number): AnsiColor | undefined =>
  !Number.isInteger(n) || n < 0 || n > 255
    ? undefined
    : n < 16
      ? n
      : nearestBasic(...rgb256(n));

const colorRgb = (v: number[]): AnsiColor | undefined =>
  v.length === 3 && v.every((x) => Number.isInteger(x) && x >= 0 && x <= 255)
    ? nearestBasic(v[0], v[1], v[2])
    : undefined;

const num = (s: string | undefined) => (s === undefined || s === "" ? NaN : Number(s));

/** `style` after an SGR sequence with these parameters. */
function applySgr(style: AnsiStyle, params: string): AnsiStyle {
  const st: AnsiStyle = { ...style };
  const parts = params === "" ? ["0"] : params.split(";");
  for (let k = 0; k < parts.length; k++) {
    const sub = parts[k].split(":");
    const code = sub[0] === "" ? 0 : Number(sub[0]);
    if (code === 38 || code === 48) {
      let c: AnsiColor | undefined;
      if (sub.length > 1) {
        // 38:5:n, 38:2:r:g:b or 38:2::r:g:b (with a color space).
        const mode = num(sub[1]);
        if (mode === 5) c = color256(num(sub[2]));
        else if (mode === 2)
          c = colorRgb((sub.length >= 6 ? sub.slice(3, 6) : sub.slice(2, 5)).map(num));
      } else {
        const mode = num(parts[k + 1]);
        if (mode === 5) {
          c = color256(num(parts[k + 2]));
          k += 2;
        } else if (mode === 2) {
          c = colorRgb(parts.slice(k + 2, k + 5).map(num));
          k += 4;
        } else {
          // Unknown: the rest can't be read reliably.
          break;
        }
      }
      if (c !== undefined) {
        if (code === 38) st.fg = c;
        else st.bg = c;
      }
      continue;
    }
    if (code === 0) {
      for (const key of Object.keys(st)) delete st[key as keyof AnsiStyle];
    } else if (code === 1) st.bold = true;
    else if (code === 2) st.dim = true;
    else if (code === 3) st.italic = true;
    else if (code === 4) st.underline = sub.length < 2 || num(sub[1]) !== 0;
    else if (code === 21) st.underline = true;
    else if (code === 22) {
      delete st.bold;
      delete st.dim;
    } else if (code === 23) delete st.italic;
    else if (code === 24) delete st.underline;
    else if (code >= 30 && code <= 37) st.fg = code - 30;
    else if (code === 39) delete st.fg;
    else if (code >= 40 && code <= 47) st.bg = code - 40;
    else if (code === 49) delete st.bg;
    else if (code >= 90 && code <= 97) st.fg = code - 90 + 8;
    else if (code >= 100 && code <= 107) st.bg = code - 100 + 8;
    if (st.underline === false) delete st.underline;
  }
  return st;
}

const sameStyle = (a: AnsiStyle, b: AnsiStyle) =>
  a.fg === b.fg &&
  a.bg === b.bg &&
  !!a.bold === !!b.bold &&
  !!a.dim === !!b.dim &&
  !!a.italic === !!b.italic &&
  !!a.underline === !!b.underline;

/** `s` as styled segments of plain text; adjacent ones of one style merged. */
export function parseAnsi(s: string): AnsiSegment[] {
  if (!SPECIAL.test(s)) return s ? [{ text: s, style: PLAIN }] : [];
  const out: AnsiSegment[] = [];
  let style: AnsiStyle = PLAIN;
  const push = (text: string) => {
    if (!text) return;
    const last = out[out.length - 1];
    if (last && sameStyle(last.style, style)) last.text += text;
    else out.push({ text, style });
  };
  let from = 0;
  let i = 0;
  while (i < s.length) {
    const c = s.charCodeAt(i);
    if (!(c < 0x20 || (c >= 0x7f && c <= 0x9f)) || c === 0x09 || c === 0x0a) {
      i++;
      continue;
    }
    push(s.slice(from, i));
    const t = scan(s, i);
    if (t.sgr !== undefined) style = applySgr(style, t.sgr);
    i = from = t.end;
  }
  push(s.slice(from));
  return out;
}

/** `s` as plain text: no escape sequences, no control characters but tab and newline. */
export function stripAnsi(s: string): string {
  if (!SPECIAL.test(s)) return s;
  return parseAnsi(s)
    .map((p) => p.text)
    .join("");
}

/**
 * The 16 colors in Pyo's theme tokens, readable in dark and light. Black
 * and bright black are the muted grey, white the text color, magenta a
 * neutral (the text color), bright ones mostly their base token.
 */
const FG: readonly string[] = [
  "text-muted-foreground", // black
  "text-destructive", // red
  "text-success", // green
  "text-warning", // yellow
  "text-primary", // blue
  "text-foreground", // magenta
  "text-cyan-700 dark:text-cyan-300", // cyan
  "text-foreground", // white
  "text-muted-foreground", // bright black
  "text-destructive", // bright red
  "text-success", // bright green
  "text-warning", // bright yellow
  "text-info", // bright blue
  "text-foreground", // bright magenta
  "text-cyan-700 dark:text-cyan-300", // bright cyan
  "text-foreground", // bright white
];

const BG: readonly string[] = [
  "bg-muted-foreground/20",
  "bg-destructive/20",
  "bg-success/20",
  "bg-warning/20",
  "bg-primary/20",
  "bg-foreground/10",
  "bg-cyan-500/20",
  "bg-foreground/15",
  "bg-muted-foreground/20",
  "bg-destructive/25",
  "bg-success/25",
  "bg-warning/25",
  "bg-info/25",
  "bg-foreground/10",
  "bg-cyan-500/25",
  "bg-foreground/15",
];

/** A segment's classes; "" for plain text. */
export function ansiClass(st: AnsiStyle): string {
  const c: string[] = [];
  if (st.fg !== undefined) c.push(FG[st.fg]);
  if (st.bg !== undefined) c.push(BG[st.bg], "rounded-[2px]");
  if (st.bold) c.push("font-bold");
  if (st.dim) c.push("opacity-60");
  if (st.italic) c.push("italic");
  if (st.underline) c.push("underline");
  return c.join(" ");
}
