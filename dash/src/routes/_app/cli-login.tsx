import { TerminalWindowIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState, type ReactNode } from "react";
import { Logo } from "~/components/site/Logo";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { ApiError, cliLogin } from "~/lib/pyo/backend";
import { usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

// `pyo login`: the CLI asks Pyo for a sign-in, shows a code and opens this
// page with the request's id. Typing the code approves it, and the CLI,
// which polls Pyo, gets its API key (crates/pyo-api/src/cli.rs). Nothing
// goes from here to the CLI. Signed-out visitors log in first.
export const Route = createFileRoute("/_app/cli-login")({
  head: () => ({ meta: seo({ title: "Sign in to the CLI" }) }),
  component: CliLogin,
});

/** Letters only, in capitals, as ABCD-EFGH. */
function formatCode(v: string) {
  const l = v
    .toUpperCase()
    .replace(/[^A-Z]/g, "")
    .slice(0, 8);
  return l.length > 4 ? `${l.slice(0, 4)}-${l.slice(4)}` : l;
}

type Request = Awaited<ReturnType<typeof cliLogin.get>>;

function CliLogin() {
  const s = usePyo();
  const [id] = useState(
    () => new URLSearchParams(window.location.search).get("id") ?? "",
  );
  const [req, setReq] = useState<Request | null>(null);
  const [ended, setEnded] = useState<string | null>(
    id ? null : "This link is incomplete. Run pyo login again.",
  );
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState<"allow" | "deny" | null>(null);
  const [wrong, setWrong] = useState<string | undefined>();

  useEffect(() => {
    if (!id) return;
    cliLogin
      .get(id)
      .then(setReq)
      .catch((e: Error) => setEnded(e.message));
  }, [id]);

  const allow = async () => {
    setBusy("allow");
    setWrong(undefined);
    try {
      await cliLogin.approve(id, code);
      setReq((r) => r && { ...r, status: "approved" });
    } catch (e) {
      if (e instanceof ApiError && e.status === 400) setWrong(e.message);
      else setEnded((e as Error).message);
    }
    setBusy(null);
  };

  const deny = async () => {
    setBusy("deny");
    try {
      await cliLogin.deny(id);
      setReq((r) => r && { ...r, status: "denied" });
    } catch (e) {
      setEnded((e as Error).message);
    }
    setBusy(null);
  };

  const status = ended ? "ended" : req?.status;

  return (
    <main
      data-ready
      className="flex min-h-dvh items-center justify-center bg-sidebar px-4 py-16 text-foreground"
    >
      <div className="w-full max-w-[400px] animate-fade-up">
        <div className="mx-auto mb-8 flex w-fit">
          <Logo showName={false} className="[&_svg]:h-9 [&_svg]:w-9" />
        </div>
        {status ? (
          <div className="rounded-[14px] border border-card-border bg-card">
            {status === "ended" ? (
              <Note title="Can't sign in the CLI">{ended}</Note>
            ) : status === "approved" ? (
              <Note title="You're signed in">Go back to your terminal.</Note>
            ) : status === "denied" ? (
              <Note title="Sign-in refused">The CLI didn't get a key.</Note>
            ) : (
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  if (code.length === 9 && !busy) void allow();
                }}
              >
                <div className="p-6">
                  <div className="text-center">
                    <span className="mx-auto mb-4 flex size-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
                      <TerminalWindowIcon size={18} />
                    </span>
                    <h1 className="text-[17px] font-semibold tracking-tight">
                      Sign in to the Pyo CLI
                    </h1>
                    <p className="mt-1.5 text-[13px] text-muted-foreground">
                      {req?.name ? (
                        <>
                          On{" "}
                          <span className="font-medium text-foreground">
                            {req.name}
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
                    {req?.client ? (
                      <p className="mt-0.5 text-[12px] text-muted-foreground">
                        {req.client}
                      </p>
                    ) : null}
                  </div>
                  <Field
                    label="Code from your terminal"
                    error={wrong}
                    className="mt-5"
                  >
                    {(p) => (
                      <Input
                        {...p}
                        mono
                        autoFocus
                        autoComplete="off"
                        autoCapitalize="characters"
                        spellCheck={false}
                        placeholder="ABCD-EFGH"
                        value={code}
                        onChange={(e) => {
                          setCode(formatCode(e.target.value));
                          setWrong(undefined);
                        }}
                        className="text-center tracking-[0.2em]"
                      />
                    )}
                  </Field>
                </div>
                <div className="flex gap-2 border-t border-border p-4">
                  <Button
                    type="button"
                    variant="secondary"
                    className="flex-1"
                    disabled={!!busy}
                    onClick={() => void deny()}
                  >
                    Refuse
                  </Button>
                  <Button
                    type="submit"
                    className="flex-1"
                    disabled={!!busy || code.length !== 9}
                  >
                    {busy === "allow" ? "Signing in…" : "Allow"}
                  </Button>
                </div>
              </form>
            )}
          </div>
        ) : null}
        {status === "pending" ? (
          <p className="mt-4 text-center text-[12px] text-muted-foreground">
            It gets an API key you can revoke under API keys.
          </p>
        ) : null}
      </div>
    </main>
  );
}

function Note({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="p-6 text-center">
      <h1 className="text-[17px] font-semibold tracking-tight">{title}</h1>
      <p className="mt-2 text-[13px] text-muted-foreground">{children}</p>
    </div>
  );
}
