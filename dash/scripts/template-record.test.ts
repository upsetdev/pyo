// What a service deployed from a template records about it, and the
// catalog's template versions.
// bun test scripts/template-record.test.ts
import { expect, test } from "bun:test";
import { companionRecord, readCatalog, templateRecord } from "../src/lib/pyo/templates";

const digest = (c: string) => `sha256:${c.repeat(64)}`;
const entry = (over: object = {}) => ({
  schema: 1,
  id: "demo",
  name: "Demo",
  description: "A small service used to test the record.",
  category: "other",
  tier: "community",
  author: "someone",
  version: "1.2.0",
  appVersion: "4.0.1",
  minPyoVersion: "0.1.0",
  source: { type: "image", image: "docker.io/acme/demo", versions: [{ tag: "v4.0.1", digest: digest("a") }, { tag: "v3.9", digest: digest("b") }] },
  kind: "application",
  workload: "Deployment",
  stateful: false,
  replicas: 1,
  resources: { cpuRequest: 100, cpuLimit: 200, memoryRequest: 64, memoryLimit: 128 },
  ports: [],
  healthCheck: { type: "tcp", port: 80, initialDelaySeconds: 5, periodSeconds: 10 },
  env: [{ key: "ADMIN_PASSWORD", secret: true }],
  fields: [
    { key: "retention", label: "Retention", type: "select", default: "15d", options: ["15d", "30d"], required: true },
    { key: "admin", label: "Admin password", type: "password", required: true, sensitive: true, envKey: "ADMIN_PASSWORD" },
    { key: "token", label: "Token", type: "password", required: false },
  ],
  hints: [],
  companions: [
    {
      name: "demo-events",
      image: "docker.io/acme/events",
      version: { tag: "24.8", digest: digest("c") },
      workload: "StatefulSet",
      stateful: true,
      replicas: 1,
      resources: { cpuRequest: 100, cpuLimit: 200, memoryRequest: 64, memoryLimit: 128 },
      ports: [],
      healthCheck: { type: "tcp", port: 8123, initialDelaySeconds: 5, periodSeconds: 10 },
      env: [],
    },
  ],
  ...over,
});
const read = (over?: object) => readCatalog({ schema: 1, templates: [entry(over)] })[0];

test("the catalog's template version and app version are read", () => {
  const t = read();
  expect(t.templateVersion).toBe("1.2.0");
  expect(t.appVersion).toBe("4.0.1");
  expect(read({ appVersion: undefined }).appVersion).toBeUndefined();
  // An entry without its own version can't be recorded, so it's left out.
  expect(read({ version: undefined })).toBeUndefined();
  expect(read({ version: "1.2" })).toBeUndefined();
});

test("a service records the template's version, images and its fields, never a secret's value", () => {
  const t = read();
  const r = templateRecord(t, { version: "v4.0.1", retention: "30d", admin: "hunter22", token: "" });
  expect(r).toEqual({
    templateVersion: "1.2.0",
    appVersion: "4.0.1",
    images: {
      demo: `docker.io/acme/demo:v4.0.1@${digest("a")}`,
      "demo-events": `docker.io/acme/events:24.8@${digest("c")}`,
    },
    fields: {
      retention: "30d",
      admin: { secret: true, set: true, env: "ADMIN_PASSWORD" },
      token: { secret: true, set: false },
    },
  });
  expect(JSON.stringify(r)).not.toContain("hunter22");
});

test("another version: its own digest, and no app version", () => {
  const r = templateRecord(read(), { version: "v3.9" });
  expect(r.images?.demo).toBe(`docker.io/acme/demo:v3.9@${digest("b")}`);
  expect(r.appVersion).toBeUndefined();
  expect(r.fields?.retention).toBe("15d");
});

test("a companion records the template and the image it gave it", () => {
  const t = read();
  expect(companionRecord(t, t.companions[0])).toEqual({
    templateId: "demo",
    templateVersion: "1.2.0",
    service: "demo-events",
    image: `docker.io/acme/events:24.8@${digest("c")}`,
  });
});
