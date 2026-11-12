import { rng } from "./format";

// Simulated build detection for Git deploys. A real implementation would
// look for a Dockerfile (else a Containerfile) in the build context and
// otherwise run the buildpacks detect phase. Here the answer is derived from the repository
// URL so it is stable for a given repo.

export type BuildMethod = "dockerfile" | "buildpacks";

export type BuildDetection = {
  method: BuildMethod;
  /** Detected language when building with buildpacks. */
  language?: string;
  /** The file that decided it, e.g. "Dockerfile", "Containerfile" or "package.json". */
  evidence: string;
};

export const BUILDPACK_BUILDER = "paketobuildpacks/builder-jammy-base";

const LANGUAGES: [string, string][] = [
  ["Node.js", "package.json"],
  ["Go", "go.mod"],
  ["Python", "requirements.txt"],
  ["Ruby", "Gemfile"],
  ["Java", "pom.xml"],
  ["PHP", "composer.json"],
];

/** The demo team's own repositories ship a Dockerfile. */
const KNOWN_DOCKERFILE = /acme\/(storefront|storefront-api|geo-api)(\.git)?$/i;

/** Repository names that give the stack away, checked before falling back to the hash. */
const HINTS: [RegExp, number][] = [
  [
    /next|react|vue|svelte|astro|nuxt|node|express|blog|web|site|frontend|ui\b/,
    0,
  ],
  [/golang|\bgo-|-go\b/, 1],
  [/django|flask|fastapi|python|py\b/, 2],
  [/rails|ruby/, 3],
  [/spring|java|kotlin/, 4],
  [/laravel|php|wordpress/, 5],
];

export function detectBuild(repoUrl: string): BuildDetection {
  const url = repoUrl.trim().replace(/\/+$/, "");
  const name =
    url
      .split("/")
      .pop()
      ?.toLowerCase()
      .replace(/\.git$/, "") ?? "";
  const r = rng(url.toLowerCase());
  if (KNOWN_DOCKERFILE.test(url) || r() < 0.3)
    return { method: "dockerfile", evidence: "Dockerfile" };
  const hinted = HINTS.find(([re]) => re.test(name))?.[1];
  const [language, evidence] =
    LANGUAGES[hinted ?? Math.floor(r() * LANGUAGES.length)];
  return { method: "buildpacks", language, evidence };
}

/** What a Git draft will actually build with, after auto-detection. */
export function resolveBuild(git: {
  repoUrl: string;
  builder: "auto" | BuildMethod;
}): BuildDetection & { auto: boolean } {
  const detected = detectBuild(git.repoUrl);
  if (git.builder === "auto") return { ...detected, auto: true };
  if (git.builder === detected.method) return { ...detected, auto: false };
  return git.builder === "dockerfile"
    ? { method: "dockerfile", evidence: "Dockerfile", auto: false }
    : {
        method: "buildpacks",
        language: detected.language,
        evidence: detected.evidence,
        auto: false,
      };
}

/** The file a Dockerfile build used, from what the builder reported. */
export function buildFileOf(detected?: string) {
  return detected === "Containerfile" ? "Containerfile" : "Dockerfile";
}

export function buildLabel(b: { method: BuildMethod; language?: string }) {
  return b.method === "dockerfile"
    ? "Dockerfile"
    : `Buildpacks${b.language ? ` · ${b.language}` : ""}`;
}
