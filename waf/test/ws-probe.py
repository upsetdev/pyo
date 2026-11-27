"""ws-probe.py PORT HOST PATH [HEADER ...]: one WebSocket handshake.

Prints the status line (or TIMEOUT), then one line per echo. A HEADER of
BODY=<text> sends that body with a Content-Length, CHUNKED=<text> sends it
chunked. N (frames, default 1), HOLD (seconds between frames) and MSG
(the frame's text) from the environment.
"""
import base64, os, socket, sys, time

port, host, path = int(sys.argv[1]), sys.argv[2], sys.argv[3]
key = base64.b64encode(os.urandom(16)).decode()
req = (f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
       f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n")
if not any(h.lower().startswith("user-agent:") for h in sys.argv[4:]):
    req += "User-Agent: ws-probe/1\r\n"
body = b""
for h in sys.argv[4:]:
    if h.startswith("BODY="):
        body = h[5:].encode()
        req += f"Content-Length: {len(body)}\r\n"
    elif h.startswith("CHUNKED="):
        b = h[8:].encode()
        body = f"{len(b):x}\r\n".encode() + b + b"\r\n0\r\n\r\n"
        req += "Transfer-Encoding: chunked\r\n"
    else:
        req += h + "\r\n"
req += "\r\n"
s = socket.create_connection(("127.0.0.1", port), timeout=8)
t = time.time()
s.sendall(req.encode() + body)
buf = b""
try:
    while b"\r\n\r\n" not in buf:
        d = s.recv(4096)
        if not d:
            break
        buf += d
except socket.timeout:
    print(f"TIMEOUT after {time.time() - t:.1f}s")
    sys.exit(0)
line = buf.split(b"\r\n", 1)[0].decode(errors="replace")
print(f"{line}  ({(time.time() - t) * 1000:.0f} ms)")
if " 101 " not in line + " ":
    sys.exit(0)
rest = buf.split(b"\r\n\r\n", 1)[1]
msg = os.environ.get("MSG", "hello <script>alert(1)</script>").encode()
hold = float(os.environ.get("HOLD", "0"))
for i in range(int(os.environ.get("N", "1"))):
    mask = os.urandom(4)
    s.sendall(bytes([0x81, 0x80 | len(msg)]) + mask + bytes(b ^ mask[j % 4] for j, b in enumerate(msg)))
    try:
        while len(rest) < 2 + len(msg):
            d = s.recv(4096)
            if not d:
                print(f"echo {i}: connection closed")
                sys.exit(0)
            rest += d
        print(f"echo {i}: {'ok' if rest[2:2 + len(msg)] == msg else 'WRONG'}")
        rest = rest[2 + len(msg):]
    except socket.timeout:
        print(f"echo {i}: TIMEOUT")
        sys.exit(0)
    if hold:
        time.sleep(hold)
