// Class strings shared by form controls. No imports, so any control can use
// them without an import cycle (Field re-exports Select, which needs these).

/**
 * Every field's look: an inset surface (a pill when single-line) with a
 * 1px edge that firms up on hover, the primary edge plus a soft
 * 3px ring on focus, the same in destructive when invalid. Read-only and
 * disabled sit on the muted surface. (Read-only keys on the attribute: CSS
 * :read-only also matches buttons.)
 */
export const fieldState =
  "border border-input bg-field text-foreground shadow-field transition-[border-color,box-shadow,background-color] hover:border-input-hover focus:border-primary focus:outline-none focus:ring-[3px] focus:ring-primary/20 focus-visible:outline-none aria-[invalid=true]:border-destructive aria-[invalid=true]:ring-[3px] aria-[invalid=true]:ring-destructive/20 [&[readonly]]:border-border [&[readonly]]:bg-muted [&[readonly]]:shadow-none disabled:cursor-not-allowed disabled:border-border disabled:bg-muted disabled:text-muted-foreground disabled:shadow-none";

/** A single-line field: a 36px (32px small) pill, 13px text. */
export const inputClass = `h-9 w-full min-w-0 rounded-full px-3.5 text-[13px] placeholder:text-muted-foreground/80 ${fieldState}`;
export const inputSmClass = `h-8 w-full min-w-0 rounded-full px-3 text-[12.5px] placeholder:text-muted-foreground/80 ${fieldState}`;
/** A multi-line field: the same surface with 12px corners. */
export const textareaClass = `min-h-[72px] w-full min-w-0 rounded-xl px-3.5 py-2 text-[13px] leading-5 placeholder:text-muted-foreground/80 ${fieldState}`;

/**
 * Several parts that read as one field (a prefix, a picker, inputs): the
 * group carries the edge and the focus ring; inner controls are bare. An
 * 18px radius: a pill at one line, a rounded box when it wraps.
 */
export const fieldGroupClass = (invalid = false) =>
  `flex h-9 min-w-0 items-stretch overflow-hidden rounded-[18px] border bg-field shadow-field transition-[border-color,box-shadow] focus-within:ring-[3px] ${
    invalid
      ? "border-destructive ring-[3px] ring-destructive/20 focus-within:ring-destructive/20"
      : "border-input hover:border-input-hover focus-within:border-primary focus-within:ring-primary/20"
  }`;
/**
 * A pick-one choice (radio card, preset, chip): the card's surface and the
 * field's edge at rest; selected is the field's focus look (primary edge, 3px ring)
 * on a faint primary tint; keyboard focus matches it; invalid and disabled
 * as fields. Cards get 12px corners, chips are pills.
 */
export const optionClass = ({
  selected = false,
  invalid = false,
  pill = false,
}: { selected?: boolean; invalid?: boolean; pill?: boolean } = {}) =>
  `border text-left transition-[border-color,box-shadow,background-color] ${pill ? "rounded-full" : "rounded-xl"} focus-visible:border-primary focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-primary/20 disabled:cursor-not-allowed disabled:border-border disabled:bg-muted disabled:text-muted-foreground disabled:shadow-none ${
    invalid
      ? "border-destructive bg-card ring-[3px] ring-destructive/20"
      : selected
        ? "border-primary bg-primary/[0.06] ring-[3px] ring-primary/20 dark:bg-primary/[0.1]"
        : "border-input bg-card hover:border-input-hover"
  }`;

/** A bare input inside a field group. */
export const groupInputClass =
  "min-w-0 bg-transparent px-3.5 text-[13px] text-foreground outline-none placeholder:text-muted-foreground/80 focus-visible:outline-none";
