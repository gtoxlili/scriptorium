# scriptorium

[English](./README.md) · [简体中文](./README.zh-CN.md)

[![CI](https://github.com/gtoxlili/scriptorium/actions/workflows/ci.yml/badge.svg)](https://github.com/gtoxlili/scriptorium/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![Transport](https://img.shields.io/badge/Transport-gRPC-lightgrey.svg)](proto/sandbox.proto)

Sandbox execution middleware for LLM-driven workloads. Scriptorium
spawns isolated containers on demand so a calling agent can run
arbitrary scripts, RPA flows, or browser automation without pulling
that infrastructure into its own process.

It runs on top of any OCI-compatible Docker daemon (OrbStack on macOS,
Docker Desktop, Colima, or dockerd on Linux) and owns the orchestration
layer: container lifecycle, workspace bind-mounts, resource caps,
concurrency throttling, URL ingress, artifact delivery.

## Architecture

```
                                         ┌──── URLs ────┐
                                         │              │
 caller (agent)              scriptorium            Docker          OSS (S3-compat)
───────────────            ─────────────────     ──────────       ─────────────────
      │                            │                 │                   │
      │  Exec(workspace_id, cmd)   │  bollard: run image,               │
      │                        ──▶ │  bind-mount $HOME,                  │
      │                            │  drop to uid 1000,  ──▶ ┌─────────┐ │
      │                            │  cpu/mem/pids caps,     │container│ │
      │                            │  wall-clock timeout.    │bash -lc │ │
      │ ◀── stdout/stderr/exit ─── │                         └─────────┘ │
      │                            │                                     │
      │  FetchIntoWorkspace(url)──▶│  host-side reqwest,                 │
      │                            │  SSRF-guarded, streamed to disk.    │
      │                            │                                     │
      │  UploadToOSS(path) ──────▶ │  tar.gz if dir, aws-sdk-s3 put      │
      │ ◀── object_key + metadata ─│  (multipart above threshold). ────▶│
      │                            │                                     │
      │  Import/Export             │                                     │
      │   WorkspaceObject     ──▶  │  streaming byte transfer for       │
      │ ◀── chunks + metadata ──── │  trusted host-bridge handoff.       │
      │                            │                                     │
      │  (caller resolves object_key to a permanent URL through its own │
      │   attachment system. scriptorium does not presign.)
```

Compute is per-call ephemeral. State is per-workspace persistent:
installed pip packages, Chromium profile, and produced artifacts survive
across calls under a host directory keyed by `workspace_id`. The id is
opaque to the service; the caller picks its own granularity (session
id, task id, whatever).

Byte-heavy transfers stay out of gRPC. Inputs arrive via HTTP URLs
(`FetchIntoWorkspace`). Outputs land in object storage and come back
as a permanent `object_key` that the caller resolves to a user-facing
URL via its own attachment system. Scriptorium does not return
TTL-limited presigned URLs because a URL the end user might bookmark
and revisit a week later would 403. The caller owns a stable URL
shape that transparently re-signs on every access.

For the full design rationale, see
[`docs/architecture.md`](docs/architecture.md).

## Features

- **gRPC API** with streaming exec. `proto/sandbox.proto` is the
  cross-language contract.
- **Per-call container spawn** against a local Docker daemon. No idle
  footprint between calls.
- **Per-workspace persistent state** bind-mounted at a fixed
  in-container path.
- **Non-root sandbox user** (UID 1000), read-only rootfs, size-capped
  `/tmp` tmpfs.
- **Per-exec resource caps**: CPU millis, memory bytes, PID limit,
  wall-clock. Concurrency gated by a configurable semaphore (default
  4); queued requests return `RESOURCE_EXHAUSTED` once
  `exec_queue_timeout_seconds` elapses.
- **Client-drop cancellation**: if `ExecStream`'s caller disconnects
  mid-flight, the in-flight container is SIGKILL-ed and removed within
  milliseconds instead of burning CPU to its wall timeout.
- **URL-in / object-out file handling**. `FetchIntoWorkspace` has an
  SSRF guard that rejects loopback, RFC1918, link-local, broadcast,
  documentation, CGNAT, and the IPv6 equivalents. `UploadToOSS`
  targets any S3-compatible store (defaults aligned with Volcano
  Engine TOS); files above `multipart_threshold_bytes` (default 64
  MiB) stream through multipart upload so peak RAM stays bounded by
  `part_size_bytes`.
- **LLM-facing tool layer**: `ListTools` publishes four OpenAI-style
  descriptors (`execute_shell`, `deliver`, plus two workspace-sandbox
  exchange tools intended for a host bridge layered above). `CallTool`
  routes `execute_shell` / `deliver` through the same code path as the
  primitive RPCs.
- **Fat sandbox image** (Debian 13) with Python 3.13 + `uv`, Node 24
  LTS, Chromium via Playwright, FFmpeg, ImageMagick, and common CLIs
  pre-installed. No runtime `apt install`.
- **Workspace id validation** (`[A-Za-z0-9_-]{1,128}`) and host-side
  path traversal guards.
- **`tini` PID 1** inside the sandbox so Chromium's helper processes
  get reaped.

## Non-goals

- Multi-host orchestration. Scriptorium is a single-node daemon. Run
  multiple instances behind a router if one box is not enough.
- Credential storage. Callers inject secrets per-exec via `env`, or
  by `FetchIntoWorkspace`-ing a short-lived credential file before
  the exec.
- Image building. Build the sandbox image out of band and reference
  it by tag.

## Repository layout

```
proto/sandbox.proto         gRPC service definition (cross-language contract)
src/
  main.rs                   Binary entry, CLI, graceful shutdown
  lib.rs                    Module tree
  config.rs                 TOML config loader + validation
  error.rs                  Error types + tonic::Status mapping
  runtime.rs                Docker-backed container runtime (bollard)
  service.rs                gRPC service impl + concurrency semaphore
  workspace.rs              Per-workspace state directory manager
  fetch.rs                  URL-to-workspace download + SSRF guard
  oss.rs                    S3-compatible upload (multipart-aware)
  tools.rs                  LLM-facing tool descriptors
docker/
  service.Dockerfile        Multi-stage Rust build of the service
  sandbox.Dockerfile        The fat sandbox image
docker-compose.yml          Reference deployment
deploy/config.example.toml  Example service configuration
docs/architecture.md        Design decisions and boundaries
.github/workflows/ci.yml    CI: fmt + clippy + check
```

## Requirements

- Rust 1.85+ (2024 edition). `rust-toolchain.toml` pins the toolchain.
- `protoc` on PATH. macOS: `brew install protobuf`. Debian/Ubuntu:
  `apt-get install -y protobuf-compiler`.
- A Docker-compatible daemon reachable via Unix socket.
- Credentials for an S3-compatible object store (Volcano Engine TOS,
  AWS S3, Tencent COS, MinIO, Cloudflare R2, …) configured under
  `[tos]`.

## Deployment

On macOS, I run scriptorium in Docker. The service inherits OrbStack's
already-granted TCC access to external volumes, which avoids the
"scriptorium wants to access Removable Volumes" prompt on every start.
On Linux, a native binary under systemd is usually simpler.

### Docker (recommended on macOS)

```bash
# 1. Prepare config.
cp deploy/config.example.toml deploy/config.toml
# Edit deploy/config.toml:
#   - [tos].access_key / .secret_key
#   - [workspace].root  — absolute host path, e.g. /Volumes/SSD/scriptorium-state
# Leave [docker].socket = "/var/run/docker.sock"; the run below mounts
# OrbStack's real socket at that path.
chmod 600 deploy/config.toml

# 2. Build the service image (first time ~1-2 min; incremental ~seconds).
docker build -f docker/service.Dockerfile -t scriptorium:latest .

# 3. Run. WS_ROOT must match [workspace].root exactly: the bind-mount
# maps the same absolute path on both sides so sandbox containers'
# bind paths resolve against the host daemon.
WS_ROOT=/Volumes/SSD/scriptorium-state
docker run -d \
  --name scriptorium \
  --restart=unless-stopped \
  -p 127.0.0.1:50051:50051 \
  -v "$HOME/.orbstack/run/docker.sock:/var/run/docker.sock" \
  -v "$WS_ROOT:$WS_ROOT" \
  -v "$(pwd)/deploy/config.toml:/etc/scriptorium/config.toml:ro" \
  -e RUST_LOG="info,bollard=warn" \
  scriptorium:latest
```

Day-to-day:

```bash
docker logs -f scriptorium
docker restart scriptorium        # after editing config.toml
docker rm -f scriptorium          # stop + remove

# After Rust changes, rebuild and re-run:
docker build -f docker/service.Dockerfile -t scriptorium:latest .
docker rm -f scriptorium
docker run -d … scriptorium:latest   # same flags as above
```

### Docker Compose (alternative)

```bash
cp .env.example .env
# edit SCRIPTORIUM_WORKSPACE_ROOT in .env
docker compose up -d --build
docker compose logs -f
docker compose down
```

### Native binary

```bash
cargo build --release
cp deploy/config.example.toml deploy/config.toml
# Edit deploy/config.toml — set docker.socket to your daemon path,
# workspace.root to a host path, and [tos] credentials.
./target/release/scriptorium --config deploy/config.toml
```

launchd-managed native processes on macOS can trigger TCC prompts for
external volumes; the Docker path sidesteps that.

## Build the sandbox image

```bash
docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian13-v1 .
```

Image size is around 3 GB. That is the cost of skipping runtime
`apt install`s. Pin the tag in `deploy/config.toml` and upgrade the
sandbox image on its own cadence, separate from service upgrades.

## Consuming from another language

`proto/sandbox.proto` is the cross-language contract. Generate stubs
in your language of choice and point the client at `grpc://<host>:<port>`.

For Go:

```bash
protoc --go_out=. --go-grpc_out=. proto/sandbox.proto
```

## Integration tests

`tests/e2e.rs` exercises every RPC against a real Docker daemon:

- `Health`: reachability and permit count
- `Exec`: stdout/stderr capture, non-zero exit propagation, wall-clock
  timeout, workspace state persistence across calls
- `ExecStream`: `Started` / chunk / `Finished` ordering
- `FetchIntoWorkspace`: SSRF guard rejects loopback targets
- `ListFiles`: recursive walk reflects exec-produced contents
- `DeleteWorkspace`: host directory removal + idempotent repeat
- `ListTools` / `CallTool`: descriptor count, schema validity,
  `execute_shell` routing, unknown-tool error shape
- Invalid `workspace_id`: `InvalidArgument`

These tests are `#[ignore]`-gated so CI (which has no Docker) stays
green. To run locally after building the sandbox image:

```bash
cargo test --test e2e -- --ignored --nocapture
```

Override the image tag via `SCRIPTORIUM_TEST_IMAGE=…` or the docker
socket via `DOCKER_HOST=unix:///path/to/docker.sock` if your setup
differs. To also exercise `UploadToOSS` against a real bucket, set
`SCRIPTORIUM_TEST_TOS_ENDPOINT` / `_REGION` / `_BUCKET` /
`_ACCESS_KEY` / `_SECRET_KEY`.

## Status

Every RPC on the current `proto/sandbox.proto` — `Exec`, `ExecStream`,
`FetchIntoWorkspace`, `UploadToOSS`, `ListFiles`, `DeleteWorkspace`,
`ImportWorkspaceObject`, `ExportWorkspaceObject`, `ListTools`,
`CallTool`, `Health` — is implemented and covered by the e2e suite
against OrbStack. Outstanding items, documented in
[`docs/architecture.md`](docs/architecture.md): optional warm-pool
for spawn latency, scheduled workspace GC, and mitmproxy-fronted
egress audit. Issues and PRs welcome.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE).
