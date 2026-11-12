import { UsersThreeIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Logo } from "~/components/site/Logo";
import { Button } from "~/components/ui/Button";
import { auth } from "~/lib/pyo/backend";
import { usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

// An invite link opened while signed in (crates/pyo-api/src/auth.rs): the
// invite is accepted by its link, with the invited address's account.
export const Route = createFileRoute("/_app/invite")({
  head: () => ({ meta: seo({ title: "Join a team" }) }),
  component: AcceptInvite,
});

function AcceptInvite() {
  const s = usePyo();
  const [invite] = useState(
    () => new URLSearchParams(window.location.search).get("invite") ?? "",
  );
  const [info, setInfo] = useState<{ email: string; team: string } | null>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    auth
      .config(invite)
      .then((c) => setInfo(c.invite))
      .catch(() => setInfo(null));
  }, [invite]);

  const join = async () => {
    setBusy(true);
    try {
      const { team } = await auth.acceptInvite(invite);
      // A full load so the snapshot has the new team.
      window.location.assign(`/${team}`);
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  };

  const wrongAccount = info && info.email !== s.user.email;
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
          <div className="p-6 text-center">
            <span className="mx-auto mb-4 flex size-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
              <UsersThreeIcon size={18} />
            </span>
            <h1 className="text-[17px] font-semibold tracking-tight">
              {info ? `Join ${info.team}` : "Join a team"}
            </h1>
            <p className="mt-1.5 text-[13px] text-muted-foreground">
              {info === undefined
                ? "Checking the invite…"
                : !info
                  ? "This invite link has expired or was replaced. Ask for a new one."
                  : (error ??
                    (wrongAccount
                      ? `This invite is for ${info.email}. Sign in with that address to accept it.`
                      : `As ${s.user.email}`))}
            </p>
          </div>
          {info && !wrongAccount ? (
            <div className="flex gap-2 border-t border-border p-4">
              <Button
                variant="secondary"
                className="flex-1"
                disabled={busy}
                onClick={() => window.location.assign("/")}
              >
                Cancel
              </Button>
              <Button
                className="flex-1"
                disabled={busy}
                onClick={() => void join()}
              >
                {busy ? "Joining…" : "Join"}
              </Button>
            </div>
          ) : null}
        </div>
      </div>
    </main>
  );
}
