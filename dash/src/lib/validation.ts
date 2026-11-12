import { z } from "zod";

// Shared input schemas for forms and search params.

export const emailSchema = z
  .string()
  .trim()
  .toLowerCase()
  .min(3)
  .max(254)
  .regex(/^[^\s@]+@[^\s@]+\.[^\s@]+$/, "Enter a valid email address");

/**
 * Same-origin path for post-auth redirects. Browsers read `\` as `/` and
 * drop tabs and newlines, so `/\evil.com` would leave the site: anything
 * that doesn't resolve to this origin is refused, and what's kept is the
 * resolved path.
 */
export function sameOriginPath(raw: unknown): string | null {
  if (typeof raw !== "string" || !raw.startsWith("/")) return null;
  // Backslashes and control characters have no place in a path we make.
  if (/[\\\u0000-\u001f\u007f]/.test(raw)) return null;
  const base = "https://pyo.invalid";
  let url: URL;
  try {
    url = new URL(raw, base);
  } catch {
    return null;
  }
  if (url.origin !== base) return null;
  return `${url.pathname}${url.search}${url.hash}`;
}

export const nextPathSchema = z
  .string()
  .refine((s) => sameOriginPath(s) !== null, "Must be a same-origin path");

export function safeNextPath(
  raw: string | null | undefined,
  fallback = "/",
): string {
  return sameOriginPath(raw) ?? fallback;
}

/**
 * Search-param schemas. TanStack Router parses the query string as JSON, so
 * `?paid=1` arrives as the number 1. Flags accept any scalar and stay as-is,
 * which keeps the URL clean when the router writes it back.
 */
export const flagParam = z
  .union([z.string(), z.number(), z.boolean()])
  .optional();
export const textParam = z
  .union([z.string(), z.number()])
  .transform(String)
  .optional();
