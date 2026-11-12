import { useMemo, type ReactNode } from "react";
import { ansiClass, parseAnsi } from "~/lib/pyo/ansi";

/**
 * Log text with its ANSI colors as styled spans (never as HTML), and the
 * first match of `query` (lowercase, in the plain text) marked.
 */
export function AnsiText({ text, query = "" }: { text: string; query?: string }) {
  const parts = useMemo(() => parseAnsi(text), [text]);
  const plain = parts.length === 1 ? parts[0].text : parts.map((p) => p.text).join("");
  const at = query ? plain.toLowerCase().indexOf(query) : -1;
  const end = at + query.length;
  const out: ReactNode[] = [];
  let pos = 0;
  parts.forEach((p, k) => {
    const cls = ansiClass(p.style);
    const from = pos;
    pos += p.text.length;
    // The piece of this segment in the match, if any.
    const a = at < 0 ? -1 : Math.max(at, from) - from;
    const b = Math.min(end, pos) - from;
    const piece = (s: string, key: string) => (cls ? <span key={key} className={cls}>{s}</span> : s);
    if (at < 0 || b <= 0 || a >= p.text.length) {
      out.push(cls ? <span key={k} className={cls}>{p.text}</span> : p.text);
      return;
    }
    out.push(
      <span key={k}>
        {a > 0 ? piece(p.text.slice(0, a), "a") : null}
        <mark className="rounded-sm bg-warning/30 text-foreground">{piece(p.text.slice(a, b), "m")}</mark>
        {b < p.text.length ? piece(p.text.slice(b), "b") : null}
      </span>,
    );
  });
  return <>{out}</>;
}
