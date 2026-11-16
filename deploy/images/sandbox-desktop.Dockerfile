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
# (sandbox-desktop/desktop.py). The first connection to either starts the
# screen (TigerVNC's Xvnc), Openbox with a tint2 panel, and Chromium, then
# passes the connection on. Chromium's CDP is on 9223 behind 9222, noVNC's
# websockify on 6081 behind 6080, VNC on 5901, all on 127.0.0.1 only.
#
# Everything runs as root, like the rest of the sandbox (see
# sandbox-base.Dockerfile), so Chromium runs with --no-sandbox: the
# sandbox itself is the isolation. Templates don't use an image's Env or
# CMD, so nothing here relies on them.
#
# About 640 MB on top of base unpacked on amd64 (about 220 MB to pull),
# most of it Chromium and its libraries.

ARG BASE=ghcr.io/upsetdev/pyo/sandbox-base:0.4.1
FROM ${BASE}
LABEL org.opencontainers.image.source=https://github.com/upsetdev/pyo \
      org.opencontainers.image.description="Pyo Sandbox's desktop template: base with a desktop, noVNC and Chromium"

ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      chromium dbus-x11 fonts-dejavu-core fonts-droid-fallback \
      fonts-liberation fonts-noto-color-emoji novnc openbox \
      python3-websockify tigervnc-standalone-server tint2 \
      x11-xserver-utils xfce4-terminal; \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb \
      /usr/share/doc/* /usr/share/man/*; \
    # noVNC at / as well as /vnc.html.
    ln -sf vnc.html /usr/share/novnc/index.html; \
    chromium --version

COPY sandbox-desktop/start /etc/pyo/start
COPY sandbox-desktop/desktop.py /usr/local/lib/pyo-desktop/desktop.py
COPY sandbox-desktop/session /usr/local/lib/pyo-desktop/session
COPY sandbox-desktop/chromium-flags /etc/chromium.d/pyo
COPY sandbox-desktop/tint2rc /etc/xdg/tint2/tint2rc

RUN chmod 755 /etc/pyo/start /usr/local/lib/pyo-desktop/session \
    && python3 -m py_compile /usr/local/lib/pyo-desktop/desktop.py

WORKDIR /root
CMD ["bash"]
