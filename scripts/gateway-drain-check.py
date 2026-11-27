"""Helpers for gateway-drain-check.sh. Run with python3 -I.

  yaml SCENARIO.json CERT KEY OUT.yaml [--before]
      The scenario's objects as a YAML stream for egctl, with a TLS Secret
      for each certificate its Gateways name. --before: as Pyo wrote them
      before every website route kept its policies (the open
      authorization, the idle rate limit and the WAF's mode override
      left out).
  rollout OLD_DIR NEW_DIR
      For each state, what the update from the old objects to the new
      ones changes in the listeners (once, on the first apply).
  compare DIR
      Compares each change's translation (DIR/<name>.json, from egctl)
      with the state before it. Exits 1 when a change that should change
      only routes changes a listener's HTTP filters.
"""
import base64, copy, hashlib, json, os, sys

# Changes that must change only routes, and those that still change a
# listener's HTTP filters (known limits).
STABLE = [
    ("ip_rule", "base"),
    ("ip_rule_changed", "ip_rule"),
    ("country_rule", "base"),
    ("rate_limit", "base"),
    ("rate_limit_changed", "rate_limit"),
    ("all_rules", "base"),
    ("base", "all_rules"),
    ("redirect", "base"),
    ("domain_added", "base"),
    ("header_rule", "base"),
    ("base", "header_rule"),
    ("path_route", "base"),
    ("base", "path_route"),
    ("waf_header_rule", "waf_block"),
    ("waf_block", "waf_header_rule"),
    ("waf_path_route", "waf_block"),
    ("service_added", "base"),
    ("base", "service_added"),
    # Traffic paths: Tailnet off and on for a ts.net name, and a domain on
    # the tunnel going from "switching" to "on".
    ("base", "tailnet_on"),
    ("tailnet_on", "base"),
    ("tunnel_on", "tunnel_switching"),
]
KNOWN = [
    ("waf_block", "base"),
    ("waf_log", "waf_block"),
    # Traffic paths, as the design's cut table says: a free name on a
    # Tailscale address loses its own chain when Tailnet goes off (it's
    # the domain that stops), a domain moving to the tunnel or back drains
    # its own chain once, and PROXY protocol drains the whole listener.
    ("base", "tailnet_free_on"),
    ("tunnel_switching", "base"),
    ("base", "tunnel_on"),
    ("proxy_protocol", "base"),
    ("base", "proxy_protocol"),
]


KNOWN_GROUPS = {"v1", "gateway.networking.k8s.io", "gateway.envoyproxy.io"}


def idle(rate):
    rules = (rate or {}).get("local", {}).get("rules", [])
    return bool(rules) and all(r.get("xRateLimitHeaders") == "Off" for r in rules)


def open_auth(auth):
    return auth.get("defaultAction") == "Allow" and [r.get("name") for r in auth.get("rules", [])] == ["acme"]


def before(objs):
    """The objects as Pyo wrote them before 0.51.1: route policies one per
    route and named after it, only while the firewall had rules (the
    message with access rules, on plain HTTP too), no mode override."""
    sps = {o["metadata"]["name"]: o for o in objs if o["kind"] == "SecurityPolicy"}
    opened = {n for n, o in sps.items() if open_auth(o["spec"].get("authorization", {}))}
    message = next((o["spec"]["responseOverride"] for o in objs
                    if o["kind"] == "BackendTrafficPolicy" and "responseOverride" in o["spec"]), None)
    out = []
    for o in objs:
        name = o["metadata"]["name"]
        if o["kind"] == "SecurityPolicy" and name in opened:
            continue
        if o["kind"] not in ("BackendTrafficPolicy", "EnvoyExtensionPolicy"):
            out.append(o)
            continue
        refs = o["spec"]["targetRefs"]
        waf = None
        if o["kind"] == "EnvoyExtensionPolicy":
            waf = json.loads(o["metadata"]["annotations"]["pyo.dev/waf"])
        for i, t in enumerate(refs):
            e = copy.deepcopy(o)
            e["metadata"]["name"] = t["name"]
            e["spec"]["targetRefs"] = [t]
            if waf is not None:
                e["metadata"]["annotations"]["pyo.dev/waf"] = json.dumps(waf[i])
                for x in e["spec"].get("extProc", []):
                    x.get("processingMode", {}).pop("allowModeOverride", None)
            elif t["kind"] == "HTTPRoute":
                if idle(e["spec"].get("rateLimit")):
                    del e["spec"]["rateLimit"]
                e["spec"].pop("responseOverride", None)
                if t["name"] in sps and t["name"] not in opened and message:
                    e["spec"]["responseOverride"] = message
                if "rateLimit" not in e["spec"] and "responseOverride" not in e["spec"]:
                    continue
            out.append(e)
    return out


def to_yaml(src, cert, key, dst, old):
    # What egctl translates (no cert-manager Certificates).
    objs = [o for o in json.load(open(src)) if o["apiVersion"].split("/")[0] in KNOWN_GROUPS]
    if old:
        objs = before(objs)
    b64 = lambda p: base64.b64encode(open(p, "rb").read()).decode()
    secrets = set()
    for o in objs:
        if o["kind"] == "Gateway":
            for l in o["spec"].get("listeners", []):
                for c in l.get("tls", {}).get("certificateRefs", []):
                    secrets.add((o["metadata"]["namespace"], c["name"]))
    for ns, name in sorted(secrets):
        objs.append({"apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/tls",
                     "metadata": {"name": name, "namespace": ns},
                     "data": {"tls.crt": b64(cert), "tls.key": b64(key)}})
    with open(dst, "w") as f:
        f.write("\n---\n".join(json.dumps(o) for o in objs))


def h(v):
    return hashlib.sha256(json.dumps(v, sort_keys=True).encode()).hexdigest()[:12]


def chains(path):
    """Each HTTP filter chain: its filters (name, hash) and the rest of its
    connection manager's settings (hash)."""
    out = {}
    for gw, v in json.load(open(path))["xds"].items():
        for c in v["configs"]:
            if "ListenersConfigDump" not in c.get("@type", ""):
                continue
            for l in c.get("dynamicListeners", []):
                lst = l["activeState"]["listener"]
                fcs = lst.get("filterChains", []) + ([lst["defaultFilterChain"]] if "defaultFilterChain" in lst else [])
                for fc in fcs:
                    for f in fc["filters"]:
                        if "http_connection_manager" not in f["name"]:
                            continue
                        hcm = dict(f["typedConfig"])
                        filters = hcm.pop("httpFilters", [])
                        out[f"{lst['name']} {fc.get('name', 'default')}"] = ([(x["name"], h(x)) for x in filters], h(hcm))
    return out


def short(n):
    return n.replace("envoy.filters.http.", "").replace("pyo-gateway/", "")


def changes(a, b):
    """What changed in the listeners: (drops connections, description). A
    new filter chain (a new domain) leaves the others' connections alone:
    Envoy updates a listener's chains in place."""
    c0, c1 = chains(a), chains(b)
    out = []
    for k in sorted(set(c0) | set(c1)):
        if c0.get(k) == c1.get(k):
            continue
        if k not in c0:
            out.append((False, f"new chain (the others untouched): {short(k)}"))
            continue
        if k not in c1:
            out.append((True, f"chain removed: {short(k)}"))
            continue
        n0, n1 = dict(c0[k][0]), dict(c1[k][0])
        what = [f"+{short(x)}" for x in n1 if x not in n0] + [f"-{short(x)}" for x in n0 if x not in n1]
        what += [f"~{short(x)}" for x in n1 if x in n0 and n0[x] != n1[x]]
        if c0[k][1] != c1[k][1]:
            what.append("~connection manager settings")
        out.append((True, f"{short(k)}: {' '.join(what)}"))
    return out


def compare(d):
    bad = 0
    for kind, pairs in (("must change only routes", STABLE), ("known to drop connections", KNOWN)):
        print(f"{kind}:")
        for new, old in pairs:
            c = changes(f"{d}/{old}.json", f"{d}/{new}.json")
            drops = any(x for x, _ in c)
            print(f"  {old} -> {new}: {'DROPS CONNECTIONS' if drops else 'routes only'}")
            for _, x in c:
                print(f"      {x}")
            if drops and pairs is STABLE:
                bad = 1
    return bad


def rollout(old, new):
    """The update itself: each state as written before, then now."""
    for f in sorted(os.listdir(new)):
        if not f.endswith(".json"):
            continue
        c = changes(f"{old}/{f}", f"{new}/{f}")
        drops = any(x for x, _ in c)
        print(f"  {f[:-5]}: {'DROPS CONNECTIONS (once)' if drops else 'routes only'}")
        for _, x in c:
            print(f"      {x}")


if __name__ == "__main__":
    if sys.argv[1] == "yaml":
        to_yaml(*sys.argv[2:6], old="--before" in sys.argv)
    elif sys.argv[1] == "compare":
        sys.exit(compare(sys.argv[2]))
    elif sys.argv[1] == "rollout":
        rollout(sys.argv[2], sys.argv[3])
