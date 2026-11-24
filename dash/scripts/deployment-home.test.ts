// Where a deployment's details open: in its service's Deployments tab while
// the service exists, on a plain page once it's deleted. Old links
// (/<team>/deployments/<id>) forward to the service.
// bun test dash/scripts/deployment-home.test.ts
import { expect, test } from "bun:test";
import {
  deploymentHome,
  deploymentHref,
  deploymentRedirect,
} from "../src/lib/pyo/deploymentHome";

const s = {
  teams: [
    { id: "tm_1", slug: "acme" },
    { id: "tm_2", slug: "other" },
  ],
  projects: [
    { id: "prj_1", namespace: "shop" },
    { id: "prj_2", namespace: "blog" },
  ],
  services: [
    { id: "svc_web", name: "web", projectId: "prj_1", teamId: "tm_1" },
    { id: "svc_api", name: "api", projectId: "prj_1", teamId: "tm_1" },
    { id: "svc_blog", name: "site", projectId: "prj_2", teamId: "tm_2" },
  ],
  deployments: [
    { id: "dpl_web", serviceId: "svc_web" },
    { id: "dpl_blog", serviceId: "svc_blog" },
    // Its service was deleted.
    { id: "dpl_gone", serviceId: "svc_gone" },
  ],
};

test("a deployment whose service exists opens in the service", () => {
  expect(deploymentHome(s, "dpl_web")).toEqual({
    kind: "service",
    service: "svc_web",
    team: "acme",
    projectId: "shop",
    serviceId: "web",
  });
  expect(deploymentHref(s, "dpl_web")).toBe("/shop/web/deployments/dpl_web");
  // The old URL forwards there.
  expect(deploymentRedirect(s, "dpl_web", null)).toEqual({
    to: "service",
    team: "acme",
    projectId: "shop",
    serviceId: "web",
  });
  // Already there: stays.
  expect(deploymentRedirect(s, "dpl_web", "svc_web")).toBeNull();
});

test("a link that names the wrong service opens the right one", () => {
  expect(deploymentRedirect(s, "dpl_web", "svc_api")).toEqual({
    to: "service",
    team: "acme",
    projectId: "shop",
    serviceId: "web",
  });
});

test("another team's deployment forwards under that team", () => {
  expect(deploymentRedirect(s, "dpl_blog", null)).toEqual({
    to: "service",
    team: "other",
    projectId: "blog",
    serviceId: "site",
  });
});

test("a deleted service's deployment shows on the plain page", () => {
  expect(deploymentHome(s, "dpl_gone")).toEqual({ kind: "deleted" });
  expect(deploymentHref(s, "dpl_gone")).toBe("/deployments/dpl_gone");
  expect(deploymentRedirect(s, "dpl_gone", null)).toBeNull();
  // Opened under some service: goes to the plain page.
  expect(deploymentRedirect(s, "dpl_gone", "svc_web")).toEqual({
    to: "plain",
  });
});

test("an unknown deployment stays put and says so", () => {
  expect(deploymentHome(s, "dpl_nope")).toEqual({ kind: "unknown" });
  expect(deploymentHref(s, "dpl_nope")).toBe("/deployments/dpl_nope");
  expect(deploymentRedirect(s, "dpl_nope", null)).toBeNull();
  expect(deploymentRedirect(s, "dpl_nope", "svc_web")).toBeNull();
});
