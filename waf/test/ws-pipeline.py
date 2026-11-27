"""ws-pipeline.py PORT HOST PATH: an upgrade request with a second request
(an XSS in its query, to /pipelined) sent right behind it in the same
write. Prints the status lines that come back within 6 s."""
import socket, sys, time

port, host, path = int(sys.argv[1]), sys.argv[2], sys.argv[3]
up = (f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
      "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
second = f"GET /pipelined?q=<script>alert(1)</script> HTTP/1.1\r\nHost: {host}\r\n\r\n"
s = socket.create_connection(("127.0.0.1", port), timeout=6)
s.sendall((up + second).encode())
buf, t = b"", time.time()
try:
    while time.time() - t < 6:
        d = s.recv(65536)
        if not d:
            break
        buf += d
except socket.timeout:
    pass
lines = [l for l in buf.decode(errors="replace").split("\r\n") if l.startswith("HTTP/1.1")]
print(f"responses: {' | '.join(lines) or 'none'}")
