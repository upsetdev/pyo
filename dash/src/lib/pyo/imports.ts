// Imports into built-in databases (crates/pyo-api/src/database_imports.rs):
// upload a dump (with progress), then start, check or retry an import.
// Starting one checks it on the cluster first, which can take a minute the
// first time (the check's image is pulled).

import { ApiError } from "./backend";
import { refresh } from "./store";
import type { Database } from "./types";

export type ImportSource = { url: string } | { uploadId: string };

export type ImportStarted = {
  importId: string;
  databaseId: string;
  clusterId: string;
  source: { version?: string; sizeBytes?: number } | null;
  extensions: { turnedOn: string[]; madeByImport: string[] };
};

export async function post<T>(path: string, body: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method: "POST",
      credentials: "same-origin",
      headers: { "x-pyo-csrf": "1", "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch {
    throw new ApiError("Can't reach the Pyo API. Check your connection.", 0);
  }
  const data = await res.json().catch(() => null);
  if (!res.ok) throw new ApiError((data && typeof data.error === "string" && data.error) || `Request failed (${res.status})`, res.status);
  return data as T;
}

/** The dump formats an engine takes, for a file input's `accept`. */
export const DUMP_ACCEPT: Record<"postgres" | "mysql", string> = {
  postgres: ".sql,.gz,.dump,.backup",
  mysql: ".sql,.gz",
};

/** A file's problem before uploading it, if any. */
export function dumpError(engine: "postgres" | "mysql", name: string) {
  const n = name.toLowerCase();
  if (n.endsWith(".sql") || n.endsWith(".sql.gz")) return "";
  if (engine === "postgres" && (n.endsWith(".dump") || n.endsWith(".backup"))) return "";
  return engine === "postgres" ? "A .sql, .sql.gz or .dump (pg_dump -Fc) file." : "A .sql or .sql.gz file (mysqldump).";
}

/** Where to send a dump's file: `createDatabaseDumpUpload`'s answer. */
type UploadTarget = { uploadId: string; uploadUrl: string; method: string; headers: Record<string, string>; expiresAt: number };

/**
 * Upload a dump into the team's storage, in two steps: say what it is, then
 * send it where Pyo says. That URL may be on another host, so it gets only
 * the headers Pyo gives (no cookies). `onProgress` gets 0 to 1.
 */
export async function uploadDump(
  teamId: string,
  engine: "postgres" | "mysql",
  file: File,
  onProgress: (p: number) => void,
  signal?: AbortSignal,
): Promise<{ uploadId: string }> {
  const t = await post<UploadTarget>("/api/v1/database-imports/uploads", {
    teamId,
    engine,
    filename: file.name,
    size: file.size,
    contentType: file.type || undefined,
  });
  if (signal?.aborted) throw new ApiError("The upload was stopped.", 0);
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open(t.method || "PUT", new URL(t.uploadUrl, window.location.origin).toString());
    xhr.withCredentials = false;
    for (const [k, v] of Object.entries(t.headers ?? {})) xhr.setRequestHeader(k, v);
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress(e.loaded / e.total);
    };
    const stop = () => xhr.abort();
    signal?.addEventListener("abort", stop, { once: true });
    const done = () => signal?.removeEventListener("abort", stop);
    xhr.onabort = () => {
      done();
      reject(new ApiError("The upload was stopped.", 0));
    };
    xhr.onerror = () => {
      done();
      reject(new ApiError("The upload broke off. Check your connection.", 0));
    };
    xhr.onload = () => {
      done();
      if (xhr.status >= 200 && xhr.status < 300) return resolve({ uploadId: t.uploadId });
      let error = "";
      try {
        const data = JSON.parse(xhr.responseText) as { error?: unknown };
        if (typeof data?.error === "string") error = data.error;
      } catch {
        error = "";
      }
      reject(new ApiError(error || `Upload failed (${xhr.status})`, xhr.status));
    };
    xhr.send(file);
  });
}

/** What a check found: the source's version and size, and the extensions turned on first. */
export type ImportChecked = Pick<ImportStarted, "source" | "extensions">;

/** Where an import goes: an empty database of the team's, or a new one (fields as createDatabase takes them). */
export type ImportInto = { databaseId: string } | { newDatabase: Record<string, unknown> };

/**
 * Everything starting an import checks, without importing: the cluster
 * connects to the source, and the database to import into must be empty.
 */
export function checkImport(teamId: string, engine: "postgres" | "mysql", into: ImportInto, source: ImportSource) {
  return post<ImportChecked>("/api/v1/database-imports/check", { teamId, engine, ...into, ...source });
}

/** Import into an empty database of the team's. */
export async function importInto(d: Pick<Database, "id" | "teamId" | "engine">, source: ImportSource) {
  const r = await post<ImportStarted>("/api/v1/database-imports", { teamId: d.teamId, engine: d.engine, databaseId: d.id, ...source });
  void refresh().catch(() => undefined);
  return r;
}

/** A new database made for the import (fields as createDatabase takes them). */
export async function importNew(teamId: string, engine: "postgres" | "mysql", newDatabase: Record<string, unknown>, source: ImportSource) {
  const r = await post<ImportStarted>("/api/v1/database-imports", { teamId, engine, newDatabase, ...source });
  void refresh().catch(() => undefined);
  return r;
}

/** Run a failed import again, from the same source. */
export async function retryImport(importId: string) {
  const r = await post<{ importId: string }>(`/api/v1/database-imports/${encodeURIComponent(importId)}/retry`, {});
  void refresh().catch(() => undefined);
  return r;
}

/** Whether an import is under way. */
export const importing = (d: Pick<Database, "import">) => d.import?.state === "pending" || d.import?.state === "running";
