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

/** How to send a dump's file: `createDatabaseDumpUpload`'s answer. */
type UploadTarget = { uploadId: string; partSize: number; parts: number };

/** Tries for each request of an upload. */
const TRIES = 6;

const stopped = () => new ApiError("The upload was stopped.", 0);

/** Whether a failed request is worth sending again: no answer, or a passing problem. */
const passing = (e: unknown) => e instanceof ApiError && (e.status === 0 || e.status >= 500 || e.status === 408 || e.status === 429);

function wait(ms: number, signal?: AbortSignal) {
  return new Promise<void>((resolve, reject) => {
    const stop = () => {
      clearTimeout(t);
      reject(stopped());
    };
    const t = setTimeout(() => {
      signal?.removeEventListener("abort", stop);
      resolve();
    }, ms);
    signal?.addEventListener("abort", stop, { once: true });
  });
}

/** Send again on a passing problem, waiting longer each time. */
async function withRetries<T>(send: () => Promise<T>, signal?: AbortSignal): Promise<T> {
  for (let tries = 1; ; tries++) {
    try {
      return await send();
    } catch (e) {
      if (signal?.aborted) throw stopped();
      if (tries >= TRIES || !passing(e)) throw e;
      await wait(Math.min(30, 2 ** tries) * 1000, signal);
    }
  }
}

/** A part's SHA-256 in hex, where the browser can tell (HTTPS or localhost). */
async function sha256(data: ArrayBuffer): Promise<string | undefined> {
  if (!globalThis.crypto?.subtle) return undefined;
  const d = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(d), (b) => b.toString(16).padStart(2, "0")).join("");
}

/** One part of an upload. `onSent` gets the bytes of it sent so far. */
function sendPart(url: string, data: ArrayBuffer, sha: string | undefined, onSent: (n: number) => void, signal?: AbortSignal) {
  return new Promise<void>((resolve, reject) => {
    if (signal?.aborted) return reject(stopped());
    const xhr = new XMLHttpRequest();
    xhr.open("PUT", url);
    xhr.setRequestHeader("x-pyo-csrf", "1");
    xhr.setRequestHeader("content-type", "application/octet-stream");
    if (sha) xhr.setRequestHeader("x-pyo-sha256", sha);
    xhr.upload.onprogress = (e) => onSent(e.loaded);
    const stop = () => xhr.abort();
    signal?.addEventListener("abort", stop, { once: true });
    const done = () => signal?.removeEventListener("abort", stop);
    xhr.onabort = () => {
      done();
      reject(stopped());
    };
    xhr.onerror = () => {
      done();
      reject(new ApiError("The upload broke off. Check your connection.", 0));
    };
    xhr.onload = () => {
      done();
      if (xhr.status >= 200 && xhr.status < 300) return resolve();
      let error = "";
      try {
        const data = JSON.parse(xhr.responseText) as { error?: unknown };
        if (typeof data?.error === "string") error = data.error;
      } catch {
        error = "";
      }
      reject(new ApiError(error || `Upload failed (${xhr.status})`, xhr.status));
    };
    xhr.send(data);
  });
}

/**
 * Upload a dump into the team's storage: say what it is, then send it in
 * the parts Pyo asks for, in order, each sent again on a passing problem.
 * Parts keep each request small enough for a proxy in front (Cloudflare
 * takes at most 100 MB). `onProgress` gets 0 to 1.
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
  const base = `/api/v1/database-imports/uploads/${encodeURIComponent(t.uploadId)}`;
  let sent = 0;
  for (let n = 0; n < t.parts; n++) {
    if (signal?.aborted) throw stopped();
    const data = await file.slice(n * t.partSize, Math.min(file.size, (n + 1) * t.partSize)).arrayBuffer();
    const sha = await sha256(data);
    await withRetries(() => sendPart(`${base}/parts/${n}`, data, sha, (b) => onProgress((sent + b) / file.size), signal), signal);
    sent += data.byteLength;
    onProgress(sent / file.size);
  }
  await withRetries(() => post(`${base}/complete`, {}), signal);
  return { uploadId: t.uploadId };
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
