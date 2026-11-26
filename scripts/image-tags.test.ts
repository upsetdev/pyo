import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = new URL("..", import.meta.url).pathname;
const run = (cmd: string[]) => {
  const out = Bun.spawnSync(cmd, { cwd: root });
  return { code: out.exitCode, stdout: out.stdout.toString(), stderr: out.stderr.toString() };
};

// The two images a release doesn't tag with its version: the pods that run
// them restart when the tag changes, so it follows the image alone.
test("geoip is tagged with its database's date, waf with a hash of its sources", () => {
  const all = run(["bash", "scripts/image-tags.sh"]);
  expect(all.code).toBe(0);
  const tags = Object.fromEntries(
    all.stdout
      .trim()
      .split("\n")
      .map((l) => l.split(" ")),
  );
  expect(Object.keys(tags).sort()).toEqual(["geoip", "waf"]);
  expect(tags.geoip).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  expect(tags.waf).toMatch(/^[0-9a-f]{16}$/);
  const dockerfile = readFileSync(join(root, "deploy/images/geoip.Dockerfile"), "utf8");
  expect(dockerfile).toContain(`\nARG GEOIP_DATE=${tags.geoip}\n`);
  expect(run(["bash", "scripts/image-tags.sh", "waf"]).stdout.trim()).toBe(tags.waf);
  // Any other image is tagged with the version.
  expect(run(["bash", "scripts/image-tags.sh", "pyo"]).code).toBe(2);
});

// Both Dockerfiles pin the same database, and every image they start from
// by digest: the WAF's tag is a hash of its Dockerfile, so an unpinned base
// would change the image without changing the tag.
test("the GeoIP databases and the base images are pinned", () => {
  const geoip = readFileSync(join(root, "deploy/images/geoip.Dockerfile"), "utf8");
  const pyo = readFileSync(join(root, "deploy/images/pyo.Dockerfile"), "utf8");
  const waf = readFileSync(join(root, "deploy/images/waf.Dockerfile"), "utf8");
  const arg = (text: string, name: string) => text.match(new RegExp(`^ARG ${name}=(\\S+)$`, "m"))?.[1];
  for (const name of ["GEOIP_DATE", "GEOIP_COMMIT"]) {
    expect(arg(geoip, name)).toBeTruthy();
    expect(arg(pyo, name)).toBe(arg(geoip, name));
  }
  expect(arg(geoip, "GEOIP_COMMIT")).toMatch(/^[0-9a-f]{40}$/);
  expect(arg(geoip, "GEOIP_COUNTRY_SHA256")).toMatch(/^[0-9a-f]{64}$/);
  expect(arg(pyo, "GEOIP_CITY_SHA256")).toMatch(/^[0-9a-f]{64}$/);
  for (const text of [geoip, pyo]) {
    expect(text).not.toContain("node-geolite2-redist/master");
    expect(text).toContain("ADD --checksum=sha256:${GEOIP_");
  }
  for (const text of [geoip, waf]) {
    const from = text.split("\n").filter((l) => l.startsWith("FROM ") && !l.startsWith("FROM scratch"));
    expect(from.length).toBeGreaterThan(0);
    for (const line of from) expect(line).toMatch(/^FROM \S+@sha256:[0-9a-f]{64}( AS \w+)?$/);
  }
});

test("release.json pins waf and geoip under their own tags", () => {
  const dir = mkdtempSync(join(tmpdir(), "pyo-image-tags-"));
  try {
    // Any Linux program for each architecture does as a verifier here.
    for (const [arch, machine] of [
      ["x86_64", 62],
      ["aarch64", 183],
    ] as const) {
      const elf = Buffer.alloc(128);
      elf.write("\x7fELF", "latin1");
      elf[4] = 2;
      elf[5] = 1;
      elf.writeUInt16LE(machine, 18);
      writeFileSync(join(dir, `pyo-verify-linux-${arch}`), elf);
    }
    const version = readFileSync(join(root, "Cargo.toml"), "utf8").match(/^version = "(.+)"$/m)![1];
    const digest = `sha256:${"a".repeat(64)}`;
    const sum = "b".repeat(64);
    const images = ["pyo", "agent", "builder", "flows", "waf", "geoip", "db-controller", "kv", "static"];
    const others = ["mysql-operator", "mysql-backup", "postgres-16", "postgres-17", "postgres-18"];
    const out = run([
      "bun",
      "run",
      "scripts/release-manifest.ts",
      ...["--version", version, "--images", "ghcr.io/upsetdev/pyo"],
      ...[...images, ...others].flatMap((i) => [`--${i}`, digest]),
      ...["--agent-access", "0123456789abcdef"],
      ...["--database-pins", JSON.stringify({ mysql: `ghcr.io/example/mysql:8@sha256:${sum}` })],
      ...["x86_64", "aarch64"].flatMap((a) => [`--sandbox-${a}`, sum, `--sandbox-gvisor-${a}`, sum]),
      ...["--verifier", dir, "--scripts", join(dir, "scripts")],
    ]);
    if (out.code !== 0) throw new Error(out.stderr);
    const release = JSON.parse(out.stdout);
    const tag = (image: string) => run(["bash", "scripts/image-tags.sh", image]).stdout.trim();
    expect(release.images.waf).toBe(`ghcr.io/upsetdev/pyo/waf:${tag("waf")}@${digest}`);
    expect(release.images.geoip).toBe(`ghcr.io/upsetdev/pyo/geoip:${tag("geoip")}@${digest}`);
    expect(release.images.pyo).toBe(`ghcr.io/upsetdev/pyo/pyo:${version}@${digest}`);
    // The manifests hand Pyo and its worker the same two references.
    const manifests = JSON.stringify(release.manifests);
    for (const image of ["waf", "geoip"]) {
      const refs = manifests.match(new RegExp(`ghcr\\.io/upsetdev/pyo/${image}:[^"]+`, "g")) ?? [];
      expect(refs.length).toBeGreaterThan(0);
      expect(new Set(refs)).toEqual(new Set([release.images[image]]));
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
