import { ArrowRightIcon } from "@phosphor-icons/react";

/** The arrow between two things ("1.2 → 1.3", "conditions → Block"):
 * the icon, never a text arrow. A path through menus takes MenuPath. Inline
 * in a line of text by default; in a button or a flex row, whose gap spaces
 * it, pass `bare`. */
export function Arrow({
  label = "to",
  size = 12,
  bare = false,
}: {
  /** What screen readers say for it. */
  label?: string;
  size?: number;
  bare?: boolean;
}) {
  return (
    <ArrowRightIcon
      size={size}
      weight="bold"
      role="img"
      aria-label={label}
      className={
        bare
          ? "shrink-0"
          : "mx-1 inline-block shrink-0 -translate-y-px align-middle text-muted-foreground"
      }
    />
  );
}
