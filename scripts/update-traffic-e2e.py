#!/usr/bin/env python3
"""Helpers of scripts/update-traffic-e2e.sh. Standard library only.

  load LOG STOP RATE NAME=HOST@IP...   traffic, one line per result
  watch LOG                            pod changes on this server (k3s)
  summary DIR                          what failed, and during which rollout
  images-pull REGISTRY DIR TAG NAME... a build's images, from a registry
  images-push REGISTRY DIR             the same images, into a registry

load sends, to each target, with no retries:
  fresh      RATE requests a second, a new connection each
  keepalive  RATE requests a second on one connection, a POST every second
  long       one request that takes 20 seconds, again and again
  ws         one WebSocket, a message every second

Its log: <epoch> <target> <kind> <result> <ms> <detail>. result is the HTTP
status or "err"; for ws it is open, drop (it was open), fail (it didn't
open) or ok (closed by the test).
"""
import base64
import hashlib
import http.client
import json
import os
import socket
import ssl
import struct
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

# ── load ────────────────────────────────────────────────────────────────────

LONG_SECONDS = 20
PARENT = os.getppid()


class Load:
    def __init__(self, log, stop, rate, targets):
        self.out = open(log, "a", buffering=1)
        self.stop = stop
        self.rate = rate
        self.targets = targets
        self.lock = threading.Lock()
        self.ctx = ssl.create_default_context()
        # The test domains are on a private address: self-signed.
        self.ctx.check_hostname = False
        self.ctx.verify_mode = ssl.CERT_NONE

    def log(self, t, name, kind, result, ms, detail=""):
        detail = detail.replace("\n", " ").replace(" ", "_")[:120] or "-"
        with self.lock:
            self.out.write(f"{t:.3f} {name} {kind} {result} {ms:.0f} {detail}\n")

    def stopped(self):
        # Also when the script that started this is gone.
        return os.path.exists(self.stop) or os.getppid() != PARENT

    def connect(self, host, ip):
        ip, _, port = ip.partition(":")
        s = socket.create_connection((ip, int(port or 443)), timeout=10)
        try:
            return self.ctx.wrap_socket(s, server_hostname=host)
        except Exception:
            s.close()
            raise

    @staticmethod
    def request(sock, host, method, path, body=b""):
        req = f"{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: pyo-e2e\r\nAccept: */*\r\n"
        if body:
            req += f"Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {len(body)}\r\n"
        sock.sendall(req.encode() + b"\r\n" + body)
        r = http.client.HTTPResponse(sock, method=method)
        r.begin()
        body = r.read()
        return r.status, (r.getheader("connection") or "").lower() == "close", answer(r, body)

    def fresh(self, t, name, host, ip, n):
        try:
            s = self.connect(host, ip)
            try:
                s.settimeout(10)
                status, _, said = self.request(s, host, "GET", f"/?n={n}")
            finally:
                s.close()
            self.log(t, name, "fresh", status, (time.time() - t) * 1000, said)
        except Exception as e:
            self.log(t, name, "fresh", "err", (time.time() - t) * 1000, err(e))

    def fresh_loop(self, name, host, ip, pool):
        n, start = 0, time.time()
        while not self.stopped():
            pool.submit(self.fresh, time.time(), name, host, ip, n)
            n += 1
            time.sleep(max(0, start + n / self.rate - time.time()))

    def keepalive_loop(self, name, host, ip):
        sock, n, start = None, 0, time.time()
        every = max(int(self.rate), 1)
        while not self.stopped():
            t = time.time()
            post = n % every == 0
            try:
                if sock is None:
                    sock = self.connect(host, ip)
                    sock.settimeout(10)
                status, close, said = self.request(sock, host, "POST" if post else "GET", f"/?k={n}", b"message=hello" if post else b"")
                self.log(t, name, "keepalive", status, (time.time() - t) * 1000, ("post:" if post else "") + said)
                if close:
                    sock.close()
                    sock = None
            except Exception as e:
                self.log(t, name, "keepalive", "err", (time.time() - t) * 1000, ("post:" if post else "") + err(e))
                if sock:
                    sock.close()
                sock = None
            n += 1
            time.sleep(max(0, start + n / self.rate - time.time()))
        if sock:
            sock.close()

    def long_loop(self, name, host, ip):
        while not self.stopped():
            t = time.time()
            try:
                s = self.connect(host, ip)
                try:
                    s.settimeout(LONG_SECONDS + 20)
                    status, _, _ = self.request(s, host, "GET", f"/?wait={LONG_SECONDS}s")
                finally:
                    s.close()
                self.log(t, name, "long", status, (time.time() - t) * 1000)
            except Exception as e:
                self.log(t, name, "long", "err", (time.time() - t) * 1000, err(e))
                time.sleep(1)

    def ws_loop(self, name, host, ip):
        while not self.stopped():
            t = time.time()
            sock, opened = None, None
            try:
                sock = self.connect(host, ip)
                sock.settimeout(10)
                key = base64.b64encode(os.urandom(16)).decode()
                sock.sendall(
                    f"GET /echo HTTP/1.1\r\nHost: {host}\r\nUser-Agent: pyo-e2e\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                    f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode()
                )
                head = b""
                while b"\r\n\r\n" not in head:
                    chunk = sock.recv(1)
                    if not chunk:
                        raise ConnectionError("closed during the upgrade")
                    head += chunk
                status = head.split(b" ", 2)[1].decode()
                if status != "101":
                    raise ConnectionError(f"the upgrade answered {status}")
                opened = time.time()
                self.log(t, name, "ws", "open", (opened - t) * 1000)
                n = 0
                sock.settimeout(5)
                while not self.stopped():
                    msg = f"m{n}"
                    sock.sendall(ws_frame(msg))
                    got = ws_read(sock)
                    if got != msg:
                        raise ConnectionError(f"echo {got!r}")
                    n += 1
                    time.sleep(1)
                self.log(time.time(), name, "ws", "ok", (time.time() - opened) * 1000, f"{n}_messages")
            except Exception as e:
                now = time.time()
                if opened:
                    self.log(now, name, "ws", "drop", (now - opened) * 1000, err(e))
                else:
                    self.log(now, name, "ws", "fail", (now - t) * 1000, err(e))
                time.sleep(1)
            finally:
                if sock:
                    sock.close()

    def run(self):
        pool = ThreadPoolExecutor(max_workers=int(self.rate * 12 * len(self.targets)) + 4)
        threads = []
        for name, host, ip in self.targets:
            for fn, args in (
                (self.fresh_loop, (name, host, ip, pool)),
                (self.keepalive_loop, (name, host, ip)),
                (self.long_loop, (name, host, ip)),
                (self.ws_loop, (name, host, ip)),
            ):
                th = threading.Thread(target=fn, args=args, daemon=True)
                th.start()
                threads.append(th)
        while not self.stopped():
            time.sleep(0.5)
        deadline = time.time() + LONG_SECONDS + 25
        for th in threads:
            th.join(timeout=max(0, deadline - time.time()))
        pool.shutdown(wait=True)


def answer(r, body):
    """Who refused a request: "cloudflare" when Cloudflare's edge did it
    (its own block or error page), "origin" when the answer came through
    from Pyo, and the start of the body. Empty for a 200."""
    if r.status == 200:
        return ""
    # 520 to 530 are Cloudflare's own (530 with "error code: 1033": the
    # tunnel has no connector), as is a page made by its edge.
    edge = (
        r.getheader("cf-mitigated")
        or 520 <= r.status <= 530
        or body[:200].lstrip().lower().startswith(b"error code:")
        or (b"cloudflare" in body[:2000].lower() and r.getheader("server", "").lower() == "cloudflare")
    )
    text = body[:60].decode(errors="replace").strip()
    return ("cloudflare:" if edge else "origin:") + text


def err(e):
    return type(e).__name__ + (":" + str(e) if str(e) else "")


def ws_frame(text):
    data = text.encode()
    mask = os.urandom(4)
    head = bytes([0x81])
    if len(data) < 126:
        head += bytes([0x80 | len(data)])
    else:
        head += bytes([0x80 | 126]) + struct.pack("!H", len(data))
    return head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data))


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("closed")
        buf += chunk
    return buf


def ws_read(sock):
    b1, b2 = recv_exact(sock, 2)
    size = b2 & 0x7F
    if size == 126:
        size = struct.unpack("!H", recv_exact(sock, 2))[0]
    elif size == 127:
        size = struct.unpack("!Q", recv_exact(sock, 8))[0]
    payload = recv_exact(sock, size)
    if b1 & 0x0F == 0x8:
        raise ConnectionError("close frame")
    return payload.decode(errors="replace")


def cmd_load(args):
    log, stop, rate = args[0], args[1], float(args[2])
    targets = []
    for t in args[3:]:
        name, rest = t.split("=", 1)
        host, ip = rest.split("@", 1)
        targets.append((name, host, ip))
    Load(log, stop, rate, targets).run()


# ── watch ───────────────────────────────────────────────────────────────────

# Its log: <epoch> <event> <namespace> <pod> <owner kind> <owner> <phase>
# <ready> <deleting> <server>, and "<epoch> RESYNC" when the watch starts
# again (k3s restarted): the pods are listed again after it.


def cmd_watch(args):
    out = open(args[0], "a", buffering=1)
    while True:
        out.write(f"{time.time():.3f} RESYNC\n")
        p = subprocess.Popen(
            os.environ.get("KUBECTL", "k3s kubectl").split() + ["get", "pods", "-A", "--watch", "--output-watch-events", "-o", "json"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        buf = ""
        for line in p.stdout:
            # An event is one line, or indented over many with older kubectl.
            buf += line
            if not line.rstrip().endswith("}"):
                continue
            try:
                ev = json.loads(buf)
            except ValueError:
                continue
            buf = ""
            if "object" not in ev:
                continue
            o = ev["object"]
            meta, status = o.get("metadata", {}), o.get("status", {})
            owner = (meta.get("ownerReferences") or [{}])[0]
            ready = next((c.get("status") for c in status.get("conditions", []) if c.get("type") == "Ready"), "False")
            out.write(
                " ".join(
                    [
                        f"{time.time():.3f}",
                        ev.get("type", "?"),
                        meta.get("namespace", "-"),
                        meta.get("name", "-"),
                        owner.get("kind", "-"),
                        owner.get("name", "-"),
                        status.get("phase", "-"),
                        "ready" if ready == "True" else "notready",
                        "deleting" if meta.get("deletionTimestamp") else "-",
                        o.get("spec", {}).get("nodeName") or "-",
                    ]
                )
                + "\n"
            )
        p.wait()
        time.sleep(1)


# ── summary ─────────────────────────────────────────────────────────────────

# What a workload is called in the output.
NAMES = [
    ("pyo-gateway/envoy-gateway", "Envoy Gateway controller"),
    ("pyo-gateway/envoy-", "Envoy"),
    ("pyo-waf/pyo-waf", "WAF"),
    ("pyo-tunnel", "tunnel connector"),
    ("kube-system/svclb-", "svclb"),
    ("pyo-system/pyo-agent", "agent"),
    ("pyo-system/pyo-tailnet", "pyo-tailnet"),
]


def workload_name(key):
    for prefix, name in NAMES:
        if key.startswith(prefix) or key.split("/", 1)[1].startswith(prefix):
            return name
    return key


def clock(t):
    return time.strftime("%H:%M:%S", time.localtime(t))


def took(seconds):
    seconds = int(round(seconds))
    return f"{seconds // 60}:{seconds % 60:02d}"


def rollouts(path, offset):
    """Each workload's rollouts: from its first pod that isn't ready (new,
    or stopping) until all its pods are ready and none is stopping."""
    pods, open_since, windows = {}, {}, []
    last = 0.0
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return windows
    for line in lines:
        f = line.split()
        if len(f) < 2:
            continue
        t = float(f[0]) - offset
        last = t
        if f[1] == "RESYNC":
            pods = {}
            continue
        if len(f) < 10:
            continue
        event, ns, pod, kind, owner, phase, ready, deleting, node = f[1:10]
        if kind == "ReplicaSet":
            owner = owner.rsplit("-", 1)[0]
        elif kind not in ("DaemonSet", "StatefulSet"):
            continue
        key = f"{ns}/{owner}"
        mine = pods.setdefault(key, {})
        if event == "DELETED" or phase in ("Succeeded", "Failed"):
            mine.pop(pod, None)
        else:
            mine[pod] = ready == "ready" and deleting == "-"
        steady = all(mine.values())
        if not steady and key not in open_since:
            open_since[key] = t
        elif steady and key in open_since:
            windows.append((open_since.pop(key), t, key))
    for key, start in open_since.items():
        windows.append((start, last, key + " (not done at the end)"))
    # A new pod turns ready a moment before the old one is told to stop:
    # one rollout, not two.
    merged = []
    for w in sorted(windows, key=lambda w: (w[2], w[0])):
        if merged and merged[-1][2] == w[2] and w[0] - merged[-1][1] <= 5:
            merged[-1] = (merged[-1][0], w[1], w[2])
        else:
            merged.append(w)
    return sorted(merged)


def read_actions(path):
    actions, starts = [], {}
    offset = 0.0
    for line in open(path).read().splitlines():
        what, t, name = line.split(" ", 2)
        if what == "offset":
            offset = float(t)
        elif what == "start":
            starts[name] = float(t)
        elif what == "end" and name in starts:
            actions.append((starts.pop(name), float(t), name))
    now = time.time()
    for name, start in starts.items():
        actions.append((start, now, name + " (not finished)"))
    return sorted(actions), offset


def overlapping(windows, start, end, margin):
    return [w for w in windows if w[0] <= end + margin and w[1] >= start - margin]


def counts(results):
    by = {}
    for r in results:
        by[r] = by.get(r, 0) + 1
    return ", ".join(f"{k}: {v}" for k, v in sorted(by.items(), key=lambda kv: -kv[1]))


def why(result, detail):
    """A failure in a word: the status, or the error's kind."""
    if result != "err":
        return result
    kind, _, text = detail.removeprefix("post:").partition(":")
    if kind == "OSError" and "]_" in text:
        # OSError:[Errno_113]_No_route_to_host
        return text.split("]_", 1)[1].replace("_", " ").lower()
    return {
        "ConnectionResetError": "connection reset",
        "ConnectionRefusedError": "connection refused",
        "RemoteDisconnected": "closed without an answer",
        "TimeoutError": "timeout",
        "SSLEOFError": "closed during TLS",
        "BrokenPipeError": "connection closed",
    }.get(kind, kind)


def source(result, detail):
    """For a failed answer: from Cloudflare's edge or from the origin."""
    if result in ("200", "err"):
        return ""
    return detail.removeprefix("post:").split(":", 1)[0]


def cmd_summary(args):
    d = args[0]
    actions, offset = read_actions(os.path.join(d, "actions.log"))
    rolls = rollouts(os.path.join(d, "pods.log"), offset)
    targets = {}
    for line in open(os.path.join(d, "targets")).read().split():
        name, rest = line.split("=", 1)
        targets[name] = rest
    short, longs, ws = [], [], []
    for line in open(os.path.join(d, "load.log")).read().splitlines():
        f = line.split()
        if len(f) < 6:
            continue
        t, name, kind, result, ms, detail = float(f[0]), f[1], f[2], f[3], float(f[4]), f[5]
        if kind in ("fresh", "keepalive"):
            # What a failure is: anything but 200, or no answer.
            short.append((t, name, kind, result == "200", why(result, detail), detail.startswith("post"), source(result, detail)))
        elif kind == "long":
            longs.append((t, t + ms / 1000, name, result == "200", why(result, detail)))
        elif kind == "ws":
            ws.append((t, name, result, detail))
    if not short:
        print("No requests were logged.")
        return 2
    first, last = min(s[0] for s in short), max(s[0] for s in short)
    rolls = [r for r in rolls if r[1] >= first and r[0] <= last]

    def during(start, end):
        a = [x[2] for x in overlapping(actions, start, end, 0)]
        r = sorted({workload_name(x[2]) for x in overlapping(rolls, start, end, 2)})
        out = []
        if a:
            out.append("during: " + ", ".join(a))
        out.append("rolling: " + (", ".join(r) if r else "nothing"))
        return "; ".join(out)

    print()
    print("Rollouts seen")
    if not rolls:
        print("  none")
    for start, end, key in rolls:
        a = [x[2] for x in overlapping(actions, start, end, 0)]
        name = workload_name(key)
        print(f"  {clock(start)} to {clock(end)}  {key if name == key else f'{name} ({key})'}" + (f"  during: {', '.join(a)}" if a else ""))

    print()
    print("Actions")
    print(f"  {'action':<34} {'took':>5} {'requests':>9} {'failed':>7} {'ws drops':>9} {'long cut':>9}")
    quiet = lambda t: not overlapping(actions, t, t, 0)
    rows = [(name, start, end) for start, end, name in actions]
    for name, start, end in rows:
        inside = [s for s in short if start <= s[0] <= end]
        row = (
            len(inside),
            sum(1 for s in inside if not s[3]),
            sum(1 for w in ws if w[2] == "drop" and start <= w[0] <= end),
            sum(1 for l in longs if not l[3] and start <= l[1] <= end),
        )
        print(f"  {name:<34} {took(end - start):>5} {row[0]:>9} {row[1]:>7} {row[2]:>9} {row[3]:>9}")
    inside = [s for s in short if quiet(s[0])]
    print(
        f"  {'between actions':<34} {'':>5} {len(inside):>9} {sum(1 for s in inside if not s[3]):>7} "
        f"{sum(1 for w in ws if w[2] == 'drop' and quiet(w[0])):>9} {sum(1 for l in longs if not l[3] and quiet(l[1])):>9}"
    )

    failed_total = 0
    for name in targets:
        mine = [s for s in short if s[1] == name]
        failed = [s for s in mine if not s[3]]
        failed_total += len(failed)
        host, ip = targets[name].split("@")
        print()
        print(f"{name}  https://{host} on {ip}")
        line = f"  requests {len(mine)}, failed {len(failed)}"
        if failed:
            line += f" ({counts([s[4] for s in failed])})"
        print(line)
        answered = [s for s in failed if s[4].isdigit()]
        if answered:
            print(f"    answers: {counts([s[4] + ' ' + s[6] for s in answered])}")
        for kind in ("fresh", "keepalive"):
            k = [s for s in failed if s[2] == kind]
            if k:
                posts = sum(1 for s in k if s[5])
                print(f"    {kind}: {len(k)} of {sum(1 for s in mine if s[2] == kind)}" + (f", {posts} of them POSTs" if posts else ""))
        # Failures less than 2 seconds apart are one window.
        bursts = []
        for s in sorted(failed):
            if bursts and s[0] - bursts[-1][-1][0] <= 2:
                bursts[-1].append(s)
            else:
                bursts.append([s])
        for b in bursts:
            start, end = b[0][0], b[-1][0]
            print(f"  failed {clock(start)} to {clock(end)}  {len(b)} ({counts([s[4] for s in b])})  {during(start, end)}")
        my_longs = [l for l in longs if l[2] == name]
        cut = [l for l in my_longs if not l[3]]
        print(f"  long requests ({LONG_SECONDS} s) {len(my_longs)}, cut {len(cut)}")
        for l in cut:
            print(f"    cut {clock(l[1])} after {took(l[1] - l[0])} ({l[4]})  {during(l[1], l[1])}")
        drops = [w for w in ws if w[1] == name and w[2] == "drop"]
        fails = [w for w in ws if w[1] == name and w[2] == "fail"]
        opened = sum(1 for w in ws if w[1] == name and w[2] == "open")
        reason = lambda w: w[3].split(":", 1)[-1].replace("_", " ")[:60]
        if not opened and fails:
            print(f"  WebSocket never opened: {len(fails)} tries ({counts([reason(w) for w in fails])})")
        else:
            print(f"  WebSocket drops {len(drops)}" + (f", {len(fails)} failed to open" if fails else ""))
            for w in drops:
                print(f"    drop {clock(w[0])}  {during(w[0], w[0])}")
            for w in fails:
                print(f"    didn't open {clock(w[0])} ({reason(w)})  {during(w[0], w[0])}")

    print()
    if failed_total:
        print(f"FAIL: {failed_total} short requests failed.")
        return 1
    print("PASS: no short request failed.")
    return 0


# ── images ──────────────────────────────────────────────────────────────────

ACCEPT = ", ".join(
    [
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ]
)


class Registry:
    def __init__(self, where):
        self.host, _, self.prefix = where.partition("/")

    def call(self, method, path, body=None, headers=None, sink=None):
        if path.startswith("http://"):
            path = "/" + path.split("/", 3)[3]
        c = http.client.HTTPConnection(self.host, timeout=300)
        c.request(method, path, body=body, headers=headers or {})
        r = c.getresponse()
        if sink:
            while chunk := r.read(1 << 20):
                sink.write(chunk)
            data = b""
        else:
            data = r.read()
        c.close()
        return r.status, dict((k.lower(), v) for k, v in r.getheaders()), data

    def repo(self, name):
        return f"/v2/{self.prefix}/{name}" if self.prefix else f"/v2/{name}"


def image_repo(name):
    """postgres-16 is the tag 16-<tag> of the postgres repository."""
    if name.startswith("postgres-"):
        return "postgres", name.split("-", 1)[1] + "-"
    return name, ""


def cmd_images_pull(args):
    """The build TAG: each image at TAG, or at its newest tag before it (an
    image that didn't change keeps its last build's tag, as pyo-up.sh
    deploys it). Writes DIR/images.json with every image's digest; the
    images named are also downloaded."""
    reg, d, tag, copy = Registry(args[0]), args[1], args[2], args[3:]
    names = ["pyo", "agent", "builder", "flows", "waf", "geoip", "db-controller", "kv", "static"]
    names += ["mysql-operator", "mysql-backup", "postgres-16", "postgres-17", "postgres-18"]
    os.makedirs(os.path.join(d, "blobs"), exist_ok=True)
    images = {}
    for name in names:
        repo, lead = image_repo(name)
        status, _, body = reg.call("GET", reg.repo(repo) + "/tags/list")
        tags = json.loads(body).get("tags") or [] if status == 200 else []
        mine = sorted(t[len(lead):] for t in tags if t.startswith(lead) and t[len(lead):].isdigit() and t[len(lead):] <= tag)
        if not mine:
            sys.exit(f"{args[0]} has no {name} image at {tag} or before it.")
        at = lead + mine[-1]
        status, head, body = reg.call("GET", reg.repo(repo) + "/manifests/" + at, headers={"Accept": ACCEPT})
        if status != 200:
            sys.exit(f"{args[0]}/{repo}:{at}: HTTP {status}")
        digest = "sha256:" + hashlib.sha256(body).hexdigest()
        images[name] = {"repo": repo, "tag": at, "digest": digest, "copied": name in copy}
        if name in copy:
            pull_manifest(reg, repo, digest, head.get("content-type", ""), body, d)
    json.dump(images, open(os.path.join(d, "images.json"), "w"), indent=1)


def pull_manifest(reg, repo, digest, media, body, d):
    mdir = os.path.join(d, "manifests", repo)
    os.makedirs(mdir, exist_ok=True)
    doc = json.loads(body)
    media = doc.get("mediaType") or media
    for child in doc.get("manifests", []):
        status, head, sub = reg.call("GET", reg.repo(repo) + "/manifests/" + child["digest"], headers={"Accept": ACCEPT})
        if status != 200:
            sys.exit(f"{repo}@{child['digest']}: HTTP {status}")
        pull_manifest(reg, repo, child["digest"], head.get("content-type", ""), sub, d)
    for blob in [doc.get("config")] + doc.get("layers", []) if "layers" in doc else []:
        path = os.path.join(d, "blobs", blob["digest"].replace(":", "_"))
        if os.path.exists(path):
            continue
        with open(path + ".part", "wb") as f:
            status, _, _ = reg.call("GET", reg.repo(repo) + "/blobs/" + blob["digest"], sink=f)
        if status != 200:
            sys.exit(f"{repo} blob {blob['digest']}: HTTP {status}")
        os.rename(path + ".part", path)
    # Children first, so a push in this order never names a missing one.
    with open(os.path.join(mdir, "order"), "a") as f:
        f.write(f"{digest} {media}\n")
    open(os.path.join(mdir, digest.replace(":", "_")), "wb").write(body)


def cmd_images_push(args):
    reg, d = Registry(args[0]), args[1]
    mroot = os.path.join(d, "manifests")
    for repo in sorted(os.listdir(mroot)):
        for line in open(os.path.join(mroot, repo, "order")).read().splitlines():
            digest, media = line.split(" ", 1)
            body = open(os.path.join(mroot, repo, digest.replace(":", "_")), "rb").read()
            doc = json.loads(body)
            for blob in [doc.get("config")] + doc.get("layers", []) if "layers" in doc else []:
                push_blob(reg, repo, blob["digest"], os.path.join(d, "blobs", blob["digest"].replace(":", "_")))
            status, _, out = reg.call("PUT", reg.repo(repo) + "/manifests/" + digest, body=body, headers={"Content-Type": media})
            if status not in (200, 201):
                sys.exit(f"{repo}@{digest}: HTTP {status} {out[:200]!r}")


def push_blob(reg, repo, digest, path):
    status, _, _ = reg.call("HEAD", reg.repo(repo) + "/blobs/" + digest)
    if status == 200:
        return
    status, head, _ = reg.call("POST", reg.repo(repo) + "/blobs/uploads/")
    if status != 202:
        sys.exit(f"{repo}: starting an upload: HTTP {status}")
    where = head["location"]
    where += ("&" if "?" in where else "?") + "digest=" + digest
    with open(path, "rb") as f:
        status, _, out = reg.call(
            "PUT", where, body=f, headers={"Content-Type": "application/octet-stream", "Content-Length": str(os.path.getsize(path))}
        )
    if status != 201:
        sys.exit(f"{repo} blob {digest}: HTTP {status} {out[:200]!r}")


COMMANDS = {
    "load": cmd_load,
    "watch": cmd_watch,
    "summary": cmd_summary,
    "images-pull": cmd_images_pull,
    "images-push": cmd_images_push,
}

if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in COMMANDS:
        sys.exit(__doc__)
    sys.exit(COMMANDS[sys.argv[1]](sys.argv[2:]))
