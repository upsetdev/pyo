// Writes a release's release.json: what Pyo needs to update itself from the
// dashboard (crates/pyo-api/src/updates.rs, crates/pyo-agent/src/upgrade.rs).
// The release workflow runs it after pushing the images and attaches the
// output to the GitHub release:
//
//   bun run scripts/release-manifest.ts --version 0.1.0 \
//     --images ghcr.io/upsetdev/pyo \
//     --pyo sha256:… --agent sha256:… --builder sha256:… --flows sha256:… --waf sha256:… --geoip sha256:… \
//     --db-controller sha256:… --kv sha256:… --mysql-operator sha256:… --mysql-backup sha256:… \
//     --postgres-16 sha256:… --postgres-17 sha256:… --postgres-18 sha256:… \
//     --agent-access "$(pyo-agent access-hash)" > release.json
//
// Images are pinned by digest. deploy/k8s/{platform,pyo}.yaml become JSON
// objects; the values only an installation knows (__TAILNET_IP__,
// __REGISTRY_PUSH__, __TAILNET_API__, __TAILNET_NAME__) stay as they are, and the upgrader
// fills them in from what's running. Notices for this release come from
// deploy/release-notices.json when its "version" is this one; the k3s a
// fresh install gets, from deploy/k3s.env. `agentAccess` is the hash of the
// roles the release's agent is installed with (`pyo-agent access-hash`):
// installations on other roles tell their admin, before updating, to run
// each cluster's connect command afterwards.

import { readdirSync, readFileSync } from "node:fs";

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
const images = need("images").replace(/\/$/, "");
const cargo = readFileSync("Cargo.toml", "utf8").match(/^version = "(.+)"$/m)?.[1];
if (cargo !== version && !args.has("allow-version-mismatch")) {
  fail(`--version ${version} doesn't match Cargo.toml (${cargo})`);
}
const ref = (name: string) => {
  const digest = need(name);
  if (!/^sha256:[0-9a-f]{64}$/.test(digest)) fail(`--${name} isn't a sha256 digest: ${digest}`);
  return `${images}/${name}:${version}@${digest}`;
};
// Each image by its name in the manifests (`__IMAGES__/<name>:__TAG__`) and
// its key in release.json.
const names = {
  pyo: "pyo",
  agent: "agent",
  builder: "builder",
  flows: "flows",
  waf: "waf",
  geoip: "geoip",
  "db-controller": "dbController",
  kv: "kv",
  // MySQL's operator and backup image: MOCO with Pyo's patches
  // (deploy/images/moco).
  "mysql-operator": "mysqlOperator",
  "mysql-backup": "mysqlBackup",
} as const;
const byName: Record<string, string> = Object.fromEntries(Object.keys(names).map((n) => [n, ref(n)]));
const pinned: Record<string, string> = Object.fromEntries(Object.entries(names).map(([n, key]) => [key, byName[n]]));
// Pyo's Postgres: one repository, a tag per version that starts with it
// (postgres:18-<version>; CloudNativePG reads the version from the tag).
const postgres: Record<string, string> = {};
for (const major of ["16", "17", "18"]) {
  const digest = need(`postgres-${major}`);
  if (!/^sha256:[0-9a-f]{64}$/.test(digest)) fail(`--postgres-${major} isn't a sha256 digest: ${digest}`);
  postgres[`__IMAGES__/postgres:${major}-__TAG__`] = `${images}/postgres:${major}-${version}@${digest}`;
  pinned[`postgres${major}`] = postgres[`__IMAGES__/postgres:${major}-__TAG__`];
}

function manifests(file: string) {
  let text = readFileSync(file, "utf8");
  for (const [name, image] of Object.entries(byName)) {
    text = text.replaceAll(`__IMAGES__/${name}:__TAG__`, image);
  }
  for (const [placeholder, image] of Object.entries(postgres)) {
    text = text.replaceAll(placeholder, image);
  }
  for (const left of ["__IMAGES__", "__TAG__"]) {
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

const release = {
  version,
  images: pinned,
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
};
// Pretty-printed, two spaces: deploy/install.sh reads it with sed.
console.log(JSON.stringify(release, null, 2));
