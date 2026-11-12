import { ArrowUpRightIcon, SparkleIcon } from "@phosphor-icons/react";
import { useMemo } from "react";
import { ButtonLink, buttonClass } from "~/components/ui/Button";

const COLORS = [
  "#3b82f6",
  "#22c55e",
  "#eab308",
  "#ec4899",
  "#a855f7",
  "#f97316",
];

/**
 * A service's first deploy finished: say so, with somewhere to go next, and
 * a short burst of confetti (none when reduced motion is preferred).
 */
export function Celebration({
  name,
  took,
  url,
  servicePath,
}: {
  name: string;
  took: string;
  url?: string;
  servicePath: string;
}) {
  const pieces = useMemo(
    () =>
      Array.from({ length: 56 }, (_, i) => ({
        left: Math.random() * 100,
        delay: Math.random() * 0.6,
        duration: 1.8 + Math.random() * 1.4,
        drift: (Math.random() - 0.5) * 160,
        spin: 360 + Math.random() * 540,
        color: COLORS[i % COLORS.length],
        w: 6 + Math.random() * 4,
        h: 8 + Math.random() * 6,
      })),
    [],
  );
  return (
    <section className="relative overflow-hidden rounded-2xl border border-success/30 bg-gradient-to-br from-success/[0.12] via-card to-card px-6 py-7">
      <style>{`
        @keyframes pyo-confetti {
          0% { transform: translate3d(0, -20px, 0) rotate(0deg); opacity: 1; }
          100% { transform: translate3d(var(--drift), 260px, 0) rotate(var(--spin)); opacity: 0; }
        }
        @media (prefers-reduced-motion: reduce) { .pyo-confetti { display: none; } }
      `}</style>
      <div
        aria-hidden
        className="pyo-confetti pointer-events-none absolute inset-0"
      >
        {pieces.map((p, i) => (
          <span
            key={i}
            className="absolute top-0 rounded-[2px]"
            style={
              {
                left: `${p.left}%`,
                width: p.w,
                height: p.h,
                background: p.color,
                animation: `pyo-confetti ${p.duration}s ${p.delay}s ease-out forwards`,
                opacity: 0,
                "--drift": `${p.drift}px`,
                "--spin": `${p.spin}deg`,
              } as React.CSSProperties
            }
          />
        ))}
      </div>
      <div className="relative flex flex-wrap items-center justify-between gap-4">
        <div className="flex items-center gap-4">
          <span className="flex h-11 w-11 items-center justify-center rounded-full bg-success/15 text-success">
            <SparkleIcon size={22} weight="fill" />
          </span>
          <div>
            <h2 className="text-[20px] font-semibold tracking-tight">
              {name} is live
            </h2>
            <p className="text-[13px] text-muted-foreground">
              Your first deploy finished in {took}.
            </p>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <ButtonLink href={servicePath} size="sm" variant="secondary">
            Open service
          </ButtonLink>
          {url ? (
            <a
              href={url}
              target="_blank"
              rel="noreferrer"
              className={buttonClass("primary", "sm")}
            >
              Visit <ArrowUpRightIcon size={13} />
            </a>
          ) : null}
        </div>
      </div>
    </section>
  );
}
