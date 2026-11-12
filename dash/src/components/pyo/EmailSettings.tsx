import {
  CheckCircleIcon,
  EnvelopeSimpleIcon,
  MinusCircleIcon,
} from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Field, Input, Segmented, Select } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { StatusNote } from "~/components/ui/StatusNote";
import { api, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import type { EmailSettings } from "~/lib/pyo/types";

// Outgoing email for the whole installation (every team): team invites, and
// later notifications. Only the installation's admin edits it; others see
// whether it's on.

export function EmailSettingsSection() {
  const s = usePyo();
  return (
    <EmailPage
      key={JSON.stringify(s.email)}
      saved={s.email}
      admin={!!s.user.isAdmin}
    />
  );
}

type Security = "starttls" | "tls" | "none";

/** Common providers: the host, port and login they expect. */
const PRESETS: {
  id: string;
  label: string;
  host: string;
  port: number;
  security: Security;
  username?: string;
  note?: string;
}[] = [
  {
    id: "resend",
    label: "Resend",
    host: "smtp.resend.com",
    port: 587,
    security: "starttls",
    username: "resend",
    note: "Password: an API key.",
  },
  {
    id: "postmark",
    label: "Postmark",
    host: "smtp.postmarkapp.com",
    port: 587,
    security: "starttls",
    note: "Username and password: a server API token.",
  },
  {
    id: "sendgrid",
    label: "SendGrid",
    host: "smtp.sendgrid.net",
    port: 587,
    security: "starttls",
    username: "apikey",
    note: "Password: an API key.",
  },
  {
    id: "mailgun",
    label: "Mailgun",
    host: "smtp.mailgun.org",
    port: 587,
    security: "starttls",
    note: "Use the domain's SMTP login.",
  },
  {
    id: "ses",
    label: "Amazon SES",
    host: "email-smtp.us-east-1.amazonaws.com",
    port: 587,
    security: "starttls",
    note: "Set your region in the host. Use SMTP credentials.",
  },
  {
    id: "gmail",
    label: "Google Workspace / Gmail",
    host: "smtp.gmail.com",
    port: 587,
    security: "starttls",
    note: "Password: an app password.",
  },
];

function EmailPage({
  saved,
  admin,
}: {
  saved: EmailSettings | null;
  admin: boolean;
}) {
  const [preset, setPreset] = useState(
    PRESETS.find((p) => p.host === saved?.host)?.id ?? "custom",
  );
  const [host, setHost] = useState(saved?.host ?? "");
  const [port, setPort] = useState(String(saved?.port ?? 587));
  const [security, setSecurity] = useState<Security>(
    saved?.security ?? "starttls",
  );
  const [username, setUsername] = useState(saved?.username ?? "");
  const [password, setPassword] = useState("");
  const [fromName, setFromName] = useState(saved?.fromName ?? "Pyo");
  const [fromEmail, setFromEmail] = useState(saved?.fromEmail ?? "");
  const [publicUrl, setPublicUrl] = useState(
    saved?.publicUrl ??
      (typeof window === "undefined" ? "" : window.location.origin),
  );
  const [touched, setTouched] = useState(false);
  const [busy, setBusy] = useState<"save" | "test" | null>(null);
  const [removing, setRemoving] = useState(false);

  const errors = {
    host:
      !host.trim() || /\s/.test(host.trim())
        ? "Enter the server's host name."
        : "",
    port:
      !/^\d+$/.test(port) || +port < 1 || +port > 65535 ? "1 to 65535." : "",
    fromEmail: !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(fromEmail.trim())
      ? "Enter a valid address."
      : "",
    publicUrl: !/^https?:\/\/\S+$/.test(publicUrl.trim())
      ? "Starts with http:// or https://."
      : "",
  };
  const invalid = Object.values(errors).some(Boolean);
  const dirty =
    !saved ||
    host !== saved.host ||
    +port !== saved.port ||
    security !== saved.security ||
    username !== saved.username ||
    !!password ||
    fromName !== saved.fromName ||
    fromEmail !== saved.fromEmail ||
    publicUrl !== saved.publicUrl;
  const presetInfo = PRESETS.find((p) => p.id === preset);
  const err = (k: keyof typeof errors) =>
    touched ? errors[k] || undefined : undefined;

  const status = saved ? (
    <StatusNote
      tone="success"
      icon={<CheckCircleIcon size={15} />}
      title="Email is set up"
    >
      Invites are sent from {saved.fromEmail}.
    </StatusNote>
  ) : (
    <StatusNote
      tone="neutral"
      icon={<MinusCircleIcon size={15} />}
      title="Email isn't set up"
    >
      No emails are sent: share sign-up links yourself.
      {admin
        ? " Add an SMTP server below to send them."
        : " The installation's admin can set it up."}
    </StatusNote>
  );

  if (!admin) return <div className="space-y-4">{status}</div>;

  return (
    <form
      className="space-y-4"
      noValidate
      onSubmit={async (e) => {
        e.preventDefault();
        setTouched(true);
        if (invalid) return;
        setBusy("save");
        const res = await api.saveEmailSettings({
          host: host.trim(),
          port: +port,
          security,
          username: username.trim(),
          password: password || undefined,
          fromEmail: fromEmail.trim(),
          fromName: fromName.trim(),
          publicUrl: publicUrl.trim(),
        });
        setBusy(null);
        if (res) {
          setPassword("");
          toast({
            title: "Email settings saved",
            description: "Send a test email to check them.",
          });
        }
      }}
    >
      {status}

      <Panel title="SMTP server">
        <div className="space-y-4 p-4">
          <Field label="Provider" hint={presetInfo?.note}>
            {(p) => (
              <Select
                {...p}
                value={preset}
                onChange={(e) => {
                  const next = PRESETS.find((x) => x.id === e.target.value);
                  setPreset(e.target.value);
                  if (!next) return;
                  setHost(next.host);
                  setPort(String(next.port));
                  setSecurity(next.security);
                  if (next.username) setUsername(next.username);
                }}
              >
                <option value="custom">Other SMTP server</option>
                {PRESETS.map((x) => (
                  <option key={x.id} value={x.id}>
                    {x.label}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <div className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_120px]">
            <Field label="Host" error={err("host")}>
              {(p) => (
                <Input
                  {...p}
                  mono
                  value={host}
                  onChange={(e) => setHost(e.target.value)}
                  placeholder="smtp.example.com"
                  autoComplete="off"
                />
              )}
            </Field>
            <Field label="Port" error={err("port")}>
              {(p) => (
                <Input
                  {...p}
                  mono
                  inputMode="numeric"
                  value={port}
                  onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
                />
              )}
            </Field>
          </div>
          <Segmented
            label="Encryption"
            value={security}
            onChange={(v) => {
              setSecurity(v);
              setPort(v === "tls" ? "465" : v === "starttls" ? "587" : "25");
            }}
            options={[
              { value: "starttls", label: "STARTTLS (587)" },
              { value: "tls", label: "TLS (465)" },
              { value: "none", label: "None" },
            ]}
          />
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="Username" optional>
              {(p) => (
                <Input
                  {...p}
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  autoComplete="off"
                />
              )}
            </Field>
            <Field
              label="Password"
              optional
              hint={
                saved?.passwordSet && !password
                  ? "Saved. Leave empty to keep it."
                  : undefined
              }
            >
              {(p) => (
                <Input
                  {...p}
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  placeholder={saved?.passwordSet ? "••••••••••" : ""}
                  autoComplete="new-password"
                />
              )}
            </Field>
          </div>
        </div>
      </Panel>

      <Panel title="Sender">
        <div className="space-y-4 p-4">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="From name">
              {(p) => (
                <Input
                  {...p}
                  value={fromName}
                  onChange={(e) => setFromName(e.target.value)}
                  placeholder="Pyo"
                />
              )}
            </Field>
            <Field
              label="From address"
              error={err("fromEmail")}
              hint="A domain your provider lets you send from."
            >
              {(p) => (
                <Input
                  {...p}
                  type="email"
                  value={fromEmail}
                  onChange={(e) => setFromEmail(e.target.value)}
                  placeholder="pyo@example.com"
                />
              )}
            </Field>
          </div>
          <Field
            label="Dashboard URL"
            error={err("publicUrl")}
            hint="The address your team opens Pyo at."
          >
            {(p) => (
              <Input
                {...p}
                mono
                value={publicUrl}
                onChange={(e) => setPublicUrl(e.target.value)}
                placeholder="https://pyo.example.com"
              />
            )}
          </Field>
        </div>
        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-border px-4 py-3">
          {saved ? (
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={dirty || busy !== null}
              title={dirty ? "Save your changes first" : undefined}
              onClick={async () => {
                setBusy("test");
                const res = await api.sendTestEmail();
                setBusy(null);
                if (res)
                  toast({
                    title: "Test email sent",
                    description: `Check ${res.to}.`,
                  });
              }}
            >
              <EnvelopeSimpleIcon size={13} />{" "}
              {busy === "test" ? "Sending…" : "Send test email"}
            </Button>
          ) : null}
          <Button type="submit" size="sm" disabled={!dirty || busy !== null}>
            {busy === "save" ? "Saving…" : "Save"}
          </Button>
        </div>
      </Panel>

      {saved ? (
        <Panel title="Turn off email" tone="danger">
          <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
            <p className="text-[12.5px] text-muted-foreground">
              Invites still work, without emails.
            </p>
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="text-destructive"
              onClick={() => setRemoving(true)}
            >
              Remove
            </Button>
          </div>
        </Panel>
      ) : null}

      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title="Turn off email?"
        description="Email stops for every team."
        confirmLabel="Turn off"
        onConfirm={async () => {
          if (await api.removeEmailSettings())
            toast({ title: "Email turned off" });
        }}
      />
    </form>
  );
}
