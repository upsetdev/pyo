import {
  DotsThreeIcon,
  LinkIcon,
  PaperPlaneTiltIcon,
  UserMinusIcon,
  UserPlusIcon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { UserAvatar } from "~/components/ui/UserAvatar";
import { useState } from "react";
import { TimeAgo } from "~/components/pyo/common";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Select } from "~/components/ui/Field";
import { Menu } from "~/components/ui/Menu";
import { Page, PageHeader, Panel } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";
import { Table, Td, Th, Tr } from "~/components/ui/Table";
import { api, membersIn, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { Role, TeamMember } from "~/lib/pyo/types";
import {
  copyInviteLink,
  InviteDialog,
  ROLES,
} from "~/components/pyo/InviteDialog";

export const Route = createFileRoute("/_app/$team/members")({
  head: () => ({ meta: seo({ title: "Members" }) }),
  component: () => (
    <Page width="max-w-4xl">
      <PageHeader title="Members" />
      <Members />
    </Page>
  ),
});

function Members() {
  const s = usePyo();
  const members = membersIn(s);
  const [inviting, setInviting] = useState(false);
  const [removing, setRemoving] = useState<TeamMember | null>(null);
  const owners = members.filter((m) => m.role === "owner").length;
  return (
    <div className="space-y-4">
      <Panel
        title="Members"

        actions={
          <Button
            size="xs"
            variant="secondary"
            onClick={() => setInviting(true)}
          >
            <UserPlusIcon size={12} /> Invite
          </Button>
        }
      >
        <Table>
          <thead>
            <tr>
              <Th>Member</Th>
              <Th>Role</Th>
              <Th>Status</Th>
              <Th>Joined</Th>
              <Th className="w-10">
                <span className="sr-only">Actions</span>
              </Th>
            </tr>
          </thead>
          <tbody>
            {members.map((m) => {
              const lastOwner = m.role === "owner" && owners === 1;
              return (
                <Tr key={m.id}>
                  <Td>
                    <div className="flex items-center gap-3">
                      <UserAvatar
                        name={m.name}
                        email={m.email}
                        gravatar={m.gravatar}
                        size={28}
                      />
                      <div className="min-w-0">
                        <p className="font-medium">{m.name || m.email}</p>
                        {m.name ? (
                          <p className="text-[11.5px] text-muted-foreground">
                            {m.email}
                          </p>
                        ) : null}
                      </div>
                    </div>
                  </Td>
                  <Td>
                    <Select
                      className="w-auto"
                      value={m.role}
                      disabled={lastOwner}
                      aria-label={`Role of ${m.email}`}
                      onChange={(e) => {
                        api.updateMemberRole(m.id, e.target.value as Role);
                        toast({
                          title: `${m.name || m.email} is now ${e.target.value}`,
                        });
                      }}
                    >
                      {ROLES.map((r) => (
                        <option key={r.value} value={r.value}>
                          {r.label}
                        </option>
                      ))}
                    </Select>
                  </Td>
                  <Td>
                    {m.status === "invited" ? (
                      <Badge tone="warning">Invited</Badge>
                    ) : (
                      <Badge tone="success">Active</Badge>
                    )}
                  </Td>
                  <Td className="text-muted-foreground">
                    <TimeAgo at={m.joinedAt} />
                  </Td>
                  <Td>
                    <Menu
                      label={`Actions for ${m.email}`}
                      items={[
                        ...(m.status === "invited"
                          ? [
                              {
                                label: "Resend invite",
                                icon: <PaperPlaneTiltIcon size={14} />,
                                disabled: !s.email,
                                hint: s.email
                                  ? undefined
                                  : "Email isn't set up",
                                onSelect: async () => {
                                  const r = await api.resendInvite(m.id);
                                  if (r)
                                    toast({
                                      title: `Invite sent to ${m.email}`,
                                    });
                                },
                              },
                              {
                                label: "Copy invite link",
                                icon: <LinkIcon size={14} />,
                                onSelect: async () => {
                                  const r = await api.resendInvite(m.id, false);
                                  if (r) await copyInviteLink(r.link);
                                },
                              },
                              { type: "separator" as const },
                            ]
                          : []),
                        {
                          label:
                            m.status === "invited"
                              ? "Revoke invite"
                              : "Remove member",
                          icon: <UserMinusIcon size={14} />,
                          danger: true,
                          disabled: lastOwner,
                          hint: lastOwner ? "Last owner" : undefined,
                          onSelect: () => setRemoving(m),
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
                  </Td>
                </Tr>
              );
            })}
          </tbody>
        </Table>
      </Panel>
      <Panel title="Roles">
        <ul className="list-rows">
          {ROLES.map((r) => (
            <li key={r.value} className="flex gap-4 px-4 py-2.5 text-[13px]">
              <span className="w-24 shrink-0 font-medium">{r.label}</span>
              <span className="text-muted-foreground">{r.body}</span>
            </li>
          ))}
        </ul>
      </Panel>
      <InviteDialog open={inviting} onClose={() => setInviting(false)} />
      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Remove ${removing?.name || removing?.email}?`}
        description="They lose access to this team."
        confirmLabel="Remove"
        onConfirm={() => {
          if (!removing) return;
          api.removeMember(removing.id);
          toast({ title: `Removed ${removing.email}` });
        }}
      />
    </div>
  );
}
