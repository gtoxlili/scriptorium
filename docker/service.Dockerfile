# syntax=docker/dockerfile:1.7
#
# Scriptorium gRPC service runtime image.
#
# This is the *service* container (the gRPC orchestrator). It talks to the
# host Docker daemon via a bind-mounted socket to spawn sandbox containers
# as its siblings (Docker-out-of-Docker). The actual sandbox runtime is a
# different image — see docker/sandbox.Dockerfile.
#
# Build from the repo root:
#   docker build -f docker/service.Dockerfile -t scriptorium:latest .
#
# Or via the compose file:
#   docker compose up -d --build

# ─── Builder ───────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder
WORKDIR /src

# aws-lc-rs (used via rustls under aws-sdk-s3) compiles C; tonic-build
# needs protoc. build-essential + cmake cover the native toolchain,
# pkg-config satisfies a few crates that probe the host.
RUN apt-get update && apt-get install -y --no-install-recommends \
      protobuf-compiler \
      build-essential \
      cmake \
      pkg-config \
 && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY proto ./proto
COPY src ./src

# Tune for whatever CPU this build host exposes. On OrbStack with a Mac
# M-series host the builder sees the Apple CPU features, so the resulting
# Linux binary picks them up automatically. If you rebuild on a different
# host (Graviton, Ampere, etc.), the image gets tuned for that host's CPU.
ENV RUSTFLAGS="-C target-cpu=native"
RUN cargo build --release --locked --bin scriptorium

# ─── Runtime ───────────────────────────────────────────────────────────
FROM debian:12-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      tini \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/scriptorium /usr/local/bin/scriptorium

EXPOSE 50051

# tini as PID 1 so SIGTERM from `docker stop` reaches scriptorium
# correctly and tokio's graceful-shutdown hook actually runs.
ENTRYPOINT ["/usr/bin/tini", "--", "scriptorium"]
CMD ["--config", "/etc/scriptorium/config.toml"]
