import { Fragment } from "react";
import { CaretRightIcon } from "@phosphor-icons/react";

/** A path through menus, like Settings › Backups, with a caret between steps.
 * Inline in a line of text by default; in a button or a flex row, whose gap
 * spaces the steps, pass `bare`. */
export function MenuPath({ steps, bare = false }: { steps: string[]; bare?: boolean }) {
  const path = steps.map((s, i) => (
    <Fragment key={s}>
      {i > 0 && (
        <CaretRightIcon
          size={11}
          weight="bold"
          role="img"
          aria-label="then"
          className={
            bare
              ? "shrink-0 text-muted-foreground"
              : "mx-1 inline-block shrink-0 -translate-y-px align-middle text-muted-foreground"
          }
        />
      )}
      {s}
    </Fragment>
  ));
  return bare ? <>{path}</> : <span className="font-medium text-foreground">{path}</span>;
}
