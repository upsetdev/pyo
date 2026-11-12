import {
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";

/**
 * A short explanation on hover or keyboard focus, centered over its label
 * with an arrow pointing at it (below the label when there's no room above).
 * Rendered on the page body, so cards with hidden overflow don't clip it.
 */
/** `focusable`: false when the trigger is already a link or button. */
export function Tooltip({
  content,
  children,
  focusable = true,
}: {
  content: ReactNode;
  children: ReactNode;
  focusable?: boolean;
}) {
  const trigger = useRef<HTMLSpanElement>(null);
  const tip = useRef<HTMLSpanElement>(null);
  const id = useId();
  const [anchor, setAnchor] = useState<{
    cx: number;
    top: number;
    bottom: number;
  } | null>(null);
  const [place, setPlace] = useState<{
    left: number;
    arrow: number;
    above: boolean;
    y: number;
  } | null>(null);

  const show = () => {
    const r = trigger.current?.getBoundingClientRect();
    if (r)
      setAnchor({ cx: r.left + r.width / 2, top: r.top, bottom: r.bottom });
  };
  const hide = () => {
    setAnchor(null);
    setPlace(null);
  };

  // Center on the label, kept inside the window; the arrow stays on the label.
  useLayoutEffect(() => {
    if (!anchor || !tip.current) return;
    const { width, height } = tip.current.getBoundingClientRect();
    const left = Math.min(
      Math.max(8, anchor.cx - width / 2),
      window.innerWidth - width - 8,
    );
    const above = anchor.top - height - 10 > 8;
    setPlace({
      left,
      arrow: anchor.cx - left,
      above,
      y: above ? anchor.top - height - 8 : anchor.bottom + 8,
    });
  }, [anchor]);

  return (
    <>
      <span
        ref={trigger}
        tabIndex={focusable ? 0 : undefined}
        aria-describedby={anchor ? id : undefined}
        onMouseEnter={show}
        onMouseLeave={hide}
        onFocus={show}
        onBlur={hide}
        className="inline-flex cursor-default rounded-full outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        {children}
      </span>
      {anchor
        ? createPortal(
            <span
              ref={tip}
              id={id}
              role="tooltip"
              style={
                place
                  ? { left: place.left, top: place.y }
                  : { left: 0, top: 0, visibility: "hidden" }
              }
              className="pointer-events-none fixed z-[70] max-w-72 rounded-lg border border-border-strong bg-popover px-2.5 py-1.5 text-center text-[12px] leading-[18px] text-foreground shadow-pop"
            >
              {content}
              <span
                aria-hidden
                style={{ left: (place?.arrow ?? 0) - 4 }}
                className={`absolute h-2 w-2 rotate-45 border-border-strong bg-popover ${place?.above === false ? "-top-[5px] border-l border-t" : "-bottom-[5px] border-b border-r"}`}
              />
            </span>,
            document.body,
          )
        : null}
    </>
  );
}
