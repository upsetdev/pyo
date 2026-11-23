// The note on a new database whose size no server of the cluster has room for.
// bun test dash/scripts/database-room.test.ts
import { expect, test } from "bun:test";
import { roomNote } from "../src/components/pyo/Databases";
import { largestFreeMemory } from "../src/lib/pyo/capacity";
import type { ClusterServer } from "../src/lib/pyo/types";

const server = (memoryGi: number, requestedGi: number, s: Partial<ClusterServer> = {}) =>
  ({
    clusterId: "cl_1",
    status: "ready",
    schedulable: true,
    maintenance: false,
    allocatable: { cpu: 2, memoryGi },
    requested: { cpu: 0.5, memoryGi: requestedGi },
    ...s,
  }) as ClusterServer;

test("the most free memory is one server's, not the sum", () => {
  expect(largestFreeMemory([server(1.45, 0.5), server(2, 1.5)], "cl_1")).toBeCloseTo(0.95);
  // Another cluster's, a cordoned one and a build server don't count.
  const others = [server(8, 0, { clusterId: "cl_2" }), server(8, 0, { schedulable: false }), server(8, 0, { builder: true })];
  expect(largestFreeMemory([server(1, 0.5), ...others], "cl_1")).toBeCloseTo(0.5);
  // Servers that haven't said: nothing to go by.
  expect(largestFreeMemory([{ ...server(1, 0), allocatable: undefined }], "cl_1")).toBeNull();
  expect(largestFreeMemory([], "cl_1")).toBeNull();
});

test("a size that can't fit on any server says so", () => {
  // 1.9 GB server: 1.45 GiB to hand out, 0.5 GiB already asked for.
  const one = [server(1.45, 0.5)];
  expect(roomNote(one, "cl_1", "mysql", "medium", 0)).toBe(
    "No server has room for this size. It needs 1 GiB of memory and the most free on one server is 972 MiB.",
  );
  // Small sets aside 128 MiB.
  expect(roomNote(one, "cl_1", "postgres", "small", 0)).toBeNull();
  expect(roomNote(one, "cl_1", "postgres", "custom", 2048)).not.toBeNull();
  expect(roomNote([server(8, 1)], "cl_1", "mysql", "large", 0)).toBeNull();
  // Older agents report no allocatable memory: no note.
  expect(roomNote([{ ...server(1, 0), allocatable: undefined }], "cl_1", "mysql", "xlarge", 0)).toBeNull();
});
