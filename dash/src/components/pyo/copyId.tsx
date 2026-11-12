import { CopyIcon } from "@phosphor-icons/react";
import type { MenuItem } from "~/components/ui/Menu";
import { toast } from "~/lib/pyo/toast";
import { copyText } from "~/lib/pyo/clipboard";

/**
 * "Copy service ID" and the like, for menus: the ids the API and MCP tools
 * take (service_id, deployment_id, ...).
 */
export function copyIdItem(kind: string, id: string): MenuItem {
  return {
    label: `Copy ${kind.toLowerCase()} ID`,
    icon: <CopyIcon size={14} />,
    onSelect: () => {
      void copyText(id).then(
        () => toast({ title: `${kind} ID copied`, description: id }),
        () => toast({ title: "Couldn't copy", description: id, tone: "error" }),
      );
    },
  };
}
