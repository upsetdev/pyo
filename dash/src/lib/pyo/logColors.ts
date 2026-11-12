/**
 * Each sender in a log (a host, a service, a pod) keeps one color, as in
 * Papertrail: picked from its name, readable in light and dark.
 */
const PALETTE = [
  "text-sky-600 dark:text-sky-400",
  "text-violet-600 dark:text-violet-400",
  "text-emerald-600 dark:text-emerald-400",
  "text-amber-600 dark:text-amber-300",
  "text-pink-600 dark:text-pink-400",
  "text-cyan-600 dark:text-cyan-300",
  "text-lime-600 dark:text-lime-300",
  "text-orange-600 dark:text-orange-300",
];

export const senderColor = (name: string, shift = 0) => {
  let n = 0;
  for (const c of name) n = (n * 31 + c.charCodeAt(0)) >>> 0;
  return PALETTE[(n + shift) % PALETTE.length];
};
