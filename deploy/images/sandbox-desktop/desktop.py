#!/usr/bin/env python3
"""The desktop template's desktop, started on first use.

/etc/pyo/start runs this when the sandbox boots. It only listens, on
127.0.0.1:
- 6080, the screen in noVNC: starts the screen, the session and noVNC's
  websockify (6081), and Chromium the first time, then passes the
  connection to websockify.
- 9222, Chromium's CDP: starts the screen and Chromium (CDP on 9223) when
  Chromium isn't running, then passes the connection to it.

So a sandbox nobody looks at runs no X server and no browser. What it
starts stays up, and is started again if it exited. It logs to
/var/log/pyo-desktop.log.
"""

import asyncio
import os
import subprocess
import time

HOST = "127.0.0.1"
DISPLAY = ":1"
VNC = 5901
SCREEN, WEBSOCKIFY = 6080, 6081
CDP, CHROMIUM = 9222, 9223
SIZE = "1280x800"

procs: dict[str, subprocess.Popen] = {}
lock = asyncio.Lock()
browser_started = False


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
    deadline = time.monotonic() + secs
    while not await answers(port):
        if time.monotonic() > deadline:
            raise RuntimeError(f"{name} didn't start in {int(secs)}s")
        await asyncio.sleep(0.1)


async def screen() -> None:
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
    global browser_started
    await screen()
    if not await answers(CHROMIUM):
        spawn("chromium", ["chromium", "--start-maximized", "about:blank"])
        await wait_for(CHROMIUM, 60, "Chromium")
    browser_started = True


async def novnc() -> None:
    await screen()
    if not alive("websockify"):
        spawn("websockify", ["websockify", "--web", "/usr/share/novnc", f"{HOST}:{WEBSOCKIFY}", f"{HOST}:{VNC}"])
        await wait_for(WEBSOCKIFY, 20, "noVNC")
    # The first time someone looks, there's a browser on the screen.
    if not browser_started:
        await browser()


async def pipe(r: asyncio.StreamReader, w: asyncio.StreamWriter) -> None:
    try:
        while data := await r.read(1 << 16):
            w.write(data)
            await w.drain()
        if w.can_write_eof():
            w.write_eof()
    except (OSError, RuntimeError):
        pass


def serve(start, target: int):
    async def handle(cr: asyncio.StreamReader, cw: asyncio.StreamWriter) -> None:
        try:
            async with lock:
                await start()
            tr, tw = await asyncio.open_connection(HOST, target)
        except Exception as e:  # noqa: BLE001 - log it, drop the connection
            log(f"couldn't start: {e}")
            cw.close()
            return
        await asyncio.gather(pipe(cr, tw), pipe(tr, cw))
        for w in (tw, cw):
            w.close()

    return handle


async def main() -> None:
    servers = [
        await asyncio.start_server(serve(novnc, WEBSOCKIFY), HOST, SCREEN, reuse_address=True),
        await asyncio.start_server(serve(browser, CHROMIUM), HOST, CDP, reuse_address=True),
    ]
    log(f"listening on {SCREEN} (screen) and {CDP} (CDP)")
    await asyncio.gather(*(s.serve_forever() for s in servers))


if __name__ == "__main__":
    asyncio.run(main())
