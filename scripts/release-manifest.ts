// Writes a release's release.json: what Pyo needs to update itself from the
// dashboard (crates/pyo-api/src/updates.rs, crates/pyo-agent/src/upgrade.rs).
// The release workflow runs it after pushing the images and attaches the
// output to the GitHub release:
//
//   bun run scripts/release-manifest.ts --version 0.1.0 \
//     --images ghcr.io/upsetdev \
//     --pyo sha256:… --agent sha256:… --builder sha256:… --flows sha256:… --waf sha256:… --geoip sha256:… \
//     --db-controller sha256:… --kv sha256:… --static sha256:… --mysql-operator sha256:… --mysql-backup sha256:… \
//     --postgres-16 sha256:… --postgres-17 sha256:… --postgres-18 sha256:… \
//     --agent-access "$(pyo-agent access-hash)" \
//     --database-pins "$(pyo-agent database-pins)" \
//     --sandbox-x86_64 <sha256> --sandbox-aarch64 <sha256> \
//     --sandbox-gvisor-x86_64 <sha256> --sandbox-gvisor-aarch64 <sha256> \
//     --verifier verifier --scripts release-scripts > release.json
//
// Images are pinned by digest, each at the release's version as its tag,
// under their flat names (deploy/pyo-images.json): --images
// ghcr.io/upsetdev gives ghcr.io/upsetdev/pyo-agent:0.1.0@sha256:….
// Two have a tag of their own, from scripts/image-tags.sh: geoip (its
// database's date) and waf (a hash of its sources). Their tag, and so
// their reference, changes only when the image does, so the pods that run
// them don't restart with every release.
// deploy/k8s/{platform,pyo}.yaml become JSON objects. The sandbox templates' images (sandboxBase, sandboxDesktop) are
// the ones Pyo ships with, from crates/pyo-api/assets/sandbox-images/. Notices for this release come from
// deploy/release-notices.json when its "version" is this one; the k3s a
// fresh install gets, from deploy/k3s.env. `agentAccess` is the hash of the
// roles and admission policy the release's agent is installed with
// (`pyo-agent access-hash`): installations on others tell their admin,
// before updating, to run each cluster's connect command afterwards. `sandbox` lists the SHA-256 of
// each architecture's sandbox server files (pyo-sandbox-linux-<arch>.tar.gz),
// and `sandboxGvisor` those of runsc (pyo-sandbox-gvisor-linux-<arch>.tar.gz)
// for servers without KVM: deploy/sandbox.sh checks them against it once
// release.json's signature checks out.
//
// `scripts` lists the SHA-256 of the release's install scripts, which
// --scripts <dir> gets: install.sh, cli.sh, connect.sh, join.sh and
// sandbox.sh, as the release publishes them. connect.sh, join.sh and
// sandbox.sh get this version on their pyo:release-version line, so they
// know their release (run_release). pyo.dev serves these files only when
// they match this list, at /<script> for the latest release and at
// /v/<version>/<script> for each one.
//
// `mirrors` lists the third-party images the release runs, from Docker
// Hub (deploy/mirrors.txt), by path and tag: "envoyproxy/envoy:distroless-v1.39.5" →
// "docker.io/envoyproxy/envoy:distroless-v1.39.5@sha256:…". The name is
// older releases': they listed Pyo's copies of these images there.
//
// `verifier` lists the SHA-256 of the release's verifier for each
// architecture (pyo-verify-linux-<arch>), read from the files in
// --verifier <dir>. A server's verifier checks release.json's signature,
// then the next verifier against these (crates/pyo-verify). connect.sh
// gets this version and the same sums on its pyo:verify-pin lines, so it
// installs only this release's verifier, whoever served the script.

import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const args = new Map<string, string>();
for (let i = 2; i < process.argv.length; i += 2) {
  args.set(process.argv[i].replace(/^--/, ""), process.argv[i + 1] ?? "");
}
const fail = (msg: string): never => {
  console.error(`release-manifest: ${msg}`);
  process.exit(1);
};
const need = (k: string) => args.get(k) || fail(`--${k} is required`);

const version = need("version").replace(/^v/, "");
const agentAccess = need("agent-access");
if (!/^[0-9a-f]{16}$/.test(agentAccess)) fail(`--agent-access isn't 16 hex digits: ${agentAccess}`);
// The database images the release's code pins itself (`pyo-agent
// database-pins`, as JSON: MySQL's and Valkey's with its modules). With the
// images below they're what the release's databases run, so an installation
// knows before it updates whether its databases restart.
const databasePins: Record<string, string> = (() => {
  try {
    return JSON.parse(need("database-pins"));
  } catch {
    return fail("--database-pins isn't JSON");
  }
})();
for (const [engine, image] of Object.entries(databasePins)) {
  if (typeof image !== "string" || !/^[^\s@]+@sha256:[0-9a-f]{64}$/.test(image)) {
    fail(`--database-pins: the ${engine} image isn't pinned by digest: ${image}`);
  }
}
if (!Object.keys(databasePins).length) fail("--database-pins names no image");
const images = need("images").replace(/\/$/, "");
const cargo = readFileSync("Cargo.toml", "utf8").match(/^version = "(.+)"$/m)?.[1];
if (cargo !== version && !args.has("allow-version-mismatch")) {
  fail(`--version ${version} doesn't match Cargo.toml (${cargo})`);
}
// The images with a tag of their own, as scripts/image-tags.sh gives them.
// The release workflow publishes them under the same tags.
const ownTags: Record<string, string> = (() => {
  const run = Bun.spawnSync(["bash", "scripts/image-tags.sh"]);
  if (run.exitCode !== 0) fail(`scripts/image-tags.sh failed: ${run.stderr.toString().trim()}`);
  const lines = run.stdout.toString().trim().split("\n");
  return Object.fromEntries(lines.map((l) => l.split(" ")));
})();
for (const name of ["geoip", "waf"]) {
  if (!ownTags[name]) fail(`scripts/image-tags.sh gave no tag for ${name}`);
}
// Pyo's own images (deploy/pyo-images.json): each one's flat name, its
// name in the manifests (`__IMAGES__/<placeholder>:__TAG__`, also the
// --<placeholder> flag) and its key in release.json. Postgres is one
// repository with a tag per version, below. The sandbox templates' come
// from elsewhere, below too.
type OwnImage = { name: string; placeholder?: string; keys: string[]; builtBy?: string };
const own: OwnImage[] = JSON.parse(readFileSync("deploy/pyo-images.json", "utf8")).images;
const built = own.filter((i) => i.placeholder && !i.builtBy && i.placeholder !== "postgres");
const repoOf = (placeholder: string) =>
  own.find((i) => i.placeholder === placeholder)?.name ?? fail(`deploy/pyo-images.json has no ${placeholder}`);
const ref = (placeholder: string) => {
  const digest = need(placeholder);
  if (!/^sha256:[0-9a-f]{64}$/.test(digest)) fail(`--${placeholder} isn't a sha256 digest: ${digest}`);
  return `${images}/${repoOf(placeholder)}:${ownTags[placeholder] ?? version}@${digest}`;
};
const byName: Record<string, string> = Object.fromEntries(built.map((i) => [i.placeholder!, ref(i.placeholder!)]));
const pinned: Record<string, string> = Object.fromEntries(built.map((i) => [i.keys[0], byName[i.placeholder!]]));
// Pyo Sandbox's built-in templates `base` and `desktop` come from
// upsetdev/pyo-sandbox-images, whose signed images.json Pyo reads itself
// (crates/pyo-api/src/sandbox/images.rs). release.json still names the
// ones this release ships with, as Pyo 0.4 reads them there. The manifests
// leave PYO_SANDBOX_BASE_REF and PYO_SANDBOX_DESKTOP_REF empty: they'd win
// over images.json.
const sandboxImages = JSON.parse(readFileSync("crates/pyo-api/assets/sandbox-images/images.json", "utf8"));
const sandboxRef = (name: string) => {
  const r: unknown = sandboxImages.images?.[name]?.ref;
  if (typeof r !== "string" || !/^[^\s@]+@sha256:[0-9a-f]{64}$/.test(r)) {
    fail(`crates/pyo-api/assets/sandbox-images/images.json has no pinned ${name} image`);
  }
  return r as string;
};
pinned.sandboxBase = sandboxRef("base");
pinned.sandboxDesktop = sandboxRef("desktop");
const leftEmpty = ["__SANDBOX_BASE_REF__", "__SANDBOX_DESKTOP_REF__"];

// Pyo's Postgres: one repository, a tag per version that starts with it
// (postgres:18-<version>; CloudNativePG reads the version from the tag).
const postgres: Record<string, string> = {};
for (const major of ["16", "17", "18"]) {
  const digest = need(`postgres-${major}`);
  if (!/^sha256:[0-9a-f]{64}$/.test(digest)) fail(`--postgres-${major} isn't a sha256 digest: ${digest}`);
  postgres[`__IMAGES__/postgres:${major}-__TAG__`] = `${images}/${repoOf("postgres")}:${major}-${version}@${digest}`;
  pinned[`postgres${major}`] = postgres[`__IMAGES__/postgres:${major}-__TAG__`];
}

// The third-party images (deploy/mirrors.txt), keyed by their path on
// Docker Hub without "library/".
const mirrors: Record<string, string> = {};
for (const line of readFileSync("deploy/mirrors.txt", "utf8").split("\n")) {
  const src = line.trim();
  if (!src || src.startsWith("#")) continue;
  const m = src.match(/^docker\.io\/([a-z0-9._/-]+:[A-Za-z0-9._-]+)@sha256:[0-9a-f]{64}$/);
  if (!m) fail(`deploy/mirrors.txt: not docker.io/<path>:<tag>@sha256:<digest>: ${src}`);
  const path = m![1].replace(/^library\//, "");
  if (mirrors[path]) fail(`deploy/mirrors.txt lists ${path} twice`);
  mirrors[path] = src;
}
if (!Object.keys(mirrors).length) fail("deploy/mirrors.txt lists no image");

function manifests(file: string) {
  let text = readFileSync(file, "utf8");
  for (const [name, image] of Object.entries(byName)) {
    text = text.replaceAll(`__IMAGES__/${name}:__TAG__`, image);
  }
  for (const [placeholder, image] of Object.entries(postgres)) {
    text = text.replaceAll(placeholder, image);
  }
  for (const placeholder of leftEmpty) {
    text = text.replaceAll(placeholder, "");
  }
  for (const left of ["__IMAGES__", "__TAG__", "__SANDBOX_"]) {
    if (text.includes(left)) fail(`${file} still has ${left} after pinning Pyo's images`);
  }
  const docs = Bun.YAML.parse(text);
  return (Array.isArray(docs) ? docs : [docs]).filter((d) => d && typeof d === "object");
}

type Notices = {
  version?: string;
  minFrom?: string | null;
  manualOnly?: boolean;
  beforeYouUpdate?: string[];
};
const notices: Notices = JSON.parse(readFileSync("deploy/release-notices.json", "utf8"));
const forThis = notices.version?.replace(/^v/, "") === version;
if (!forThis && (notices.beforeYouUpdate?.length || notices.manualOnly || notices.minFrom)) {
  console.error(
    `release-manifest: deploy/release-notices.json is for ${notices.version}, not ${version}; leaving its notices out`,
  );
}

// The k3s install.sh installs (deploy/k3s.env): its version, and the
// SHA-256 of k3s's installer at that tag and of its binaries.
const k3sEnv = Object.fromEntries(
  readFileSync("deploy/k3s.env", "utf8")
    .split("\n")
    .map((l) => l.match(/^(PYO_K3S_[A-Z0-9_]+)=(\S+)$/))
    .filter((m) => m !== null)
    .map((m) => [m[1], m[2]]),
);
const k3sField = (key: string, pattern: RegExp) => {
  const value = k3sEnv[key] ?? "";
  if (!pattern.test(value)) fail(`deploy/k3s.env: ${key} is missing or malformed (${value})`);
  return value;
};
const sha256 = /^[0-9a-f]{64}$/;
const k3s = {
  version: k3sField("PYO_K3S_VERSION", /^v\d+\.\d+\.\d+\+k3s\d+$/),
  installerSha256: k3sField("PYO_K3S_INSTALLER_SHA256", sha256),
  amd64Sha256: k3sField("PYO_K3S_SHA256_AMD64", sha256),
  arm64Sha256: k3sField("PYO_K3S_SHA256_ARM64", sha256),
};

// The sandbox server files' SHA-256, by architecture (sandbox.sh's ARCH).
const sums = (flag: string) =>
  Object.fromEntries(
    ["x86_64", "aarch64"].map((arch) => {
      const sum = need(`${flag}-${arch}`).toLowerCase();
      if (!sha256.test(sum)) fail(`--${flag}-${arch} isn't a sha256: ${sum}`);
      return [arch, sum];
    }),
  );
const sandbox = sums("sandbox");
const sandboxGvisor = sums("sandbox-gvisor");

// The release's verifier for each architecture, from --verifier <dir>: the
// very files the release uploads. Each has to be a Linux program for its
// architecture, no larger than a verifier downloads (pyo-verify's
// MAX_BINARY).
const VERIFIER_MAX = 64 * 1024 * 1024;
const ELF_MACHINE: Record<string, number> = { x86_64: 62, aarch64: 183 };
const verifierDir = need("verifier");
const verifier: Record<string, string> = {};
const verifierSum: Record<string, string> = {};
for (const [arch, machine] of Object.entries(ELF_MACHINE)) {
  const file = join(verifierDir, `pyo-verify-linux-${arch}`);
  if (!existsSync(file)) fail(`--verifier has no pyo-verify-linux-${arch}`);
  const bytes = readFileSync(file);
  const elf = bytes.length > 64 && bytes.readUInt32BE(0) === 0x7f454c46 && bytes[4] === 2 && bytes[5] === 1;
  if (!elf || bytes.readUInt16LE(18) !== machine) fail(`${file} isn't a Linux program for ${arch}`);
  if (bytes.length > VERIFIER_MAX) fail(`${file} is larger than a verifier downloads (${bytes.length} bytes)`);
  verifierSum[arch] = createHash("sha256").update(bytes).digest("hex");
  verifier[`linux-${arch}`] = verifierSum[arch];
}
// connect.sh's pyo:verify-pin lines, empty in the repo, and what the
// release writes on each.
const VERIFY_PINS: [string, string, string][] = [
  ["release", "PYO_VERIFY_RELEASE", version],
  ["x86_64", "PYO_VERIFY_SHA256_X86_64", verifierSum.x86_64],
  ["aarch64", "PYO_VERIFY_SHA256_AARCH64", verifierSum.aarch64],
];
const verifyPin = (name: string, variable: string, value: string) =>
  `\n${variable}="${value}" # pyo:verify-pin ${name}\n`;

// The install scripts as the release publishes them, written to --scripts.
const RELEASE_LINE = '\n  local release="" # pyo:release-version\n';
const SCRIPTS: [string, string, boolean][] = [
  ["install.sh", "deploy/install.sh", false],
  ["cli.sh", "deploy/cli.sh", false],
  ["connect.sh", "crates/pyo-api/assets/connect.sh", true],
  ["join.sh", "crates/pyo-api/assets/join.sh", true],
  ["sandbox.sh", "deploy/sandbox.sh", true],
];
const scriptsDir = need("scripts");
mkdirSync(scriptsDir, { recursive: true });
const scripts: Record<string, string> = {};
for (const [name, source, stamped] of SCRIPTS) {
  let text = readFileSync(source, "utf8");
  if (stamped) {
    if (text.split(RELEASE_LINE).length !== 2) fail(`${source} needs exactly one pyo:release-version line`);
    text = text.replace(RELEASE_LINE, `\n  local release="${version}" # pyo:release-version\n`);
  }
  if (name === "connect.sh") {
    for (const [pin, variable, value] of VERIFY_PINS) {
      const empty = verifyPin(pin, variable, "");
      if (text.split(empty).length !== 2) fail(`${source} needs exactly one pyo:verify-pin ${pin} line`);
      text = text.replace(empty, verifyPin(pin, variable, value));
    }
  } else if (text.includes("pyo:verify-pin")) {
    fail(`${source} has a pyo:verify-pin line, which only connect.sh gets`);
  }
  writeFileSync(join(scriptsDir, name), text);
  scripts[name] = createHash("sha256").update(text).digest("hex");
}

const release = {
  version,
  images: pinned,
  mirrors,
  k3s,
  manifests: {
    platform: manifests("deploy/k8s/platform.yaml"),
    pyo: manifests("deploy/k8s/pyo.yaml"),
  },
  migrations: readdirSync("crates/pyo-api/migrations")
    .filter((f) => /^\d+_.+\.sql$/.test(f))
    .sort(),
  minFrom: forThis ? (notices.minFrom ?? null) : null,
  manualOnly: forThis ? !!notices.manualOnly : false,
  beforeYouUpdate: forThis ? (notices.beforeYouUpdate ?? []) : [],
  agentAccess,
  databasePins,
  sandbox,
  sandboxGvisor,
  verifier,
  scripts,
};
// Pretty-printed, two spaces: deploy/install.sh reads it with sed.
console.log(JSON.stringify(release, null, 2));
