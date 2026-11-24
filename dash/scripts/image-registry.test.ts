// Which registry a pasted image comes from: Docker Hub only for Docker
// Hub's images, any other host by its own name (registry.rs, label).
// bun test dash/scripts/image-registry.test.ts
import { expect, test } from "bun:test";
import { parseImageInput, publicRegistryId } from "../src/lib/pyo/draft";

const registries = [
  { id: "reg_dockerhub", host: "docker.io" },
  { id: "reg_ghcr", host: "ghcr.io" },
  { id: "reg_quay", host: "quay.io" },
];

test("GitHub's registry is never Docker Hub", () => {
  const p = parseImageInput("ghcr.io/acme/web:latest", registries);
  expect(p).toEqual({ image: "ghcr.io/acme/web", reference: "latest", registryId: "reg_ghcr" });
});

test("Docker Hub, named or not", () => {
  expect(parseImageInput("docker.io/acme/api:2", registries).registryId).toBe("reg_dockerhub");
  const bare = parseImageInput("nginx", registries);
  expect(bare).toEqual({ image: "docker.io/library/nginx", reference: "latest", registryId: "reg_dockerhub" });
});

test("a registry of its own goes by its host", () => {
  const p = parseImageInput("registry.example.com:5000/team/web:v1", registries);
  expect(p.registryId).toBe("reg_public:registry.example.com:5000");
  expect(p.image).toBe("registry.example.com:5000/team/web");
  expect(p.reference).toBe("v1");
});

test("public registries by host", () => {
  expect(publicRegistryId("docker.io")).toBe("reg_dockerhub");
  expect(publicRegistryId("ghcr.io")).toBe("reg_ghcr");
  expect(publicRegistryId("quay.io")).toBe("reg_quay");
  expect(publicRegistryId("registry.example.com")).toBe("reg_public:registry.example.com");
});
