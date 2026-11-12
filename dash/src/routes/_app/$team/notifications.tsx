import {
  BellIcon,
  EnvelopeSimpleIcon,
  SlackLogoIcon,
} from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { Badge } from "~/components/ui/Badge";
import { Switch } from "~/components/ui/Field";
import { Page, PageHeader, Panel } from "~/components/ui/Panel";
import { seo } from "~/lib/seo";
import { api, isMuted, NOTIFICATION_TOPICS, usePyo } from "~/lib/pyo/store";

// What reaches you, and where. Topics filter the bell in the header; muted
// events still appear in the activity log.
export const Route = createFileRoute("/_app/$team/notifications")({
  head: () => ({ meta: seo({ title: "Notifications" }) }),
  component: () => (
    <Page width="max-w-4xl">
      <PageHeader title="Notifications" />
      <Notifications />
    </Page>
  ),
});

function Notifications() {
  const s = usePyo();
  return (
    <div className="space-y-4">
      <Panel title="Notify me about">
        <ul className="list-rows">
          {NOTIFICATION_TOPICS.map((t) => (
            <li key={t.id} className="px-4 py-3">
              <Switch
                checked={!isMuted(s, t.id)}
                onChange={(on) => api.setNotificationTopic(t.id, on)}
                label={t.label}
                description={t.description}
              />
            </li>
          ))}
        </ul>
      </Panel>
      <Panel title="Channels">
        <ul className="list-rows">
          <Channel
            icon={<BellIcon size={15} />}
            title="In-app"
            body="The bell in the header."
            right={<Badge tone="success">On</Badge>}
          />
          <Channel
            icon={<EnvelopeSimpleIcon size={15} />}
            title="Email"
            body={`To ${s.user.email}`}
            right={<Badge>Not yet</Badge>}
          />
          <Channel
            icon={<SlackLogoIcon size={15} />}
            title="Slack and webhooks"
            body="Post events to a channel."
            right={<Badge>Not yet</Badge>}
          />
        </ul>
      </Panel>
    </div>
  );
}

function Channel({
  icon,
  title,
  body,
  right,
}: {
  icon: ReactNode;
  title: string;
  body: string;
  right: ReactNode;
}) {
  return (
    <li className="flex items-center gap-3 px-4 py-3">
      <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground">
        {icon}
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-[13px] font-medium">{title}</p>
        <p className="truncate text-[12px] text-muted-foreground">{body}</p>
      </div>
      {right}
    </li>
  );
}
