import {
  ArrowClockwiseIcon,
  ArrowUpIcon,
  DownloadSimpleIcon,
  FileIcon,
  FolderIcon,
  LinkSimpleIcon,
  UploadSimpleIcon,
} from "@phosphor-icons/react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Button } from "~/components/ui/Button";
import { Input } from "~/components/ui/Field";
import { EmptyState, ErrorState, Skeleton } from "~/components/ui/Panel";
import { megabytes, timeAgo } from "~/lib/pyo/format";
import { daemonApi, type FileEntry, type Sandbox } from "~/lib/pyo/sandbox";
import { toast } from "~/lib/pyo/toast";

// The sandbox's files, through its server's daemon: browse a folder,
// download a file, upload into the folder. Files over 64 MB go through the
// SDK or the CLI instead.

const join = (dir: string, name: string) => `${dir.replace(/\/$/, "")}/${name}`;
const parent = (dir: string) => dir.replace(/\/[^/]+\/?$/, "") || "/";

function size(bytes: number) {
  return bytes < 1024 ? `${bytes} B` : bytes < 1024 * 1024 ? `${Math.round(bytes / 1024)} KB` : megabytes(bytes / 1024 / 1024);
}

export function SandboxFiles({ sandbox }: { sandbox: Sandbox }) {
  const [dir, setDir] = useState("/");
  const [typed, setTyped] = useState("/");
  const [entries, setEntries] = useState<FileEntry[] | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const upload = useRef<HTMLInputElement>(null);
  const running = sandbox.state === "running";

  const load = useCallback(
    async (path: string) => {
      setError("");
      setEntries(null);
      try {
        const list = await daemonApi.list(sandbox.id, path);
        list.sort((a, b) =>
          a.kind === "dir" && b.kind !== "dir" ? -1 : b.kind === "dir" && a.kind !== "dir" ? 1 : a.name.localeCompare(b.name),
        );
        setEntries(list);
      } catch (e) {
        setError((e as Error).message);
      }
    },
    [sandbox.id],
  );

  useEffect(() => {
    setTyped(dir);
    if (running) void load(dir);
  }, [dir, running, load]);

  const download = async (f: FileEntry) => {
    try {
      const blob = await daemonApi.read(sandbox.id, f.path);
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = f.name;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 10_000);
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    }
  };

  const send = async (files: FileList | null) => {
    if (!files?.length) return;
    setBusy(true);
    try {
      for (const f of Array.from(files)) await daemonApi.write(sandbox.id, join(dir, f.name), f);
      toast({ title: files.length === 1 ? `Uploaded ${files[0].name}` : `Uploaded ${files.length} files` });
      void load(dir);
    } catch (e) {
      toast({ tone: "error", title: (e as Error).message });
    } finally {
      setBusy(false);
      if (upload.current) upload.current.value = "";
    }
  };

  if (!running)
    return <EmptyState icon={FolderIcon} title="Not running" description="Resume the sandbox to see its files." />;

  return (
    <div>
      <form
        className="flex flex-wrap items-center gap-2 border-b border-border px-4 py-2.5"
        onSubmit={(e) => {
          e.preventDefault();
          setDir(typed.trim() || "/");
        }}
      >
        <Button
          variant="ghost"
          size="icon"
          aria-label="Up one folder"
          disabled={dir === "/"}
          onClick={() => setDir(parent(dir))}
        >
          <ArrowUpIcon size={14} />
        </Button>
        <Input
          mono
          aria-label="Folder"
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          className="min-w-0 flex-1 basis-40"
        />
        <Button variant="ghost" size="icon" aria-label="Refresh" onClick={() => void load(dir)}>
          <ArrowClockwiseIcon size={14} />
        </Button>
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => upload.current?.click()}>
          <UploadSimpleIcon size={13} /> {busy ? "Uploading…" : "Upload"}
        </Button>
        <input ref={upload} type="file" multiple hidden onChange={(e) => void send(e.target.files)} />
      </form>
      {error ? (
        <ErrorState title="Couldn't list this folder" description={error} onRetry={() => void load(dir)} />
      ) : !entries ? (
        <div className="space-y-2 p-4">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-5 w-full" />
          ))}
        </div>
      ) : !entries.length ? (
        <EmptyState compact title="Empty folder" />
      ) : (
        <ul className="list-rows">
          {entries.map((f) => (
            <li key={f.path} className="flex items-center gap-3 px-4 py-2 text-[13px]">
              {f.kind === "dir" ? (
                <FolderIcon size={15} className="shrink-0 text-muted-foreground" />
              ) : f.kind === "symlink" ? (
                <LinkSimpleIcon size={15} className="shrink-0 text-muted-foreground" />
              ) : (
                <FileIcon size={15} className="shrink-0 text-muted-foreground" />
              )}
              {f.kind === "dir" ? (
                <button type="button" className="min-w-0 flex-1 truncate text-left font-mono text-[12.5px] hover:underline" onClick={() => setDir(f.path)}>
                  {f.name}/
                </button>
              ) : (
                <span className="min-w-0 flex-1 truncate font-mono text-[12.5px]">
                  {f.name}
                  {f.target ? <span className="text-muted-foreground"> → {f.target}</span> : null}
                </span>
              )}
              <span className="hidden w-20 shrink-0 text-right text-[12px] tabular-nums text-muted-foreground sm:block">
                {f.kind === "file" ? size(f.size) : ""}
              </span>
              <span className="hidden w-20 shrink-0 text-right text-[12px] text-muted-foreground sm:block">
                {f.modified ? timeAgo(f.modified) : ""}
              </span>
              <span className="w-8 shrink-0">
                {f.kind === "file" ? (
                  <Button variant="ghost" size="icon" className="h-7 w-7" aria-label={`Download ${f.name}`} onClick={() => void download(f)}>
                    <DownloadSimpleIcon size={14} />
                  </Button>
                ) : null}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
