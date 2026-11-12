import type { ReactNode } from "react";
import { STAGE_LABEL, isActive } from "~/lib/pyo/simulation";
import type {
  ServiceStatus,
  ClusterStatus,
  DeploymentStatus,
  ServerStatus,
  TlsStatus,
  VolumeStatus,
  WorkloadStatus,
} from "~/lib/pyo/types";

export type Tone =
  "neutral" | "success" | "warning" | "danger" | "info" | "progress";

const toneClass: Record<Tone, string> = {
  neutral: "bg-secondary text-muted-foreground",
  success: "bg-success/12 text-success",
  warning: "bg-warning/12 text-warning",
  danger: "bg-destructive/12 text-destructive",
  info: "bg-primary/12 text-primary dark:text-info",
  progress: "bg-primary/12 text-primary dark:text-info",
};

const dotClass: Record<Tone, string> = {
  neutral: "bg-muted-foreground/60",
  success: "bg-success",
  warning: "bg-warning",
  danger: "bg-destructive",
  // The brighter blue in dark (5:1 on a card); a halo pulses, the dot stays solid.
  info: "bg-primary dark:bg-info",
  progress: "bg-primary text-primary dark:bg-info dark:text-info animate-pulse-dot",
};

export function Badge({
  tone = "neutral",
  children,
  className = "",
  mono,
}: {
  tone?: Tone;
  children: ReactNode;
  className?: string;
  mono?: boolean;
}) {
  return (
    <span
      className={`inline-flex h-5 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-md px-2 text-[11px] font-medium ${mono ? "font-mono" : ""} ${toneClass[tone]} ${className}`}
    >
      {children}
    </span>
  );
}

export function Dot({
  tone,
  className = "",
}: {
  tone: Tone;
  className?: string;
}) {
  return (
    <span
      aria-hidden
      className={`inline-block h-1.5 w-1.5 shrink-0 rounded-full ${dotClass[tone]} ${className}`}
    />
  );
}

/** Status with a dot and a label, so meaning never rests on color alone. */
export function Status({
  tone,
  label,
  className = "",
}: {
  tone: Tone;
  label: ReactNode;
  className?: string;
}) {
  return (
    <span
      className={`inline-flex items-center gap-2 whitespace-nowrap text-[13px] ${className}`}
    >
      <Dot tone={tone} className="h-2 w-2" />
      <span
        className={
          tone === "neutral" ? "text-muted-foreground" : "text-foreground"
        }
      >
        {label}
      </span>
    </span>
  );
}

export const serviceTone: Record<ServiceStatus, Tone> = {
  running: "success",
  deploying: "progress",
  degraded: "warning",
  failed: "danger",
  stopped: "neutral",
  restoring: "progress",
};
export const serviceLabel: Record<ServiceStatus, string> = {
  running: "Running",
  deploying: "Deploying",
  degraded: "Degraded",
  failed: "Failed",
  stopped: "Stopped",
  restoring: "Restoring",
};

export function ServiceStatusBadge({ status }: { status: ServiceStatus }) {
  return <Status tone={serviceTone[status]} label={serviceLabel[status]} />;
}

export const deploymentTone = (s: DeploymentStatus): Tone =>
  s === "succeeded"
    ? "success"
    : s === "failed"
      ? "danger"
      : s === "cancelled"
        ? "neutral"
        : s === "rolling_back"
          ? "warning"
          : "progress";

export function DeploymentStatusBadge({
  status,
}: {
  status: DeploymentStatus;
}) {
  const tone = deploymentTone(status);
  return (
    <Status
      tone={isActive(status) ? "progress" : tone}
      label={STAGE_LABEL[status]}
    />
  );
}

export const clusterTone: Record<ClusterStatus, Tone> = {
  connected: "success",
  degraded: "warning",
  unreachable: "danger",
  pending: "progress",
};
export const clusterLabel: Record<ClusterStatus, string> = {
  connected: "Connected",
  degraded: "Degraded",
  unreachable: "Unreachable",
  pending: "Connecting",
};

export function ClusterStatusBadge({ status }: { status: ClusterStatus }) {
  return <Status tone={clusterTone[status]} label={clusterLabel[status]} />;
}

export function NodeStatusBadge({
  status,
  schedulable,
  maintenance,
}: {
  status: ServerStatus;
  schedulable: boolean;
  maintenance: boolean;
}) {
  if (status !== "ready")
    return (
      <Status
        tone="danger"
        label={status === "not_ready" ? "Offline" : "Unknown"}
      />
    );
  if (maintenance) return <Status tone="warning" label="Maintenance" />;
  return (
    <Status
      tone={schedulable ? "success" : "warning"}
      label={schedulable ? "Online" : "Scheduling paused"}
    />
  );
}

export const workloadTone: Record<WorkloadStatus, Tone> = {
  healthy: "success",
  progressing: "progress",
  degraded: "warning",
  failed: "danger",
  stopped: "neutral",
  completed: "neutral",
};

export function WorkloadStatusBadge({ status }: { status: WorkloadStatus }) {
  return (
    <Status
      tone={workloadTone[status]}
      label={status[0].toUpperCase() + status.slice(1)}
    />
  );
}

export const volumeTone: Record<VolumeStatus, Tone> = {
  bound: "success",
  pending: "progress",
  degraded: "warning",
  error: "danger",
};

export function VolumeStatusBadge({ status }: { status: VolumeStatus }) {
  return (
    <Status
      tone={volumeTone[status]}
      label={status[0].toUpperCase() + status.slice(1)}
    />
  );
}

/** Marks UI that shows simulated, not real, operations. */
export function SimulatedBadge({ className = "" }: { className?: string }) {
  return (
    <Badge tone="neutral" className={className}>
      Simulated
    </Badge>
  );
}

export function TlsBadge({ status }: { status: TlsStatus }) {
  const map: Record<TlsStatus, [Tone, string]> = {
    issued: ["success", "TLS issued"],
    pending: ["neutral", "TLS pending"],
    failed: ["danger", "TLS failed"],
    disabled: ["neutral", "No TLS"],
  };
  const [tone, label] = map[status];
  return <Badge tone={tone}>{label}</Badge>;
}
