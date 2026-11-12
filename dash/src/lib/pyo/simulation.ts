import { BUILDPACK_BUILDER } from "./detect";
import { hex, shortSha } from "./format";
import type {
  Application,
  ServiceSource,
  Deployment,
  DeploymentEvent,
  DeploymentStage,
  DeploymentStatus,
  LogLevel,
} from "./types";

// Deterministic deployment simulation. Nothing here talks to a cluster: the
// store advances each active deployment through its stage plan on a timer and
// appends the lines below. The UI labels all of it as simulated.

export const STAGE_LABEL: Record<DeploymentStatus, string> = {
  queued: "Queued",
  building: "Building",
  pushing: "Pushing image",
  waiting_database: "Waiting for database",
  applying: "Applying settings",
  scheduling: "Finding a server",
  pulling: "Downloading image",
  starting: "Starting",
  waiting_ready: "Running health check",
  rolling_back: "Rolling back",
  succeeded: "Succeeded",
  failed: "Failed",
  cancelled: "Cancelled",
};

export const STAGE_DURATION: Record<DeploymentStage, number> = {
  queued: 1500,
  building: 7000,
  pushing: 2500,
  waiting_database: 2000,
  applying: 1500,
  scheduling: 1200,
  pulling: 3000,
  starting: 2000,
  waiting_ready: 3500,
  rolling_back: 2500,
};

export const ACTIVE_STATUSES: DeploymentStatus[] = [
  "queued",
  "building",
  "pushing",
  "waiting_database",
  "applying",
  "scheduling",
  "pulling",
  "starting",
  "waiting_ready",
  "rolling_back",
];

export const isActive = (s: DeploymentStatus) => ACTIVE_STATUSES.includes(s);

export function stagePlan(
  source: ServiceSource,
  trigger: Deployment["trigger"],
): DeploymentStage[] {
  if (trigger === "rollback")
    return [
      "queued",
      "rolling_back",
      "applying",
      "scheduling",
      "starting",
      "waiting_ready",
    ];
  if (source.type === "git")
    return [
      "queued",
      "building",
      "pushing",
      "applying",
      "scheduling",
      "pulling",
      "starting",
      "waiting_ready",
    ];
  return [
    "queued",
    "applying",
    "scheduling",
    "pulling",
    "starting",
    "waiting_ready",
  ];
}

export function imageRef(
  app: Pick<Application, "source" | "name">,
  revision: string,
) {
  const s = app.source;
  if (s.type === "git")
    return `registry.pyo.internal/${app.name}:${revision.slice(0, 12)}`;
  return `${s.image}:${s.reference}`;
}

/** Revision identity for a new rollout of the given source. */
export function revisionFor(source: ServiceSource, seed: string) {
  if (source.type === "git") {
    const sha = source.commit || hex(40, seed);
    return { revision: sha, revisionLabel: shortSha(sha) };
  }
  const digest = `sha256:${hex(64, seed)}`;
  return {
    revision: digest,
    revisionLabel: source.reference.startsWith("sha256:")
      ? shortSha(source.reference)
      : source.reference,
  };
}

type Line = {
  offset: number;
  level: LogLevel;
  source: string;
  message: string;
};

export function stageLines(
  dep: Deployment,
  app: Application,
  stage: DeploymentStage,
  nodeName: string,
  namespace: string,
): Line[] {
  const s = dep.source;
  const pod = `${app.name}-${dep.id.slice(-5)}`;
  const ready = dep.config.replicas;
  const d = STAGE_DURATION[stage];
  const at = (f: number) => Math.round(d * f);
  switch (stage) {
    case "queued":
      return [
        {
          offset: 0,
          level: "info",
          source: "pyo",
          message: `Deployment ${dep.id} queued (${dep.trigger.replace("_", " ")})`,
        },
        {
          offset: at(0.6),
          level: "info",
          source: "pyo",
          message: `Target: cluster ${dep.clusterId}, namespace ${namespace}`,
        },
      ];
    case "building": {
      if (s.type !== "git") return [];
      const args = s.buildArgs.length
        ? ` with ${s.buildArgs.length} build arg(s)`
        : "";
      if (s.builder === "buildpacks")
        return [
          {
            offset: 0,
            level: "info",
            source: "git",
            message: `Fetching ${s.repoUrl} (${s.branch})`,
          },
          {
            offset: at(0.08),
            level: "info",
            source: "git",
            message: `Checked out ${dep.revisionLabel}`,
          },
          {
            offset: at(0.15),
            level: "info",
            source: "pack",
            message: `No Dockerfile found. Building with Cloud Native Buildpacks (${BUILDPACK_BUILDER})`,
          },
          {
            offset: at(0.25),
            level: "info",
            source: "pack",
            message: `Detected ${s.buildpackLanguage ?? "an application"}`,
          },
          {
            offset: at(0.38),
            level: "info",
            source: "pack",
            message: "Restoring build cache",
          },
          {
            offset: at(0.55),
            level: "info",
            source: "pack",
            message: "Installing dependencies and building",
          },
          {
            offset: at(0.8),
            level: "info",
            source: "pack",
            message: "Setting default process type: web",
          },
          {
            offset: at(0.95),
            level: "info",
            source: "pack",
            message: "Exporting image layers",
          },
        ];
      return [
        {
          offset: 0,
          level: "info",
          source: "git",
          message: `Fetching ${s.repoUrl} (${s.branch})`,
        },
        {
          offset: at(0.08),
          level: "info",
          source: "git",
          message: `Checked out ${dep.revisionLabel}`,
        },
        {
          offset: at(0.15),
          level: "info",
          source: "buildkit",
          message: `Loading ${s.dockerfilePath} from context ${s.buildContext}${args}`,
        },
        {
          offset: at(0.25),
          level: "info",
          source: "buildkit",
          message: "#2 [internal] load metadata for base image",
        },
        {
          offset: at(0.35),
          level: "info",
          source: "buildkit",
          message: "#5 [deps 2/4] COPY package files (cached)",
        },
        {
          offset: at(0.5),
          level: "info",
          source: "buildkit",
          message: "#7 [build 3/4] RUN build step",
        },
        {
          offset: at(0.78),
          level: "info",
          source: "buildkit",
          message: "#9 [runtime 4/4] COPY --from=build /app/dist ./dist",
        },
        {
          offset: at(0.95),
          level: "info",
          source: "buildkit",
          message: "#10 exporting layers done",
        },
      ];
    }
    case "pushing":
      return [
        {
          offset: 0,
          level: "info",
          source: "registry",
          message: `Pushing ${imageRef(app, dep.revision)}`,
        },
        {
          offset: at(0.9),
          level: "info",
          source: "registry",
          message: `Pushed digest sha256:${hex(12, dep.id)}`,
        },
      ];
    case "rolling_back":
      return [
        {
          offset: 0,
          level: "warn",
          source: "pyo",
          message: `Rolling back to deployment ${dep.rollbackOf} (${dep.revisionLabel})`,
        },
        {
          offset: at(0.5),
          level: "info",
          source: "pyo",
          message:
            "Restoring previous workload spec and configuration snapshot",
        },
      ];
    case "waiting_database":
      return [
        {
          offset: 0,
          level: "info",
          source: "pyo",
          message: "Waiting for database",
        },
      ];
    case "applying": {
      const secrets = app.env.filter((e) => e.secret).length;
      const lines: Line[] = [
        {
          offset: 0,
          level: "info",
          source: "kube",
          message: `apply ${app.workloadKind.toLowerCase()}/${app.name} -n ${namespace}`,
        },
        {
          offset: at(0.2),
          level: "info",
          source: "kube",
          message: `apply configmap/${app.name}-env (${app.env.length - secrets} keys)`,
        },
      ];
      if (secrets)
        lines.push({
          offset: at(0.35),
          level: "info",
          source: "kube",
          message: `apply secret/${app.name}-secrets (${secrets} keys, values hidden)`,
        });
      if (app.ports.length)
        lines.push({
          offset: at(0.5),
          level: "info",
          source: "kube",
          message: `apply service/${app.name} (${app.ports.map((p) => `${p.containerPort}/${p.protocol === "UDP" ? "UDP" : "TCP"}`).join(", ")})`,
        });
      if (app.domains.length)
        lines.push({
          offset: at(0.7),
          level: "info",
          source: "kube",
          message: `apply ingressroute/${app.name} (${app.domains.map((x) => x.hostname).join(", ")})`,
        });
      return lines;
    }
    case "scheduling":
      return [
        {
          offset: 0,
          level: "info",
          source: "scheduler",
          message: `Creating ${ready} replica(s)`,
        },
        {
          offset: at(0.7),
          level: "info",
          source: "scheduler",
          message: `Assigned ${namespace}/${pod} to ${nodeName}`,
        },
      ];
    case "pulling":
      return [
        {
          offset: 0,
          level: "info",
          source: "kubelet",
          message: `Pulling image "${imageRef(app, dep.revision)}"`,
        },
        {
          offset: at(0.9),
          level: "info",
          source: "kubelet",
          message: `Successfully pulled image in ${((d / 1000) * 0.9).toFixed(1)}s`,
        },
      ];
    case "starting":
      return [
        {
          offset: 0,
          level: "info",
          source: "kubelet",
          message: `Created container ${app.name}`,
        },
        {
          offset: at(0.5),
          level: "info",
          source: "kubelet",
          message: `Started container ${app.name}`,
        },
      ];
    case "waiting_ready": {
      const h = app.healthCheck;
      const probe =
        h.type === "http"
          ? `HTTP GET :${h.port}${h.path}`
          : h.type === "tcp"
            ? `TCP :${h.port}`
            : h.type === "exec"
              ? `exec "${h.command}"`
              : "none (process running)";
      return [
        {
          offset: 0,
          level: "info",
          source: "pyo",
          message: `Readiness probe: ${probe}`,
        },
        {
          offset: at(0.55),
          level: "info",
          source: "pyo",
          message: `${Math.max(0, ready - 1)}/${ready} replicas ready`,
        },
        {
          offset: at(0.95),
          level: "info",
          source: "pyo",
          message: `${ready}/${ready} replicas ready`,
        },
      ];
    }
  }
}

export function stageEvents(
  dep: Deployment,
  app: Application,
  stage: DeploymentStage,
  nodeName: string,
): Omit<DeploymentEvent, "at">[] {
  const obj = `pod/${app.name}-${dep.id.slice(-5)}`;
  switch (stage) {
    case "applying":
      return [
        {
          type: "Normal",
          reason: "ScalingReplicaSet",
          object: `${app.workloadKind.toLowerCase()}/${app.name}`,
          message: `Scaled up to ${dep.config.replicas}`,
        },
      ];
    case "scheduling":
      return [
        {
          type: "Normal",
          reason: "Scheduled",
          object: obj,
          message: `Assigned to ${nodeName}`,
        },
      ];
    case "pulling":
      return [
        {
          type: "Normal",
          reason: "Pulled",
          object: obj,
          message: "Container image pulled",
        },
      ];
    case "starting":
      return [
        {
          type: "Normal",
          reason: "Started",
          object: obj,
          message: `Started container ${app.name}`,
        },
      ];
    default:
      return [];
  }
}

export function failureFor(stage: DeploymentStage, app: Application) {
  switch (stage) {
    case "building":
      return {
        reason: "BuildFailed",
        message:
          "Build step exited with code 1: type errors in src/server/routes.ts",
      };
    case "pushing":
      return {
        reason: "PushFailed",
        message: "Registry rejected the push: quota exceeded",
      };
    case "pulling":
      return {
        reason: "ErrImagePull",
        message:
          "Image pull failed: manifest unknown for the requested reference",
      };
    case "starting":
      return {
        reason: "CrashLoopBackOff",
        message: `Container ${app.name} exited with code 1 three times`,
      };
    case "waiting_ready":
      return {
        reason: "ReadinessProbeFailed",
        message:
          app.healthCheck.type === "http"
            ? `Health check ${app.healthCheck.path} returned 503`
            : "Health check timed out",
      };
    case "scheduling":
      return {
        reason: "FailedScheduling",
        message: "No server has enough free memory",
      };
    default:
      return {
        reason: "ApplyFailed",
        message: "Server-side apply was rejected by admission",
      };
  }
}
