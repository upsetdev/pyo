import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Input, Select } from "~/components/ui/Field";
import { copyText } from "~/lib/pyo/clipboard";
import { api, membersIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Role } from "~/lib/pyo/types";

export const ROLES: { value: Role; label: string; body: string }[] = [
  {
    value: "owner",
    label: "Owner",
    body: "Full control, including deleting the team.",
  },
  {
    value: "admin",
    label: "Admin",
    body: "Manage members, clusters and every project.",
  },
  {
    value: "developer",
    label: "Developer",
    body: "Deploy and configure services.",
  },
  { value: "viewer", label: "Viewer", body: "Read-only access." },
];

/** The sign-up link for an invited address, on this dashboard. */
export async function copyInviteLink(link: string) {
  const url = /^https?:/.test(link) ? link : `${window.location.origin}${link}`;
  try {
    await copyText(url);
    toast({ title: "Invite link copied" });
  } catch {
    toast({ tone: "error", title: "Couldn't copy", description: url });
  }
}

export function InviteDialog({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const s = usePyo();
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<Role>("developer");
  const [touched, setTouched] = useState(false);
  const [sending, setSending] = useState(false);
  const exists = membersIn(s).some(
    (m) => m.email.toLowerCase() === email.trim().toLowerCase(),
  );
  const error = !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email.trim())
    ? "Enter a valid email address."
    : exists
      ? "Already a member or invited."
      : "";
  const close = () => {
    setEmail("");
    setTouched(false);
    onClose();
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      title="Invite member"
      description={
        s.email
          ? "Existing users are added right away."
          : "Email isn't set up, so share the sign-up link with them."
      }
      onSubmit={async () => {
        setTouched(true);
        if (error || sending) return;
        const address = email.trim().toLowerCase();
        setSending(true);
        const r = await api.inviteMember(address, role);
        setSending(false);
        if (!r) return;
        close();
        if (r.emailed) toast({ title: `Invite emailed to ${address}` });
        else {
          await copyInviteLink(r.link);
          toast({
            tone: r.emailError ? "error" : "info",
            title: r.emailError ?? `Invited ${address}`,
            description: "The sign-up link is copied. Send it to them.",
          });
        }
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={sending}>
            {sending ? "Sending…" : s.email ? "Send invite" : "Invite"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Field label="Email" error={touched ? error || undefined : undefined}>
          {(p) => (
            <Input
              {...p}
              type="email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              placeholder="teammate@company.com"
            />
          )}
        </Field>
        <Field label="Role" hint={ROLES.find((r) => r.value === role)?.body}>
          {(p) => (
            <Select
              {...p}
              value={role}
              onChange={(e) => setRole(e.target.value as Role)}
            >
              {ROLES.filter((r) => r.value !== "owner").map((r) => (
                <option key={r.value} value={r.value}>
                  {r.label}
                </option>
              ))}
            </Select>
          )}
        </Field>
      </div>
    </Dialog>
  );
}
