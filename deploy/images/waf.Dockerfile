# The WAF's processor (waf/): Coraza and the OWASP Core Rule Set (compiled
# in) as an Envoy external processor. Static, non-root, nothing else in the
# image. Build context: the repository root.
#
# Both images are pinned by digest: the image is tagged with a hash of this
# file and the WAF's sources (scripts/image-tags.sh), and a release only
# builds it when that hash is new. So the WAF restarts on clusters only
# when a line here or its code changes, a new digest included.
# golang:1.26 is Go 1.26.11 at this digest.
FROM golang:1.26@sha256:6e59e17a20f13c102e49d82dffab66d41cffe6c266b225d82794328aea859ab5 AS build
WORKDIR /src
COPY waf/go.mod waf/go.sum ./
RUN go mod download
COPY waf/cmd ./cmd
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /pyo-waf ./cmd/pyo-waf

FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab
COPY --from=build /pyo-waf /pyo-waf
USER 65532:65532
EXPOSE 9002
ENTRYPOINT ["/pyo-waf"]
