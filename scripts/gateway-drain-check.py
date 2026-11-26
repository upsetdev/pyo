"""Helpers for gateway-drain-check.sh. Run with python3 -I.

  yaml SCENARIO.json CERT KEY OUT.yaml [--before]
      The scenario's objects as a YAML stream for egctl, with a TLS Secret
      for each certificate its Gateways name. --before: as Pyo wrote them
      before every website route kept its policies (the open
      authorization, the idle rate limit and the WAF's mode override
      left out).
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
]
KNOWN = [
    ("waf_block", "base"),
    ("waf_log", "waf_block"),
    ("header_rule", "base"),
    ("service_added", "base"),
]


KNOWN_GROUPS = {"v1", "gateway.networking.k8s.io", "gateway.envoyproxy.io"}


def idle(rate):
    rules = (rate or {}).get("local", {}).get("rules", [])
    return bool(rules) and all(r.get("xRateLimitHeaders") == "Off" for r in rules)


def open_auth(auth):
    return auth.get("defaultAction") == "Allow" and [r.get("name") for r in auth.get("rules", [])] == ["acme"]


def before(objs):
    opened = {o["metadata"]["name"] for o in objs if o["kind"] == "SecurityPolicy" and open_auth(o["spec"].get("authorization", {}))}
    out = []
    for o in objs:
        o = copy.deepcopy(o)
        name = o["metadata"]["name"]
        if o["kind"] == "SecurityPolicy" and name in opened:
            continue
        if o["kind"] == "BackendTrafficPolicy":
            if idle(o["spec"].get("rateLimit")):
                del o["spec"]["rateLimit"]
            if name in opened:
                o["spec"].pop("responseOverride", None)
            if "rateLimit" not in o["spec"] and "responseOverride" not in o["spec"]:
                continue
        if o["kind"] == "EnvoyExtensionPolicy":
            for e in o["spec"].get("extProc", []):
                e.get("processingMode", {}).pop("allowModeOverride", None)
        out.append(o)
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


if __name__ == "__main__":
    if sys.argv[1] == "yaml":
        to_yaml(*sys.argv[2:6], old="--before" in sys.argv)
    elif sys.argv[1] == "compare":
        sys.exit(compare(sys.argv[2]))
