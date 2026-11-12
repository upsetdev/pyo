import {
  ArrowClockwiseIcon,
  MagicWandIcon,
  TerminalIcon,
  TerminalWindowIcon,
} from "@phosphor-icons/react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useBlocker } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { siGnubash, siZsh } from "simple-icons";
import { BrandMark } from "~/components/pyo/common";
import { Button } from "~/components/ui/Button";
import { ConfirmDialog } from "~/components/ui/Dialog";
import { Select } from "~/components/ui/Field";
import { ZoomButton, Zoomable } from "~/components/ui/Zoom";
import type { Application } from "~/lib/pyo/types";

// A shell in one of the service's running replicas, over a WebSocket to the API
// (crates/pyo-api/src/terminal.rs). Keystrokes and output are binary
// frames; text frames are control messages.

type Frame =
  | { type: "ready"; pod: string }
  | { type: "error"; message: string }
  | { type: "exit" };
type Phase =
  | { kind: "idle" }
  | { kind: "connecting" }
  | { kind: "open"; pod: string }
  | { kind: "closed"; message: string; error?: boolean };

/** The app's theme colors, so the terminal matches light and dark mode. */
function theme() {
  const css = getComputedStyle(document.documentElement);
  const hsl = (name: string) => `hsl(${css.getPropertyValue(name).trim()})`;
  return {
    background: hsl("--background"),
    foreground: hsl("--foreground"),
    cursor: hsl("--foreground"),
    selectionBackground: hsl("--accent"),
  };
}

// Auto starts bash when the image has it, else sh.
const SHELLS = [
  {
    value: "auto",
    label: "Auto",
    icon: (
      <MagicWandIcon size={14} className="shrink-0 text-muted-foreground" />
    ),
  },
  {
    value: "bash",
    label: "bash",
    icon: <BrandMark icon={siGnubash} size={14} />,
  },
  {
    value: "sh",
    label: "sh",
    icon: <TerminalIcon size={14} className="shrink-0 text-muted-foreground" />,
  },
  { value: "zsh", label: "zsh", icon: <BrandMark icon={siZsh} size={14} /> },
  {
    value: "ash",
    label: "ash",
    icon: <TerminalIcon size={14} className="shrink-0 text-muted-foreground" />,
  },
];

export function ServiceTerminal({ app }: { app: Application }) {
  const host = useRef<HTMLDivElement>(null);
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const [session, setSession] = useState(0);
  const [replica, setReplica] = useState("");
  const [shell, setShell] = useState("auto");
  const [zoomed, setZoomed] = useState(false);
  // Leaving the page ends the shell: ask first (and on reload or closing the tab).
  const live = phase.kind === "open" || phase.kind === "connecting";
  const blocker = useBlocker({
    shouldBlockFn: () => live,
    enableBeforeUnload: () => live,
    withResolver: true,
  });
  const leaving = useRef(false);
  const replicas = app.runtime.replicas ?? [];
  // A picked replica that's gone (replaced by a deploy) falls back to any.
  const target = replicas.some((r) => r.name === replica) ? replica : "";
  // Changing either while connected starts a new session with it.
  const pick = (set: (v: string) => void) => (v: string) => {
    set(v);
    if (session) setSession((n) => n + 1);
  };

  useEffect(() => {
    if (!session || !host.current) return;
    const term = new Terminal({
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      fontSize: 12.5,
      lineHeight: 1.2,
      cursorBlink: true,
      theme: theme(),
      scrollback: 5000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    fit.fit();
    term.focus();

    const q = new URLSearchParams({
      shell,
      ...(target ? { replica: target } : {}),
    });
    const ws = new WebSocket(
      `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/v1/services/${encodeURIComponent(app.id)}/terminal?${q}`,
    );
    ws.binaryType = "arraybuffer";
    const enc = new TextEncoder();
    let ended = false;
    const end = (message: string, error = false) => {
      if (ended) return;
      ended = true;
      setPhase({ kind: "closed", message, error });
    };
    const resize = () => {
      fit.fit();
      if (ws.readyState === WebSocket.OPEN)
        ws.send(
          JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }),
        );
    };
    setPhase({ kind: "connecting" });
    // Measure again once layout and fonts settle: measuring too early gives
    // too many columns, and long lines run off the edge instead of wrapping.
    const frame = requestAnimationFrame(resize);
    void document.fonts?.ready.then(resize);
    ws.onopen = resize;
    ws.onmessage = (e) => {
      if (typeof e.data !== "string")
        return term.write(new Uint8Array(e.data as ArrayBuffer));
      const f = JSON.parse(e.data) as Frame;
      if (f.type === "ready") {
        setPhase({ kind: "open", pod: f.pod });
        resize();
      } else if (f.type === "error") end(f.message, true);
      else if (f.type === "exit") end("The shell exited.");
    };
    ws.onclose = () => end("Disconnected.");
    const input = term.onData(
      (d) => ws.readyState === WebSocket.OPEN && ws.send(enc.encode(d)),
    );
    const ro = new ResizeObserver(() => resize());
    ro.observe(host.current);
    return () => {
      ended = true;
      cancelAnimationFrame(frame);
      ro.disconnect();
      input.dispose();
      ws.close();
      term.dispose();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- a session starts with the picks made then
  }, [session, app.id]);

  const stopped = app.status === "stopped";
  return (
    // Esc belongs to the shell (vim, less), so it doesn't close full screen.
    <Zoomable
      zoomed={zoomed}
      onClose={() => setZoomed(false)}
      label="Terminal"
      closeOnEscape={false}
    >
      <div className={zoomed ? "flex min-h-0 flex-1 flex-col" : ""}>
        <div className="flex min-h-11 flex-wrap items-center gap-2 border-b border-border px-4 py-2 text-[12.5px]">
          <TerminalWindowIcon size={15} className="text-muted-foreground" />
          {phase.kind === "idle" ? (
            <span className="text-muted-foreground">
              A shell in one of {app.name}'s running replicas.
            </span>
          ) : phase.kind === "connecting" ? (
            <span className="text-muted-foreground">Connecting…</span>
          ) : phase.kind === "open" ? (
            <span>
              Connected to <span className="font-mono">{phase.pod}</span>
            </span>
          ) : (
            <span
              className={
                phase.error ? "text-destructive" : "text-muted-foreground"
              }
            >
              {phase.message}
            </span>
          )}
          <div className="ml-auto flex flex-wrap items-center gap-2">
            {replicas.length > 1 ? (
              <Select
                className="w-auto"
                aria-label="Replica"
                value={target}
                onChange={(e) => pick(setReplica)(e.target.value)}
              >
                <option value="">Any running replica</option>
                {replicas.map((r) => (
                  <option key={r.name} value={r.name} disabled={!r.ready}>
                    {r.name}
                    {r.server ? ` on ${r.server}` : ""}
                    {r.ready ? "" : " (not running)"}
                  </option>
                ))}
              </Select>
            ) : null}
            <Select
              className="w-auto"
              aria-label="Shell"
              value={shell}
              onChange={(e) => pick(setShell)(e.target.value)}
            >
              {SHELLS.map((x) => (
                <option key={x.value} value={x.value}>
                  <span className="inline-flex items-center gap-2">
                    {x.icon}
                    {x.label}
                  </span>
                </option>
              ))}
            </Select>
            {phase.kind === "closed" ? (
              <Button
                size="xs"
                variant="secondary"
                onClick={() => setSession((n) => n + 1)}
              >
                <ArrowClockwiseIcon size={12} /> Reconnect
              </Button>
            ) : null}
            <ZoomButton zoomed={zoomed} onClick={() => setZoomed((z) => !z)} />
          </div>
        </div>
        {session ? (
          <div
            className={`${zoomed ? "min-h-0 flex-1" : "h-[60vh] min-h-[320px]"} bg-background px-3 py-2`}
          >
            <div ref={host} className="h-full w-full" />
          </div>
        ) : (
          <div className="flex flex-col items-center gap-3 px-4 py-14 text-center">
            <p className="max-w-sm text-[13px] text-muted-foreground">
              {stopped
                ? "The service is stopped. Start it to open a terminal."
                : "Changes are lost on restart, except in volumes."}
            </p>
            <Button size="sm" disabled={stopped} onClick={() => setSession(1)}>
              <TerminalWindowIcon size={14} /> Open terminal
            </Button>
          </div>
        )}
      </div>
      <ConfirmDialog
        open={blocker.status === "blocked"}
        tone="primary"
        title="Close the terminal?"
        description="Leaving ends your terminal session."
        confirmLabel="Leave and close"
        onConfirm={() => {
          leaving.current = true;
          blocker.proceed?.();
        }}
        onClose={() => {
          if (!leaving.current) blocker.reset?.();
          leaving.current = false;
        }}
      />
    </Zoomable>
  );
}
