import { useEffect, useState, type ReactNode } from "react";

/** Closing time, in ms (opening takes 220). */
export const REVEAL_OUT = 180;

const reduced = () => typeof window !== "undefined" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/**
 * Content that opens and closes smoothly: its height (grid rows 0fr ↔ 1fr),
 * opacity and a slight lift, so what's below moves with it. Closed, it's
 * inert; open and settled, it stops clipping (shadows, popovers).
 */
export function Reveal({ open, id, className = "", children }: { open: boolean; id?: string; className?: string; children: ReactNode }) {
  const [settled, setSettled] = useState(open);
  useEffect(() => {
    if (!open) setSettled(false);
    else if (reduced()) setSettled(true);
  }, [open]);
  return (
    <div
      id={id}
      inert={!open}
      aria-hidden={!open || undefined}
      onTransitionEnd={(e) => {
        if (open && e.target === e.currentTarget && e.propertyName === "grid-template-rows") setSettled(true);
      }}
      className={`grid ease-out transition-[grid-template-rows,opacity,translate] motion-reduce:transition-none ${
        open ? "translate-y-0 grid-rows-[1fr] opacity-100 duration-[220ms]" : "-translate-y-1 grid-rows-[0fr] opacity-0 duration-[180ms]"
      } ${className}`}
    >
      <div className={`min-h-0 ${open && settled ? "" : "overflow-hidden"}`}>{children}</div>
    </div>
  );
}

/**
 * Keeps something mounted while it fades out: `mounted` while shown or
 * leaving, `visible` once it has entered (a frame after mounting) and until
 * it starts leaving.
 */
export function usePresence(show: boolean) {
  const [mounted, setMounted] = useState(show);
  const [visible, setVisible] = useState(show);
  useEffect(() => {
    if (show) {
      setMounted(true);
      if (reduced()) return setVisible(true);
      // Two frames: the first paints it at 0, the second fades it in.
      let b = 0;
      const a = requestAnimationFrame(() => (b = requestAnimationFrame(() => setVisible(true))));
      return () => {
        cancelAnimationFrame(a);
        cancelAnimationFrame(b);
      };
    }
    setVisible(false);
    if (reduced()) return setMounted(false);
    const t = setTimeout(() => setMounted(false), REVEAL_OUT);
    return () => clearTimeout(t);
  }, [show]);
  return { mounted: mounted || show, visible: visible && show };
}
