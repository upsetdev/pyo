import {
  CopyIcon,
  DeviceMobileIcon,
  DownloadSimpleIcon,
  KeyIcon,
} from "@phosphor-icons/react";
import { type FormEvent, useState } from "react";
import { Alert } from "~/components/ui/Alert";
import { Badge } from "~/components/ui/Badge";
import { Button } from "~/components/ui/Button";
import { Dialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { twoFactor } from "~/lib/pyo/backend";
import { copyText } from "~/lib/pyo/clipboard";
import { plural } from "~/lib/pyo/format";
import { refresh, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";

const message = (e: unknown) =>
  e instanceof Error ? e.message : "That didn't work. Try again.";

// The signed-in account's two-factor authentication (twofactor.rs): set up
// with an authenticator app, recovery codes, turning it off. A required
// account (a team, the installation or Pyo Cloud's staff rule) can't turn
// it off.
export function TwoFactorPanel() {
  const s = usePyo();
  const tf = s.user.twoFactor;
  const [setup, setSetup] = useState(false);
  const [asking, setAsking] = useState<"disable" | "codes" | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const on = !!tf?.enabled;
  const required =
    !!tf?.required ||
    s.teams.some(
      (t) =>
        t.requireTwoFactor &&
        s.members.some(
          (m) =>
            m.teamId === t.id &&
            m.email === s.user.email &&
            m.status === "active",
        ),
    );

  return (
    <Panel
      id="two-factor"
      title="Two-factor authentication"
      actions={
        on ? (
          <Badge tone="success">On</Badge>
        ) : (
          <Button size="sm" variant="secondary" onClick={() => setSetup(true)}>
            Set up
          </Button>
        )
      }
    >
      {on ? (
        <ul className="list-rows">
          <li className="flex items-center gap-3 px-4 py-3">
            <DeviceMobileIcon
              size={15}
              className="shrink-0 text-muted-foreground"
            />
            <div className="min-w-0 flex-1">
              <p className="text-[13px] font-medium">Authenticator app</p>
              <p className="text-[12px] text-muted-foreground">
                Asked for after your password. A passkey is enough on its own.
              </p>
            </div>
            <Button
              size="sm"
              variant="ghost"
              disabled={required}
              title={required ? "Required for your account." : undefined}
              onClick={() => setAsking("disable")}
            >
              Turn off
            </Button>
          </li>
          <li className="flex items-center gap-3 px-4 py-3">
            <KeyIcon size={15} className="shrink-0 text-muted-foreground" />
            <div className="min-w-0 flex-1">
              <p className="text-[13px] font-medium">Recovery codes</p>
              <p className="text-[12px] text-muted-foreground">
                {plural(tf?.recoveryCodesLeft ?? 0, "code")} left
              </p>
            </div>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setAsking("codes")}
            >
              New codes
            </Button>
          </li>
        </ul>
      ) : (
        <div className="space-y-3 p-4">
          <p className="text-[13px] text-muted-foreground">
            A code from an authenticator app after your password.
          </p>
          {tf?.setupNeeded || tf?.lockedTeams.length ? (
            <Alert tone="warning">
              {tf.setupNeeded
                ? "Set up two-factor authentication to use your teams."
                : `${tf.lockedTeams.map((t) => t.name).join(", ")} ${tf.lockedTeams.length === 1 ? "requires" : "require"} two-factor authentication.`}
            </Alert>
          ) : null}
        </div>
      )}

      <SetupDialog
        open={setup}
        onClose={() => setSetup(false)}
        onCodes={(c) => {
          setSetup(false);
          setCodes(c);
        }}
      />
      <CodeDialog
        open={asking !== null}
        title={
          asking === "disable"
            ? "Turn off two-factor"
            : "New recovery codes"
        }
        action={asking === "disable" ? "Turn off" : "Make new codes"}
        danger={asking === "disable"}
        onClose={() => setAsking(null)}
        onCode={async (code) => {
          if (asking === "disable") {
            await twoFactor.disable(code);
            toast({ title: "Two-factor is off" });
          } else {
            setCodes((await twoFactor.recoveryCodes(code)).recoveryCodes);
          }
          setAsking(null);
          await refresh().catch(() => undefined);
        }}
      />
      <RecoveryCodesDialog
        codes={codes}
        email={s.user.email}
        onClose={() => {
          setCodes(null);
          void refresh().catch(() => undefined);
        }}
      />
    </Panel>
  );
}

/** Password, then the QR code and the first code from the app. */
function SetupDialog({
  open,
  onClose,
  onCodes,
}: {
  open: boolean;
  onClose: () => void;
  onCodes: (codes: string[]) => void;
}) {
  const [password, setPassword] = useState("");
  const [secret, setSecret] = useState<{
    secret: string;
    qr: string;
  } | null>(null);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  function close() {
    if (busy) return;
    setPassword("");
    setSecret(null);
    setCode("");
    setError("");
    onClose();
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      if (!secret) {
        const r = await twoFactor.setup(password);
        setSecret({ secret: r.secret, qr: r.qr });
        setPassword("");
      } else {
        const r = await twoFactor.enable(code.trim());
        setSecret(null);
        setCode("");
        onCodes(r.recoveryCodes);
      }
    } catch (err) {
      setError(message(err));
    }
    setBusy(false);
  }

  return (
    <Dialog
      open={open}
      onClose={close}
      title="Set up two-factor"
      description={
        secret
          ? "Scan this with your authenticator app, then enter the code it shows."
          : "Enter your password to start."
      }
      onSubmit={submit}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button
            type="submit"
            size="sm"
            disabled={busy || (secret ? code.length !== 6 : !password)}
          >
            {busy ? "Checking…" : secret ? "Turn on" : "Continue"}
          </Button>
        </>
      }
    >
      {secret ? (
        <div className="space-y-4">
          <div className="flex flex-col items-center gap-3 sm:flex-row sm:items-start">
            <img
              src={`data:image/svg+xml;utf8,${encodeURIComponent(secret.qr)}`}
              alt="QR code for your authenticator app"
              width={168}
              height={168}
              className="shrink-0 rounded-lg border border-border bg-white p-1"
            />
            <div className="min-w-0 space-y-1.5 text-[12.5px] text-muted-foreground">
              <p>Can't scan it? Enter this key in the app.</p>
              <button
                type="button"
                onClick={() =>
                  copyText(secret.secret.replace(/\s/g, "")).then(
                    () => toast({ title: "Key copied" }),
                    () => undefined,
                  )
                }
                className="inline-flex max-w-full items-center gap-1.5 break-all rounded-md border border-border bg-muted/40 px-2 py-1 text-left font-mono text-[12px] text-foreground transition-colors hover:bg-accent"
              >
                {secret.secret}
                <CopyIcon size={12} className="shrink-0" />
              </button>
            </div>
          </div>
          <Field label="Code" error={error || undefined}>
            {(p) => (
              <Input
                {...p}
                value={code}
                onChange={(e) =>
                  setCode(e.target.value.replace(/\D/g, "").slice(0, 6))
                }
                inputMode="numeric"
                autoComplete="one-time-code"
                placeholder="000000"
                className="font-mono tracking-[0.3em]"
                autoFocus
              />
            )}
          </Field>
        </div>
      ) : (
        <Field label="Password" error={error || undefined}>
          {(p) => (
            <Input
              {...p}
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoFocus
            />
          )}
        </Field>
      )}
    </Dialog>
  );
}

/** A code from the app (or a recovery code) before a change. */
function CodeDialog({
  open,
  title,
  action,
  danger,
  onClose,
  onCode,
}: {
  open: boolean;
  title: string;
  action: string;
  danger?: boolean;
  onClose: () => void;
  onCode: (code: string) => Promise<void>;
}) {
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const close = () => {
    if (busy) return;
    setCode("");
    setError("");
    onClose();
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      title={title}
      description="Enter a code from your app, or a recovery code."
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError("");
        try {
          await onCode(code.trim());
          setCode("");
        } catch (err) {
          setError(message(err));
        }
        setBusy(false);
      }}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={close}>
            Cancel
          </Button>
          <Button
            type="submit"
            size="sm"
            variant={danger ? "danger" : "primary"}
            disabled={busy || !code.trim()}
          >
            {busy ? "Checking…" : action}
          </Button>
        </>
      }
    >
      <Field label="Code" error={error || undefined}>
        {(p) => (
          <Input
            {...p}
            value={code}
            onChange={(e) => setCode(e.target.value)}
            autoComplete="one-time-code"
            autoCapitalize="off"
            spellCheck={false}
            autoFocus
          />
        )}
      </Field>
    </Dialog>
  );
}

/** The recovery codes, shown once. */
function RecoveryCodesDialog({
  codes,
  email,
  onClose,
}: {
  codes: string[] | null;
  email: string;
  onClose: () => void;
}) {
  const text = codes
    ? `Pyo recovery codes for ${email}\nEach works once.\n\n${codes.join("\n")}\n`
    : "";
  return (
    <Dialog
      open={!!codes}
      onClose={onClose}
      title="Save your recovery codes"
      description="Each one signs you in once if you lose your app. They're shown only now."
      footer={
        <Button size="sm" onClick={onClose}>
          Done
        </Button>
      }
    >
      <ol className="grid grid-cols-2 gap-x-6 gap-y-1.5 rounded-lg border border-border bg-muted/30 px-4 py-3 font-mono text-[13px]">
        {codes?.map((c) => (
          <li key={c}>{c}</li>
        ))}
      </ol>
      <div className="mt-3 flex flex-wrap gap-2">
        <Button
          size="sm"
          variant="secondary"
          onClick={() =>
            copyText(text).then(
              () => toast({ title: "Codes copied" }),
              () => toast({ tone: "error", title: "Couldn't copy" }),
            )
          }
        >
          <CopyIcon size={14} />
          Copy
        </Button>
        <Button
          size="sm"
          variant="secondary"
          onClick={() => {
            const a = document.createElement("a");
            a.href = URL.createObjectURL(
              new Blob([text], { type: "text/plain" }),
            );
            a.download = "pyo-recovery-codes.txt";
            a.click();
            URL.revokeObjectURL(a.href);
          }}
        >
          <DownloadSimpleIcon size={14} />
          Download
        </Button>
      </div>
    </Dialog>
  );
}
