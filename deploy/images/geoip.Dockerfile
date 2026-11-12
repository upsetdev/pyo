# The GeoIP country database the traffic proxy's firewall places visitors
# with (GeoLite2 Country, the same source as analytics' bundled GeoLite2
# City). Just the file: the proxies mount this image read-only as a volume
# (PYO_GEOIP_IMAGE; pyo-proto's system.rs), so they need no credentials or
# downloads for it. Built with each release, and by scripts/pyo-up.sh.
FROM alpine:3.24.3 AS geo
RUN wget -qO- https://raw.githubusercontent.com/GitSquared/node-geolite2-redist/master/redist/GeoLite2-Country.tar.gz | tar xz -C /tmp \
 && mkdir -p /geo && mv /tmp/GeoLite2-Country_*/GeoLite2-Country.mmdb /tmp/GeoLite2-Country_*/LICENSE.txt /tmp/GeoLite2-Country_*/COPYRIGHT.txt /geo/

FROM scratch
COPY --from=geo /geo/ /
