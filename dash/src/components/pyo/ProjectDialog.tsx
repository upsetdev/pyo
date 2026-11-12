import { useNavigate } from "@tanstack/react-router";
import { RESERVED_PROJECT_SLUGS } from "~/lib/pyo/paths";
import { useEffect, useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { DNS_LABEL, slugify } from "~/lib/pyo/format";
import { api, projectsIn, usePyo, projectParams } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Project } from "~/lib/pyo/types";
import { currentSlug } from "~/lib/pyo/paths";

const RESERVED = [
  "default",
  "kube-system",
  "kube-public",
  "kube-node-lease",
  "pyo-system",
];

export function useProjectForm(project?: Project) {
  const s = usePyo();
  const [name, setName] = useState(project?.name ?? "");
  const [namespace, setNamespace] = useState(project?.namespace ?? "");
  const [nsEdited, setNsEdited] = useState(!!project);
  const [description, setDescription] = useState(project?.description ?? "");
  const [touched, setTouched] = useState(false);

  const others = projectsIn(s).filter((p) => p.id !== project?.id);
  const ns = nsEdited ? namespace : slugify(name);
  const errors = {
    name: !name.trim()
      ? "Name is required."
      : name.trim().length > 48
        ? "Use 48 characters or fewer."
        : others.some((p) => p.name.toLowerCase() === name.trim().toLowerCase())
          ? "A project with this name exists in this team."
          : "",
    namespace: project
      ? ""
      : !ns
        ? "Slug is required."
        : !DNS_LABEL.test(ns)
          ? "Lowercase letters, digits and dashes."
          : RESERVED.includes(ns) || RESERVED_PROJECT_SLUGS.includes(ns)
            ? "This name is reserved."
            : others.some((p) => p.namespace === ns)
              ? "Another project already uses this name."
              : "",
  };
  const valid = !errors.name && !errors.namespace;
  return {
    name,
    setName,
    namespace: ns,
    setNamespace: (v: string) => {
      setNsEdited(true);
      setNamespace(v);
    },
    description,
    setDescription,
    touched,
    setTouched,
    errors,
    valid,
  };
}

export function ProjectFormFields({
  f,
  editing,
  autoFocus = true,
}: {
  f: ReturnType<typeof useProjectForm>;
  editing?: boolean;
  autoFocus?: boolean;
}) {
  return (
    <div className="space-y-4">
      <Field
        label="Name"
        required
        error={f.touched ? f.errors.name || undefined : undefined}
      >
        {(p) => (
          <Input
            {...p}
            value={f.name}
            onChange={(e) => f.setName(e.target.value)}
            placeholder="Storefront"
            autoFocus={autoFocus}
          />
        )}
      </Field>
      <Field
        label="Slug"
        required={!editing}
        error={f.touched ? f.errors.namespace || undefined : undefined}
        hint={
          editing
            ? "Can't change once workloads exist."
            : "Used on every cluster."
        }
      >
        {(p) => (
          <Input
            {...p}
            mono
            value={f.namespace}
            disabled={editing}
            onChange={(e) => f.setNamespace(e.target.value.toLowerCase())}
            placeholder="storefront"
          />
        )}
      </Field>
    </div>
  );
}

/** Create (no project) or edit (with project). */
export function ProjectDialog({
  open,
  onClose,
  project,
}: {
  open: boolean;
  onClose: () => void;
  project?: Project;
}) {
  const navigate = useNavigate();
  const f = useProjectForm(project);
  useEffect(() => {
    if (!open) return;
    f.setName(project?.name ?? "");
    f.setDescription(project?.description ?? "");
    f.setTouched(false);
    // Reset when reopened.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={project ? "Edit project" : "Create project"}
      description={project ? undefined : "Group related services."}
      onSubmit={() => {
        f.setTouched(true);
        if (!f.valid) return;
        if (project) {
          api.updateProject(project.id, {
            name: f.name.trim(),
            description: f.description.trim(),
          });
          toast({ title: "Project updated" });
          onClose();
        } else {
          const id = api.createProject({
            name: f.name,
            description: f.description,
            namespace: f.namespace,
          });
          toast({ title: `Project ${f.name.trim()} created` });
          onClose();
          void navigate({
            to: "/$team/$projectId",
            params: { team: currentSlug(), ...projectParams(id) },
          });
        }
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" size="sm">
            {project ? "Save changes" : "Create project"}
          </Button>
        </>
      }
    >
      <ProjectFormFields f={f} editing={!!project} autoFocus={false} />
    </Dialog>
  );
}
