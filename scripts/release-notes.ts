// Writes a release's notes: the changes since the previous release, from
// the Conventional Commits in between. The release workflow runs it and
// uses the output as the GitHub release's text:
//
//   bun run scripts/release-notes.ts --to v0.2.3 > notes.md
//   bun run scripts/release-notes.ts --from v0.2.1 --to v0.2.2
//
// --from defaults to the highest version tag below --to. The changes are
// the commits reachable from --to and from no earlier version tag (so a
// hotfix released on its own line isn't listed again), merge commits left
// out but the commits they bring in kept. A commit whose subject was
// already released, or appears twice (a cherry-pick), is listed once.
//
// Sections, in this order and only when they have something:
// Features (feat), Bug Fixes (fix), Security Fixes, Performance (perf),
// Docs (docs) and Other (refactor, build, revert, and subjects that aren't
// Conventional Commits). chore, ci, test and style are left out.
//
// A commit is a security fix when its scope is `security`
// (`fix(security): …`) or its message has a `Security:` footer line
// (`Security: GHSA-xxxx-xxxx-xxxx`, or a few words). A GHSA id in the
// footer links the advisory. See RELEASING.md.
//
// A commit whose author's GitHub login isn't one of the maintainers gets
// "(Thanks @login)". Logins come from `gh api` (GH_TOKEN in CI).
// --maintainers a,b replaces the list below. --offline skips the lookups.
//
// The notices in deploy/release-notices.json at --to (when its "version"
// is this release's) go first, under "Before you update".

import { execFileSync } from "node:child_process";

export const REPO = "upsetdev/pyo";
export const MAINTAINERS = ["fransallen"];

export type Commit = { sha: string; subject: string; body: string };

export type Section = "feat" | "fix" | "security" | "perf" | "docs" | "other";

export const SECTIONS: [Section, string][] = [
  ["feat", "🚀 Features"],
  ["fix", "🐞 Bug Fixes"],
  ["security", "⬆️ Security Fixes"],
  ["perf", "⚡ Performance"],
  ["docs", "📝 Docs"],
  ["other", "🧰 Other"],
];

const HEADER = /^(\w+)(?:\(([^)]*)\))?(!)?:\s*(.+)$/;
const SKIP = new Set(["chore", "ci", "test", "style"]);
const OTHER = new Set(["refactor", "build", "revert"]);
const GHSA = /\bGHSA(?:-[23456789cfghjmpqrvwx]{4}){3}\b/i;

export const securityFooter = (body: string) => body.match(/^Security:\s*(.*)$/im)?.[1].trim();

// Which section a commit goes in, or null to leave it out.
export function classify(c: Commit): Section | null {
  const m = c.subject.match(HEADER);
  if (!m) return "other";
  const [, type, scope] = m;
  const t = type.toLowerCase();
  if (scope?.toLowerCase() === "security" || securityFooter(c.body) !== undefined) return "security";
  if (t === "feat" || t === "fix" || t === "perf" || t === "docs") return t;
  if (OTHER.has(t)) return "other";
  if (SKIP.has(t)) return null;
  return "other";
}

// The commit's link: its pull request when the subject ends with (#123),
// the advisory for a security fix with a GHSA id, else the commit.
export function bullet(c: Commit, login?: string | null, maintainers = MAINTAINERS): string {
  let subject = c.subject.trim();
  let link = `[\`${c.sha.slice(0, 7)}\`](https://github.com/${REPO}/commit/${c.sha})`;
  const pr = subject.match(/\s*\(#(\d+)\)$/);
  const ghsa = securityFooter(c.body)?.match(GHSA)?.[0];
  if (ghsa) {
    link = `[${ghsa}](https://github.com/${REPO}/security/advisories/${ghsa})`;
  } else if (pr) {
    link = `[#${pr[1]}](https://github.com/${REPO}/pull/${pr[1]})`;
  }
  if (pr) subject = subject.slice(0, pr.index).trim();
  const lower = maintainers.map((m) => m.toLowerCase());
  const thanks = login && !lower.includes(login.toLowerCase()) ? ` (Thanks @${login})` : "";
  return `- ${link} ${subject}${thanks}`;
}

// The commits to list: one per subject, none already released.
export function dedupe(commits: Commit[], released: Iterable<string>): Commit[] {
  const seen = new Set(released);
  return commits.filter((c) => {
    if (seen.has(c.subject)) return false;
    seen.add(c.subject);
    return true;
  });
}

export type Notices = { version?: string; minFrom?: string | null; manualOnly?: boolean; beforeYouUpdate?: string[] };

export function render(opts: {
  from: string | null;
  to: string;
  commits: Commit[];
  logins?: Map<string, string | null>;
  maintainers?: string[];
  notices?: Notices | null;
}): string {
  const out: string[] = [];
  const n = opts.notices;
  const before: string[] = [...(n?.beforeYouUpdate ?? [])];
  if (n?.minFrom) before.push(`Update to ${n.minFrom} first if you run an earlier version.`);
  if (n?.manualOnly) before.push("Automatic updates skip this release. Apply it from Settings › Updates.");
  if (before.length) out.push("### ⚠️ Before you update", "", ...before.map((b) => `- ${b}`), "");

  if (!opts.from) out.push("Initial release.", "");
  const groups = new Map<Section, string[]>();
  for (const c of opts.commits) {
    const s = classify(c);
    if (!s) continue;
    const login = opts.logins?.get(c.sha) ?? null;
    groups.set(s, [...(groups.get(s) ?? []), bullet(c, login, opts.maintainers)]);
  }
  for (const [s, title] of SECTIONS) {
    const lines = groups.get(s);
    if (lines?.length) out.push(`### ${title}`, "", ...lines, "");
  }
  if (opts.from) {
    out.push(`**Full Changelog**: https://github.com/${REPO}/compare/${opts.from}...${opts.to}`);
  } else {
    out.push(`**Full Changelog**: https://github.com/${REPO}/commits/${opts.to}`);
  }
  return out.join("\n") + "\n";
}

// --- git and GitHub ---

const run = (cmd: string, args: string[]) => execFileSync(cmd, args, { encoding: "utf8", maxBuffer: 1 << 28 });
const git = (...args: string[]) => run("git", args);

const semver = (t: string) => t.replace(/^v/, "").split(".").map(Number);
export function compareTags(a: string, b: string) {
  const [x, y] = [semver(a), semver(b)];
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] - y[i];
  return 0;
}

function versionTags(): string[] {
  return git("tag", "-l", "v*")
    .split("\n")
    .filter((t) => /^v\d+\.\d+\.\d+$/.test(t))
    .sort(compareTags);
}

function readCommits(range: string[]): Commit[] {
  const raw = git("log", "--no-merges", "--reverse", "--topo-order", "--format=%H%x1f%s%x1f%b%x1e", ...range);
  return raw
    .split("\x1e")
    .map((r) => r.replace(/^\n/, ""))
    .filter(Boolean)
    .map((r) => {
      const [sha, subject, body] = r.split("\x1f");
      return { sha, subject, body: body ?? "" };
    });
}

function login(sha: string): string | null {
  try {
    const l = run("gh", ["api", `repos/${REPO}/commits/${sha}`, "--jq", ".author.login // empty"]).trim();
    return l || null;
  } catch {
    return null;
  }
}

function notices(tag: string): Notices | null {
  try {
    const n: Notices = JSON.parse(git("show", `${tag}:deploy/release-notices.json`));
    return n.version === tag.replace(/^v/, "") ? n : null;
  } catch {
    return null;
  }
}

function main() {
  const args = new Map<string, string>();
  const argv = process.argv.slice(2);
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i].replace(/^--/, "");
    if (k === "offline") args.set(k, "1");
    else args.set(k, argv[++i] ?? "");
  }
  const to = args.get("to");
  if (!to) {
    console.error("release-notes: --to is required");
    process.exit(1);
  }
  const earlier = versionTags().filter((t) => compareTags(t, to) < 0);
  const from = args.get("from") ?? earlier.at(-1) ?? null;
  const commits = readCommits([to, ...earlier.map((t) => `^${t}`)]);
  const released = earlier.length ? readCommits(earlier).map((c) => c.subject) : [];
  const listed = dedupe(commits, released);
  const maintainers = args.get("maintainers")?.split(",").map((m) => m.trim()).filter(Boolean) ?? MAINTAINERS;
  const logins = new Map<string, string | null>();
  if (!args.has("offline")) {
    for (const c of listed) if (classify(c)) logins.set(c.sha, login(c.sha));
  }
  process.stdout.write(render({ from, to, commits: listed, logins, maintainers, notices: notices(to) }));
}

if (import.meta.main) main();
