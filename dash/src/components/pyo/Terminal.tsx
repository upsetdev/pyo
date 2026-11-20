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

// ANSI colors per mode. Each one reads at 4.5:1 or more on the page
// background (#fff light, #000 dark), so "white" is a grey in light mode
// and "black" a grey in dark mode.
const LIGHT = {
  black: "#171717",
  red: "#c4161c",
  green: "#18794e",
  yellow: "#8a5a00",
  blue: "#0b57d0",
  magenta: "#9c27b0",
  cyan: "#0b6e83",
  white: "#6b6b6b",
  brightBlack: "#5c5c5c",
  brightRed: "#d42a2a",
  brightGreen: "#1a7f37",
  brightYellow: "#9a6700",
  brightBlue: "#1f6feb",
  brightMagenta: "#b0359f",
  brightCyan: "#0a7c8c",
  brightWhite: "#757575",
};
const DARK = {
  black: "#767676",
  red: "#f26b6b",
  green: "#4cc76f",
  yellow: "#e5b93e",
  blue: "#5b9cf6",
  magenta: "#c58af9",
  cyan: "#3cc4d6",
  white: "#d4d4d4",
  brightBlack: "#8f8f8f",
  brightRed: "#ff9a9a",
  brightGreen: "#7ee29b",
  brightYellow: "#f7d774",
  brightBlue: "#8ec0ff",
  brightMagenta: "#ddb3ff",
  brightCyan: "#7fe3f0",
  brightWhite: "#ffffff",
};

/** The app's theme colors, so the terminal matches light and dark mode. */
export function theme() {
  const root = document.documentElement;
  const css = getComputedStyle(root);
  const hsl = (name: string) => `hsl(${css.getPropertyValue(name).trim()})`;
  const light = root.dataset.theme
    ? root.dataset.theme === "light"
    : matchMedia("(prefers-color-scheme: light)").matches;
  return {
    ...(light ? LIGHT : DARK),
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

/** An xterm in `host`, styled like the dashboard and sized to it. */
export function openXterm(host: HTMLElement) {
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
  // Follow theme changes live: ThemeProvider sets data-theme on <html>, also
  // when the system theme flips while on "system".
  const watch = new MutationObserver(() => {
    term.options.theme = theme();
  });
  watch.observe(document.documentElement, { attributeFilter: ["data-theme"] });
  // An addon is disposed with the terminal, so the observer goes with it.
  term.loadAddon({ activate() {}, dispose: () => watch.disconnect() });
  term.open(host);
  fit.fit();
  term.focus();
  return { term, fit };
}

/**
 * Leaving the page ends a live terminal: ask first (and on reload or
 * closing the tab). Render what it returns.
 */
export function useLeaveGuard(live: boolean) {
  const blocker = useBlocker({
    shouldBlockFn: () => live,
    enableBeforeUnload: () => live,
    withResolver: true,
  });
  const leaving = useRef(false);
  return (
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
  );
}

export function ServiceTerminal({ app }: { app: Application }) {
  const host = useRef<HTMLDivElement>(null);
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const [session, setSession] = useState(0);
  const [replica, setReplica] = useState("");
  const [shell, setShell] = useState("auto");
  const [zoomed, setZoomed] = useState(false);
  // Leaving the page ends the shell: ask first (and on reload or closing the tab).
  const live = phase.kind === "open" || phase.kind === "connecting";
  const guard = useLeaveGuard(live);
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
    const { term, fit } = openXterm(host.current);

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
                  <span className="flex items-center gap-2">
                    {x.icon}
                    {x.label}
                  </span>
                </option>
              ))}
            </Select>
            {phase.kind === "closed" ? (
              <Button
                size="sm"
                variant="secondary"
                className="h-9"
                onClick={() => setSession((n) => n + 1)}
              >
                <ArrowClockwiseIcon size={12} /> Reconnect
              </Button>
            ) : null}
            <ZoomButton zoomed={zoomed} onClick={() => setZoomed((z) => !z)} className="h-9 w-9" />
          </div>
        </div>
        {session ? (
          <div
            className={`${zoomed ? "min-h-0 flex-1" : "h-[60vh] min-h-[320px]"} bg-background px-3 py-2`}
          >
            <div ref={host} className="h-full w-full" />
          </div>
        ) : (
          <div
            className={`${zoomed ? "min-h-0 flex-1" : "h-[60vh] min-h-[320px]"} flex flex-col items-center justify-center gap-3 px-4 text-center`}
          >
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
      {guard}
    </Zoomable>
  );
}
