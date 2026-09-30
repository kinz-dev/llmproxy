# syntax=docker/dockerfile:1

# Build stage: `rust:1-slim-bookworm` tracks the latest stable (edition 2024 needs
# rustc >= 1.85) and shares Debian bookworm's glibc ABI with the runtime stage.
FROM rust:1-slim-bookworm AS build
WORKDIR /app

# ring (rustls) and mimalloc build bundled C via `cc`; the rust-slim image
# already ships a C toolchain, ca-certificates/pkg-config keep it self-contained.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Compile the real sources in one clean step. A warm dependency stub layer is
# deliberately avoided: Docker preserves source mtimes, which lets cargo reuse a
# stub fingerprint and ship the wrong binary. A fresh target with only the real
# crate present has no cached fingerprint, so it always compiles the actual code.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release \
    && cp target/release/llmproxy /usr/local/bin/llmproxy

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10002 --shell /usr/sbin/nologin llmproxy

COPY --from=build /usr/local/bin/llmproxy /usr/local/bin/llmproxy
COPY ui /home/llmproxy/ui

# Proxy listens on 0.0.0.0 via `--listen`; the loopback admin (default 11180)
# comes from config.toml. Trace logs land under $HOME/.llmproxy, mounted rw.
ENV HOME=/home/llmproxy \
    LLMPROXY_UI_DIR=/home/llmproxy/ui \
    PROXY_PORT=8080 \
    RUST_LOG=info

EXPOSE 8080 11180

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PROXY_PORT:-8080}/health" || exit 1

USER llmproxy
WORKDIR /home/llmproxy

ENTRYPOINT ["/usr/local/bin/llmproxy", "--foreground"]
CMD []
