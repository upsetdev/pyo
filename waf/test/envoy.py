"""Writes the test Envoy configs: a baseline (no WAF) and one with Coraza.

python3 envoy.py <out-dir> <wasm-path-inside-container>
"""
import json, sys

def directives(engine, level=1, exceptions=()):
    d = ["Include @recommended-conf", "SecAuditEngine Off", "Include @crs-setup-conf",
         f"SecRuleEngine {engine}",
         f'SecAction "id:900000,phase:1,pass,t:none,nolog,setvar:tx.blocking_paranoia_level={level}"',
         "Include @owasp_crs/*.conf"]
    d += [f"SecRuleRemoveById {i}" for i in exceptions]
    return d

plugin = {
    "directives_map": {
        "block": directives("On"),
        "log": directives("DetectionOnly"),
        "block-pl2": directives("On", 2),
        "block-except": directives("On", 1, [942100]),
        "off": directives("Off"),
    },
    "default_directives": "off",
    "per_authority_directives": {
        "block.test": "block", "log.test": "log", "pl2.test": "block-pl2",
        "except.test": "block-except", "off.test": "off",
    },
}

def config(port, admin, wasm, extproc=None):
    filters = []
    clusters = []
    if extproc:
        filters.append({
            "name": "envoy.filters.http.ext_proc",
            "typed_config": {
                "@type": "type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExternalProcessor",
                "grpc_service": {"envoy_grpc": {"cluster_name": "waf"}, "timeout": "1s"},
                "failure_mode_allow": True,
                "message_timeout": "1s",
                "request_attributes": ["source.address"],
                "processing_mode": {"request_header_mode": "SEND", "response_header_mode": "SKIP", "request_body_mode": "BUFFERED_PARTIAL",
                                    "response_body_mode": "NONE", "request_trailer_mode": "SKIP", "response_trailer_mode": "SKIP"},
            },
        })
        host, wport = extproc.split(":")
        clusters.append({"name": "waf", "type": "STRICT_DNS", "connect_timeout": "1s",
                         "typed_extension_protocol_options": {"envoy.extensions.upstreams.http.v3.HttpProtocolOptions": {
                             "@type": "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions",
                             "explicit_http_config": {"http2_protocol_options": {}}}},
                         "load_assignment": {"cluster_name": "waf", "endpoints": [{"lb_endpoints": [{"endpoint": {"address": {"socket_address": {"address": host, "port_value": int(wport)}}}}]}]}})
    if wasm:
        filters.append({
            "name": "envoy.filters.http.wasm",
            "typed_config": {
                "@type": "type.googleapis.com/envoy.extensions.filters.http.wasm.v3.Wasm",
                "config": {
                    "name": "coraza", "root_id": "",
                    "configuration": {"@type": "type.googleapis.com/google.protobuf.StringValue", "value": json.dumps(plugin)},
                    "vm_config": {"runtime": "envoy.wasm.runtime.v8", "vm_id": "coraza",
                                  "code": {"local": {"filename": wasm}}},
                },
            },
        })
    filters.append({"name": "envoy.filters.http.router", "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}})
    return {
        "admin": {"address": {"socket_address": {"address": "0.0.0.0", "port_value": admin}}},
        "static_resources": {"clusters": clusters, "listeners": [{
            "address": {"socket_address": {"address": "0.0.0.0", "port_value": port}},
            # Pyo's client policy (WAF_BODY_LIMIT): what Block mode inspects.
            "per_connection_buffer_limit_bytes": 262144,
            "filter_chains": [{"filters": [{
                "name": "envoy.filters.network.http_connection_manager",
                "typed_config": {
                    "@type": "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager",
                    "stat_prefix": "http",
                    "access_log": [{"name": "envoy.access_loggers.stdout", "typed_config": {
                        "@type": "type.googleapis.com/envoy.extensions.access_loggers.stream.v3.StdoutAccessLog",
                        "log_format": {"json_format": {"status": "%RESPONSE_CODE%", "host": "%REQ(:AUTHORITY)%", "path": "%REQ(:PATH)%", "details": "%RESPONSE_CODE_DETAILS%"}}}}],
                    "route_config": {"virtual_hosts": [{"name": "all", "domains": ["*"], "routes": [{
                        "match": {"prefix": "/"}, "direct_response": {"status": 200, "body": {"inline_string": "ok\n"}}}]}]},
                    "http_filters": filters,
                }}]}],
        }]},
    }

out = sys.argv[1]
json.dump(config(10000, 9901, None), open(f"{out}/base.json", "w"), indent=1)
json.dump(config(10000, 9901, sys.argv[2]), open(f"{out}/waf.json", "w"), indent=1)
# One directive set only (every host the same), to see what each set costs.
full = dict(plugin)
plugin = {"directives_map": {"block": directives("On")}, "default_directives": "block", "per_authority_directives": {}}
json.dump(config(10000, 9901, sys.argv[2]), open(f"{out}/waf1.json", "w"), indent=1)
plugin = full
json.dump(config(10000, 9901, None, "waf-extproc:9002"), open(f"{out}/extproc.json", "w"), indent=1)
# pyo-waf's own settings: the same hosts and sets.
json.dump({"sets": {"block": {"engine": "On", "level": 1}, "log": {"engine": "DetectionOnly", "level": 1},
                    "block-pl2": {"engine": "On", "level": 2}, "block-except": {"engine": "On", "level": 1, "exceptions": [942100]}},
           "hosts": {"block.test": "block", "log.test": "log", "pl2.test": "block-pl2", "except.test": "block-except"}},
          open(f"{out}/waf-config.json", "w"), indent=1)
