import { AppLink } from "~/components/ui/AppLink";
import { Callout } from "~/components/ui/Panel";
import { fetchMeta } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";

/** On this machine, with Pyo not yet on its tailnet over HTTPS: how to
 * reach it from other devices. Nothing anywhere else. */
export function RemoteAccessNote() {
  const meta = useQuery("meta", fetchMeta);
  const local = typeof window !== "undefined" && ["localhost", "127.0.0.1", "[::1]"].includes(window.location.hostname);
  if (!local || meta.status !== "success" || meta.data.publicUrl !== null) return null;
  return (
    <Callout tone="neutral">
      To open Pyo from other devices,{" "}
      <AppLink href="/integrations#tailscale" className="font-medium text-foreground hover:text-primary">
        set up Tailscale
      </AppLink>
      .
    </Callout>
  );
}
