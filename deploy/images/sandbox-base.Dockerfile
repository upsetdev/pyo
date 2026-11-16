# Pyo Sandbox's built-in template `base`: Debian 13 with the tools agents
# expect. The release publishes it multi-arch as
# ghcr.io/upsetdev/pyo/sandbox-base:<version>, and release.json pins its
# digest (images.sandboxBase), which Pyo seeds the `base` template from
# (crates/pyo-api/src/sandbox/base.rs).
#
# Templates don't use an image's User, Env or WorkingDir yet: pyo-envd
# runs every command as root in /root, with HOME=/root, LANG=C.UTF-8 and
# PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin. So
# everything here works as root with that PATH, and nothing relies on ENV.
#
# Versions are pinned. Bump them together, and Debian's point release
# with them. About 790 MB unpacked on amd64 (about 275 MB to pull), most of
# it the compilers, Node.js and pnpm.

ARG NODE_VERSION=26.13.0
ARG NODE_SHA256_AMD64=d28a96af6c38596f6d9d97c0ee531c13d238d757742387d1928de822d959e997
ARG NODE_SHA256_ARM64=406fe3fbde1c054adff8b297c16fdb5af6659db327c1ac8acd46b56ef1af9a92
ARG COREPACK_VERSION=0.36.0
ARG PNPM_VERSION=12.16.1

# uv and uvx, Astral's static binaries for each architecture.
FROM ghcr.io/astral-sh/uv:0.13.6@sha256:af22a52f5eafef3d78e551599d405c02df61919d18028f0cea8eda93bc9986d0 AS uv

FROM debian:13.7-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
ARG NODE_VERSION NODE_SHA256_AMD64 NODE_SHA256_ARM64 COREPACK_VERSION PNPM_VERSION
ARG TARGETARCH
LABEL org.opencontainers.image.source=https://github.com/upsetdev/pyo \
      org.opencontainers.image.description="Pyo Sandbox's base template: Debian 13 with git, Python, uv, Node.js and pnpm"

# C.UTF-8 is built into Debian's libc, so it needs no locales package.
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      bash-completion build-essential ca-certificates curl git iproute2 \
      iputils-ping jq less lsof netcat-openbsd openssh-client procps python3 \
      python3-pip python3-venv sudo tzdata unzip vim-tiny wget xz-utils zip; \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb; \
    # A sandbox is thrown away: let pip install for the system Python,
    # as agents expect.
    rm -f /usr/lib/python3.*/EXTERNALLY-MANAGED; \
    ln -s /usr/bin/python3 /usr/local/bin/python; \
    ln -s /usr/bin/vim.tiny /usr/local/bin/vim

# Node.js LTS, the official build, checked against its sha256.
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) arch=x64; sum=$NODE_SHA256_AMD64 ;; \
      arm64) arch=arm64; sum=$NODE_SHA256_ARM64 ;; \
      *) echo "No Node.js for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    f="node-v$NODE_VERSION-linux-$arch.tar.xz"; \
    curl -fsSLo "/tmp/$f" "https://nodejs.org/dist/v$NODE_VERSION/$f"; \
    echo "$sum  /tmp/$f" | sha256sum -c -; \
    tar -xJf "/tmp/$f" -C /usr/local --strip-components=1 --no-same-owner \
      --exclude='*/CHANGELOG.md' --exclude='*/README.md' --exclude='*/LICENSE' --exclude='*/doc' \
      --exclude='*/include/node/openssl/archs'; \
    rm "/tmp/$f"; \
    # pnpm through corepack, which Node no longer ships. pnpm is fetched
    # now, so it works in a sandbox without network too.
    npm install -g "corepack@$COREPACK_VERSION"; \
    corepack enable pnpm; \
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack install -g "pnpm@$PNPM_VERSION"; \
    npm cache clean --force; \
    rm -rf /tmp/* /root/.npm; \
    node --version; npm --version; pnpm --version

COPY --from=uv /uv /uvx /usr/local/bin/

RUN uv --version && python3 --version && git --version

WORKDIR /root
CMD ["bash"]
