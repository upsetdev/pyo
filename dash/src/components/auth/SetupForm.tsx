import { ArrowRightIcon } from "@phosphor-icons/react";
import { type FormEvent, useState } from "react";
import { site } from "~/config/site";
import { auth } from "~/lib/pyo/backend";
import { Alert } from "../ui/Alert";
import { Button } from "../ui/Button";
import { inputClass, labelClass } from "./AuthForm";

/** The first sign-in: swaps the login the installer printed for the admin's own. */
export function SetupForm() {
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    const address = email.trim().toLowerCase();
    if (!name.trim()) return setError("Enter your name.");
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(address))
      return setError("Enter a valid email address.");
    if (password.length < 10)
      return setError("Use at least 10 characters for the password.");
    setLoading(true);
    setError("");
    try {
      await auth.setup(name.trim(), address, password);
      window.location.assign("/?onboarding=1");
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work. Try again.");
      setLoading(false);
    }
  }

  return (
    <>
      <div className="text-center">
        <h1 className="text-[22px] font-semibold tracking-tight text-foreground">
          Set up {site.name}
        </h1>
        <p className="mt-1.5 text-[13.5px] text-muted-foreground">
          Replace the login from the installer with your own.
        </p>
      </div>

      <form onSubmit={submit} noValidate className="mt-8 space-y-4">
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
          />
        </div>
        <div>
          <label htmlFor="password" className={labelClass}>
            New password
            <span className="font-normal text-muted-foreground">
              {" "}
              · 10+ characters
            </span>
          </label>
          <input
            id="password"
            className={inputClass}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            type="password"
            autoComplete="new-password"
          />
        </div>
        {error ? <Alert>{error}</Alert> : null}
        <Button
          type="submit"
          size="lg"
          disabled={loading}
          className="!mt-6 w-full"
        >
          {loading ? "Saving…" : "Finish setup"}
          {loading ? null : (
            <ArrowRightIcon size={16} weight="bold" aria-hidden />
          )}
        </Button>
      </form>
    </>
  );
}
