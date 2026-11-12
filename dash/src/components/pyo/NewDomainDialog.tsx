import { useState } from "react";
import { AddDomainDialog } from "~/components/pyo/AddDomainDialog";
import { AppLink } from "~/components/ui/AppLink";
import { Dialog } from "~/components/ui/Dialog";
import { Input } from "~/components/ui/Field";
import { projectsIn, servicesIn, usePyo } from "~/lib/pyo/store";
import type { Application } from "~/lib/pyo/types";

/**
 * Add a domain from the New menu: pick the service first (grouped by
 * project, searchable), then the regular add-domain form for it.
 */
export function NewDomainDialog({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const s = usePyo();
  const [query, setQuery] = useState("");
  const [app, setApp] = useState<Application | null>(null);
  const services = servicesIn(s).filter((a) => a.workloadKind !== "CronJob");
  const q = query.trim().toLowerCase();
  const groups = projectsIn(s)
    .map((p) => ({
      p,
      services: services
        .filter(
          (a) =>
            a.projectId === p.id &&
            (!q ||
              a.name.toLowerCase().includes(q) ||
              p.name.toLowerCase().includes(q)),
        )
        .sort((x, y) => x.name.localeCompare(y.name)),
    }))
    .filter((g) => g.services.length)
    .sort((x, y) => x.p.name.localeCompare(y.p.name));
  const close = () => {
    setQuery("");
    setApp(null);
    onClose();
  };
  return (
    <>
      <Dialog
        open={open && !app}
        onClose={close}
        title="Add domain"
        width="max-w-md"
      >
        {services.length ? (
          <div className="space-y-3">
            <Input
              type="search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                // Enter picks the first match that can take a domain.
                if (e.key !== "Enter") return;
                e.preventDefault();
                const first = groups
                  .flatMap((g) => g.services)
                  .find((a) => a.ports.some((x) => x.protocol === "HTTP"));
                if (first) setApp(first);
              }}
              placeholder="Find a service"
              aria-label="Find a service"
            />
            <div className="-mx-2 max-h-72 overflow-y-auto">
              {groups.length ? (
                groups.map(({ p, services }) => (
                  <div key={p.id} className="pb-1">
                    <div className="px-2 pb-1 pt-1.5 text-[11px] font-medium text-muted-foreground">
                      {p.name}
                    </div>
                    {services.map((a) => {
                      const http = a.ports.some((x) => x.protocol === "HTTP");
                      return (
                        <button
                          key={a.id}
                          type="button"
                          disabled={!http}
                          onClick={() => setApp(a)}
                          className="flex w-full items-center gap-2 rounded-lg px-2 py-2 text-left text-[13px] text-foreground outline-none transition-colors hover:bg-accent focus-visible:bg-accent disabled:pointer-events-none disabled:opacity-45"
                        >
                          <span className="min-w-0 flex-1 truncate">
                            {a.name}
                          </span>
                          {http ? null : (
                            <span className="text-[11px] text-muted-foreground">
                              No HTTP port
                            </span>
                          )}
                        </button>
                      );
                    })}
                  </div>
                ))
              ) : (
                <div className="px-2 py-2 text-[13px] text-muted-foreground">
                  No matches
                </div>
              )}
            </div>
          </div>
        ) : (
          <div className="py-2 text-[13px] text-muted-foreground">
            No services yet.{" "}
            <AppLink
              href="/deploy/new"
              onClick={close}
              className="font-medium text-primary hover:text-primary/80"
            >
              Deploy a service
            </AppLink>
          </div>
        )}
      </Dialog>
      <AddDomainDialog app={app ?? undefined} open={!!app} onClose={close} />
    </>
  );
}
