import { PlusIcon } from "@phosphor-icons/react";
import { Button } from "./Button";
import { Menu, type MenuItem } from "./Menu";

/**
 * A "New" button whose menu opens on hover, and on click, tap or Enter. The
 * top bar's and the pages' New buttons are this one.
 */
export function NewButton({
  items,
  width = "w-56",
}: {
  items: MenuItem[];
  width?: string;
}) {
  return (
    <Menu
      width={width}
      label="New"
      hover
      trigger={(p) => (
        <Button size="sm" {...p}>
          <PlusIcon size={13} weight="bold" />
          <span className="hidden sm:inline">New</span>
        </Button>
      )}
      items={items}
    />
  );
}
