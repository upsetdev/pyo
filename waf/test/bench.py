"""bench.py PORT HOST [N] [PATH]: N sequential keep-alive GETs; prints p50/p99/mean in ms."""
import http.client, sys, time, statistics
port, host = int(sys.argv[1]), sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 2000
path = sys.argv[4] if len(sys.argv) > 4 else "/index.html?page=2&sort=name"
c = http.client.HTTPConnection("127.0.0.1", port)
ts = []
for i in range(n + 200):
    t = time.perf_counter()
    c.request("GET", path, headers={"Host": host, "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) bench", "Accept": "text/html"})
    r = c.getresponse(); r.read()
    if i >= 200:  # warm-up
        ts.append((time.perf_counter() - t) * 1000)
ts.sort()
print(f"port {port} host {host}: p50 {ts[len(ts)//2]:.3f} ms, p99 {ts[int(len(ts)*0.99)]:.3f} ms, mean {statistics.mean(ts):.3f} ms (n={n}, status {r.status})")
