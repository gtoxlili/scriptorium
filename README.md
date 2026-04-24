# scriptorium

[![CI](https://github.com/gtoxlili/scriptorium/actions/workflows/ci.yml/badge.svg)](https://github.com/gtoxlili/scriptorium/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![Transport](https://img.shields.io/badge/Transport-gRPC-lightgrey.svg)](proto/sandbox.proto)

Sandbox execution middleware for LLM-driven workloads. Scriptorium spawns
isolated containers on demand so an upstream agent can run arbitrary scripts,
RPA flows, or browser automation without baking that infrastructure into the
agent's own runtime.

It is a thin, opinionated layer over any OCI-compatible Docker daemon —
OrbStack on macOS, Docker Desktop, Colima, or plain `dockerd` on Linux.
Scriptorium itself does not implement sandboxing; it **orchestrates** it,
the same way an application server orchestrates request handlers.

---

## Design in one picture

```
                                           ┌──────────────── URLs ────────────────┐
                                           │                                      │
 caller (agent)                     scriptorium                    Docker          OSS (S3-compat)
────────────────                  ─────────────────              ──────────       ─────────────────
      │                                   │                          │                   │
      │  Exec(workspace_id, cmd)     ───▶ │  bollard: run image,     │                   │
      │                                   │  bind-mount $HOME,       │                   │
      │                                   │  drop to uid 1000,    ──▶│  ┌──────────────┐ │
      │                                   │  cpu/mem/pids caps,      │  │  container   │ │
      │                                   │  wall-clock timeout.     │  │  (bash -lc)  │ │
      │  ◀── stdout / stderr / exit ──    │                          │  └──────────────┘ │
      │                                   │                          │                   │
      │  FetchIntoWorkspace(url, path) ──▶│  host-side reqwest,      │                   │
      │                                   │  SSRF-guarded,        ──▶│  writes to $HOME  │
      │                                   │  streamed to disk.       │                   │
      │                                   │                                              │
      │  UploadToOSS(path, compress?) ───▶│  tar.gz if dir,  ──────────────────────────▶ │
      │  ◀──── object_key + metadata ──   │  aws-sdk-s3 PUT.                             │
      │                                   │                                              │
      │  Import/ExportWorkspaceObject ───▶│  direct byte streaming for trusted           │
      │  ◀────── chunks + metadata ───    │  host-bridge handoff flows.                  │
      │                                   │                                              │
      │  (caller resolves object_key to a permanent URL via its own                      │
      │   attachment system — scriptorium does not presign.)                             │

  ListTools / CallTool: LLM-shaped convenience wrappers over the core
  primitives above — same semantics, OpenAI function-call descriptors.
```

Compute lifetime is **per-call ephemeral**. State lifetime is **per-workspace
persistent** (installed pip packages, Chromium profile, produced artifacts).
`workspace_id` is opaque to the service; the caller picks its granularity —
session, task, whatever.

Data never flows through gRPC as raw bytes: inputs come in via HTTP URLs
(`FetchIntoWorkspace`), outputs land in object storage and come back as
a **permanent `object_key`** — the caller resolves that key to a
user-facing URL through its own attachment system. Scriptorium does not
return TTL-limited presigned URLs because artifacts the user wants to
revisit days later would 403; the caller (agent-core) issues stable
`/api/v1/public/attachments/{id}` handles that re-sign on each access.

Full design rationale: [`docs/architecture.md`](docs/architecture.md).

## Features

- **gRPC API** with streaming exec (`proto/sandbox.proto` is the source of truth).
- **Per-call container spawn** via a local Docker daemon — zero idle footprint.
- **Per-workspace persistent state** mounted at a fixed in-container path.
- **Fixed non-root user** (UID 1000) with read-only rootfs and sized tmpfs.
- **Hard resource caps** per exec: CPU millis, memory bytes, PID limit, wall
  clock. Admission is throttled by a configurable concurrency semaphore
  (default 4); queued requests return `RESOURCE_EXHAUSTED` after the timeout.
- **URL-in / object-key-out file handling** — `FetchIntoWorkspace` with
  SSRF defence against loopback / RFC1918 / link-local / ULA, and
  `UploadToOSS` uploading to any S3-compatible object store (defaults
  aligned with Volcano Engine TOS) and returning the permanent
  `object_key`. Callers mint user-facing URLs from that key through
  their own attachment system.
- **AI-facing tool layer** — `ListTools` publishes four OpenAI-compatible
  descriptors (`execute_shell`, `deliver`, and two workspace-sandbox
  exchange tools). `CallTool` routes `execute_shell` / `deliver` through
  the same implementation path as the primitives; the exchange tools are
  intended for a host bridge layered above scriptorium.
- **Fat sandbox image** (Debian 13) with Python 3.13 + `uv`, Node 24 LTS,
  Chromium/Playwright, FFmpeg, ImageMagick, and common CLIs pre-installed —
  no runtime `apt install` required.
- **Workspace id validation** (`[A-Za-z0-9_-]{1,128}`) with host-side path
  traversal guards.
- **tini init** inside the sandbox so Chromium child processes are reaped.

## Non-goals

- Multi-host orchestration. Scriptorium is a single-node daemon; cluster it
  by placing instances behind a router.
- Credential storage. Scriptorium does not remember secrets — callers inject
  them per-exec via `env` or by `FetchIntoWorkspace`-ing a short-lived
  credential file before the exec.
- Image building. Build the sandbox image out of band and reference it by tag.

## Repository layout

```
Cargo.toml                  Rust project manifest
rust-toolchain.toml         Pinned toolchain (stable)
build.rs                    tonic-build plumbing
proto/sandbox.proto         gRPC service definition — source of truth
src/
  main.rs                   Binary entry, CLI, graceful shutdown
  lib.rs                    Module tree
  config.rs                 TOML config loader + validation
  error.rs                  Error types + tonic::Status mapping
  runtime.rs                Docker-backed container runtime (bollard)
  service.rs                gRPC service impl + concurrency semaphore
  workspace.rs              Per-workspace state directory manager
  fetch.rs                  URL-to-workspace download + SSRF guard
  oss.rs                    S3-compatible upload + signed-URL presign
  tools.rs                  AI-facing tool descriptors + arg/result types
docker/service.Dockerfile   Service runtime image (multi-stage Rust build)
docker-compose.yml          Reference Docker deployment
.env.example                Compose env vars template
docker/sandbox.Dockerfile   The fat sandbox image
deploy/config.example.toml  Example service configuration
docs/architecture.md        Design decisions and boundaries
.github/workflows/ci.yml    CI: fmt + clippy + check
```

## Requirements

- Rust 1.85+ (2024 edition). The toolchain file pins `stable`.
- `protoc` on PATH. On macOS: `brew install protobuf`. On Debian/Ubuntu:
  `apt-get install -y protobuf-compiler`.
- A Docker-compatible daemon reachable via Unix socket — OrbStack, Docker
  Desktop, Colima, or `dockerd` on Linux.
- Credentials for an S3-compatible object store (Volcano Engine TOS, AWS
  S3, Tencent COS, MinIO, Cloudflare R2, ...) configured under `[tos]` —
  `UploadToOSS` signed URLs are how artifacts get delivered to end users.

## Deployment

On macOS, **running in Docker is preferred** — the service inherits
OrbStack's already-granted access to external volumes, so no TCC prompt
each time it reads from a mounted SSD. On Linux, either shape works;
native binary + systemd is usually simpler.

### Docker (recommended on macOS)

```bash
# 1. Prepare config (TOS creds + workspace root)
cp deploy/config.example.toml deploy/config.toml
# Edit deploy/config.toml — fill [tos].access_key / .secret_key and set
# [workspace].root to the ABSOLUTE host path (e.g. /Volumes/SSD/scriptorium-state).
# Leave [docker].socket = "/var/run/docker.sock" — the run command below
# mounts OrbStack's real socket at that path.
chmod 600 deploy/config.toml      # TOS creds — don't leave world-readable

# 2. Build service image (first time ~1-2 min; incremental ~seconds)
docker build -f docker/service.Dockerfile -t scriptorium:latest .

# 3. Run it. WS_ROOT MUST match [workspace].root in config.toml exactly —
#    the bind-mount maps the same absolute path on both sides so
#    sandbox-container bind paths resolve against the host daemon.
WS_ROOT=/Volumes/SSD/scriptorium-state     # <— edit to match your SSD
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

Control:

```bash
docker logs -f scriptorium        # tail
docker restart scriptorium        # after editing config.toml
docker rm -f scriptorium          # stop + remove

# After Rust code changes: rebuild image and re-run
docker build -f docker/service.Dockerfile -t scriptorium:latest .
docker rm -f scriptorium
docker run -d … scriptorium:latest   # (same flags as above)
```

### Docker Compose (alternative)

A `docker-compose.yml` is also provided. Copy `.env.example` to `.env`,
set `SCRIPTORIUM_WORKSPACE_ROOT`, then:

```bash
docker compose up -d --build
docker compose logs -f
docker compose down
```

### Native binary

```bash
cargo build --release
cp deploy/config.example.toml deploy/config.toml
# Edit deploy/config.toml — set docker.socket to your OrbStack path,
# workspace.root to a host path, and [tos] credentials.
./target/release/scriptorium --config deploy/config.toml
```

For a long-running launchd service, see `docs/architecture.md`. Note
that launchd-managed native processes on macOS can trigger TCC prompts
for external volumes — the Docker path sidesteps that.

## Build the sandbox image

```bash
docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian13-v1 .
```

Image size is ~3 GB. The one-time download is the cost you pay for
avoiding runtime `apt install`s. Tag the image and pin it in config.

## Consuming from another language

`proto/sandbox.proto` is the cross-language contract. Generate stubs in your
language of choice and point the client at `grpc://<host>:<port>`.

For example, Go:

```bash
protoc --go_out=. --go-grpc_out=. proto/sandbox.proto
```

## Integration testing

`tests/e2e.rs` covers every RPC end-to-end against a real Docker daemon:

- `Health` — reachability and permit count reporting
- `Exec` — stdout/stderr capture, non-zero exit propagation, wall-clock
  timeout, and persistent workspace state across calls
- `ExecStream` — `Started`/chunk/`Finished` ordering
- `FetchIntoWorkspace` — SSRF guard rejects loopback targets
- `ListFiles` — recursive walk reflects exec-produced contents
- `DeleteWorkspace` — host directory removal + idempotent repeat
- `ListTools` / `CallTool` — descriptor count, schema validity,
  `execute_shell` routing, unknown-tool error shape
- Invalid `workspace_id` → `InvalidArgument`

These tests are `#[ignore]`-gated so CI (no Docker) is unaffected. To run
locally after building the sandbox image:

```bash
cargo test --test e2e -- --ignored --nocapture
```

Override the image tag via `SCRIPTORIUM_TEST_IMAGE=...` or the docker
socket via `DOCKER_HOST=unix:///path/to/docker.sock` if you are not on
OrbStack's default path. To also exercise `UploadToOSS` against a real
bucket, set `SCRIPTORIUM_TEST_TOS_ENDPOINT` / `_REGION` / `_BUCKET` /
`_ACCESS_KEY` / `_SECRET_KEY`.

## Status

`Exec`, `ExecStream`, `FetchIntoWorkspace`, `UploadToOSS`, `ListFiles`,
`DeleteWorkspace`, `ListTools`, `CallTool`, and `Health` are all
implemented and covered by the e2e suite (13 cases, all green against
OrbStack). Follow-up work documented in
[`docs/architecture.md`](docs/architecture.md): optional warm-pool,
automatic workspace GC, and mitmproxy-fronted egress allowlists.
Issues and PRs welcome.

## License

Scriptorium is licensed under the GNU General Public License v3.0 or later —
see [`LICENSE`](LICENSE). This matches the author's other Rust projects and
keeps derived works under the same terms.
