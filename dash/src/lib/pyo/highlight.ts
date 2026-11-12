// Simple token colouring for connection details and short code: `.env`
// lines, connection URLs, shell and JavaScript. No grammar, just the parts
// that help reading: keys, URL pieces, strings, keywords, numbers, comments.

export type TokenKind = "plain" | "punct" | "key" | "host" | "string" | "keyword" | "number" | "comment";
export type Token = { kind: TokenKind; text: string };
export type Lang = "value" | "env" | "sh" | "js";

/** Classes per kind: existing tokens only. */
export const TOKEN_CLASS: Record<TokenKind, string> = {
  plain: "",
  punct: "text-muted-foreground",
  key: "text-foreground",
  host: "text-info",
  string: "text-info",
  keyword: "font-medium text-foreground",
  number: "text-warning",
  comment: "text-muted-foreground",
};

/** For the element holding the tokens: monospace without ligatures. */
export const CODE_CLASS = "font-mono [font-variant-ligatures:none]";

const URL_RE = /^([a-z][a-z0-9+.-]*:\/\/)(?:([^:@/\s]*)(?::([^@/\s]*))?@)?([^:/?#\s]+)(?::(\d+))?([^\s]*)$/i;

/** scheme:// user:password@ host :port /path?query */
function url(text: string): Token[] | null {
  const m = URL_RE.exec(text);
  if (!m) return null;
  const [, scheme, user, password, host, port, rest] = m;
  const out: Token[] = [{ kind: "punct", text: scheme }];
  if (user !== undefined) {
    out.push({ kind: "key", text: user });
    if (password !== undefined) out.push({ kind: "punct", text: ":" }, { kind: "plain", text: password });
    out.push({ kind: "punct", text: "@" });
  }
  out.push({ kind: "host", text: host });
  if (port) out.push({ kind: "punct", text: ":" }, { kind: "number", text: port });
  if (rest) out.push({ kind: "plain", text: rest });
  return out;
}

/** A bare value: a URL in parts, anything else as a string. */
function value(text: string): Token[] {
  return url(text) ?? [{ kind: "string", text }];
}

/** KEY=value or KEY="value" lines; # comments. */
function env(text: string): Token[] {
  const out: Token[] = [];
  text.split("\n").forEach((line, i) => {
    if (i) out.push({ kind: "plain", text: "\n" });
    const m = /^(\s*)(export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(line);
    if (line.trimStart().startsWith("#") || !m) {
      out.push({ kind: line.trimStart().startsWith("#") ? "comment" : "plain", text: line });
      return;
    }
    const [, lead, exp, key, raw] = m;
    if (lead) out.push({ kind: "plain", text: lead });
    if (exp) out.push({ kind: "keyword", text: exp });
    out.push({ kind: "key", text: key }, { kind: "punct", text: "=" });
    const q = /^(["'])(.*)\1$/.exec(raw);
    if (q) out.push({ kind: "punct", text: q[1] }, ...value(q[2]), { kind: "punct", text: q[1] });
    else if (raw) out.push(...value(raw));
  });
  return out;
}

const KEYWORDS: Record<"sh" | "js", string[]> = {
  sh: ["export", "curl", "psql", "mysql", "valkey-cli", "redis-cli"],
  js: ["import", "from", "const", "let", "await", "async", "new", "return", "if", "throw"],
};

function code(text: string, lang: "sh" | "js"): Token[] {
  const comment = lang === "sh" ? "#[^\\n]*" : "\\/\\/[^\\n]*";
  const kw = KEYWORDS[lang].map((k) => k.replace(/-/g, "\\-")).join("|");
  const re = new RegExp(
    `(${comment})|("(?:\\\\.|[^"\\\\\\n])*"|'(?:\\\\.|[^'\\\\\\n])*'|\`(?:\\\\.|[^\`\\\\])*\`)|(?<![\\w-])(${kw})(?![\\w-])|\\b(\\d+)\\b`,
    "g",
  );
  const out: Token[] = [];
  let at = 0;
  for (const m of text.matchAll(re)) {
    if (m.index > at) out.push({ kind: "plain", text: text.slice(at, m.index) });
    out.push({ kind: m[1] ? "comment" : m[2] ? "string" : m[3] ? "keyword" : "number", text: m[0] });
    at = m.index + m[0].length;
  }
  if (at < text.length) out.push({ kind: "plain", text: text.slice(at) });
  return out;
}

export function highlight(text: string, lang: Lang): Token[] {
  if (lang === "value") return value(text);
  if (lang === "env") return env(text);
  return code(text, lang);
}
