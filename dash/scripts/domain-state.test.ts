// A domain's one status label.
// bun test dash/scripts/domain-state.test.ts
import { expect, test } from "bun:test";
import { domainState } from "../src/components/pyo/DomainState";
import type { Domain } from "../src/lib/pyo/types";

const domain = (hostname: string, d: Partial<Domain> = {}): Domain => ({
  id: "dom_1",
  hostname,
  portName: "http",
  status: "pending_dns",
  tls: "pending",
  tlsMode: "auto",
  redirectHttps: true,
  createdAt: 1,
  ...d,
});

test("a tailnet domain is set up, not waiting for DNS", () => {
  expect(domainState(domain("blog.tail1.ts.net"), [])).toEqual({
    tone: "busy",
    text: "Setting up on Tailscale…",
  });
  expect(
    domainState(
      domain("blog.tail1.ts.net", { status: "error", error: "Taken on your tailnet" }),
      [],
    ),
  ).toEqual({ tone: "broken", text: "Tailscale failed", hint: "Taken on your tailnet" });
  expect(
    domainState(domain("blog.tail1.ts.net", { status: "active" }), []).text,
  ).toBe("Issuing certificate…");
  expect(
    domainState(domain("blog.tail1.ts.net", { status: "active", tls: "issued" }), []),
  ).toEqual({ tone: "ok", text: "HTTPS", hint: "Tailscale certificate" });
});

test("other domains still wait for DNS", () => {
  expect(domainState(domain("app.203.0.113.7.nip.io"), []).text).toBe("Checking DNS…");
  expect(domainState(domain("shop.example.com"), []).text).toBe("Waiting for DNS");
});

test("a tunnel domain moves to the tunnel, not waiting for DNS", () => {
  expect(domainState(domain("shop.example.com", { tunnel: "switching" }), [])).toEqual({
    tone: "busy",
    text: "Moving to the tunnel…",
  });
  // Live through the tunnel: its own status, no DNS wording.
  expect(
    domainState(domain("shop.example.com", { tunnel: "on", status: "active", tls: "issued", tlsMode: "auto" }), []).text,
  ).toBe("HTTPS");
});
