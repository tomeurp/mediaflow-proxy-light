# ---------------------------------------------------------------------------
# Multi-stage Dockerfile — builds from source.
#
# Multi-arch (linux/amd64 + linux/arm64) is handled by Docker buildx.  Under
# QEMU the arm64 build is slower, but this lets the Docker job run in
# parallel with the desktop/mobile binary builds in CI without any
# cross-job artifact choreography.
#
# TLS backend: `tls-rustls` (the crate default) → no system OpenSSL needed.
# ---------------------------------------------------------------------------

FROM rust:1.95-slim-bookworm AS builder

WORKDIR /usr/src/app

# Minimal build deps — no OpenSSL (rustls is pure Rust).
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        build-essential \
        && rm -rf /var/lib/apt/lists/*

# Dependency-only prebuild for layer caching
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "pub fn add(l: usize, r: usize) -> usize { l + r }" > src/lib.rs && \
    cargo build --release && \
    rm -rf src target/release/deps/mediaflow*

# Real build
COPY src  ./src
COPY tools ./tools
RUN cargo build --release

# ---------------------------------------------------------------------------
# Runtime — distroless (glibc only, no shell/apt/etc.)
# ---------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12

WORKDIR /app

COPY --from=builder /usr/src/app/target/release/mediaflow-proxy-light /app/
COPY config-example.toml /app/config.toml

ENV RUST_LOG=info
ENV CONFIG_PATH=/app/config.toml

EXPOSE 8888

ENTRYPOINT ["/app/mediaflow-proxy-light"]
