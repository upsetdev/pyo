import type { Tone } from "~/components/ui/Badge";
import type { Builder } from "./types";

// Labels shared by the builders list and a builder's page.

export const BUILDER_STATUS: Record<
  Builder["status"],
  { label: string; tone: Tone }
> = {
  provisioning: { label: "Setting up", tone: "progress" },
  ready: { label: "Ready", tone: "success" },
  offline: { label: "Offline", tone: "neutral" },
  error: { label: "Error", tone: "danger" },
  removing: { label: "Removing", tone: "neutral" },
};

/** What removing a builder does, for its confirmation. */
export function removeCopy(b: Builder | null | undefined) {
  if (b?.kind === "server")
    return b.clusterId
      ? "Its builder stops. The server stays in its cluster."
      : "Its containers stay on the machine: docker rm -f pyo-builder pyo-buildkitd";
  return "Its build cache is removed. Running builds fail.";
}

/** "server-1 · home" or "Build-only server · build-box". */
export function builderWhere(b: Builder, clusterName?: string) {
  if (b.kind === "server")
    return b.info.hostname
      ? `Build-only server · ${b.info.hostname}`
      : "Build-only server";
  return `${b.server} · ${clusterName ?? "cluster"}`;
}
