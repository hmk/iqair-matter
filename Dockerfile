# syntax=docker/dockerfile:1

# ── toolchain + cargo-chef ──────────────────────────────────────────────
FROM rust:1-alpine AS chef
RUN apk add --no-cache musl-dev gcc \
 && cargo install cargo-chef --locked --version ^0.1
WORKDIR /src

# ── recipe: the dependency graph, without our code ──────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── build: dependencies in their own layer (cached until Cargo.lock
#    changes), then our crate. Fully static musl binary. ─────────────────
FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
RUN cargo build --release --locked \
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
