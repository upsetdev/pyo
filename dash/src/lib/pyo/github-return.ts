import { sameOriginPath } from "~/lib/validation";

// Connecting GitHub leaves the dashboard: GitHub installs the app, then the
// API sends the person to /<team>/integrations?github=<result>. A page that
// starts it (a deploy link) keeps its own path here first, and Integrations
// sends them back to it. Kept in this tab only, for a short while, and only
// ever a path on this dashboard.

const KEY = "pyo.githubReturn";
/** The GitHub round trip takes a minute or two. Older means it was dropped. */
const MAX_AGE_MS = 30 * 60 * 1000;

type Saved = { path: string; teamId: string; at: number };

/** Before leaving for GitHub: come back to `path` (a same-origin path). */
export function rememberGithubReturn(path: string, teamId: string) {
  const safe = sameOriginPath(path);
  if (!safe) return;
  try {
    sessionStorage.setItem(KEY, JSON.stringify({ path: safe, teamId, at: Date.now() }));
  } catch {
    // Private mode: Integrations opens as before.
  }
}

export function forgetGithubReturn() {
  try {
    sessionStorage.removeItem(KEY);
  } catch {
    // Nothing kept.
  }
}

/** Where to go back to for this team, once: `null` when nothing (valid) was kept. */
export function takeGithubReturn(teamId: string): string | null {
  let raw: string | null = null;
  try {
    raw = sessionStorage.getItem(KEY);
    sessionStorage.removeItem(KEY);
  } catch {
    return null;
  }
  if (!raw) return null;
  let saved: Partial<Saved>;
  try {
    saved = JSON.parse(raw) as Partial<Saved>;
  } catch {
    return null;
  }
  if (saved.teamId !== teamId) return null;
  if (typeof saved.at !== "number" || Date.now() - saved.at > MAX_AGE_MS) return null;
  // Checked again: sessionStorage is the page's, but not ours to trust.
  return sameOriginPath(saved.path);
}
