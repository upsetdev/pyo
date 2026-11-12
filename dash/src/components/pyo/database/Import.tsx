import { ArrowClockwiseIcon, FileArrowUpIcon, TrashIcon, WarningIcon } from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { Button } from "~/components/ui/Button";
import { Field, Input } from "~/components/ui/Field";
import { Callout, Panel } from "~/components/ui/Panel";
import { DUMP_ACCEPT, dumpError, importing, retryImport, uploadDump, type ImportSource } from "~/lib/pyo/imports";
import { DB_SECTION, api, usePyo } from "~/lib/pyo/store";
import { currentSlug, teamPath } from "~/lib/pyo/paths";
import { toast } from "~/lib/pyo/toast";
import type { Database } from "~/lib/pyo/types";

// Importing into a Postgres or MySQL database: from another database's URL
// or an uploaded dump. The Import page (/import) takes the source; the
// database's overview shows how it's going.

export type SourceInput = { mode: "url" | "file"; url: string; file: File | null };

type SqlEngine = "postgres" | "mysql";

const EXAMPLE: Record<SqlEngine, string> = {
  postgres: "postgres://user:password@host:5432/db",
  mysql: "mysql://user:password@host:3306/db",
};

/** What's wrong with the source as typed ("" when nothing's typed yet). */
export function sourceError(engine: SqlEngine, v: SourceInput) {
  if (v.mode === "file") return v.file ? dumpError(engine, v.file.name) : "";
  const u = v.url.trim();
  if (!u) return "";
  const ok = engine === "postgres" ? /^postgres(ql)?:\/\/.+@.+\/.+/.test(u) : /^mysql:\/\/.+@.+\/.+/.test(u);
  return ok ? "" : `Like ${EXAMPLE[engine]}.`;
}

export const sourceReady = (engine: SqlEngine, v: SourceInput) =>
  !sourceError(engine, v) && (v.mode === "file" ? !!v.file : !!v.url.trim());

/** Hides a URL's password: postgres://user:••••@host/db. */
export const maskUrl = (u: string) => u.replace(/^([a-z0-9+.-]+:\/\/[^:@/\s]*:)([^@/\s]+)@/i, (_, head: string, pw: string) => `${head}${"•".repeat(Math.min(pw.length, 8))}@`);

/** The source database's URL, its password hidden while not editing it. */
export function UrlField({ engine, value, onChange, disabled }: { engine: SqlEngine; value: string; onChange: (v: string) => void; disabled?: boolean }) {
  const [editing, setEditing] = useState(false);
  const error = sourceError(engine, { mode: "url", url: value, file: null });
  return (
    <Field label="Connection URL" hint={error ? undefined : "Reachable from the cluster."} error={error || undefined}>
      {(p) => (
        <Input
          {...p}
          mono
          autoComplete="off"
          spellCheck={false}
          placeholder={EXAMPLE[engine]}
          value={editing ? value : maskUrl(value)}
          disabled={disabled}
          onFocus={() => setEditing(true)}
          onBlur={() => setEditing(false)}
          onChange={(e) => onChange(e.target.value)}
        />
      )}
    </Field>
  );
}

/** A dump file: dropped, or picked. `progress`: its upload's, 0 to 1. */
export function DumpField({
  engine,
  file,
  onChange,
  disabled,
  progress,
}: {
  engine: SqlEngine;
  file: File | null;
  onChange: (f: File | null) => void;
  disabled?: boolean;
  progress?: number | null;
}) {
  const input = useRef<HTMLInputElement>(null);
  const [over, setOver] = useState(false);
  const error = file ? dumpError(engine, file.name) : "";
  const types = engine === "postgres" ? ".sql, .sql.gz or .dump" : ".sql or .sql.gz";
  return (
    <div className="space-y-2">
      <input
        ref={input}
        type="file"
        accept={DUMP_ACCEPT[engine]}
        className="hidden"
        tabIndex={-1}
        aria-hidden
        onChange={(e) => {
          onChange(e.target.files?.[0] ?? null);
          e.target.value = "";
        }}
      />
      <button
        type="button"
        disabled={disabled}
        onClick={() => input.current?.click()}
        onDragOver={(e) => {
          e.preventDefault();
          if (!disabled) setOver(true);
        }}
        onDragLeave={() => setOver(false)}
        onDrop={(e) => {
          e.preventDefault();
          setOver(false);
          const f = e.dataTransfer.files?.[0];
          if (f && !disabled) onChange(f);
        }}
        aria-describedby="dump-types"
        className={`flex w-full flex-col items-center justify-center gap-2 rounded-xl border border-dashed px-4 py-8 text-center outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-60 ${
          over ? "border-primary bg-primary/[0.06]" : error ? "border-destructive/50" : "border-border-strong hover:bg-accent/60"
        }`}
      >
        <FileArrowUpIcon size={22} className={file && !error ? "text-primary" : "text-muted-foreground"} />
        {file ? (
          <span className="min-w-0 max-w-full">
            <span className="block truncate font-mono text-[13px] text-foreground">{file.name}</span>
            <span className="mt-0.5 block text-[12px] text-muted-foreground">{fileSize(file.size)} · Choose another</span>
          </span>
        ) : (
          <span className="text-[13px] text-foreground">
            Drop a dump here, or <span className="font-medium text-primary">choose a file</span>
          </span>
        )}
      </button>
      <p id="dump-types" className={`text-[11.5px] ${error ? "text-destructive" : "text-muted-foreground"}`}>
        {error || types}
      </p>
      {progress !== null && progress !== undefined ? (
        <div className="space-y-1" role="status" aria-live="polite">
          <div className="h-1.5 overflow-hidden rounded-full bg-muted">
            <div className="h-full rounded-full bg-primary transition-[width]" style={{ width: `${Math.round(progress * 100)}%` }} />
          </div>
          <p className="text-[12px] text-muted-foreground">{progress < 1 ? `Uploading… ${Math.round(progress * 100)}%` : "Uploaded"}</p>
        </div>
      ) : null}
    </div>
  );
}

export function fileSize(b: number) {
  if (b < 1024 * 1024) return `${Math.max(1, Math.round(b / 1024))} KB`;
  if (b < 1024 ** 3) return `${(b / 1024 / 1024).toFixed(1)} MB`;
  return `${(b / 1024 ** 3).toFixed(2)} GB`;
}

/** A signal for each upload, aborted when the form goes away. */
export function useUploadSignal() {
  const ref = useRef<AbortController | null>(null);
  useEffect(() => () => ref.current?.abort(), []);
  return () => {
    ref.current?.abort();
    ref.current = new AbortController();
    return ref.current.signal;
  };
}

/** Upload the file if there is one: the source an import takes. */
export async function resolveSource(
  teamId: string,
  engine: SqlEngine,
  v: SourceInput,
  onProgress: (p: number) => void,
  signal?: AbortSignal,
): Promise<ImportSource> {
  if (v.mode === "url") return { url: v.url.trim() };
  if (!v.file) throw new Error("Choose a file.");
  const r = await uploadDump(teamId, engine, v.file, onProgress, signal);
  return { uploadId: r.uploadId };
}

/** On the overview: an import under way, or one that failed. */
export function ImportPanel({ d }: { d: Database }) {
  const s = usePyo();
  const navigate = useNavigate();
  const [busy, setBusy] = useState(false);
  const imp = d.import;
  if (!imp || imp.state === "succeeded") return null;
  const role = s.members.find((m) => m.teamId === s.currentTeamId && m.email === s.user.email)?.role;
  const admin = role === "owner" || role === "admin";
  if (importing(d))
    return (
      <Panel title="Importing">
        <div className="space-y-2 p-4" role="status" aria-live="polite">
          <div className="h-1.5 overflow-hidden rounded-full bg-muted">
            <div className="h-full w-full animate-pulse rounded-full bg-primary/60" />
          </div>
          <p className="text-[13px]">{imp.phase || (imp.state === "pending" ? "Waiting for the database" : "Running")}</p>
          <p className="truncate text-[12px] text-muted-foreground">From {imp.from}</p>
        </div>
      </Panel>
    );
  const retry = async () => {
    setBusy(true);
    try {
      await retryImport(imp.id);
      toast({ title: `Importing into ${d.name} again` });
    } catch (e) {
      toast({ tone: "error", title: e instanceof Error ? e.message : "That didn't work." });
    } finally {
      setBusy(false);
    }
  };
  const remove = async () => {
    setBusy(true);
    const r = await api.deleteDatabase(d.id);
    setBusy(false);
    if ("error" in r) return toast({ tone: "error", title: r.error });
    void navigate({ href: teamPath(currentSlug(), `/${DB_SECTION[d.engine]}`) });
  };
  return (
    <Callout tone="danger" icon={WarningIcon} title="Import failed">
      <div className="space-y-2.5">
        <p className="break-words">{imp.message || "It stopped. Its output is in Logs."}</p>
        <p className="text-muted-foreground">{imp.newDatabase ? "Nothing was kept." : "The database was left empty."}</p>
        {admin ? (
          <div className="flex flex-wrap gap-2">
            {imp.retryable ? (
              <Button size="xs" variant="secondary" disabled={busy} onClick={() => void retry()}>
                <ArrowClockwiseIcon size={13} />
                Retry
              </Button>
            ) : null}
            {imp.newDatabase ? (
              <Button size="xs" variant="ghost" disabled={busy} onClick={() => void remove()}>
                <TrashIcon size={13} />
                Delete database
              </Button>
            ) : null}
          </div>
        ) : null}
      </div>
    </Callout>
  );
}
