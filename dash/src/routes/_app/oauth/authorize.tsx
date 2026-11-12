import { CheckIcon, PlugsConnectedIcon } from "@phosphor-icons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Logo } from "~/components/site/Logo";
import { Button } from "~/components/ui/Button";
import { Segmented } from "~/components/ui/Field";
import { oauth } from "~/lib/pyo/backend";
import { usePyo } from "~/lib/pyo/store";
import { seo } from "~/lib/seo";

// OAuth consent: an assistant (Claude, Cursor, …) asks to use Pyo as the
// signed-in user over MCP. The _app layout already sent signed-out visitors
// to the login page and back here.
export const Route = createFileRoute("/_app/oauth/authorize")({
  head: () => ({ meta: seo({ title: "Connect an assistant" }) }),
  component: AuthorizePage,
});

const PARAMS = [
  "client_id",
  "redirect_uri",
  "response_type",
  "code_challenge",
  "code_challenge_method",
  "state",
  "scope",
  "resource",
];

// The same levels as API keys; the grant's scope carries the choice.
type Access = "full" | "deploy" | "read";
const SCOPE: Record<Access, string> = {
  full: "pyo",
  deploy: "pyo:deploy",
  read: "pyo:read",
};
const CAN: Record<Access, string[]> = {
  full: [
    "Everything you can do in Pyo with your role, in every team you're in",
    "That includes creating, changing and deleting projects, services, clusters and servers",
    "Not your account or this installation's settings: those stay here",
  ],
  deploy: [
    "See your teams, services, servers, logs and metrics",
    "Deploy, roll back, restart, stop and scale services",
    "Nothing else: no creating or deleting",
  ],
  read: [
    "See your teams, services, servers, logs and metrics",
    "Change nothing",
  ],
};

function AuthorizePage() {
  const s = usePyo();
  const [params] = useState(() => {
    const q = new URLSearchParams(window.location.search);
    return Object.fromEntries(
      PARAMS.flatMap((k) => (q.has(k) ? [[k, q.get(k)!]] : [])),
    );
  });
  const [client, setClient] = useState<{
    name: string;
    redirectHost: string;
  } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"allow" | "deny" | null>(null);
  // What the app asked for, else everything.
  const [access, setAccess] = useState<Access>(() => {
    const asked = (params.scope ?? "").split(" ");
    return asked.includes("pyo:read")
      ? "read"
      : asked.includes("pyo:deploy")
        ? "deploy"
        : "full";
  });

  useEffect(() => {
    oauth
      .client(new URLSearchParams(params).toString())
      .then(setClient)
      .catch((e: Error) => setError(e.message));
  }, [params]);

  const answer = async (allow: boolean) => {
    setBusy(allow ? "allow" : "deny");
    try {
      const { redirect } = await oauth.approve(
        { ...params, scope: SCOPE[access] },
        allow,
      );
      window.location.assign(redirect);
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
        {error ? (
          <div className="rounded-[14px] border border-card-border bg-card p-6 text-center">
            <h1 className="text-[17px] font-semibold tracking-tight">
              Can't connect this app
            </h1>
            <p className="mt-2 text-[13px] text-muted-foreground">{error}</p>
            <p className="mt-4 text-[12.5px] text-muted-foreground">
              Start the connection again from your assistant.
            </p>
          </div>
        ) : !client ? (
          <p className="text-center text-[13px] text-muted-foreground">
            Loading…
          </p>
        ) : (
          <div className="rounded-[14px] border border-card-border bg-card">
            <div className="p-6 text-center">
              <span className="mx-auto mb-4 flex size-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
                <PlugsConnectedIcon size={18} />
              </span>
              <h1 className="text-[17px] font-semibold tracking-tight">
                {client.name} wants to use Pyo as you
              </h1>
              {/* The name is the app's own claim; where it returns isn't. */}
              <p className="mt-1.5 break-all font-mono text-[12.5px] text-foreground">
                {client.redirectHost}
              </p>
              <p className="mt-1 text-[13px] text-muted-foreground">
                Signed in as{" "}
                <span className="font-medium text-foreground">
                  {s.user.email}
                </span>
              </p>
            </div>
            <div className="border-t border-border px-6 pt-4">
              <Segmented
                label="Access"
                value={access}
                onChange={setAccess}
                className="w-full"
                options={[
                  { value: "full", label: "Full" },
                  { value: "deploy", label: "Deploy" },
                  { value: "read", label: "Read only" },
                ]}
              />
            </div>
            <ul className="space-y-2 px-6 py-4 text-[13px]">
              {CAN[access].map((t) => (
                <li key={t} className="flex gap-2.5">
                  <CheckIcon
                    size={14}
                    className="mt-0.5 shrink-0 text-muted-foreground"
                  />
                  {t}
                </li>
              ))}
            </ul>
            <div className="flex gap-2 border-t border-border p-4">
              <Button
                variant="secondary"
                className="flex-1"
                disabled={!!busy}
                onClick={() => answer(false)}
              >
                {busy === "deny" ? "Cancelling…" : "Cancel"}
              </Button>
              <Button
                className="flex-1"
                disabled={!!busy}
                onClick={() => answer(true)}
              >
                {busy === "allow" ? "Connecting…" : "Allow"}
              </Button>
            </div>
          </div>
        )}
        {client ? (
          <p className="mt-4 text-center text-[12px] text-muted-foreground">
            Only allow apps you started connecting. Disconnect it any time from
            your profile.
          </p>
        ) : null}
      </div>
    </main>
  );
}
