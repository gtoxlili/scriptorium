# Architecture

[English](./architecture.md) · [简体中文](./architecture.zh-CN.md)

Design notes for scriptorium: why it exists as a separate service, what
the isolation model actually guarantees, and where the boundaries sit.
Read this before touching `runtime.rs` or the proto.

## Why this is a separate service

An agent runtime eventually needs to execute arbitrary scripts, browser
automations, and RPA flows on behalf of a user. Keeping that capability
inside the agent process has three problems:

1. **Blast radius.** A misbehaving user script that leaks memory or
   crashes the process must not take the agent runtime with it.
2. **Deployment cadence.** The sandbox image evolves on a different
   cycle from the agent codebase (Chromium security patches, new
   Python libs). Coupling them forces lockstep releases.
3. **Reusability.** A future agent — not just the first one — should
   be able to consume the same middleware over gRPC without importing
   a Docker client, image definitions, or workspace state logic.

Scriptorium is a standalone binary with its own repo, its own
deployment artifact, and a stable gRPC contract.

## Core model

### Compute lifecycle: per-call ephemeral

Every `Exec` RPC spawns a fresh container, runs the requested shell
command via `bash -lc`, and tears the container down when the call
returns. There is no server-side session and no idle container pool.

Chat sessions upstream have no fixed expiration, so a
container-per-session model would accumulate indefinitely. The
per-call model trades roughly one to three seconds of docker-run
overhead for zero idle footprint.

### State lifecycle: per-workspace persistent

State that must survive across calls — `pip --user` packages, Chromium
cookies, scripts written by the LLM, artifacts produced during exec —
lives in a host directory bind-mounted into every container that shares
a `workspace_id`:

```
host:   {workspace_root}/{workspace_id}/home/
mount:  /home/agent                    (inside the container)
```

The in-container user has a fixed UID/GID (default `1000:1000`). The
host directory is chowned to that pair on first access so bind-mount
writes land with the right ownership. When the chown is refused with
`EPERM` (macOS dev setups where the service runs as a regular user),
the manager falls back to `chmod 0777` on that single per-workspace
directory so the container user can still write. Production Linux
deployments should run the service as root, or with `CAP_CHOWN`, to
stay on the tighter chown path.

### workspace_id is opaque

The service does not model "chat session", "brand", or "tenant".
Callers supply a `workspace_id` string; the service stores state under
that key. This keeps the contract narrow and means scriptorium never
has to learn upstream concepts.

`workspace_id` is validated against `[A-Za-z0-9_-]{1,128}`. Whitelist,
not blacklist: every "reject `..` and slashes" approach I've seen has
leaked eventually (NUL bytes, Unicode lookalikes, exotic path
separators).

Callers pick a granularity that matches their intended state lifetime:
session id, task id, job id.

`tenant_id` rides along on each `Exec` call for audit logging; it
does not participate in directory keying.

### Two kinds of state, kept apart

- **Agent working state** (this service): temp files, downloaded data,
  ad-hoc scripts the LLM writes, packages it installs. Scoped to
  `workspace_id`, ephemeral on the timescale of the caller's
  workspace lifetime.
- **Business credentials** (NOT this service): platform login
  cookies, API tokens, user-owned assets. Callers keep those in their
  own encrypted store and inject them into individual `Exec` calls
  via the `env` field, or by `FetchIntoWorkspace`-ing a short-lived
  credential file before the exec. Scriptorium never stores
  long-lived secrets.

Mixing the two would let any script in the sandbox read any
credential and would couple credential lifetime to sandbox GC. Hard
boundary.

### Data flow: URLs and object keys for user-facing transfers

Ingress prefers URLs. `FetchIntoWorkspace(url, target_path)` is a
host-side `reqwest` that streams the body into the workspace under
the SSRF guard.

Delivery prefers object storage. `UploadToOSS(source_path, compress?)`
tars + gzips directories as needed, uploads to an S3-compatible store
(Volcano Engine TOS by default), and returns the permanent
`object_key` plus size, content-type, sha256, and basename. Files
above `multipart_threshold_bytes` stream through multipart upload so
peak RAM stays bounded by `part_size_bytes`, regardless of how big
the artifact is.

Scriptorium intentionally does not return a presigned URL. The caller
resolves `object_key` to a user-facing URL through its own attachment
layer, typically by issuing a stable URL of the shape
`https://{host}/api/v1/public/attachments/{id}?access_key={secret}`
that re-signs a fresh short-TTL download link on every hit. The URL
the end user bookmarks never expires, which is the actual product
requirement a presigned URL fails to meet.

For trusted host-integrated callers, scriptorium also exposes direct
workspace import/export RPCs that stream bytes over gRPC. These are
meant for local handoff flows such as a host application's own
workspace bridge; they are not the default internet-facing path.

Consequences:

- The caller never proxies artifact bytes, so it stays out of I/O
  hot paths.
- Signed-URL TTL is invisible to the end user; the permanent handle
  outlives any specific presign.
- Third-party credentials stay in the caller's vault and ride into
  a single exec on the `env` field, or via a short-lived
  `FetchIntoWorkspace` of a credential file.

## Isolation model

### Kernel

Container isolation works at the kernel level:

- **macOS (OrbStack / Docker Desktop / Colima)** runs a Linux VM via
  `Virtualization.framework`. The sandbox container shares the VM
  kernel with other containers but is cut off from the macOS host
  kernel.
- **Linux hosts** use standard namespaces and cgroups.

### In-container

- Non-root user (UID 1000).
- Rootfs mounted read-only; only `/home/agent` (bind-mounted
  workspace) and `/tmp` (tmpfs, size-capped) are writable.
- `tini` as PID 1 reaps zombie subprocesses, which matters for
  long-running Chromium sessions that fork helpers.
- Per-container resource caps: CPU millis, memory bytes, PID count,
  tmpfs size. Defaults live in config; callers may override within
  hardcoded ceilings.
- Hard wall-clock timeout. Expired containers are force-killed and
  the call returns `DeadlineExceeded`.
- Mid-flight cancellation: if the caller drops an `ExecStream`
  before the `Finished` event is drained, the container is
  SIGKILL-ed and removed instead of burning resources until the
  command exits on its own.

### Network

Current behaviour: bridge networking, unrestricted outbound. That is
the right default: the primary workloads (scraping, RPA, media
processing, API calls) all need the internet.

`FetchIntoWorkspace` on the host side is SSRF-guarded. The resolved
IP must not fall into loopback, RFC1918, link-local, broadcast,
documentation, CGNAT (100.64/10), or the IPv6 equivalents (loopback,
ULA, link-local). `fetch.allow_private_network = true` disables the
guard for deployments that genuinely need to pull from internal hosts.

A planned hardening phase will route container egress through a
service-managed mitmproxy, log host + path per request for audit, and
optionally enforce per-tenant egress allowlists.

### Admission control

Every `Exec` / `ExecStream` acquires a permit from a tokio semaphore
whose capacity is `concurrency.max_concurrent_execs` (default 4).
When the pool saturates, requests queue for up to
`concurrency.exec_queue_timeout_seconds` (default 30 s) and then
return `RESOURCE_EXHAUSTED` with a retry hint. This keeps an N-user
burst from simultaneously reserving N × 8 GiB memory caps and OOM-ing
the host.

`FetchIntoWorkspace`, `UploadToOSS`, `ImportWorkspaceObject`,
`ExportWorkspaceObject`, `ListFiles`, and `DeleteWorkspace` do not
consume permits; they are host-side I/O with no container cost.
`CallTool` routes `execute_shell` and `deliver` through the same
helpers as the primitive RPCs, so the semaphore gates `execute_shell`
consistently whichever surface the caller uses. The two
workspace-sandbox exchange descriptors are catalog-only at the
scriptorium layer; they are meant for a host bridge above it.

`Health` reports `exec_permits_available` for observability.

## Image design choices

- **Fat base on purpose.** Python, Node, Chromium/Playwright, FFmpeg,
  and common CLIs are pre-installed so the agent does not need to
  `apt install` on every call. It couldn't anyway: non-root user,
  read-only rootfs.
- **Playwright browsers pinned to `/opt/ms-playwright`.** Playwright's
  default of `$HOME/.cache/ms-playwright` gets shadowed by the
  workspace bind-mount at runtime. Setting `PLAYWRIGHT_BROWSERS_PATH`
  at build time keeps the browser inside the read-only layer.
- **No apt-installed Chromium.** Shipping both the Debian package
  and Playwright's bundled browser causes version drift; the
  Playwright copy is the only truth.

## Two public surfaces, one implementation

Primitive RPCs (`Exec`, `ExecStream`, `FetchIntoWorkspace`,
`UploadToOSS`, `ListFiles`, `DeleteWorkspace`, `Health`) are the
protocol-level truth. The LLM-facing tool layer (`ListTools`,
`CallTool`) sits on top of them. It publishes OpenAI-function-call /
MCP-shaped descriptors for four tools: `execute_shell`, `deliver`,
and two workspace-sandbox exchange tools. `execute_shell` and
`deliver` route through the same `do_*` helpers that back the
primitive handlers. The two exchange tools advertise host-bridge
contracts and return an explicit error when invoked directly against
bare scriptorium.

The split exists because engine callers want streaming-capable
primitives directly, while LLM callers benefit from a thinner
catalog with JSON Schema metadata. Either surface works. They cannot
drift.

## Why Rust + tonic + bollard

- **Rust + tonic.** Scriptorium is a latency-sensitive proxy between
  gRPC calls and Docker API calls. Rust gives bounded latency and
  memory with no GC pauses, and async via tokio is the ecosystem
  default. tonic is the mature gRPC stack on top of that.
- **bollard.** Actively maintained Docker client for Rust. Native
  streaming for attach/logs is what makes `ExecStream` workable.

## Non-goals

- **Multi-host orchestration.** Single-node daemon. If one box is
  not enough, front multiple instances with a router. No cluster
  manager here.
- **Image build pipeline.** The sandbox image is built out of band
  by CI or by hand and referenced by tag. Scriptorium does not
  build or pull images on startup, though the Docker daemon's own
  cache will pull on first-exec if the tag is missing.
- **Credential vault.** See "Two kinds of state". Intentional, hard
  boundary.

## Open questions

1. **Warm pool.** A small pool of pre-created stopped containers per
   image could shave spawn cost for hot callers. Deferred until
   measurements say it matters. In practice, cold start sits around
   1.5 s; once an LLM round trip is in the call graph, that is not
   where the latency budget disappears.
2. **Scheduled workspace GC.** Manual `DeleteWorkspace` exists. A
   background sweeper that evicts workspaces inactive for longer
   than a configurable TTL is the next piece.
3. **mitmproxy egress audit.** Planned hardening (see Network
   above): route container egress through a service-managed
   mitmproxy for per-request logging and optional per-tenant
   allowlists.
