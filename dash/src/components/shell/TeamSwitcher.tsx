import {
  CaretUpDownIcon,
  CheckIcon,
  GearIcon,
  PlusIcon,
} from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { toast } from "~/lib/pyo/toast";
import { api, currentTeam, getState, usePyo } from "~/lib/pyo/store";

export function TeamAvatar({
  name,
  size = 22,
}: {
  name: string;
  size?: number;
}) {
  return (
    <span
      aria-hidden
      style={{ width: size, height: size }}
      className="flex shrink-0 items-center justify-center rounded-full bg-primary/15 text-[10.5px] font-semibold uppercase text-primary"
    >
      {name.slice(0, 1)}
    </span>
  );
}

export function TeamSwitcher() {
  const s = usePyo();
  const ws = currentTeam(s);
  const navigate = useNavigate();
  const [creating, setCreating] = useState(false);

  // Entity pages belong to the old team; go to the new team's overview.
  const open = (slug: string) =>
    void navigate({ to: "/$team", params: { team: slug } });

  return (
    <>
      <Menu
        align="start"
        width="w-64"
        label="Switch team"
        trigger={(p) => (
          <button
            type="button"
            {...p}
            className="flex min-w-0 max-w-full items-center gap-2 rounded-md py-1 pl-1 pr-2 text-left transition-colors hover:bg-accent"
          >
            <TeamAvatar name={ws?.name ?? "?"} />
            <span className="truncate text-[13px] font-medium text-foreground">
              {ws?.name ?? "No team"}
            </span>
            <CaretUpDownIcon
              size={13}
              className="shrink-0 text-muted-foreground"
            />
          </button>
        )}
        items={[
          { type: "label", label: "Teams" },
          ...s.teams.map((w) => ({
            label: w.name,
            icon: <TeamAvatar name={w.name} size={18} />,
            hint: w.id === ws?.id ? <CheckIcon size={13} /> : undefined,
            onSelect: () => open(w.slug),
          })),
          { type: "separator" as const },
          {
            label: "Create team",
            icon: <PlusIcon size={14} />,
            onSelect: () => setCreating(true),
          },
          {
            label: "Team settings",
            icon: <GearIcon size={14} />,
            href: "/settings",
          },
        ]}
      />
      <CreateTeamDialog
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={(slug) => open(slug)}
      />
    </>
  );
}

export function CreateTeamDialog({
  open,
  onClose,
  onCreated,
}: {
  open: boolean;
  onClose: () => void;
  onCreated?: (slug: string) => void;
}) {
  const s = usePyo();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [touched, setTouched] = useState(false);
  const trimmed = name.trim();
  const error = !trimmed
    ? "Name is required."
    : trimmed.length > 48
      ? "Use 48 characters or fewer."
      : s.teams.some((w) => w.name.toLowerCase() === trimmed.toLowerCase())
        ? "A team with this name exists."
        : "";

  const close = () => {
    setName("");
    setDescription("");
    setTouched(false);
    onClose();
  };

  return (
    <Dialog
      open={open}
      onClose={close}
      title="Create team"
      onSubmit={() => {
        setTouched(true);
        if (error) return;
        const id = api.createTeam({ name: trimmed, description });
        toast({
          title: `Team ${trimmed} created`,
          description: "Add a cluster to start deploying.",
        });
        close();
        const slug = getState().teams.find((t) => t.id === id)?.slug;
        if (slug) onCreated?.(slug);
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" size="sm">
            Create team
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Field label="Name" required error={touched ? error : undefined}>
          {(p) => (
            <Input
              {...p}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Platform team"
            />
          )}
        </Field>
      </div>
    </Dialog>
  );
}
