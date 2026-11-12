// A custom WAF rule's conditions, checked against a request the way
// the WAF's processor checks them (waf/cmd/pyo-waf/rules.go), for the rule
// builder's "Test". Both are held to the same cases: waf-vectors.json here, a
// copy of waf/cmd/pyo-waf/testdata/vectors.json (scripts/waf-match.test.ts
// checks they're the same and that this passes them).

import type { WafCondition } from "./types";

export type TestRequest = {
  method: string;
  host: string;
  /** Path and query, as sent: "/a%20b?x=1". */
  uri: string;
  /** Lower-case names. */
  headers?: Record<string, string>;
  body?: string;
  /** The visitor's address. */
  client: string;
  /** Its country, "" when unknown. */
  country: string;
};

export type TestRule = { match: "all" | "any"; conditions: WafCondition[] };

function path(uri: string) {
  const i = uri.search(/[?#]/);
  const p = i >= 0 ? uri.slice(0, i) : uri;
  // Compared decoded, so "/%77p-admin" is "/wp-admin" (Go's PathUnescape).
  if (/%(?![0-9a-fA-F]{2})/.test(p)) return p;
  try {
    return decodeURIComponent(p);
  } catch {
    return p;
  }
}

function query(uri: string) {
  const i = uri.indexOf("?");
  if (i < 0) return "";
  const q = uri.slice(i + 1);
  const j = q.indexOf("#");
  return j >= 0 ? q.slice(0, j) : q;
}

function cookie(header: string, name: string) {
  for (const part of header.split(";")) {
    const t = part.trim();
    const eq = t.indexOf("=");
    if (eq > 0 && t.slice(0, eq) === name) return t.slice(eq + 1).replace(/^"(.*)"$/, "$1");
  }
  return "";
}

/** An address as a number and its width, or null. */
function addr(s: string): { n: bigint; bits: number } | null {
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(s)) {
    const parts = s.split(".").map(Number);
    if (parts.some((o) => o > 255)) return null;
    return { n: parts.reduce((n, o) => (n << 8n) | BigInt(o), 0n), bits: 32 };
  }
  if (!s.includes(":") || !/^[0-9a-fA-F:.]+$/.test(s)) return null;
  // An IPv4-mapped IPv6 address counts as IPv4 (Go's Unmap).
  const mapped = /^::ffff:(\d{1,3}(\.\d{1,3}){3})$/i.exec(s);
  if (mapped) return addr(mapped[1]);
  const [head, tail = ""] = s.split("::");
  if (s.split("::").length > 2) return null;
  const h = head ? head.split(":") : [];
  const t = tail ? tail.split(":") : [];
  const groups = s.includes("::") ? [...h, ...Array(8 - h.length - t.length).fill("0"), ...t] : h;
  if (groups.length !== 8 || groups.some((g) => !/^[0-9a-fA-F]{1,4}$/.test(g))) return null;
  return { n: groups.reduce((n, g) => (n << 16n) | BigInt(parseInt(g, 16)), 0n), bits: 128 };
}

/** The address is inside the range ("203.0.113.0/24") or is it. */
export function inRange(ip: string, range: string) {
  const a = addr(ip);
  const [net, bitsText] = range.split("/");
  const b = addr(net);
  if (!a || !b || a.bits !== b.bits) return false;
  const bits = bitsText === undefined ? b.bits : Number(bitsText);
  if (!Number.isInteger(bits) || bits < 0 || bits > b.bits) return false;
  const shift = BigInt(b.bits - bits);
  return a.n >> shift === b.n >> shift;
}

function value(c: WafCondition, r: TestRequest) {
  const h = r.headers ?? {};
  switch (c.field) {
    case "path":
      return path(r.uri);
    case "query":
      return query(r.uri);
    case "method":
      return r.method.toUpperCase();
    case "host":
      return r.host;
    case "header":
      return h[(c.name ?? "").toLowerCase()] ?? "";
    case "userAgent":
      return h["user-agent"] ?? "";
    case "cookie":
      return cookie(h["cookie"] ?? "", c.name ?? "");
    case "body":
      return r.body ?? "";
    case "country":
      return r.country;
    default:
      return "";
  }
}

/** An RE2 pattern as a JS RegExp: leading flags like "(?i)" become flags. */
export function jsRegex(src: string) {
  const m = /^\(\?([imsU]+)\)/.exec(src);
  const flags = m ? [...new Set(m[1].replace("U", "").split(""))].join("") : "";
  return new RegExp(m ? src.slice(m[0].length) : src, flags);
}

/** One condition, before its negation. */
function test(c: WafCondition, r: TestRequest) {
  if (c.field === "ip") return c.values.some((v) => inRange(r.client, v));
  const v = value(c, r);
  if (c.field === "country" && !v) return false; // unknown: no country test matches it
  switch (c.op) {
    case "equals":
      return v === c.values[0];
    case "contains":
      return v.includes(c.values[0]);
    case "startsWith":
      return v.startsWith(c.values[0]);
    case "endsWith":
      return v.endsWith(c.values[0]);
    case "in":
      return c.values.includes(v);
    case "regex":
      try {
        return jsRegex(c.values[0]).test(v);
      } catch {
        return false;
      }
  }
  return false;
}

/** The rule matches the request. */
export function matches(rule: TestRule, r: TestRequest) {
  const results = rule.conditions.map((c) => test(c, r) !== !!c.negate);
  return rule.match === "any" ? results.some(Boolean) : results.every(Boolean);
}
