import { Callout } from "~/components/ui/Panel";
import { fetchMeta } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { MenuPath } from "~/components/ui/MenuPath";

/** On this machine, with Pyo not yet on its tailnet over HTTPS: how to
 * reach it from other devices. Nothing anywhere else. */
export function RemoteAccessNote() {
  const meta = useQuery("meta", fetchMeta);
  const local = typeof window !== "undefined" && ["localhost", "127.0.0.1", "[::1]"].includes(window.location.hostname);
  if (!local || meta.status !== "success" || meta.data.publicUrl !== null) return null;
  return (
    <Callout tone="neutral">
      Pyo is only on this machine. To use it from other devices, put the machine on Tailscale, turn on MagicDNS
      and HTTPS Certificates (<MenuPath steps={["Tailscale admin console", "DNS"]} />), and run Pyo's setup again.
    </Callout>
  );
}
