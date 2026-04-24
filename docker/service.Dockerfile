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
# rust:slim = latest stable Rust on the current Debian slim base. Unpinned
# so the image tracks upstream toolchain updates automatically — cargo.lock
# and rust-toolchain.toml still pin the actual compiler used for the build.
FROM rust:slim AS builder
WORKDIR /src

# ─── Build-time mirrors ──────────────────────────────────────────────────
# Pin Debian apt to TUNA and crates.io to USTC. apt metadata is GPG-signed
# and cargo validates every crate by checksum, so serving both over plain
# HTTP is safe and sidesteps any TLS-termination quirks from the local
# network path.
RUN find /etc/apt -maxdepth 2 -type f \
        \( -name '*.list' -o -name '*.sources' \) -exec sed -i \
        -e 's|http://deb.debian.org/debian|http://mirrors.tuna.tsinghua.edu.cn/debian|g' \
        -e 's|https://deb.debian.org/debian|http://mirrors.tuna.tsinghua.edu.cn/debian|g' \
        -e 's|http://security.debian.org/debian-security|http://mirrors.tuna.tsinghua.edu.cn/debian-security|g' \
        -e 's|https://security.debian.org/debian-security|http://mirrors.tuna.tsinghua.edu.cn/debian-security|g' \
        {} + \
 && printf '%s\n' \
      '[source.crates-io]' \
      'replace-with = "ustc"' \
      '' \
      '[source.ustc]' \
      'registry = "sparse+http://mirrors.ustc.edu.cn/crates.io-index/"' \
      '' \
      '[net]' \
      'retry = 10' \
      > "${CARGO_HOME:-/usr/local/cargo}/config.toml"

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
# debian:stable-slim follows whichever Debian release is currently stable
# (bookworm → trixie → …). apt-source rewriting below handles both the
# legacy sources.list format and the deb822 debian.sources format.
FROM debian:stable-slim

# Same Debian apt mirror as the builder stage.
RUN find /etc/apt -maxdepth 2 -type f \
        \( -name '*.list' -o -name '*.sources' \) -exec sed -i \
        -e 's|http://deb.debian.org/debian|http://mirrors.tuna.tsinghua.edu.cn/debian|g' \
        -e 's|https://deb.debian.org/debian|http://mirrors.tuna.tsinghua.edu.cn/debian|g' \
        -e 's|http://security.debian.org/debian-security|http://mirrors.tuna.tsinghua.edu.cn/debian-security|g' \
        -e 's|https://security.debian.org/debian-security|http://mirrors.tuna.tsinghua.edu.cn/debian-security|g' \
        {} +

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
