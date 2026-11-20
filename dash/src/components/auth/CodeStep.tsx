import { ArrowRightIcon } from "@phosphor-icons/react";
import { type FormEvent, useState } from "react";
import { auth } from "~/lib/pyo/backend";
import type { User } from "~/lib/pyo/types";
import { Alert } from "../ui/Alert";
import { Button } from "../ui/Button";
import { inputClass, labelClass } from "./AuthForm";

/**
 * Signing in with two-factor on: after the password (or a sign-in link),
 * a code from the authenticator app, or one of the recovery codes.
 */
export function CodeStep({
  challenge,
  onDone,
  onRestart,
}: {
  challenge: string;
  onDone: (user: User) => void;
  /** The sign-in timed out or ran out of tries: start over, saying why. */
  onRestart: (why: string) => void;
}) {
  const [recovery, setRecovery] = useState(false);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    const value = code.trim();
    if (!value)
      return setError(recovery ? "Enter a recovery code." : "Enter the code.");
    setLoading(true);
    setError("");
    try {
      onDone(await auth.loginCode(challenge, value));
    } catch (e) {
      const message =
        e instanceof Error ? e.message : "That didn't work. Try again.";
      if (/sign in again/i.test(message)) return onRestart(message);
      setError(message);
      setCode("");
      setLoading(false);
    }
  }

  return (
    <>
      <div className="text-center">
        <h1 className="text-[22px] font-semibold tracking-tight text-foreground">
          Two-factor authentication
        </h1>
        <p className="mt-1.5 text-[13.5px] text-muted-foreground">
          {recovery
            ? "Enter one of your recovery codes."
            : "Enter the code from your authenticator app."}
        </p>
      </div>
      <form onSubmit={submit} noValidate className="mt-8 space-y-4">
        <div>
          <label htmlFor="code" className={labelClass}>
            {recovery ? "Recovery code" : "Code"}
          </label>
          <input
            id="code"
            key={recovery ? "recovery" : "app"}
            className={`${inputClass} ${recovery ? "" : "text-center font-mono text-[18px] tracking-[0.4em]"}`}
            value={code}
            onChange={(e) =>
              setCode(
                recovery
                  ? e.target.value
                  : e.target.value.replace(/\D/g, "").slice(0, 6),
              )
            }
            placeholder={recovery ? "xxxxx-xxxxx" : "000000"}
            inputMode={recovery ? "text" : "numeric"}
            autoComplete="one-time-code"
            autoCapitalize="off"
            spellCheck={false}
            autoFocus
          />
        </div>
        {error ? <Alert>{error}</Alert> : null}
        <Button
          type="submit"
          size="lg"
          disabled={loading}
          className="!mt-6 w-full"
        >
          {loading ? "Checking…" : "Continue"}
          {loading ? null : (
            <ArrowRightIcon size={16} weight="bold" aria-hidden />
          )}
        </Button>
      </form>
      <p className="mt-6 text-center text-[13px] text-muted-foreground">
        <button
          type="button"
          onClick={() => {
            setRecovery(!recovery);
            setCode("");
            setError("");
          }}
          className="font-medium text-foreground transition hover:text-primary"
        >
          {recovery ? "Use the app instead" : "Use a recovery code"}
        </button>
      </p>
    </>
  );
}
