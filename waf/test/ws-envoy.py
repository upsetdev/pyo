"""ws-envoy.py ENVOY_JSON WAF_JSON: configs for websocket.sh.

One listener per case, each with the ext_proc filter as Envoy Gateway
v1.9.3 writes it for Pyo's WAF policy (`waf_ext_proc` in pyo-proto's
system.rs: other phases SKIP, the source address, 503 when the WAF can't
be reached), and WebSocket upgrades on, as Envoy Gateway sets them per
route. The WAF settings: block.test blocks, log.test only logs.
"""
import json, sys

# port: (WAF, body mode, fails open, message timeout, allowModeOverride)
CASES = {
    10001: ("zd-ws-waf", "BUFFERED_PARTIAL", False, "5s", True),  # block mode
    10002: ("zd-ws-waf", "STREAMED", True, "0.5s", True),  # log only
    10003: ("zd-ws-waf", "BUFFERED_PARTIAL", False, "5s", False),  # block, older policy
    10004: ("zd-ws-waf", "STREAMED", True, "0.5s", False),  # log only, older policy
    10005: ("zd-ws-waf-old", "BUFFERED_PARTIAL", False, "5s", True),  # older WAF
    10006: ("zd-ws-waf-old", "STREAMED", True, "0.5s", True),  # older WAF, log only
}


def cluster(name, host, port, h2=False):
    c = {"name": name, "type": "STRICT_DNS", "connect_timeout": "1s",
         "load_assignment": {"cluster_name": name, "endpoints": [{"lb_endpoints": [
             {"endpoint": {"address": {"socket_address": {"address": host, "port_value": port}}}}]}]}}
    if h2:
        c["typed_extension_protocol_options"] = {"envoy.extensions.upstreams.http.v3.HttpProtocolOptions": {
            "@type": "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions",
            "explicit_http_config": {"http2_protocol_options": {}}}}
    return c


def listener(port, waf, body, fail_open, timeout, override):
    ep = {"@type": "type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExternalProcessor",
          "grpc_service": {"envoy_grpc": {"cluster_name": waf}, "timeout": "10s"},
          "failure_mode_allow": fail_open, "message_timeout": timeout,
          "status_on_error": {"code": 503},
          "request_attributes": ["source.address"],
          "allow_mode_override": override,
          "processing_mode": {"request_header_mode": "SEND", "response_header_mode": "SKIP",
                              "request_body_mode": body, "response_body_mode": "NONE",
                              "request_trailer_mode": "SKIP", "response_trailer_mode": "SKIP"}}
    log = {"name": "envoy.access_loggers.stdout", "typed_config": {
        "@type": "type.googleapis.com/envoy.extensions.access_loggers.stream.v3.StdoutAccessLog",
        "log_format": {"json_format": {"port": port, "status": "%RESPONSE_CODE%", "path": "%REQ(:PATH)%",
                                       "details": "%RESPONSE_CODE_DETAILS%", "ms": "%DURATION%"}}}}
    route = {"match": {"prefix": "/"}, "route": {"cluster": "backend", "timeout": "60s",
                                                 "upgrade_configs": [{"upgrade_type": "websocket"}]}}
    return {"name": f"l{port}", "address": {"socket_address": {"address": "0.0.0.0", "port_value": port}},
            "per_connection_buffer_limit_bytes": 262144,
            "filter_chains": [{"filters": [{"name": "envoy.filters.network.http_connection_manager", "typed_config": {
                "@type": "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager",
                "stat_prefix": f"l{port}", "access_log": [log],
                "route_config": {"virtual_hosts": [{"name": "all", "domains": ["*"], "routes": [route]}]},
                "http_filters": [{"name": "envoy.filters.http.ext_proc", "typed_config": ep},
                                 {"name": "envoy.filters.http.router", "typed_config": {
                                     "@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}}],
            }}]}]}


cfg = {"admin": {"address": {"socket_address": {"address": "0.0.0.0", "port_value": 9901}}},
       "static_resources": {
           "clusters": [cluster("backend", "zd-ws-backend", 80),
                        cluster("zd-ws-waf", "zd-ws-waf", 9002, True),
                        cluster("zd-ws-waf-old", "zd-ws-waf-old", 9002, True)],
           "listeners": [listener(p, *c) for p, c in CASES.items()]}}
json.dump(cfg, open(sys.argv[1], "w"), indent=1)
json.dump({"sets": {"block": {"engine": "On", "level": 1}, "log": {"engine": "DetectionOnly", "level": 1}},
           "hosts": {"block.test": "block", "log.test": "log"}}, open(sys.argv[2], "w"), indent=1)
