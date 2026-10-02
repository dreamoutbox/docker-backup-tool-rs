# syntax=docker/dockerfile:1.7

# ---- Stage 1: chef base (toolchain + cargo-chef preinstalled) ----
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

# ---- Stage 2: planner (dependency recipe; changes only when Cargo.toml/lock change) ----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Stage 3: builder ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cached layer: compiles dependencies only
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --recipe-path recipe.json
# Only this layer rebuilds on source changes
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked --bin dvb

# ---- Stage 4: runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates openssh-client tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/dvb /etc/dvb
COPY --from=builder /app/target/release/dvb /usr/local/bin/dvb
LABEL org.opencontainers.image.title="dvb" \
    org.opencontainers.image.description="Docker volume backup tool: tar+compress mounted paths and stream them to object storage" \
    org.opencontainers.image.licenses="MIT"
ENV TZ=UTC
# Runs as root on purpose: it must be able to read arbitrary mounted volumes and
# talk to the Docker socket. Override with `user:` for read-only volume setups
# that do not need container control (drop stop_containers/stop_label then).
ENTRYPOINT ["dvb"]
CMD ["run", "--config", "/etc/dvb/config.toml"]
