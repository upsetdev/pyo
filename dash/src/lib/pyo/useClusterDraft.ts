import { useEffect, useRef, useState } from "react";
import { opOnLeave } from "./backend";
import { DNS_LABEL, newId } from "./format";
import { RESERVED_CLUSTER_NAMES } from "./location";
import { api, clustersIn, usePyo } from "./store";
import { useLiveCommand } from "./useLiveCommand";

export type ClusterDraft = {
  id: string;
  token: string;
  command: string;
  thisMachine: boolean;
  /** Over the public internet instead of the tailnet. */
  publicNet: boolean;
  /** A Tailscale key is saved, and this Pyo is on a tailnet. */
  tailscale: boolean;
  tailnet: boolean;
};

/**
 * The cluster behind an add-cluster form. It's made as the form opens (that
 * issues its agent token) and edits after that rename it. Leaving without
 * `keep` takes it back, unless its agent has connected (the API checks).
 */
export function useClusterDraft(name = "", tags: string[] = [], publicNet = false) {
  const s = usePyo();
  const [draft, setDraft] = useState<ClusterDraft | null>(null);
  const draftRef = useRef<ClusterDraft | null>(null);
  const kept = useRef(false);
  useEffect(() => {
    const discard = () => {
      const d = draftRef.current;
      if (d && !kept.current) opOnLeave("discardClusterDraft", { id: d.id });
    };
    window.addEventListener("pagehide", discard);
    return () => {
      window.removeEventListener("pagehide", discard);
      discard();
    };
  }, []);
  // One request at a time, so a slow create can't be made twice.
  const chain = useRef<Promise<unknown>>(Promise.resolve());
  // "This machine": the one Pyo runs on. First thing to connect for a new team.
  const local = clustersIn(s).find((c) => c.sameCluster);
  const otherLocal = local && local.id !== draft?.id ? local : undefined;
  const [where, setWhere] = useState<"local" | "other">(() =>
    !local && !clustersIn(s).length ? "local" : "other",
  );
  const thisMachine = where === "local";

  // Blank: named after the server's hostname once the agent reports in.
  const nameError = !name
    ? ""
    : !DNS_LABEL.test(name)
      ? "Lowercase letters, digits and dashes."
      : RESERVED_CLUSTER_NAMES.includes(name)
        ? "That name is reserved."
        : clustersIn(s).some((c) => c.name === name && c.id !== draft?.id)
          ? "A cluster with this name exists."
          : "";
  const showForm = !(thisMachine && otherLocal);
  const cluster = draft ? s.clusters.find((x) => x.id === draft.id) : undefined;
  const connected = !!cluster && cluster.status !== "pending";

  const live = useLiveCommand<ClusterDraft>(
    `${where}|${name}|${tags.join(",")}|${publicNet}`,
    // Connected: its token isn't to be changed from here.
    showForm && !nameError && !connected,
    () => {
      const next = chain.current.then(async () => {
        const d = draftRef.current;
        if (d && d.thisMachine === thisMachine && d.publicNet === publicNet) {
          const r = await api.updateClusterDraft(d.id, name, tags);
          return "error" in r ? r : { data: d };
        }
        // Switched machine, or tailnet and public internet: start over.
        if (d) await api.discardClusterDraft(d.id);
        const id = newId("cl");
        const r = await api.draftCluster({
          id,
          name,
          tags,
          thisMachine,
          ...(publicNet && !thisMachine ? { publicApiUrl: window.location.origin } : {}),
        });
        if ("error" in r) {
          draftRef.current = null;
          setDraft(null);
          return r;
        }
        const made = {
          id,
          token: r.data.token,
          command: r.data.command,
          thisMachine,
          publicNet,
          tailscale: r.data.tailscale !== false,
          tailnet: r.data.tailnet !== false,
        };
        draftRef.current = made;
        setDraft(made);
        return { data: made };
      });
      chain.current = next.catch(() => undefined);
      return next;
    },
  );

  return {
    where,
    setWhere,
    thisMachine,
    nameError,
    showForm,
    otherLocal,
    draft,
    cluster,
    connected,
    live,
    /** Keep the cluster after leaving: "Finish later". */
    keep: () => {
      kept.current = true;
    },
    /** Take the cluster back now and don't try again on leaving. */
    cancel: async () => {
      const d = draftRef.current;
      kept.current = true;
      if (d && !connected) await api.discardClusterDraft(d.id);
    },
  };
}
