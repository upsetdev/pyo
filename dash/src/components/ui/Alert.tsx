import type { ReactNode } from "react";

type Tone = "error" | "success" | "info" | "warning";

const tones: Record<Tone, string> = {
  error: "border-destructive/40 bg-destructive/10 text-destructive",
  success: "border-success/40 bg-success/10 text-success",
  info: "border-primary/40 bg-primary/10 text-foreground",
  warning: "border-warning/40 bg-warning/10 text-foreground",
};

export function Alert({
  children,
  tone = "error",
}: {
  children: ReactNode;
  tone?: Tone;
}) {
  return (
    <div
      role={tone === "error" ? "alert" : "status"}
      className={`rounded-lg border px-3 py-2.5 text-[13px] ${tones[tone]}`}
    >
      {children}
    </div>
  );
}
