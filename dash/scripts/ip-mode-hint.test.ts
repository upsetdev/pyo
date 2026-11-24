// What Settings › Observability says about full IP addresses: they stay
// with the requests on each cluster, 31 days at most (pyo-proto's
// traffic.rs, MAX_DAYS_RANGE).
// bun test dash/scripts/ip-mode-hint.test.ts
import { expect, test } from "bun:test";
import { MODES } from "../src/components/pyo/ObservabilitySettings";
import { REQUEST_LOG_DAYS, REQUEST_LOG_MAX_DAYS } from "../src/lib/pyo/requestLogs";

test("request logs are kept 31 days at most", () => {
  expect(REQUEST_LOG_MAX_DAYS).toBe(31);
  expect(Math.max(...REQUEST_LOG_DAYS)).toBe(REQUEST_LOG_MAX_DAYS);
});

test("full addresses are kept as long as the request logs", () => {
  const full = MODES.find((m) => m.value === "full");
  expect(full?.hint).toBe(
    "Kept with each team's request logs, up to 31 days. May need consent under laws like GDPR.",
  );
});
