#!/usr/bin/env bash
# Builds bin/pyo-waf with Go in docker (no local Go needed). GOCACHE_DIR: a
# folder for Go's caches (default ./.cache, ignored by git).
set -euo pipefail
cd "$(dirname "$0")"
C=${GOCACHE_DIR:-$PWD/.cache}; mkdir -p "$C"
docker run --rm -u "$(id -u):$(id -g)" -v "$PWD:/src" -v "$C:/cache" -w /src -e HOME=/tmp \
  -e GOCACHE=/cache/go -e GOMODCACHE=/cache/mod -e CGO_ENABLED=0 golang:1.26 \
  sh -c 'go vet ./... && go test ./... && go build -trimpath -ldflags="-s -w" -o bin/pyo-waf ./cmd/pyo-waf'
