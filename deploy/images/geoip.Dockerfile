# The GeoIP country database Pyo Gateway's firewall places visitors
# with (GeoLite2 Country, the same source as analytics' bundled GeoLite2
# City). Just the file: Pyo Gateway mounts this image read-only as a volume
# (PYO_GEOIP_IMAGE; pyo-proto's system.rs), so it needs no credentials or
# downloads for it.
#
# The database is pinned: the date MaxMind built it, the upstream commit
# that holds it, and the tarball's SHA-256 as that commit's
# GeoLite2-Country.tar.gz.sha256 gives it. The image is tagged with the
# date (scripts/image-tags.sh), not with Pyo's version: the proxies, the
# WAF and the traffic store mount it, and they restart only when it
# changes. scripts/geoip-pin.sh moves the three lines to a newer database
# (RELEASING.md, "Bump the GeoIP database").
ARG GEOIP_DATE=2026-11-06
ARG GEOIP_COMMIT=dc0190a06c54571fb68c4f171de9b11da1ed3cc3
ARG GEOIP_COUNTRY_SHA256=c84a4f959317d005960a87bbdccaa32f47b4563ea3253e25705b37fad1f0946f

FROM alpine:3.24.3@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a AS geo
ARG GEOIP_DATE
ARG GEOIP_COMMIT
ARG GEOIP_COUNTRY_SHA256
ADD --checksum=sha256:${GEOIP_COUNTRY_SHA256} \
    https://raw.githubusercontent.com/GitSquared/node-geolite2-redist/${GEOIP_COMMIT}/redist/GeoLite2-Country.tar.gz /tmp/country.tar.gz
# The tarball's folder carries the database's date: it has to be the pinned one.
RUN d="/tmp/GeoLite2-Country_$(echo "$GEOIP_DATE" | tr -d -)" \
 && tar xzf /tmp/country.tar.gz -C /tmp \
 && { [ -d "$d" ] || { echo "The tarball isn't the database of $GEOIP_DATE: $(ls /tmp)" >&2; exit 1; }; } \
 && mkdir -p /geo && mv "$d/GeoLite2-Country.mmdb" "$d/LICENSE.txt" "$d/COPYRIGHT.txt" /geo/

FROM scratch
COPY --from=geo /geo/ /
