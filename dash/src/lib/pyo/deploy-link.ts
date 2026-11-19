// "Deploy with Pyo" links: /new?repo=<https Git URL> or ?image=<image>.
// They come from anywhere, so nothing in them is trusted: each is checked
// here before the deploy page uses it, and it only fills the form in.
// Deploying still takes a click.
//
// A repository is on github.com, gitlab.com or the host of one of the
// team's GitLab connections. A tree link picks a branch and a folder:
// GitHub .../tree/<branch>/<folder>, GitLab .../-/tree/<branch>/<folder>.
// The first part after tree is the branch, so a branch with a slash in its
// name can't be picked this way.

export type DeployLink =
  | {
      kind: "git";
      provider: "github" | "gitlab";
      /** https://host/owner/repo: no .git, no tree. */
      repo: string;
      /** "" for the repository's default branch. */
      branch: string;
      /** The folder to build, "" for the repository's root. */
      path: string;
    }
  | { kind: "image"; image: string }
  | { kind: "invalid"; message: string; title?: string };

export type DeployLinkParams = { repo?: string; image?: string };

type Provider = "github" | "gitlab";

const REPO_MESSAGE = "The repository should look like https://github.com/owner/repo.";

/** GitHub: a user or organization, then a repository. */
const GITHUB_OWNER = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$/;
const GITHUB_REPO = /^[A-Za-z0-9._-]{1,100}$/;
/** GitLab: groups and subgroups, then a project. */
const GITLAB_PART = /^[A-Za-z0-9_][A-Za-z0-9_.-]{0,254}$/;
/** A branch name without slashes, by Git's rules. */
const BRANCH =
  /^(?!-)(?!.*\.\.)(?!.*@\{)(?!.*\.lock$)(?!.*\.$)[^\s~^:?*[\\\u0000-\u001f\u007f]{1,255}$/;
/** One folder name in a repository. */
const FOLDER = /^[^/\\\u0000-\u001f\u007f]{1,255}$/;

// The image reference grammar Docker and registries use: an optional
// registry host, lowercase path components, then a tag and/or a digest.
const DOMAIN_PART = "(?:[a-zA-Z0-9]|[a-zA-Z0-9][a-zA-Z0-9-]*[a-zA-Z0-9])";
const DOMAIN = `(?:${DOMAIN_PART}(?:\\.${DOMAIN_PART})*|\\[[0-9a-fA-F:]+\\])(?::[0-9]{1,5})?`;
const PATH_PART = "[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*";
const TAG = "[\\w][\\w.-]{0,127}";
const DIGEST = "[A-Za-z][A-Za-z0-9]*(?:[-_+.][A-Za-z][A-Za-z0-9]*)*:[0-9a-fA-F]{32,}";
const IMAGE = new RegExp(
  `^(?:${DOMAIN}/)?${PATH_PART}(?:/${PATH_PART})*(?::${TAG})?(?:@${DIGEST})?$`,
);

const dots = (s: string) => s === "." || s === "..";

/** A URL path's parts, decoded. `null` when one can't be used. */
function pathParts(pathname: string): string[] | null {
  const parts: string[] = [];
  for (const p of pathname.replace(/\/+$/, "").split("/").slice(1)) {
    let part: string;
    try {
      part = decodeURIComponent(p);
    } catch {
      return null;
    }
    if (!FOLDER.test(part) || dots(part)) return null;
    parts.push(part);
  }
  return parts;
}

/** The repository's own parts, then the branch and folder after tree. */
function split(provider: Provider, parts: string[]) {
  // GitHub: owner/repo/tree/<branch>/<folder>.
  // GitLab: group/…/project/-/tree/<branch>/<folder>.
  const at = provider === "github" ? 2 : parts.indexOf("-");
  const repo = at < 0 ? parts : parts.slice(0, at);
  const rest = at < 0 ? [] : parts.slice(provider === "github" ? at : at + 1);
  if (rest.length && (rest[0] !== "tree" || rest.length < 2)) return null;
  const [, branch = "", ...path] = rest;
  if (branch && !BRANCH.test(branch)) return null;
  if (repo.length) repo[repo.length - 1] = repo[repo.length - 1].replace(/\.git$/i, "");
  return { repo, branch, path };
}

function repoOk(provider: Provider, repo: string[]) {
  if (provider === "github")
    return (
      repo.length === 2 &&
      GITHUB_OWNER.test(repo[0]) &&
      GITHUB_REPO.test(repo[1]) &&
      !dots(repo[1])
    );
  return (
    repo.length >= 2 &&
    repo.length <= 20 &&
    repo.every((p) => GITLAB_PART.test(p) && !dots(p)) &&
    !repo[repo.length - 1].toLowerCase().endsWith(".atom")
  );
}

/** A GitLab connection's address as an origin and a path to strip. */
function gitlabBase(raw: string) {
  try {
    const u = new URL(raw);
    return { origin: u.origin, path: u.pathname.replace(/\/+$/, "") };
  } catch {
    return null;
  }
}

/**
 * A repository's https:// URL, checked. `gitlabs` are the addresses of the
 * team's GitLab connections: a repository on one of their hosts works too.
 */
export function gitRepo(raw: string, gitlabs: string[] = []): DeployLink {
  const value = raw.trim();
  const bad = { kind: "invalid", message: REPO_MESSAGE } as const;
  if (value.length > 2048 || !/^https:\/\//i.test(value)) return bad;
  // URL would resolve . and .. away: a link has no reason to hold them.
  if (/\/(?:\.|%2e){1,2}(?:[/?#]|$)/i.test(value)) return bad;
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return bad;
  }
  if (url.protocol !== "https:" || url.username || url.password) return bad;

  let provider: Provider | null =
    url.host === "github.com" ? "github" : url.host === "gitlab.com" ? "gitlab" : null;
  // The team's own GitLab, maybe under a path (https://example.com/gitlab).
  let prefix = "";
  if (!provider)
    for (const b of gitlabs.map(gitlabBase)) {
      if (!b || b.origin !== url.origin) continue;
      if (b.path && !url.pathname.startsWith(`${b.path}/`)) continue;
      provider = "gitlab";
      prefix = b.path;
      break;
    }
  if (!provider)
    return {
      kind: "invalid",
      title: "This Git host isn't supported yet",
      message: "Pyo deploys from GitHub and GitLab.",
    };

  const parts = pathParts(url.pathname.slice(prefix.length));
  const found = parts && split(provider, parts);
  if (!found || !repoOk(provider, found.repo)) return bad;
  return {
    kind: "git",
    provider,
    repo: `${url.origin}${prefix}/${found.repo.map(encodeURIComponent).join("/")}`,
    branch: found.branch,
    path: found.path.join("/"),
  };
}

export function imageReference(raw: string): string | null {
  if (raw.length > 512) return null;
  const name = raw.split("@")[0].replace(/:[\w][\w.-]*$/, "");
  if (name.length > 255) return null;
  return IMAGE.test(raw) ? raw : null;
}

/**
 * What a link asks to deploy: one repo or one image. `gitlabs` are the
 * addresses of the team's GitLab connections.
 */
export function parseDeployLink(p: DeployLinkParams, gitlabs: string[] = []): DeployLink {
  if (p.repo !== undefined && p.image !== undefined)
    return { kind: "invalid", message: "Use one repo or one image." };
  if (p.repo !== undefined) return gitRepo(p.repo, gitlabs);
  if (p.image !== undefined) {
    const image = imageReference(p.image.trim());
    return image
      ? { kind: "image", image }
      : { kind: "invalid", message: "The image should look like nginx:1.27." };
  }
  return { kind: "invalid", message: "The link should have a repo or an image." };
}
