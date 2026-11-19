import { PaperPlaneTiltIcon, PlusIcon, TrashIcon, WebhooksLogoIcon } from "@phosphor-icons/react";
import { useState, type ReactNode } from "react";
import { siDiscord, siSlack, siTelegram } from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import { IntegrationCard } from "~/components/pyo/IntegrationCard";
import {
  Busy,
  ConfirmNote,
  DialogRow,
  DialogRows,
  DialogSection,
  IntegrationDialog,
} from "~/components/pyo/IntegrationDialog";
import { useTeamAdmin } from "~/components/pyo/database/NewDatabase";
import type { Tone } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Checkbox, Field, Input, SecretInput } from "~/components/ui/Field";
import { timeAgo } from "~/lib/pyo/format";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { AlertChannel, AlertEvent, AlertKind } from "~/lib/pyo/types";

// Alerts: the team's important events sent to Telegram, Discord, Slack or a
// webhook. Per team, set up by owners and admins. Each kind is a card, its
// dialog lists that kind's channels.

const EVENTS: { id: AlertEvent; label: string }[] = [
  { id: "deploy_failed", label: "Deploy failed" },
  { id: "server_offline", label: "Server offline and back online" },
  { id: "backup_failed", label: "Backup failed" },
  { id: "certificate", label: "Certificate expiring or not renewed" },
];

const KINDS: {
  kind: AlertKind;
  name: string;
  description: string;
  icon: (size: number) => ReactNode;
}[] = [
  {
    kind: "telegram",
    name: "Telegram",
    description: "A bot posts to a chat.",
    icon: (size) => <BrandMark icon={siTelegram} size={size} />,
  },
  {
    kind: "discord",
    name: "Discord",
    description: "Posts to a channel.",
    icon: (size) => <BrandMark icon={siDiscord} size={size} />,
  },
  {
    kind: "slack",
    name: "Slack",
    description: "Posts to a channel.",
    icon: (size) => <BrandMark icon={siSlack} size={size} />,
  },
  {
    kind: "webhook",
    name: "Webhook",
    description: "Signed JSON to your URL.",
    icon: (size) => <WebhooksLogoIcon size={size} />,
  },
];

const kindOf = (k: AlertKind) => KINDS.find((x) => x.kind === k)!;

/** Its last send failed, and nothing went out since. */
const failing = (c: AlertChannel) =>
  !!c.lastError && (c.lastErrorAt ?? 0) >= (c.lastSentAt ?? 0);

function lastResult(c: AlertChannel) {
  if (failing(c)) return `Last failed: ${c.lastError}`;
  if (c.lastSentAt) return `Last sent ${timeAgo(c.lastSentAt)}`;
  return "Nothing sent yet";
}

const plural = (n: number, one: string) => `${n} ${n === 1 ? one : `${one}s`}`;

/** Notifications › Alerts: one card per kind. */
export function AlertsSection() {
  const s = usePyo();
  const admin = useTeamAdmin();
  const [open, setOpen] = useState<AlertKind | null>(null);
  const all = s.alertChannels.filter((c) => c.teamId === s.currentTeamId);
  return (
    <section aria-labelledby="alerts-title" className="space-y-3">
      <h2 id="alerts-title" className="text-[13px] font-medium">
        Alerts
      </h2>
      <div className="grid gap-3 sm:grid-cols-2">
        {KINDS.map((k) => {
          const channels = all.filter((c) => c.kind === k.kind);
          const bad = channels.filter(failing);
          const status: { tone: Tone; label: string } = !channels.length
            ? { tone: "neutral", label: "Not set up" }
            : bad.length
              ? { tone: "danger", label: "Needs attention" }
              : { tone: "success", label: "On" };
          const detail = !channels.length
            ? null
            : channels.length === 1
              ? lastResult(channels[0])
              : bad.length
                ? `${plural(bad.length, "channel")} failing`
                : plural(channels.length, "channel");
          return (
            <IntegrationCard
              key={k.kind}
              id={`alerts-${k.kind}`}
              name={k.name}
              icon={k.icon(18)}
              description={k.description}
              status={status}
              detail={detail}
              action={
                admin ? (
                  <Button
                    size="xs"
                    variant="secondary"
                    aria-label={`${channels.length ? "Manage" : "Set up"} ${k.name}`}
                    onClick={() => setOpen(k.kind)}
                  >
                    {channels.length ? "Manage" : "Set up"}
                  </Button>
                ) : (
                  <span className="shrink-0 text-[12px] text-muted-foreground">Ask an admin</span>
                )
              }
            />
          );
        })}
      </div>
      {open ? <AlertChannelsDialog kind={open} onClose={() => setOpen(null)} /> : null}
    </section>
  );
}

/** One kind's channels, and the form to add or change one. Opens on the form while there are none. */
function AlertChannelsDialog({ kind, onClose }: { kind: AlertKind; onClose: () => void }) {
  const s = usePyo();
  const k = kindOf(kind);
  const channels = s.alertChannels.filter((c) => c.teamId === s.currentTeamId && c.kind === kind);
  const [form, setForm] = useState<"add" | AlertChannel | null>(channels.length ? null : "add");

  if (form)
    return (
      <ChannelForm
        key={form === "add" ? "add" : form.id}
        kind={kind}
        channel={form === "add" ? undefined : form}
        onDone={() => setForm(null)}
        onCancel={() => (channels.length ? setForm(null) : onClose())}
        onClose={onClose}
      />
    );

  return (
    <IntegrationDialog
      icon={k.icon(15)}
      name={k.name}
      description="Alerts for this team."
      docs="alerts"
      onClose={onClose}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => setForm("add")}>
            <PlusIcon size={12} weight="bold" /> Add channel
          </Button>
          <Button size="sm" onClick={onClose}>
            Done
          </Button>
        </>
      }
    >
      <DialogRows>
        {channels.map((c) => (
          <ChannelRow key={c.id} channel={c} icon={k.icon(15)} onEdit={() => setForm(c)} />
        ))}
      </DialogRows>
    </IntegrationDialog>
  );
}

function ChannelRow({
  channel: c,
  icon,
  onEdit,
}: {
  channel: AlertChannel;
  icon: ReactNode;
  onEdit: () => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const [testing, setTesting] = useState(false);
  const test = async () => {
    setTesting(true);
    const r = await api.testAlertChannel(c.id);
    setTesting(false);
    if ("error" in r) toast({ tone: "error", title: r.error });
    else toast({ title: `Test sent to ${c.name}` });
  };
  return (
    <DialogRow
      icon={icon}
      title={c.name}
      meta={
        <>
          {c.hint} ·{" "}
          <span className={failing(c) ? "text-destructive" : undefined}>{lastResult(c)}</span>
        </>
      }
      actions={
        confirming ? null : (
          <>
            <Button size="xs" variant="ghost" disabled={testing} onClick={() => void test()}>
              {testing ? (
                <Busy>Sending…</Busy>
              ) : (
                <>
                  <PaperPlaneTiltIcon size={12} /> Send test
                </>
              )}
            </Button>
            <Button size="xs" variant="ghost" onClick={onEdit}>
              Edit
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-7 w-7"
              aria-label={`Remove ${c.name}`}
              onClick={() => setConfirming(true)}
            >
              <TrashIcon size={14} />
            </Button>
          </>
        )
      }
    >
      {confirming ? (
        <ConfirmNote
          title={`Remove ${c.name}?`}
          confirmLabel="Remove"
          onCancel={() => setConfirming(false)}
          onConfirm={() => {
            setConfirming(false);
            void api.removeAlertChannel(c.id);
          }}
        >
          Alerts stop going there.
        </ConfirmNote>
      ) : null}
    </DialogRow>
  );
}

type Form = {
  name: string;
  webhookUrl: string;
  token: string;
  chatId: string;
  secret: string;
  events: AlertEvent[];
};

const URL_PLACEHOLDER: Record<AlertKind, string> = {
  telegram: "",
  discord: "https://discord.com/api/webhooks/…",
  slack: "https://hooks.slack.com/services/…",
  webhook: "https://example.com/pyo",
};

/** Add a channel, or change one. Its token or URL is never shown again. */
function ChannelForm({
  kind,
  channel,
  onDone,
  onCancel,
  onClose,
}: {
  kind: AlertKind;
  channel?: AlertChannel;
  onDone: () => void;
  onCancel: () => void;
  onClose: () => void;
}) {
  const k = kindOf(kind);
  const [form, setForm] = useState<Form>({
    name: channel?.name ?? k.name,
    webhookUrl: "",
    token: "",
    chatId: "",
    secret: "",
    events: channel?.events ?? EVENTS.map((e) => e.id),
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const set = (patch: Partial<Form>) => setForm({ ...form, ...patch });
  const t = (v: string) => v.trim() || undefined;
  const ready =
    !!form.name.trim() &&
    form.events.length > 0 &&
    (!!channel ||
      (kind === "telegram" ? !!form.token.trim() && !!form.chatId.trim() : !!form.webhookUrl.trim()));
  const unchanged = channel ? "Unchanged" : undefined;

  const submit = async () => {
    if (!ready || busy) return;
    setBusy(true);
    setError("");
    const fields = {
      name: form.name.trim(),
      events: form.events,
      webhookUrl: t(form.webhookUrl),
      token: t(form.token),
      chatId: t(form.chatId),
      secret: t(form.secret),
    };
    const r = channel
      ? await api.updateAlertChannel(channel.id, fields)
      : await api.addAlertChannel({ kind, ...fields });
    setBusy(false);
    if ("error" in r) return setError(r.error);
    toast({ title: channel ? "Saved" : `Added ${fields.name}` });
    onDone();
  };

  const secretField = (label: string, key: "webhookUrl" | "token" | "secret", placeholder?: string, hint?: string, optional?: boolean) => (
    <Field label={label} hint={hint} optional={optional}>
      {(p) => (
        <SecretInput
          {...p}
          autoComplete="off"
          value={form[key]}
          onChange={(e) => set({ [key]: e.target.value })}
          placeholder={unchanged ?? placeholder}
        />
      )}
    </Field>
  );

  return (
    <IntegrationDialog
      icon={k.icon(15)}
      name={k.name}
      description={channel ? `Edit ${channel.name}.` : "Checked before saving."}
      docs="alerts"
      onClose={onClose}
      onSubmit={() => void submit()}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onCancel}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={!ready || busy}>
            {busy ? <Busy>Checking…</Busy> : channel ? "Save" : "Add"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Field label="Name">
          {(p) => (
            <Input {...p} autoComplete="off" value={form.name} onChange={(e) => set({ name: e.target.value })} />
          )}
        </Field>
        {kind === "telegram" ? (
          <>
            {secretField("Bot token", "token", "123456:ABC-DEF…", "From @BotFather.")}
            <Field label="Chat ID" hint="Add the bot to the chat first.">
              {(p) => (
                <Input
                  {...p}
                  autoComplete="off"
                  value={form.chatId}
                  onChange={(e) => set({ chatId: e.target.value })}
                  placeholder={unchanged ?? "-1001234567890"}
                />
              )}
            </Field>
          </>
        ) : (
          secretField("Webhook URL", "webhookUrl", URL_PLACEHOLDER[kind])
        )}
        {kind === "webhook"
          ? secretField("Secret", "secret", "", "Signs each request.", true)
          : null}
        <DialogSection title="Send">
          <div className="grid gap-2">
            {EVENTS.map((e) => (
              <Checkbox
                key={e.id}
                label={e.label}
                checked={form.events.includes(e.id)}
                onChange={(on) =>
                  set({
                    events: on
                      ? EVENTS.map((x) => x.id).filter((x) => x === e.id || form.events.includes(x))
                      : form.events.filter((x) => x !== e.id),
                  })
                }
              />
            ))}
          </div>
        </DialogSection>
        {error ? (
          <p role="alert" className="text-[12.5px] text-destructive">
            {error}
          </p>
        ) : null}
      </div>
    </IntegrationDialog>
  );
}
