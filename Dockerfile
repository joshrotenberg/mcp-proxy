FROM rust:1.90-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY benches/ benches/

RUN cargo build --release --locked --bin mcp-proxy

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Fixed non-root identity, matching the Helm chart's default security context.
# /var/lib/mcp-proxy is the home and working directory and the one writable
# application location (for example a SQLite cache in images built with the
# sqlite-cache feature); the rest of the filesystem can be mounted read-only.
RUN groupadd --system --gid 65532 mcp-proxy \
    && useradd --system --uid 65532 --gid 65532 --home-dir /var/lib/mcp-proxy \
       --no-create-home --shell /usr/sbin/nologin mcp-proxy \
    && install -d -o 65532 -g 65532 -m 0750 /var/lib/mcp-proxy

COPY --from=builder /build/target/release/mcp-proxy /usr/local/bin/mcp-proxy

USER 65532:65532
WORKDIR /var/lib/mcp-proxy

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD curl -f http://localhost:8080/livez || exit 1

ENTRYPOINT ["mcp-proxy"]
CMD ["--config", "/etc/mcp-proxy/proxy.toml"]
