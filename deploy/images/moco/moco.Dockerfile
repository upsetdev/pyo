# syntax=docker/dockerfile:1.7
# Pyo's MySQL operator and its backup image: MOCO (cybozu-go/moco,
# Apache-2.0) at the tag below, with the patches in patches/ (see README.md).
# Two targets, built from deploy/images/moco as the context:
#
#   operator -> pyo/mysql-operator (moco-controller)
#   backup   -> pyo/mysql-backup   (moco-backup: backup and restore Jobs)
#
# The stages follow upstream's own Dockerfile at that tag; only the source
# (cloned and patched here, not copied) differs. moco-agent, the sidecar in
# each MySQL pod, isn't patched and stays upstream's image.

ARG MOCO_TAG=v0.38.0
ARG MOCO_COMMIT=acb2716d1ef13a8953dd18d55cf230c0edf90f04
ARG GOLANG=ghcr.io/cybozu/golang:1.27-noble@sha256:d78937fddb26c4350824d4022f5c9dfe20bf762811ea31065f8c6d467c58bc30
ARG UBUNTU=ghcr.io/cybozu/ubuntu:24.04@sha256:add3f3caca7a2a11a0e8f4e14aaee896f59b5a12917af34ab798315188f67c8e
# MySQL's own binaries for the backup image (mysqlbinlog, mysql), as upstream.
ARG MYSQL=ghcr.io/cybozu-go/moco/mysql:8.4.10.1@sha256:51dffb347697795e1680b9930fea1e1055a71387a0e769cc61ab4efc539c149b

FROM --platform=$BUILDPLATFORM ${GOLANG} AS builder
ARG MOCO_TAG
ARG MOCO_COMMIT
ARG TARGETARCH
WORKDIR /work
RUN git clone --depth 1 --branch "$MOCO_TAG" https://github.com/cybozu-go/moco.git . \
  && test "$(git rev-parse HEAD)" = "$MOCO_COMMIT" \
  || { echo "moco $MOCO_TAG isn't commit $MOCO_COMMIT" >&2; exit 1; }
COPY patches/ /patches/
RUN git apply --verbose /patches/*.patch
RUN --mount=type=cache,target=/root/go/pkg/mod \
    --mount=type=cache,target=/root/.cache/go-build \
    GOARCH=${TARGETARCH} CGO_ENABLED=0 go build -ldflags="-w -s" -o moco-controller ./cmd/moco-controller \
  && GOARCH=${TARGETARCH} go build -ldflags="-w -s" -o moco-backup ./cmd/moco-backup

FROM --platform=$TARGETPLATFORM scratch AS operator
LABEL org.opencontainers.image.source=https://github.com/cybozu-go/moco
COPY --from=builder /work/moco-controller ./
USER 10000:10000
ENTRYPOINT ["/moco-controller"]

FROM --platform=$TARGETPLATFORM ${MYSQL} AS mysql

FROM --platform=$TARGETPLATFORM ${UBUNTU} AS backup
LABEL org.opencontainers.image.source=https://github.com/cybozu-go/moco

# https://bugs.mysql.com/bug.php?id=121027
ARG MYSQLSH_VERSION=8.4.8
ARG MYSQLSH_GLIBC_VERSION=2.28
ARG TARGETARCH

COPY --from=builder /work/moco-backup /moco-backup

COPY --from=mysql /usr/local/mysql/LICENSE         /usr/local/mysql/LICENSE
COPY --from=mysql /usr/local/mysql/bin/mysqlbinlog /usr/local/mysql/bin/mysqlbinlog
COPY --from=mysql /usr/local/mysql/bin/mysql       /usr/local/mysql/bin/mysql

RUN apt-get update \
  && apt-get install -y --no-install-recommends zstd python3 libpython3.10 s3cmd libgoogle-perftools4 \
  && rm -rf /var/lib/apt/lists/* \
  && if [ "${TARGETARCH}" = 'amd64' ]; then MYSQLSH_ARCH='x86-64'; fi \
  && if [ "${TARGETARCH}" = 'arm64' ]; then MYSQLSH_ARCH='arm-64'; fi \
  && MYSQLSH_TARBALL="mysql-shell-${MYSQLSH_VERSION}-linux-glibc${MYSQLSH_GLIBC_VERSION}-${MYSQLSH_ARCH:-unknown}bit.tar.gz" \
  # MySQL removes a version from the download site once a newer one is released,
  # so fall back to the archive site.
  && (curl -o /tmp/mysqlsh.tar.gz -fsSL --retry 2 "https://cdn.mysql.com/Downloads/MySQL-Shell/${MYSQLSH_TARBALL}" \
      || curl -o /tmp/mysqlsh.tar.gz -fsSL --retry 2 "https://cdn.mysql.com/archives/mysql-shell/${MYSQLSH_TARBALL}") \
  && mkdir /usr/local/mysql-shell \
  && tar -xf /tmp/mysqlsh.tar.gz -C /usr/local/mysql-shell --strip-components=1 \
  && rm -f /tmp/mysqlsh.tar.gz

ENV PATH=/usr/local/mysql/bin:/usr/local/mysql-shell/bin:"$PATH"
USER 10000:10000
ENTRYPOINT ["/moco-backup"]
