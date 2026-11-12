import { TerminalWindowIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { Logo } from "~/components/site/Logo";
import { Button } from "~/components/ui/Button";
import { approveCli } from "~/lib/pyo/backend";
import { usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

// `pyo login`: the CLI opens this page; approving makes a one-time code
// and hands it to the CLI on localhost, which trades it for an API key
// (crates/pyo-api/src/cli.rs). Signed-out visitors log in first.
export const Route = createFileRoute("/_app/cli-login")({
  head: () => ({ meta: seo({ title: "Sign in to the CLI" }) }),
  component: CliLogin,
});

function CliLogin() {
  const s = usePyo();
  const [q] = useState(() => new URLSearchParams(window.location.search));
  const port = Number(q.get("port"));
  const state = q.get("state") ?? "";
  const challenge = q.get("challenge") ?? "";
  const name = (q.get("name") ?? "").slice(0, 40);
  const valid =
    Number.isInteger(port) &&
    port > 1023 &&
    port < 65536 &&
    /^[0-9a-f]{64}$/.test(challenge) &&
    state.length > 0;
  const [busy, setBusy] = useState<"allow" | "deny" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  const back = (params: Record<string, string>) =>
    window.location.assign(
      `http://127.0.0.1:${port}/callback?${new URLSearchParams({ state, ...params })}`,
    );

  const allow = async () => {
    setBusy("allow");
    try {
      const { code } = await approveCli(challenge, name);
      setDone(true);
      back({ code });
    } catch (e) {
      setError((e as Error).message);
      setBusy(null);
    }
  };

  return (
    <main
      data-ready
      className="flex min-h-dvh items-center justify-center bg-sidebar px-4 py-16 text-foreground"
    >
      <div className="w-full max-w-[400px] animate-fade-up">
        <div className="mx-auto mb-8 flex w-fit">
          <Logo showName={false} className="[&_svg]:h-9 [&_svg]:w-9" />
        </div>
        <div className="rounded-[14px] border border-card-border bg-card">
          {!valid || error ? (
            <div className="p-6 text-center">
              <h1 className="text-[17px] font-semibold tracking-tight">
                Can't sign in the CLI
              </h1>
              <p className="mt-2 text-[13px] text-muted-foreground">
                {error ?? "This link is incomplete."} Run{" "}
                <span className="font-mono">pyo login</span> again.
              </p>
            </div>
          ) : done ? (
            <div className="p-6 text-center">
              <h1 className="text-[17px] font-semibold tracking-tight">
                You're signed in
              </h1>
              <p className="mt-2 text-[13px] text-muted-foreground">
                Go back to your terminal.
              </p>
            </div>
          ) : (
            <>
              <div className="p-6 text-center">
                <span className="mx-auto mb-4 flex size-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
                  <TerminalWindowIcon size={18} />
                </span>
                <h1 className="text-[17px] font-semibold tracking-tight">
                  Sign in to the Pyo CLI
                </h1>
                <p className="mt-1.5 text-[13px] text-muted-foreground">
                  {name ? (
                    <>
                      On{" "}
                      <span className="font-medium text-foreground">
                        {name}
                      </span>
                      , as{" "}
                    </>
                  ) : (
                    "As "
                  )}
                  <span className="font-medium text-foreground">
                    {s.user.email}
                  </span>
                </p>
              </div>
              <div className="flex gap-2 border-t border-border p-4">
                <Button
                  variant="secondary"
                  className="flex-1"
                  disabled={!!busy}
                  onClick={() => {
                    setBusy("deny");
                    back({ error: "denied" });
                  }}
                >
                  Cancel
                </Button>
                <Button
                  className="flex-1"
                  disabled={!!busy}
                  onClick={() => void allow()}
                >
                  {busy === "allow" ? "Signing in…" : "Allow"}
                </Button>
              </div>
            </>
          )}
        </div>
        {valid && !error && !done ? (
          <p className="mt-4 text-center text-[12px] text-muted-foreground">
            It gets an API key you can revoke under API keys.
          </p>
        ) : null}
      </div>
    </main>
  );
}
