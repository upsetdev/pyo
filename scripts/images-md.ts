// Writes IMAGES.md, the list of every image Pyo uses, from the repo's own
// lists: deploy/pyo-images.json (Pyo's images) and deploy/mirrors.txt (the
// third-party ones, each under the comment that says what it's for).
//
//   bun run scripts/images-md.ts           # write IMAGES.md
//   bun run scripts/images-md.ts --check   # fail when IMAGES.md isn't what this writes (CI)
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "..");
const FILE = join(root, "IMAGES.md");

type Own = {
  name: string;
  placeholder?: string;
  keys: string[];
  about: string;
  builtBy?: string;
  publishedAs?: string;
};
type List = { registries: string[]; old: { prefix: string; until: string }; images: Own[] };

export type ThirdParty = { source: string; version: string; digest: string; usedFor: string };

/** deploy/mirrors.txt's images, each with the comment above its group. */
export function thirdParty(text: string): ThirdParty[] {
  const out: ThirdParty[] = [];
  let comment: string[] = [];
  let usedFor = "";
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) {
      comment = [];
      continue;
    }
    if (line.startsWith("#")) {
      comment.push(line.replace(/^#\s?/, ""));
      continue;
    }
    if (comment.length) {
      // "Builds (platform.yaml, pyo-agent builder.rs)." → "Builds"
      usedFor = comment.join(" ").replace(/\s*\([^)]*\)/g, "").replace(/\.$/, "").trim();
      comment = [];
    }
    const m = line.match(/^(docker\.io\/[a-z0-9._/-]+):([A-Za-z0-9._-]+)@(sha256:[0-9a-f]{64})$/);
    if (!m) throw new Error(`deploy/mirrors.txt: not docker.io/<path>:<tag>@sha256:<digest>: ${line}`);
    out.push({ source: m[1], version: m[2], digest: m[3], usedFor });
  }
  return out;
}

/** An image's page on Docker Hub: official images under /_/. */
function hubPage(source: string): string {
  const path = source.replace(/^docker\.io\//, "");
  return path.startsWith("library/") ? `https://hub.docker.com/_/${path.slice(8)}` : `https://hub.docker.com/r/${path}`;
}

const ghcrPage = (name: string) =>
  `https://github.com/orgs/upsetdev/packages/container/package/${encodeURIComponent(name)}`;

export function render(list: List, mirrors: string): string {
  const own = list.images;
  const [ghcr, hub] = list.registries;
  const lines: string[] = [
    "# Images",
    "",
    "Every image Pyo runs. `scripts/images-md.ts` writes this file from",
    "`deploy/pyo-images.json` and `deploy/mirrors.txt`. Don't edit it by hand:",
    "change those and run `bun run scripts/images-md.ts`. CI fails when it's",
    "out of date.",
    "",
    "## Pyo's own",
    "",
    `Each one has the same name on \`${ghcr}\` and \`${hub}\`. Pyo pulls from`,
    "ghcr.io. A release pins each image by digest in its `release.json`.",
    "",
    "| Image | What it is | ghcr.io | Docker Hub |",
    "|---|---|---|---|",
  ];
  for (const i of own.filter((i) => !i.builtBy)) {
    lines.push(
      `| \`upsetdev/${i.name}\` | ${i.about} | [ghcr.io](${ghcrPage(i.name)}) | [Docker Hub](https://hub.docker.com/r/upsetdev/${i.name}) |`,
    );
  }
  const elsewhere = own.filter((i) => i.builtBy);
  if (elsewhere.length) {
    lines.push(
      "",
      `The sandbox templates' images are built in [${elsewhere[0].builtBy}](https://github.com/${elsewhere[0].builtBy}).`,
      "They keep their current names until that repository publishes the flat",
      "ones.",
      "",
      "| Image | What it is | Published as | ghcr.io |",
      "|---|---|---|---|",
    );
    for (const i of elsewhere) {
      const now = i.publishedAs!;
      lines.push(
        `| \`upsetdev/${i.name}\` | ${i.about} | \`${now}\` | [ghcr.io](${ghcrPage(now.replace(/^ghcr\.io\/upsetdev\//, ""))}) |`,
      );
    }
  }

  lines.push(
    "",
    "## Third-party images",
    "",
    "Pulled straight from Docker Hub, pinned by tag and digest.",
    "",
    "| Image | Version | Digest | Used for |",
    "|---|---|---|---|",
  );
  for (const t of thirdParty(mirrors)) {
    lines.push(
      `| [\`${t.source}\`](${hubPage(t.source)}) | \`${t.version}\` | \`${t.digest.slice(0, 19)}…\` | ${t.usedFor} |`,
    );
  }

  lines.push(
    "",
    "## Old names",
    "",
    `Up to 0.52.0, Pyo's images were \`${list.old.prefix}/<name>\`. Each release copies its`,
    `images there too until the date below. Releases after it aren't copied. Tags`,
    "published before it stay.",
    "",
    "| Old name | Now | Stops |",
    "|---|---|---|",
  );
  for (const i of own.filter((i) => i.placeholder)) {
    lines.push(`| \`${list.old.prefix}/${i.placeholder}\` | \`${ghcr}/${i.name}\` | ${list.old.until} |`);
  }
  for (const i of elsewhere) {
    lines.push(`| \`${i.publishedAs}\` | \`${ghcr}/${i.name}\` | When ${i.builtBy} moves |`);
  }
  lines.push(
    "| `ghcr.io/upsetdev/pyo-mirror/<path>` | the image on Docker Hub | Stopped with 0.52.1. The packages stay for clusters on 0.52.0 |",
    "",
  );
  return lines.join("\n");
}

if (import.meta.main) {
  const list: List = JSON.parse(readFileSync(join(root, "deploy/pyo-images.json"), "utf8"));
  const text = render(list, readFileSync(join(root, "deploy/mirrors.txt"), "utf8"));
  if (process.argv.includes("--check")) {
    let have = "";
    try {
      have = readFileSync(FILE, "utf8");
    } catch {}
    if (have !== text) {
      console.error(
        "images-md: IMAGES.md is out of date with deploy/pyo-images.json and deploy/mirrors.txt. Run `bun run scripts/images-md.ts` and commit IMAGES.md.",
      );
      process.exit(1);
    }
    console.log("images-md: IMAGES.md is up to date.");
  } else {
    writeFileSync(FILE, text);
    console.log("images-md: wrote IMAGES.md.");
  }
}
