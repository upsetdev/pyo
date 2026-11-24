// How source lines show an image: tags as they are, digests cut short,
// the full reference for tooltips and copying.
// bun test dash/scripts/image-ref.test.ts
import { expect, test } from "bun:test";
import { fullImageRef, refLabel } from "../src/lib/pyo/format";

const digest = `sha256:${"0123456789abcdef0123456789abcdef".repeat(2)}`;

test("a digest shows its first 12 characters", () => {
  expect(refLabel(digest)).toBe("@sha256:0123456789ab");
});

test("a tag stays as it is", () => {
  expect(refLabel("22")).toBe(":22");
});

test("no reference shows the image alone", () => {
  expect(fullImageRef("ghcr.io/acme/api", "")).toBe("ghcr.io/acme/api");
});

test("the full reference keeps the whole digest", () => {
  expect(fullImageRef("ghcr.io/acme/api", digest)).toBe(
    `ghcr.io/acme/api@${digest}`,
  );
  expect(fullImageRef("registry.example.com:5000/team/web", "v1")).toBe(
    "registry.example.com:5000/team/web:v1",
  );
});
