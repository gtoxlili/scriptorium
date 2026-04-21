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
 caller (agent)                         scriptorium                        OCI daemon
────────────────                      ─────────────────                   ──────────────
      │                                      │                                   │
      │  gRPC: Exec(workspace_id, cmd) ───▶  │                                   │
      │                                      │  bollard: run image,              │
      │                                      │  bind-mount {root}/{id}/home,  ──▶│  ┌──────────────┐
      │                                      │  drop to uid 1000,                │  │  container   │
      │                                      │  enforce cpu/mem/pids,            │  │  (bash -lc)  │
      │                                      │  wall-clock timeout.              │  └──────────────┘
      │  ◀─── stdout / stderr / exit ────    │                                   │
      │                                      │                                   │
```

Compute lifetime is **per-call ephemeral**. State lifetime is **per-workspace
persistent** (installed pip packages, Chromium profile, produced artifacts).
`workspace_id` is opaque to the service; the caller picks its granularity —
session, task, whatever.

Full design rationale: [`docs/architecture.md`](docs/architecture.md).

## Features

- **gRPC API** with streaming exec (`proto/sandbox.proto` is the source of truth).
- **Per-call container spawn** via a local Docker daemon — zero idle footprint.
- **Per-workspace persistent state** mounted at a fixed in-container path.
- **Fixed non-root user** (UID 1000) with read-only rootfs and sized tmpfs.
- **Hard resource caps** per exec: CPU millis, memory bytes, PID limit, wall
  clock.
- **Fat sandbox image** with Python, Node 20, Chromium/Playwright, FFmpeg,
  ImageMagick, and common CLIs pre-installed — no runtime `apt install` required.
- **Workspace id validation** (`[A-Za-z0-9_-]{1,128}`) with host-side path
  traversal guards.
- **tini init** inside the sandbox so Chromium child processes are reaped.

## Non-goals

- Multi-host orchestration. Scriptorium is a single-node daemon; cluster it
  by placing instances behind a router.
- Credential storage. Scriptorium does not remember secrets — callers inject
  them per-exec via `env` or a short-lived `PutFile`.
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
  service.rs                gRPC service implementation
  workspace.rs              Per-workspace state directory manager
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

## Build

```bash
cargo build --release
```

## Run

```bash
cp deploy/config.example.toml deploy/config.toml
# Edit deploy/config.toml — set docker.socket and workspace.root for your host.
cargo run --release -- --config deploy/config.toml
```

## Build the sandbox image

```bash
docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian12-v1 .
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

- `Health` reachability
- `Exec` — stdout/stderr capture, non-zero exit propagation, wall-clock
  timeout, and persistent workspace state across calls
- `ExecStream` — `Started`/chunk/`Finished` ordering
- `PutFile` / `GetFile` — binary roundtrip with chunked streaming
- `ListFiles` — recursive walk reflects exec-produced contents
- `DeleteWorkspace` — host directory removal + idempotent repeat
- Invalid `workspace_id` → `InvalidArgument`

These tests are `#[ignore]`-gated so CI (no Docker) is unaffected. To run
locally after building the sandbox image:

```bash
cargo test --test e2e -- --ignored --nocapture
```

Override the image tag via `SCRIPTORIUM_TEST_IMAGE=...` or the docker
socket via `DOCKER_HOST=unix:///path/to/docker.sock` if you are not on
OrbStack's default path.

## Status

The scaffold and protocol are stable; `Exec`, `ExecStream`, and workspace
file I/O are implemented and exercised by the e2e suite. Follow-up work
documented in [`docs/architecture.md`](docs/architecture.md) includes the
optional warm-pool, automatic workspace GC, and mitmproxy-fronted egress
allowlists. Issues and PRs welcome.

## License

Scriptorium is licensed under the GNU General Public License v3.0 or later —
see [`LICENSE`](LICENSE). This matches the author's other Rust projects and
keeps derived works under the same terms.
