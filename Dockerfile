# syntax=docker/dockerfile:1.7

# Multi-stage build for Timbang web binary.
# Build stage compiles a release binary; runtime stage carries only what the
# binary needs to serve. `prompts/`, `config.toml`, and `sesi/` are copied into
# the image; runtime overrides go through env vars and volume mounts.

FROM rust:1.90-bookworm AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY prompts ./prompts
COPY config.toml ./config.toml

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin web \
    && cp target/release/web /web

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=build /web /app/web
COPY prompts /app/prompts
COPY config.toml /app/config.toml

# Session files persist across restarts. Mount a Coolify volume at /app/sesi.
RUN mkdir -p /app/sesi
VOLUME /app/sesi

# Bind to all interfaces INSIDE the container. This is only safe because the
# container is on Coolify's private network; the public path is Traefik →
# Cloudflare Tunnel → Cloudflare Access (auth). See CLAUDE.md §6.
ENV TIMBANG_ALLOW_PUBLIC_BIND=1
ENV TIMBANG_BIND=0.0.0.0:7878

EXPOSE 7878

# ROUTER_API_KEY comes from Coolify's secret env, never baked into the image.
CMD ["/app/web"]
