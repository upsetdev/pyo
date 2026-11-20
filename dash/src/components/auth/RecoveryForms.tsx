import { ArrowRightIcon } from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useEffect, useState } from "react";
import { auth, needsCode } from "~/lib/pyo/backend";
import type { User } from "~/lib/pyo/types";
import { Alert } from "../ui/Alert";
import { Button } from "../ui/Button";
import { inputClass, labelClass } from "./AuthForm";
import { CodeStep } from "./CodeStep";

function Heading({ title, sub }: { title: string; sub: string }) {
  return (
    <div className="text-center">
      <h1 className="text-[22px] font-semibold tracking-tight text-foreground">
        {title}
      </h1>
      <p className="mt-1.5 text-[13.5px] text-muted-foreground">{sub}</p>
    </div>
  );
}

function BackToSignIn() {
  return (
    <p className="mt-6 text-center text-[13px] text-muted-foreground">
      <Link
        to="/login"
        className="font-medium text-foreground transition hover:text-primary"
      >
        Back to sign in
      </Link>
    </p>
  );
}

/** "Forgot password?": a sign-in link by email. Says the same whether or not an account has the address. */
export function ForgotForm() {
  const [email, setEmail] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [sent, setSent] = useState("");

  async function submit(event: FormEvent) {
    event.preventDefault();
    const address = email.trim().toLowerCase();
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(address))
      return setError("Enter a valid email address.");
    setLoading(true);
    setError("");
    try {
      await auth.forgot(address);
      setSent(address);
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work. Try again.");
    }
    setLoading(false);
  }

  if (sent)
    return (
      <>
        <Heading
          title="Check your email"
          sub={`If ${sent} has an account, a sign-in link is on its way. It works once, for 30 minutes.`}
        />
        <BackToSignIn />
      </>
    );

  return (
    <>
      <Heading title="Forgot password?" sub="We'll email you a sign-in link." />
      <form onSubmit={submit} noValidate className="mt-8 space-y-4">
        <div>
          <label htmlFor="email" className={labelClass}>
            Email
          </label>
          <input
            id="email"
            className={inputClass}
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            placeholder="you@example.com"
            type="email"
            autoComplete="email"
            autoFocus
          />
        </div>
        {error ? <Alert>{error}</Alert> : null}
        <Button type="submit" size="lg" disabled={loading} className="!mt-6 w-full">
          {loading ? "Sending…" : "Send link"}
          {loading ? null : <ArrowRightIcon size={16} weight="bold" aria-hidden />}
        </Button>
      </form>
      <BackToSignIn />
    </>
  );
}

function signedIn(user: User) {
  window.location.assign(user.mustSetUp ? "/setup" : "/");
}

/** A sign-in link: choose a new password, then on into Pyo. */
export function ResetForm({ token }: { token: string }) {
  const [email, setEmail] = useState<string | null>(null);
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [dead, setDead] = useState("");
  const [loading, setLoading] = useState(false);
  /** Two-factor is on: the new password is saved, a code signs in. */
  const [challenge, setChallenge] = useState<string | null>(null);

  useEffect(() => {
    if (!token) {
      setDead("This link is incomplete. Copy the whole link and try again.");
      return;
    }
    auth
      .signInLink(token)
      .then((r) => setEmail(r.email))
      .catch((e) =>
        setDead(e instanceof Error ? e.message : "This link doesn't work."),
      );
  }, [token]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (password.length < 10)
      return setError("Use at least 10 characters for the password.");
    setLoading(true);
    setError("");
    try {
      const res = await auth.resetPassword(token, password);
      if (needsCode(res)) return setChallenge(res.challenge);
      signedIn(res);
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work. Try again.");
      setLoading(false);
    }
  }

  if (challenge)
    return (
      <CodeStep
        challenge={challenge}
        onDone={signedIn}
        // The password is saved: signing in again uses it.
        onRestart={() => window.location.assign("/login")}
      />
    );

  if (dead)
    return (
      <>
        <Heading title="Choose a new password" sub="" />
        <div className="mt-8">
          <Alert>{dead}</Alert>
        </div>
        <BackToSignIn />
      </>
    );

  return (
    <>
      <Heading
        title="Choose a new password"
        sub={email ? `For ${email}` : "Checking the link…"}
      />
      <form onSubmit={submit} noValidate className="mt-8 space-y-4">
        <input type="email" hidden readOnly value={email ?? ""} autoComplete="username" />
        <div>
          <label htmlFor="password" className={labelClass}>
            New password
            <span className="font-normal text-muted-foreground"> · 10+ characters</span>
          </label>
          <input
            id="password"
            className={inputClass}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            type="password"
            autoComplete="new-password"
            autoFocus
          />
        </div>
        {error ? <Alert>{error}</Alert> : null}
        <Button
          type="submit"
          size="lg"
          disabled={loading || !email}
          className="!mt-6 w-full"
        >
          {loading ? "Saving…" : "Save and sign in"}
          {loading ? null : <ArrowRightIcon size={16} weight="bold" aria-hidden />}
        </Button>
      </form>
    </>
  );
}
