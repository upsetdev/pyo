// Drives pyo-kv with the Redis REST SDK (default: base64 responses), from
// the variables a connected service gets: KV_REST_API_URL, KV_REST_API_TOKEN.
import { Redis } from "@upstash/redis";
const url = process.env.KV_REST_API_URL ?? "http://127.0.0.1:18079";
const token = process.env.KV_REST_API_TOKEN ?? "tok";
const r = new Redis({ url, token });
const plain = new Redis({ url, token, responseEncoding: false });
const ro = new Redis({ url, token: process.env.KV_REST_API_READ_ONLY_TOKEN ?? "ro" });
let fail = 0, pass = 0;
const eq = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  ok ? pass++ : fail++;
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${ok ? "" : `: got ${JSON.stringify(got)} want ${JSON.stringify(want)}`}`);
};
await r.flushdb();
eq("ping", await r.ping(), "PONG");
eq("set", await r.set("k", "héllo wörld ✓"), "OK");
eq("get unicode", await r.get("k"), "héllo wörld ✓");
eq("set object", await r.set("obj", { a: 1, b: [1, 2] }, { ex: 100 }), "OK");
eq("get object", await r.get("obj"), { a: 1, b: [1, 2] });
eq("ttl", (await r.ttl("obj")) > 90, true);
eq("incr", await r.incr("n"), 1);
eq("incrby", await r.incrby("n", 41), 42);
eq("get number", await r.get("n"), 42);
eq("missing", await r.get("nope"), null);
eq("hset", await r.hset("h", { name: "pyo", stars: 5 }), 2);
eq("hgetall", await r.hgetall("h"), { name: "pyo", stars: 5 });
eq("hget", await r.hget("h", "name"), "pyo");
eq("rpush", await r.rpush("l", "a", "b", "c"), 3);
eq("lrange", await r.lrange("l", 0, -1), ["a", "b", "c"]);
eq("zadd", await r.zadd("z", { score: 1, member: "one" }, { score: 2, member: "two" }), 2);
eq("zrange withscores", await r.zrange("z", 0, -1, { withScores: true }), ["one", 1, "two", 2]);
eq("sadd/smembers", (await r.sadd("s", "x"), await r.smembers("s")), ["x"]);
eq("mset/mget", (await r.mset({ m1: "a", m2: "b" }), await r.mget("m1", "m2", "m3")), ["a", "b", null]);
eq("type (status reply)", await r.type("h"), "hash");
eq("eval", await r.eval("return {KEYS[1], ARGV[1], 7}", ["key"], ["arg"]), ["key", "arg", 7]);
eq("scan", (await r.scan(0, { match: "m*" }))[1].sort(), ["m1", "m2"]);
const p = r.pipeline();
p.set("p1", "v1"); p.get("p1"); p.incr("pn"); p.hset("ph", { f: "v" }); p.hgetall("ph");
eq("pipeline", await p.exec(), ["OK", "v1", 1, 1, { f: "v" }]);
const tx = r.multi();
tx.set("t", 10); tx.incrby("t", 5); tx.get("t");
eq("multi-exec", await tx.exec(), ["OK", 15, 15]);
try { const bad = r.multi(); bad.set("a", 1); bad.lpush(); await bad.exec(); eq("multi with error throws", false, true); }
catch { eq("multi with error throws", true, true); }
eq("plain encoding", (await plain.set("pk", "plain ✓"), await plain.get("pk")), "plain ✓");
eq("read-only can read", await ro.get("k"), "héllo wörld ✓");
try { await ro.set("k", "x"); eq("read-only refuses write", false, true); }
catch (e) { eq("read-only refuses write", /NOPERM/.test(e.message), true); }
try { await r.incr("k"); eq("server error thrown", false, true); } catch (e) { eq("server error thrown", /not an integer/.test(e.message), true); }
// Auto-pipelining batches concurrent commands into one /pipeline request.
const auto = new Redis({ url, token, enableAutoPipelining: true });
const many = await Promise.all(Array.from({ length: 50 }, (_, i) => auto.incr("auto")));
eq("auto pipeline 50", many.sort((a, b) => a - b).at(-1), 50);
const big = "x".repeat(1 << 20);
eq("1 MiB value", (await r.set("big", big), (await r.get("big")).length), 1 << 20);
eq("del", await r.del("k", "obj", "big"), 3);
console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
