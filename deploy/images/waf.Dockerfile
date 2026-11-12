# The WAF's processor (waf/): Coraza and the OWASP Core Rule Set (compiled
# in) as an Envoy external processor. Static, non-root, nothing else in the
# image. Build context: the repository root.
FROM golang:1.26 AS build
WORKDIR /src
COPY waf/go.mod waf/go.sum ./
RUN go mod download
COPY waf/cmd ./cmd
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /pyo-waf ./cmd/pyo-waf

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /pyo-waf /pyo-waf
USER 65532:65532
EXPOSE 9002
ENTRYPOINT ["/pyo-waf"]
