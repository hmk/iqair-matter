# syntax=docker/dockerfile:1

# ── build: fully static musl binary ─────────────────────────────────────
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/iqair-matter /iqair-matter
RUN mkdir /data

# ── runtime: just the binary + CA certificates ──────────────────────────
FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /iqair-matter /iqair-matter
COPY --from=build --chown=65532:65532 /data /data
ENV DATA_DIR=/data \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
VOLUME /data
EXPOSE 8080/tcp 5540/udp 5353/udp
HEALTHCHECK --interval=60s --timeout=10s --start-period=30s CMD ["/iqair-matter", "healthcheck"]
ENTRYPOINT ["/iqair-matter"]
