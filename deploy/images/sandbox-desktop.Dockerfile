# Pyo Sandbox's built-in template `desktop`: `base` with a desktop and
# Chromium. The release publishes it multi-arch as
# ghcr.io/upsetdev/pyo/sandbox-desktop:<version>, built on that release's
# sandbox-base (BASE), and release.json pins its digest
# (images.sandboxDesktop), which Pyo seeds the `desktop` template from
# (crates/pyo-api/src/sandbox/base.rs).
#
# One image, two uses:
# - Agents drive Chromium over CDP at 127.0.0.1:9222 in the sandbox.
# - People see the screen through noVNC at port 6080 (the dashboard's
#   Open desktop).
#
# Nothing runs until it's used. pyo-envd runs /etc/pyo/start at boot,
# which starts only a small listener on 6080 and 9222
# (sandbox-desktop/desktop.py). It serves noVNC's files and WebSocket on
# 6080 itself. The first use of either port starts the screen (TigerVNC's
# Xvnc) with Openbox and a tint2 panel, then Chromium. Chromium's CDP is
# on 9223 behind 9222, VNC on 5901, all on 127.0.0.1 only.
#
# Everything runs as root, like the rest of the sandbox (see
# sandbox-base.Dockerfile), so Chromium runs with --no-sandbox: the
# sandbox itself is the isolation. Templates don't use an image's Env or
# CMD, so nothing here relies on them.
#
# About 910 MB on top of base unpacked on amd64 (about 355 MB more to
# pull), most of it Chromium and the graphics libraries it needs.

ARG BASE=ghcr.io/upsetdev/pyo/sandbox-base:0.4.1
FROM ${BASE}
ARG NOVNC_VERSION=1.6.0
ARG NOVNC_SHA256=5066103959ef4e9b10f37e5a148627360dd8414e4cf8a7db92bdbd022e728aaa
LABEL org.opencontainers.image.source=https://github.com/upsetdev/pyo \
      org.opencontainers.image.description="Pyo Sandbox's desktop template: base with a desktop, noVNC and Chromium"

ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      chromium dbus-x11 fonts-dejavu-core fonts-droid-fallback \
      fonts-liberation fonts-noto-color-emoji openbox \
      tigervnc-standalone-server tint2 wmctrl \
      x11-xserver-utils xfce4-terminal; \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb \
      /usr/share/doc/* /usr/share/man/*; \
    chromium --version

# noVNC's web files, checked against their sha256. Debian's package would
# bring Node.js along.
RUN set -eux; \
    curl -fsSLo /tmp/novnc.tar.gz "https://github.com/novnc/noVNC/archive/refs/tags/v$NOVNC_VERSION.tar.gz"; \
    echo "$NOVNC_SHA256  /tmp/novnc.tar.gz" | sha256sum -c -; \
    mkdir -p /usr/share/novnc; \
    tar -xzf /tmp/novnc.tar.gz -C /usr/share/novnc --strip-components=1 --no-same-owner; \
    rm /tmp/novnc.tar.gz; \
    # Its own tooling and tests only. Not vendor/pako/lib/utils, which the
    # page imports.
    cd /usr/share/novnc; \
    rm -rf docs tests snap utils po .github; \
    # noVNC at / as well as /vnc.html.
    ln -sf vnc.html index.html

COPY sandbox-desktop/check-novnc.py /tmp/check-novnc.py
RUN python3 /tmp/check-novnc.py /usr/share/novnc && rm /tmp/check-novnc.py

COPY sandbox-desktop/start /etc/pyo/start
COPY sandbox-desktop/desktop.py /usr/local/lib/pyo-desktop/desktop.py
COPY sandbox-desktop/session /usr/local/lib/pyo-desktop/session
COPY sandbox-desktop/chromium-flags /etc/chromium.d/pyo
COPY sandbox-desktop/tint2rc /etc/xdg/tint2/tint2rc

RUN chmod 755 /etc/pyo/start /usr/local/lib/pyo-desktop/session \
    && python3 -m py_compile /usr/local/lib/pyo-desktop/desktop.py

WORKDIR /root
CMD ["bash"]
