#!/usr/bin/env python3
"""The desktop template's desktop, started on first use.

/etc/pyo/start runs this when the sandbox boots. It listens on 127.0.0.1:
- 6080, the screen in noVNC. It serves noVNC's files itself, at once and
  with no other program, and its WebSocket at /websockify, which it
  bridges to the screen's VNC (5901). The first request starts the screen
  (Xvnc) and the session in the background, so they're up by the time
  noVNC connects. A WebSocket waits only for the screen. Chromium starts
  once the screen is up, and nothing waits for it.
- 9222, Chromium's CDP: starts the screen and Chromium (CDP on 9223) when
  Chromium isn't running, then passes the connection to it.

So a sandbox nobody looks at runs no X server and no browser. What it
starts stays up, and is started again if it exited. It logs to
/var/log/pyo-desktop.log, with how long each start took.
"""

import asyncio
import base64
import hashlib
import mimetypes
import os
import struct
import subprocess
import time
import urllib.parse

HOST = "127.0.0.1"
DISPLAY = ":1"
VNC = 5901
SCREEN = 6080
CDP, CHROMIUM = 9222, 9223
SIZE = "1280x800"
NOVNC = "/usr/share/novnc"
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

procs: dict[str, subprocess.Popen] = {}
lock = asyncio.Lock()
chromium_lock = asyncio.Lock()
browser_started = False
starting: asyncio.Task | None = None


def log(msg: str) -> None:
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def env() -> dict:
    e = dict(os.environ)
    e.setdefault("HOME", "/root")
    e.setdefault("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
    e.setdefault("LANG", "C.UTF-8")
    e["DISPLAY"] = DISPLAY
    e["XDG_RUNTIME_DIR"] = "/tmp/runtime-root"
    return e


def alive(name: str) -> bool:
    p = procs.get(name)
    return p is not None and p.poll() is None


def spawn(name: str, args: list[str]) -> None:
    log(f"starting {name}")
    os.makedirs("/tmp/runtime-root", mode=0o700, exist_ok=True)
    procs[name] = subprocess.Popen(
        args,
        env=env(),
        cwd=env()["HOME"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


async def answers(port: int) -> bool:
    try:
        _, w = await asyncio.open_connection(HOST, port)
    except OSError:
        return False
    w.close()
    return True


async def wait_for(port: int, secs: float, name: str) -> None:
    t = time.monotonic()
    while not await answers(port):
        if time.monotonic() - t > secs:
            raise RuntimeError(f"{name} didn't start in {int(secs)}s")
        await asyncio.sleep(0.05)
    log(f"{name} is up in {int((time.monotonic() - t) * 1000)}ms")


async def screen() -> None:
    """The screen and its session, once."""
    async with lock:
        if not alive("screen"):
            spawn("screen", [
                "Xvnc", DISPLAY, "-rfbport", str(VNC), "-localhost", "-SecurityTypes", "None",
                "-geometry", SIZE, "-depth", "24", "-AlwaysShared", "-AcceptSetDesktopSize",
            ])
            await wait_for(VNC, 20, "the screen")
            # A new screen needs a new session.
            procs.pop("session", None)
        if not alive("session"):
            spawn("session", ["dbus-launch", "--exit-with-session", "/usr/local/lib/pyo-desktop/session"])


async def browser() -> None:
    """Chromium on the screen, with CDP up."""
    global browser_started
    await screen()
    # Its own lock: the screen's WebSocket never waits on Chromium.
    async with chromium_lock:
        browser_started = True
        if not await answers(CHROMIUM):
            if not alive("chromium"):
                spawn("chromium", ["chromium", "--start-maximized", "about:blank"])
            await wait_for(CHROMIUM, 60, "Chromium")


async def desktop() -> None:
    """The screen, then (the first time someone looks) Chromium on it."""
    await screen()
    if not browser_started:
        await browser()


def start_desktop() -> asyncio.Task:
    """Starts the desktop in the background, once at a time."""
    global starting
    if starting is None or starting.done():
        starting = asyncio.ensure_future(desktop())
        starting.add_done_callback(lambda t: t.cancelled() or not t.exception() or log(f"couldn't start: {t.exception()}"))
    return starting


# ── 6080: noVNC's files and its WebSocket ───────────────────────────────────

files: dict[str, tuple[bytes, str]] = {}


def static(path: str) -> tuple[bytes, str] | None:
    """A file of noVNC's, kept in memory once read."""
    if path in ("", "/"):
        path = "/vnc.html"
    if path in files:
        return files[path]
    full = os.path.realpath(os.path.join(NOVNC, path.lstrip("/")))
    if not full.startswith(NOVNC + "/") or not os.path.isfile(full):
        return None
    with open(full, "rb") as f:
        body = f.read()
    kind = mimetypes.guess_type(full)[0] or "application/octet-stream"
    if kind.startswith("text/") or kind == "application/javascript":
        kind += "; charset=utf-8"
    files[path] = (body, kind)
    return files[path]


def reply(w: asyncio.StreamWriter, status: str, body: bytes, kind: str = "text/plain; charset=utf-8", cache: bool = False) -> None:
    head = [
        f"HTTP/1.1 {status}",
        f"Content-Type: {kind}",
        f"Content-Length: {len(body)}",
        # Its files only change with the image.
        "Cache-Control: " + ("private, max-age=86400" if cache else "no-store"),
        "Connection: close",
    ]
    w.write(("\r\n".join(head) + "\r\n\r\n").encode() + body)


async def read_head(r: asyncio.StreamReader) -> tuple[str, str, dict[str, str]] | None:
    try:
        raw = await asyncio.wait_for(r.readuntil(b"\r\n\r\n"), 30)
    except (asyncio.IncompleteReadError, asyncio.LimitOverrunError, asyncio.TimeoutError, OSError):
        return None
    lines = raw.decode("latin-1").split("\r\n")
    parts = lines[0].split(" ")
    if len(parts) < 3:
        return None
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    return parts[0], parts[1], headers


async def handle_screen(r: asyncio.StreamReader, w: asyncio.StreamWriter) -> None:
    try:
        head = await read_head(r)
        if head is None:
            return
        method, target, headers = head
        path = urllib.parse.unquote(urllib.parse.urlsplit(target).path)
        # Whatever is asked first, the screen starts now.
        task = start_desktop()
        if path.rstrip("/").endswith("/websockify") or path == "/websockify":
            if headers.get("upgrade", "").lower() != "websocket":
                reply(w, "400 Bad Request", b"This is noVNC's WebSocket.\n")
                return
            await websocket(r, w, headers, task)
            return
        if method not in ("GET", "HEAD"):
            reply(w, "405 Method Not Allowed", b"")
            return
        found = static(path)
        if found is None:
            reply(w, "404 Not Found", b"Not found.\n")
            return
        body, kind = found
        reply(w, "200 OK", b"" if method == "HEAD" else body, kind, cache=True)
        await w.drain()
    except (OSError, RuntimeError):
        pass
    finally:
        w.close()


def ws_frame(payload: bytes, opcode: int = 2) -> bytes:
    n = len(payload)
    if n < 126:
        head = struct.pack(">BB", 0x80 | opcode, n)
    elif n < 1 << 16:
        head = struct.pack(">BBH", 0x80 | opcode, 126, n)
    else:
        head = struct.pack(">BBQ", 0x80 | opcode, 127, n)
    return head + payload


async def websocket(r: asyncio.StreamReader, w: asyncio.StreamWriter, headers: dict[str, str], task: asyncio.Task) -> None:
    """noVNC's WebSocket, bridged to the screen's VNC once it's up."""
    key = headers.get("sec-websocket-key", "")
    accept = base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
    try:
        await asyncio.shield(screen())
        vr, vw = await asyncio.open_connection(HOST, VNC)
    except (OSError, RuntimeError) as e:
        log(f"couldn't reach the screen: {e}")
        reply(w, "503 Service Unavailable", b"The screen didn't start.\n")
        return
    head = ["HTTP/1.1 101 Switching Protocols", "Upgrade: websocket", "Connection: Upgrade", f"Sec-WebSocket-Accept: {accept}"]
    offered = [p.strip() for p in headers.get("sec-websocket-protocol", "").split(",") if p.strip()]
    if "binary" in offered:
        head.append("Sec-WebSocket-Protocol: binary")
    w.write(("\r\n".join(head) + "\r\n\r\n").encode())
    await w.drain()

    async def to_vnc() -> None:
        try:
            while True:
                b0, b1 = await r.readexactly(2)
                opcode, n = b0 & 0x0F, b1 & 0x7F
                if n == 126:
                    (n,) = struct.unpack(">H", await r.readexactly(2))
                elif n == 127:
                    (n,) = struct.unpack(">Q", await r.readexactly(8))
                mask = await r.readexactly(4) if b1 & 0x80 else b""
                data = await r.readexactly(n)
                if mask:
                    data = bytes(b ^ mask[i & 3] for i, b in enumerate(data))
                if opcode == 0x8:  # close
                    w.write(ws_frame(data[:2], 0x8))
                    break
                if opcode == 0x9:  # ping
                    w.write(ws_frame(data, 0xA))
                elif opcode in (0x0, 0x1, 0x2):
                    vw.write(data)
                    await vw.drain()
        except (asyncio.IncompleteReadError, OSError):
            pass
        finally:
            vw.close()

    async def to_browser() -> None:
        try:
            while data := await vr.read(1 << 16):
                w.write(ws_frame(data))
                await w.drain()
        except OSError:
            pass
        finally:
            w.close()

    await asyncio.gather(to_vnc(), to_browser())


# ── 9222: Chromium's CDP ───────────────────────────────────────────────────


async def pipe(r: asyncio.StreamReader, w: asyncio.StreamWriter) -> None:
    try:
        while data := await r.read(1 << 16):
            w.write(data)
            await w.drain()
        if w.can_write_eof():
            w.write_eof()
    except (OSError, RuntimeError):
        pass


async def handle_cdp(cr: asyncio.StreamReader, cw: asyncio.StreamWriter) -> None:
    try:
        await browser()
        tr, tw = await asyncio.open_connection(HOST, CHROMIUM)
    except Exception as e:  # noqa: BLE001 - log it, drop the connection
        log(f"couldn't start: {e}")
        cw.close()
        return
    await asyncio.gather(pipe(cr, tw), pipe(tr, cw))
    for w in (tw, cw):
        w.close()


async def main() -> None:
    servers = [
        await asyncio.start_server(handle_screen, HOST, SCREEN, reuse_address=True),
        await asyncio.start_server(handle_cdp, HOST, CDP, reuse_address=True),
    ]
    log(f"listening on {SCREEN} (screen) and {CDP} (CDP)")
    await asyncio.gather(*(s.serve_forever() for s in servers))


if __name__ == "__main__":
    asyncio.run(main())
