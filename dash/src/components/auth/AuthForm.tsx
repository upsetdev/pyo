import { fieldState } from "~/components/ui/fieldStyles";
import { ArrowRightIcon, FingerprintIcon } from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { site } from "~/config/site";
import { auth } from "~/lib/pyo/backend";
import {
  passkeyAutofillSupported,
  passkeyCancelled,
  passkeysSupported,
  signInWithPasskey,
} from "~/lib/pyo/passkey";
import type { User } from "~/lib/pyo/types";
import { safeNextPath } from "~/lib/validation";
import { Alert } from "../ui/Alert";
import { Button } from "../ui/Button";

export const labelClass = "mb-1.5 block text-[12.5px] font-medium text-foreground";

export const inputClass =
  `h-11 w-full rounded-full px-4 text-[14px] placeholder:text-muted-foreground/80 ${fieldState}`;

/** Sign in or create an account with an email and password (the Rust API checks both). */
export function AuthForm({
  intent,
  next,
  invite,
}: {
  intent: "login" | "signup";
  next?: string;
  /** An invite link's secret. */
  invite?: string;
}) {
  const signup = intent === "signup";
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [invited, setInvited] = useState<{
    email: string;
    team: string;
  } | null>(null);
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [signupOpen, setSignupOpen] = useState(true);
  const [canPasskey, setCanPasskey] = useState(false);
  const autofill = useRef<AbortController | null>(null);

  useEffect(() => {
    auth
      .config(invite)
      .then((c) => {
        setSignupOpen(c.signupOpen);
        setInvited(c.invite);
        if (c.invite) setEmail(c.invite.email);
      })
      .catch(() => undefined);
  }, [invite]);

  // Passkeys: a button, and offered in the email field where the browser can.
  useEffect(() => {
    if (signup || !passkeysSupported()) return;
    setCanPasskey(true);
    let stopped = false;
    void passkeyAutofillSupported().then(async (ok) => {
      if (!ok || stopped) return;
      const ctl = new AbortController();
      autofill.current = ctl;
      try {
        done(await signInWithPasskey(ctl.signal));
      } catch (e) {
        if (!ctl.signal.aborted && !passkeyCancelled(e))
          setError(e instanceof Error ? e.message : "That passkey didn't work.");
      }
    });
    return () => {
      stopped = true;
      autofill.current?.abort();
    };
  }, [signup]);

  function done(user: User) {
    // A full load so the dashboard starts from a fresh snapshot.
    // Signing up with nowhere else to go: onboarding, this once.
    const to = safeNextPath(next);
    window.location.assign(
      user.mustSetUp
        ? "/setup"
        : signup && !invite && to === "/"
          ? "/?onboarding=1"
          : to,
    );
  }

  async function passkey() {
    autofill.current?.abort();
    setLoading(true);
    setError("");
    try {
      done(await signInWithPasskey());
    } catch (e) {
      if (!passkeyCancelled(e))
        setError(e instanceof Error ? e.message : "That passkey didn't work.");
      setLoading(false);
    }
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    const address = email.trim().toLowerCase();
    if (signup && !name.trim()) return setError("Enter your name.");
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(address))
      return setError("Enter a valid email address.");
    if (signup && password.length < 10)
      return setError("Use at least 10 characters for the password.");
    if (!password) return setError("Enter your password.");
    setLoading(true);
    setError("");
    try {
      const user = signup
        ? await auth.signup(name.trim(), address, password, invite)
        : await auth.login(address, password);
      done(user);
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work. Try again.");
      setLoading(false);
    }
  }

  return (
    <>
      <div className="text-center">
        <h1 className="text-[22px] font-semibold tracking-tight text-foreground">
          {signup
            ? invited
              ? `Join ${invited.team}`
              : "Create your account"
            : `Sign in to ${site.name}`}
        </h1>
        <p className="mt-1.5 text-[13.5px] text-muted-foreground">
          {signup
            ? invited
              ? "Create an account with this email to join."
              : "You'll start with a team of your own."
            : "Welcome back."}
        </p>
      </div>

      {signup && !signupOpen ? (
        <div className="mt-8">
          <Alert>
            {invite
              ? "This invite link has expired or was replaced. Ask for a new one."
              : "Sign-up is closed. Ask a team owner to invite you."}
          </Alert>
        </div>
      ) : (
        <form onSubmit={submit} noValidate className="mt-8 space-y-4">
          {signup ? (
            <div>
              <label htmlFor="name" className={labelClass}>
                Name
              </label>
              <input
                id="name"
                className={inputClass}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Ada Lovelace"
                autoComplete="name"
                autoFocus
              />
            </div>
          ) : null}
          <div>
            <label htmlFor="email" className={labelClass}>
              Email
            </label>
            <input
              id="email"
              className={inputClass}
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              readOnly={!!invited}
              placeholder="you@example.com"
              type="email"
              autoComplete={signup ? "email" : "username webauthn"}
              autoFocus={!signup}
            />
          </div>
          <div>
            <label htmlFor="password" className={labelClass}>
              Password
              {signup ? (
                <span className="font-normal text-muted-foreground">
                  {" "}
                  · 10+ characters
                </span>
              ) : null}
            </label>
            <input
              id="password"
              className={inputClass}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              type="password"
              autoComplete={signup ? "new-password" : "current-password"}
            />
          </div>
          {error ? <Alert>{error}</Alert> : null}
          <Button
            type="submit"
            size="lg"
            disabled={loading}
            className="!mt-6 w-full"
          >
            {loading
              ? signup
                ? "Creating account…"
                : "Signing in…"
              : signup
                ? "Create account"
                : "Sign in"}
            {loading ? null : (
              <ArrowRightIcon size={16} weight="bold" aria-hidden />
            )}
          </Button>
          {canPasskey ? (
            <Button
              variant="secondary"
              size="lg"
              disabled={loading}
              onClick={passkey}
              className="w-full"
            >
              <FingerprintIcon size={16} aria-hidden />
              Sign in with passkey
            </Button>
          ) : null}
        </form>
      )}

      {signup || signupOpen ? (
        <p className="mt-6 text-center text-[13px] text-muted-foreground">
          {signup ? "Already have an account? " : "New here? "}
          <Link
            to={signup ? "/login" : "/signup"}
            search={
              signup && invite
                ? { next: `/invite?${new URLSearchParams({ invite })}` }
                : next
                  ? { next }
                  : {}
            }
            className="font-medium text-foreground transition hover:text-primary"
          >
            {signup ? "Sign in" : "Create an account"}
          </Link>
        </p>
      ) : null}
    </>
  );
}
