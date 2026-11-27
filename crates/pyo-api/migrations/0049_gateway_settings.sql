-- The Gateway section (docs/traffic-and-gateway-design.md §13): a
-- cluster's gateway settings. Every cluster starts with what its gateway
-- does today, so nothing changes until someone saves.

-- HTTP/3 on UDP 443 next to HTTPS.
ALTER TABLE pyo.clusters ADD COLUMN http3 boolean NOT NULL DEFAULT false;

-- Responses on services' HTTPS domains compressed (Zstd, Brotli, Gzip).
ALTER TABLE pyo.clusters ADD COLUMN compression boolean NOT NULL DEFAULT false;

-- The TLS minimum on HTTPS: '1.2' or '1.3'.
ALTER TABLE pyo.clusters ADD COLUMN tls_min text NOT NULL DEFAULT '1.2';

-- Defaults for services without their own, in seconds. The request
-- timeout: 0 for no limit. The idle timeout: how long a WebSocket or
-- stream may stay quiet.
ALTER TABLE pyo.clusters ADD COLUMN request_timeout integer NOT NULL DEFAULT 300;
ALTER TABLE pyo.clusters ADD COLUMN idle_timeout integer NOT NULL DEFAULT 3600;
