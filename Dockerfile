FROM rust:1.90-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release --bin mcp-proxy

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/mcp-proxy /usr/local/bin/mcp-proxy

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start_period=10s \
    CMD curl -f http://localhost:8080/admin/backends || exit 1

ENTRYPOINT ["mcp-proxy"]
CMD ["--config", "/etc/mcp-proxy/gateway.toml"]
