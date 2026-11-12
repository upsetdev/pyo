import {
  FingerprintIcon,
  PencilSimpleIcon,
  TrashIcon,
} from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog, Dialog } from "~/components/ui/Dialog";
import { Field, Input } from "~/components/ui/Field";
import { Panel } from "~/components/ui/Panel";
import { ApiError, passkeys, type Passkey } from "~/lib/pyo/backend";
import { timeAgo } from "~/lib/pyo/format";
import {
  addPasskey,
  passkeyCancelled,
  passkeysSupported,
} from "~/lib/pyo/passkey";
import { toast } from "~/lib/pyo/toast";

const failed = (e: unknown) =>
  toast({
    tone: "error",
    title: e instanceof Error ? e.message : "That didn't work.",
  });

// The signed-in user's passkeys (passkeys.rs). Each works at the host name
// it was added at; that's shown when it isn't this one.
export function PasskeysPanel() {
  const [list, setList] = useState<Passkey[] | null>(null);
  const [asking, setAsking] = useState(false);
  const [password, setPassword] = useState("");
  const [passwordError, setPasswordError] = useState("");
  const [adding, setAdding] = useState(false);
  const [renaming, setRenaming] = useState<Passkey | null>(null);
  const [name, setName] = useState("");
  const [removing, setRemoving] = useState<Passkey | null>(null);
  const here = typeof window === "undefined" ? "" : window.location.hostname;

  const load = () =>
    passkeys
      .list()
      .then(setList)
      .catch((e) => {
        setList([]);
        failed(e);
      });
  useEffect(() => {
    void load();
  }, []);

  async function add() {
    if (!password || adding) return;
    setAdding(true);
    setPasswordError("");
    try {
      const p = await addPasskey(password);
      setAsking(false);
      toast({ title: "Passkey added", description: p.name });
      await load();
    } catch (e) {
      if (e instanceof ApiError && (e.status === 403 || e.status === 429))
        setPasswordError(e.message);
      else {
        setAsking(false);
        if (!passkeyCancelled(e)) failed(e);
      }
    }
    setPassword("");
    setAdding(false);
  }

  return (
    <Panel
      title="Passkeys"
      actions={
        passkeysSupported() ? (
          <Button
            size="sm"
            variant="secondary"
            onClick={() => {
              setPassword("");
              setPasswordError("");
              setAsking(true);
            }}
          >
            Add passkey
          </Button>
        ) : null
      }
    >
      {list?.length ? (
        <ul className="list-rows">
          {list.map((p) => (
            <li key={p.id} className="flex items-center gap-3 px-4 py-3">
              <FingerprintIcon
                size={15}
                className="shrink-0 text-muted-foreground"
              />
              <div className="min-w-0 flex-1">
                <p className="truncate text-[13px] font-medium">{p.name}</p>
                <p className="truncate text-[12px] text-muted-foreground">
                  {p.rpId !== here ? `${p.rpId} · ` : ""}added{" "}
                  {timeAgo(p.createdAt)} ·{" "}
                  {p.lastUsedAt ? `used ${timeAgo(p.lastUsedAt)}` : "never used"}
                </p>
              </div>
              <Button
                variant="ghost"
                size="icon"
                aria-label={`Rename ${p.name}`}
                onClick={() => {
                  setName(p.name);
                  setRenaming(p);
                }}
              >
                <PencilSimpleIcon size={14} />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                aria-label={`Delete ${p.name}`}
                onClick={() => setRemoving(p)}
              >
                <TrashIcon size={14} />
              </Button>
            </li>
          ))}
        </ul>
      ) : (
        <p className="px-4 py-6 text-center text-[13px] text-muted-foreground">
          {list ? "No passkeys yet." : "Loading…"}
        </p>
      )}

      <Dialog
        open={asking}
        onClose={() => !adding && setAsking(false)}
        title="Add passkey"
        onSubmit={add}
        footer={
          <>
            <Button variant="ghost" size="sm" onClick={() => setAsking(false)}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={!password || adding}>
              {adding ? "Adding…" : "Continue"}
            </Button>
          </>
        }
      >
        <Field label="Password" error={passwordError || undefined}>
          {(p) => (
            <Input
              {...p}
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          )}
        </Field>
      </Dialog>

      <Dialog
        open={!!renaming}
        onClose={() => setRenaming(null)}
        title="Rename passkey"
        onSubmit={async () => {
          if (!renaming || !name.trim()) return;
          try {
            await passkeys.rename(renaming.id, name.trim());
            setRenaming(null);
            await load();
          } catch (e) {
            failed(e);
          }
        }}
        footer={
          <>
            <Button variant="ghost" size="sm" onClick={() => setRenaming(null)}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={!name.trim()}>
              Save
            </Button>
          </>
        }
      >
        <Field label="Name">
          {(p) => (
            <Input
              {...p}
              value={name}
              onChange={(e) => setName(e.target.value)}
              maxLength={60}
            />
          )}
        </Field>
      </Dialog>

      <ConfirmDialog
        open={!!removing}
        onClose={() => setRemoving(null)}
        title={`Delete "${removing?.name}"?`}
        description="You can still sign in with your password."
        confirmLabel="Delete passkey"
        onConfirm={async () => {
          if (!removing) return;
          try {
            await passkeys.remove(removing.id);
            toast({ title: "Passkey deleted" });
            await load();
          } catch (e) {
            failed(e);
          }
        }}
      />
    </Panel>
  );
}
