import {
  CopyIcon,
  DotsThreeIcon,
  TrashIcon,
  WebhooksLogoIcon,
} from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Input } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { DeployHook } from "~/lib/pyo/types";
import { copyText } from "~/lib/pyo/clipboard";

// Deploy hooks: a secret URL per service or per project. A POST to it deploys
// with the current settings, so CI, a CMS or a cron can ship without a login.

/** Full URL on this dashboard (or the public URL email links use). */
function hookUrl(path: string, publicUrl?: string) {
  return `${(publicUrl || window.location.origin).replace(/\/$/, "")}${path}`;
}

export function DeployHooksPanel({
  projectId,
  serviceId,
}: {
  projectId: string;
  serviceId?: string;
}) {
  const s = usePyo();
  const hooks = s.deployHooks.filter(
    (h) => h.projectId === projectId && (serviceId ? h.serviceId === serviceId : !h.serviceId),
  );
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [fresh, setFresh] = useState<string | null>(null);
  const [removing, setRemoving] = useState<DeployHook | null>(null);
  const scope = serviceId ? "this service" : "every service in this project";

  return (
    <Panel title="Deploy hooks">
      {hooks.length ? (
        <ul className="list-rows">
          {hooks.map((h) => (
            <li key={h.id} className="flex items-center gap-3 px-4 py-3">
              <WebhooksLogoIcon
                size={16}
                className="shrink-0 text-muted-foreground"
              />
              <div className="min-w-0 flex-1">
                <p className="truncate text-[13px] font-medium">{h.name}</p>
                <p className="truncate text-[12px] text-muted-foreground">
                  Created by {h.createdBy} {timeAgo(h.createdAt)} ·{" "}
                  {h.lastUsedAt
                    ? `last used ${timeAgo(h.lastUsedAt)}`
                    : "never used"}
                </p>
              </div>
              <Button
                variant="secondary"
                size="xs"
                onClick={async () => {
                  const r = await api.revealDeployHook(h.id);
                  if (!r)
                    return toast({
                      tone: "error",
                      title: "Only developers of the team can see hook URLs.",
                    });
                  try {
                    await copyText(
                      hookUrl(r.path, s.email?.publicUrl),
                    );
                    toast({ title: "Hook URL copied" });
                  } catch {
                    toast({
                      tone: "error",
                      title: "Couldn't copy",
                      description: hookUrl(r.path, s.email?.publicUrl),
                    });
                  }
                }}
              >
                <CopyIcon size={12} /> Copy URL
              </Button>
              <Menu
                label={`Actions for ${h.name}`}
                items={[
                  {
                    label: "Delete hook",
                    icon: <TrashIcon size={14} />,
                    danger: true,
                    onSelect: () => setRemoving(h),
                  },
                ]}
                trigger={(t) => (
                  <Button
                    {...t}
                    variant="ghost"
                    size="icon"
                    className="h-7 w-7"
                  >
                    <DotsThreeIcon size={16} weight="bold" />
                  </Button>
                )}
              />
            </li>
          ))}
        </ul>
      ) : null}

      {fresh ? (
        <div className="space-y-2 border-t border-border bg-muted/30 px-4 py-3 first:border-t-0">
          <p className="text-[12.5px] font-medium">
            Hook created. Deploy with:
          </p>
          <div className="flex items-start gap-2 rounded-lg border border-border bg-background px-3 py-2">
            <code className="min-w-0 flex-1 break-all font-mono text-[12px]">
              curl -X POST {fresh}
            </code>
            <CopyButton value={`curl -X POST ${fresh}`} label="Copy command" />
          </div>
          <p className="text-[12px] text-muted-foreground">
            You can copy the URL again from the list.
          </p>
        </div>
      ) : null}

      <form
        className="flex flex-wrap items-center gap-2 border-t border-border px-4 py-3 first:border-t-0"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!name.trim() || creating) return;
          setCreating(true);
          const r = await api.createDeployHook(
            projectId,
            serviceId ?? null,
            name.trim(),
          );
          setCreating(false);
          if (!r) return;
          setFresh(hookUrl(r.path, s.email?.publicUrl));
          setName("");
        }}
      >
        <Input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="Hook name, e.g. GitHub Actions"
          aria-label="Hook name"
          className="min-w-0 flex-1"
          maxLength={60}
        />
        <Button
          type="submit"
          size="sm"
          variant="secondary"
          disabled={!name.trim() || creating}
        >
          {creating ? "Creating…" : "Create hook"}
        </Button>
      </form>

      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Delete "${removing?.name}"?`}
        description="Its URL stops working right away."
        confirmLabel="Delete hook"
        onConfirm={async () => {
          if (!removing) return;
          if (await api.removeDeployHook(removing.id))
            toast({ title: "Hook deleted" });
          if (fresh) setFresh(null);
        }}
      />
    </Panel>
  );
}
