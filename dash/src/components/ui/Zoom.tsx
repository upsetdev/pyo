import { ArrowsInIcon, ArrowsOutIcon } from "@phosphor-icons/react";
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { Button } from "./Button";

/**
 * Shows its children in place, or in a fullscreen overlay while `zoomed`.
 * The children render once into a node that's moved between the two spots,
 * so nothing remounts: a terminal keeps its session and a log keeps its
 * scroll position.
 */
export function Zoomable({
  zoomed,
  onClose,
  label,
  closeOnEscape = true,
  children,
}: {
  zoomed: boolean;
  onClose: () => void;
  label: string;
  closeOnEscape?: boolean;
  children: ReactNode;
}) {
  const slot = useRef<HTMLDivElement>(null);
  const overlay = useRef<HTMLDivElement>(null);
  const [node] = useState(() =>
    typeof document === "undefined" ? null : document.createElement("div"),
  );

  useLayoutEffect(() => {
    if (!node) return;
    node.className = zoomed ? "flex min-h-0 flex-1 flex-col" : "";
    (zoomed ? overlay.current : slot.current)?.appendChild(node);
  }, [zoomed, node]);

  useEffect(() => {
    if (!zoomed) return;
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    const key = (e: KeyboardEvent) =>
      closeOnEscape && e.key === "Escape" && onClose();
    window.addEventListener("keydown", key);
    return () => {
      document.body.style.overflow = prev;
      window.removeEventListener("keydown", key);
    };
  }, [zoomed, closeOnEscape, onClose]);

  return (
    <>
      <div ref={slot} />
      {zoomed
        ? createPortal(
            <div
              role="dialog"
              aria-modal="true"
              aria-label={label}
              className="fixed inset-0 z-[60] flex flex-col bg-black/50 p-2 backdrop-blur-[2px] sm:p-4"
              onMouseDown={(e) => e.target === e.currentTarget && onClose()}
            >
              <div
                ref={overlay}
                className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-[14px] border border-card-border bg-card shadow-2xl"
              />
            </div>,
            document.body,
          )
        : null}
      {node ? createPortal(children, node) : null}
    </>
  );
}

/** The toolbar button that opens and closes the fullscreen view. */
export function ZoomButton({
  zoomed,
  onClick,
}: {
  zoomed: boolean;
  onClick: () => void;
}) {
  const label = zoomed ? "Exit full screen" : "Full screen";
  return (
    <Button
      variant="ghost"
      size="icon"
      aria-label={label}
      title={label}
      onClick={onClick}
    >
      {zoomed ? <ArrowsInIcon size={14} /> : <ArrowsOutIcon size={14} />}
    </Button>
  );
}
