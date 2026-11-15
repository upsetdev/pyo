import { ArrowClockwiseIcon, TerminalWindowIcon } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";
import { openXterm, useLeaveGuard } from "~/components/pyo/Terminal";
import { Button } from "~/components/ui/Button";
import { ZoomButton, Zoomable } from "~/components/ui/Zoom";
import {
  Kind,
  OFF_TAILNET,
  connectSocket,
  decodeFrame,
  encodeFrame,
  jsonFrame,
  parseFrame,
  type Sandbox,
} from "~/lib/pyo/sandbox";

// A terminal in the sandbox, straight to its server's daemon over the
// WebSocket (crates/pyo-sandbox/src/api.rs), with the client token Pyo
// signs. The daemon speaks envd's frames: a `pty` request opens stream 1,
// keys go up as Stdin on it and the terminal comes back as Stdout.

type Phase =
  | { kind: "idle" }
  | { kind: "connecting" }
  | { kind: "open" }
  | { kind: "closed"; message: string; error?: boolean };

type Res =
  | { type: "started"; pid: number }
  | { type: "exit"; code?: number; signal?: number }
  | { type: "error"; message: string }
  | { type: string };

const PTY = 1;

export function SandboxTerminal({
  sandbox,
  autoStart = false,
}: {
  sandbox: Sandbox;
  /** Open the terminal at once (a sandbox just made). */
  autoStart?: boolean;
}) {
  const host = useRef<HTMLDivElement>(null);
  const running = sandbox.state === "running";
  const [session, setSession] = useState(autoStart && running ? 1 : 0);
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const [zoomed, setZoomed] = useState(false);
  const guard = useLeaveGuard(phase.kind === "open" || phase.kind === "connecting");

  useEffect(() => {
    if (!session || !host.current) return;
    const { term, fit } = openXterm(host.current);
    const enc = new TextEncoder();
    let ws: WebSocket | null = null;
    let started = false;
    let ended = false;
    const end = (message: string, error = false) => {
      if (ended) return;
      ended = true;
      setPhase({ kind: "closed", message, error });
    };
    const send = (b: Uint8Array) => {
      if (ws?.readyState === WebSocket.OPEN) ws.send(b);
    };
    const resize = () => {
      if (ended) return;
      fit.fit();
      if (started)
        send(jsonFrame(Kind.Request, PTY, { op: "resize", cols: term.cols, rows: term.rows }));
    };
    setPhase({ kind: "connecting" });
    const frame = requestAnimationFrame(resize);
    void document.fonts?.ready.then(resize);
    let disposed = false;
    void connectSocket(sandbox.id)
      .then((socket) => {
        if (disposed) return socket.close();
        ws = socket;
        socket.onerror = () => !started && end(OFF_TAILNET, true);
        socket.onclose = () => end(started ? "Disconnected." : OFF_TAILNET, !started);
        socket.onmessage = (e) => {
          if (typeof e.data === "string") return;
          let f;
          try {
            f = decodeFrame(new Uint8Array(e.data as ArrayBuffer));
          } catch {
            return;
          }
          if (f.kind === Kind.Hello) {
            fit.fit();
            send(jsonFrame(Kind.Request, PTY, { op: "pty", cols: term.cols, rows: term.rows }));
          } else if (f.stream !== PTY) return;
          else if (f.kind === Kind.Stdout || f.kind === Kind.Stderr) term.write(f.payload);
          else if (f.kind === Kind.Response) {
            const r = parseFrame<Res>(f);
            if (r.type === "started") {
              started = true;
              setPhase({ kind: "open" });
              resize();
            } else if (r.type === "exit") end("The shell exited.");
            else if (r.type === "error") end((r as { message: string }).message, true);
          }
        };
      })
      .catch((e: Error) => end(e.message, true));
    const input = term.onData((d) => started && send(encodeFrame(Kind.Stdin, PTY, enc.encode(d))));
    const ro = new ResizeObserver(() => resize());
    ro.observe(host.current);
    return () => {
      disposed = true;
      ended = true;
      cancelAnimationFrame(frame);
      ro.disconnect();
      input.dispose();
      ws?.close();
      term.dispose();
    };
  }, [session, sandbox.id]);

  return (
    <Zoomable zoomed={zoomed} onClose={() => setZoomed(false)} label="Terminal" closeOnEscape={false}>
      <div className={zoomed ? "flex min-h-0 flex-1 flex-col" : ""}>
        <div className="flex min-h-11 flex-wrap items-center gap-2 border-b border-border px-4 py-2 text-[12.5px]">
          <TerminalWindowIcon size={15} className="text-muted-foreground" />
          {phase.kind === "idle" ? (
            <span className="text-muted-foreground">A shell in {sandbox.id}.</span>
          ) : phase.kind === "connecting" ? (
            <span className="text-muted-foreground">Connecting…</span>
          ) : phase.kind === "open" ? (
            <span>
              Connected to <span className="font-mono">{sandbox.id}</span>
            </span>
          ) : (
            <span className={phase.error ? "text-destructive" : "text-muted-foreground"}>
              {phase.message}
            </span>
          )}
          <div className="ml-auto flex items-center gap-2">
            {phase.kind === "closed" ? (
              <Button size="xs" variant="secondary" onClick={() => setSession((n) => n + 1)}>
                <ArrowClockwiseIcon size={12} /> Reconnect
              </Button>
            ) : null}
            <ZoomButton zoomed={zoomed} onClick={() => setZoomed((z) => !z)} />
          </div>
        </div>
        {session ? (
          <div className={`${zoomed ? "min-h-0 flex-1" : "h-[60vh] min-h-[320px]"} bg-background px-3 py-2`}>
            <div ref={host} className="h-full w-full" />
          </div>
        ) : (
          <div className="flex flex-col items-center gap-3 px-4 py-14 text-center">
            <p className="max-w-sm text-[13px] text-muted-foreground">
              {running ? "Needs your tailnet for now." : "Resume the sandbox to open a terminal."}
            </p>
            <Button size="sm" disabled={!running} onClick={() => setSession(1)}>
              <TerminalWindowIcon size={14} /> Open terminal
            </Button>
          </div>
        )}
      </div>
      {guard}
    </Zoomable>
  );
}
