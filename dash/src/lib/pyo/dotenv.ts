import { ENV_KEY } from "./format";

// .env parsing for pasting into the environment editor.
//
//   # comment            skipped, as are blank lines
//   export KEY=value     "export " is dropped
//   KEY=value # note     inline comment after an unquoted value
//   KEY='literal $x\n'   single quotes: taken as is
//   KEY="a\nb"           double quotes: \n \r \t \" \\ escapes, may span lines
//   KEY=                 empty value

export type EnvEntry = [key: string, value: string];

export type ParsedEnv = {
  entries: EnvEntry[];
  /** Lines that aren't KEY=value (trimmed), comments and blanks excluded. */
  invalid: string[];
};

/** Keys a pasted or new variable is suggested as a secret for. */
export const SECRET_HINT = /SECRET|PASSWORD|TOKEN|KEY/;

const ESCAPES: Record<string, string> = {
  n: "\n",
  r: "\r",
  t: "\t",
  '"': '"',
  "\\": "\\",
};

/** Index of the closing `quote` in `s` from `from`, or -1. */
function closing(s: string, quote: string, from: number) {
  for (let i = from; i < s.length; i++) {
    if (quote === '"' && s[i] === "\\") i++;
    else if (s[i] === quote) return i;
  }
  return -1;
}

export function parseEnv(text: string): ParsedEnv {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const entries: EnvEntry[] = [];
  const invalid: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim();
    if (!line || line.startsWith("#")) continue;
    const m = /^(?:export\s+)?([^=]*?)\s*=\s*(.*)$/.exec(line);
    if (!m || !ENV_KEY.test(m[1])) {
      invalid.push(line);
      continue;
    }
    const key = m[1];
    let rest = m[2];
    const quote = rest[0];
    if (quote === '"' || quote === "'") {
      // A quoted value runs to its closing quote, over lines if need be.
      let end = closing(rest, quote, 1);
      let j = i;
      while (end < 0 && j + 1 < lines.length) {
        rest += "\n" + lines[++j];
        end = closing(rest, quote, 1);
      }
      if (end < 0) {
        invalid.push(line);
        continue;
      }
      i = j;
      const body = rest.slice(1, end);
      entries.push([
        key,
        quote === "'"
          ? body
          : body.replace(/\\(.)/g, (all, c: string) => ESCAPES[c] ?? all),
      ]);
    } else {
      entries.push([key, rest.replace(/\s+#.*$/, "").trim()]);
    }
  }
  return { entries, invalid };
}

/**
 * The entries of pasted text if it reads as .env: two or more variables,
 * or one `KEY=value` pasted into a key field. Otherwise null, for a plain
 * paste.
 */
export function envFromPaste(
  text: string,
  into: "key" | "value",
): ParsedEnv | null {
  const parsed = parseEnv(text);
  const n = parsed.entries.length;
  if (n >= 2) return parsed;
  if (n === 1 && into === "key" && !parsed.invalid.length) return parsed;
  return null;
}

/** One entry per key, the last one winning; `repeated` lists those keys. */
export function lastWins(entries: EnvEntry[]) {
  const map = new Map<string, string>();
  const repeated = new Set<string>();
  for (const [k, v] of entries) {
    if (map.has(k)) repeated.add(k);
    map.set(k, v);
  }
  return { entries: [...map] as EnvEntry[], repeated: [...repeated] };
}
