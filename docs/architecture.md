# Architecture

This document captures the design decisions behind scriptorium and the
constraints that shaped them. It is the primary reference for anyone
extending the service — read this before touching `runtime.rs` or the proto.

## Why this is a separate service

Agent runtimes need the ability to execute arbitrary scripts, browser
automations and RPA flows on behalf of a user. Three properties make that
capability a poor fit for living inside the agent itself:

1. **Blast radius isolation.** If a misbehaving user script exhausts
   resources or crashes, it must not take the agent runtime down with it.
2. **Different deployment cadence.** The sandbox image evolves on a
   different cycle from the agent codebase — security patches to Chromium,
   new Python libs, etc. — and coupling them would force lockstep releases.
3. **Reusability.** Any future agent can consume the same middleware over
   gRPC without dragging in the Docker client, image definitions, or
   workspace state logic.

Scriptorium is therefore a standalone binary with its own repo, its own
deployment artifact, and a stable gRPC contract.

## Core model

### Compute lifecycle: per-call ephemeral

Every `Exec` RPC spawns a fresh container, runs the requested shell command
via `bash -lc`, and tears the container down on return. There is no
server-side "session" concept and no idle container pool.

Rationale: upstream chat sessions have no expiration, so a
container-per-session model would pile up indefinitely in memory. The
per-call model trades ~1–3s of docker-run overhead for O(0) idle footprint.

### State lifecycle: per-workspace persistent

State that must survive between calls — installed `pip --user` packages,
Chromium cookies, scripts written by the LLM, produced artifacts — lives
in a host directory that is bind-mounted into every container for the same
`workspace_id`:

```
host:   {workspace_root}/{workspace_id}/home/
mount:  /home/agent                    (inside the container)
```

The in-container user is a fixed UID/GID (default `1000:1000`). The host
directory is chowned to that pair on first access so bind-mount writes
land with the correct ownership. When the chown is refused with `EPERM`
(e.g. macOS dev setups where the service runs as a regular user), the
manager falls back to `chmod 0777` on that single per-workspace directory
so the container user can still write. Production Linux deployments
should run the service as root — or with `CAP_CHOWN` — to prefer the
tighter chown path.

### workspace_id is opaque

The service does not model "chat session", "brand", or "tenant". Callers
supply a `workspace_id` string and the service stores state under that key.
This keeps the contract narrow and means the service never has to learn
upstream concepts.

`workspace_id` is validated against a tight `[A-Za-z0-9_-]{1,128}` whitelist.
This is a deliberate whitelist rather than a blacklist: historically every
"reject `..` and slashes" blacklist has leaked (NUL bytes, Unicode
lookalikes, rare path separators).

Callers should pick a granularity that matches **their** intended state
lifetime — session id, task id, job id, etc.

`tenant_id` is carried on each `Exec` call purely for audit logging; it
does not participate in directory keying.

### Two kinds of state, deliberately separated

- **Agent working state** (this service): temp files, downloaded data,
  ad-hoc scripts the LLM writes, packages the LLM installs. Scoped to
  `workspace_id`. Ephemeral on the timescale of the caller's workspace.
- **Business credentials** (NOT this service): platform login cookies,
  API tokens, user assets. Callers must keep those in their own encrypted
  store and inject them into individual `Exec` calls via the `env` field
  or by `FetchIntoWorkspace`-ing a short-lived credential file before the
  exec. Scriptorium never stores long-lived secrets.

Mixing the two would let any script in the sandbox read any credential, and
would couple credential lifetime to sandbox GC. We keep them separated as a
hard boundary.

### Data flow: URLs in, object keys out

Scriptorium deliberately does **not** shuttle file bytes through gRPC.
Inputs land via `FetchIntoWorkspace(url, target_path)` — a host-side
reqwest that streams the body into the workspace under an SSRF guard.
Outputs leave via `UploadToOSS(source_path, compress?)` — the service
tars + gzips directories as needed, uploads to an S3-compatible object
store (Volcano Engine TOS by default), and returns the **permanent
`object_key`** (plus size / content-type / sha256 / basename) — but
**no signed URL**.

The signed-URL concern belongs to the caller, not to scriptorium.
Presigned URLs are TTL-limited by design, and artifacts the user wants
to revisit days later would 403. The caller's host system (agent-core,
specifically) owns an attachment table keyed by UUID and issues a
stable, permanent URL of the shape
`https://{base}/api/v1/public/attachments/{id}?access_key={secret}`.
That endpoint re-signs a fresh short-TTL presigned URL on every hit, so
the URL the end user bookmarks never expires.

Scriptorium therefore does one job — put the bytes in the bucket and
return the handle — and leaves delivery UX to the caller's existing
machinery. Agent-core in particular has an attachment repository
(`attachments` table, `RegisterExistingObject` entry point) that takes
the object_key + metadata scriptorium returns and mints a permanent
token.

Consequences:

- The caller never has to proxy artifact bytes; the agent service stays
  slim and stays out of I/O hot paths.
- Signed-URL TTL is invisible to the end user — the attachment handle
  outlives any particular presign.
- Credentials for third-party services live in the caller's secure
  store; they're injected into a single `Exec` call via `env` or via a
  short-lived `FetchIntoWorkspace` of a credential file.

## Isolation model

### Kernel

The container backend delivers kernel-level isolation:

- **macOS (OrbStack / Docker Desktop / Colima)** runs the Linux VM via
  `Virtualization.framework`. The sandbox container shares the VM kernel
  with other containers but is cut off from the macOS host kernel.
- **Linux hosts** use the standard kernel namespaces + cgroups.

### In-container

- Fixed non-root user (UID 1000).
- Root filesystem mounted read-only; only `/home/agent` (bind-mounted
  state) and `/tmp` (tmpfs, size-capped) are writable.
- `tini` PID 1 reaps zombie subprocesses — critical for long-running
  Chromium sessions that fork helper processes.
- Resource caps applied per container: CPU millis, memory bytes, PID count,
  tmpfs bytes. Defaults are configured server-side; callers may override
  within hardcoded ceilings.
- Hard wall-clock timeout enforced by the service; expired containers are
  force-killed and the call returns `DeadlineExceeded`.

### Network

MVP: the container has default bridge networking and unrestricted egress.
This is deliberate — the primary workloads (scraping, RPA, media
processing, API calls) all need outbound network.

`FetchIntoWorkspace` on the host side *is* SSRF-guarded: the resolved
IP must not fall into loopback, RFC1918, link-local, broadcast,
documentation, CGNAT (100.64/10), or IPv6 loopback / ULA / link-local
ranges. `fetch.allow_private_network` in the config lifts the guard if
the deployment genuinely needs to pull from internal hosts.

A follow-up hardening phase will:
1. Front all container egress with a service-managed mitmproxy, logging
   host+path per request for audit.
2. Optionally enforce per-tenant egress allowlists.

### Admission control: concurrency semaphore

Every `Exec` / `ExecStream` acquires a permit from a tokio semaphore
whose capacity is `concurrency.max_concurrent_execs` (default 4). When
the pool is saturated, requests queue for up to
`concurrency.exec_queue_timeout_seconds` (default 30 s) and then return
`RESOURCE_EXHAUSTED` with a retry hint. This prevents an N-user burst
from all simultaneously reserving their per-container 8 GiB memory cap
and triggering an OOM on the host.

`FetchIntoWorkspace`, `UploadToOSS`, `ListFiles`, and `DeleteWorkspace`
do **not** consume permits — they are host-side I/O with no container
cost. `CallTool` routes through the same `do_exec_oneshot` / `do_fetch`
/ `do_upload` helpers as the primitives, so the semaphore gates
`execute_shell` consistently whichever surface the caller uses.

`Health` reports `exec_permits_available` for observability.

## Image design choices

- **Fat base on purpose.** Python, Node, Chromium/Playwright, FFmpeg and
  common CLIs are pre-installed so the agent does not need to `apt install`
  on every call (it cannot anyway — non-root user, read-only rootfs).
- **Playwright browsers pinned to `/opt/ms-playwright`.** Playwright
  defaults to downloading Chromium into `$HOME/.cache/ms-playwright`, but
  `$HOME` is shadowed by the bind mount at runtime, so a default install
  would lose its browser on first exec. Setting `PLAYWRIGHT_BROWSERS_PATH`
  at build time keeps the browser in the read-only layer.
- **No apt-installed Chromium.** Having both the Debian chromium package
  and Playwright's bundled browser causes version drift; the Playwright one
  is the only truth.

## Two public surfaces, one implementation

Primitive RPCs (`Exec`, `ExecStream`, `FetchIntoWorkspace`,
`UploadToOSS`, `ListFiles`, `DeleteWorkspace`, `Health`) are the
protocol-level truth. A second, AI-facing tool layer (`ListTools`,
`CallTool`) sits in front of them: it publishes OpenAI-function-call /
MCP-shaped descriptors for three tools — `execute_shell`, `fetch`,
`deliver` — and routes invocations through the same `do_*` helpers that
back the primitive handlers. There is no independent code path.

The split exists because engine callers want the streaming-capable
primitives directly, while LLM callers benefit from a thinner catalog
with JSON Schema metadata. Either surface works — they cannot drift.

## Why Rust + tonic + bollard

- **Rust / tonic** — the service is a tight, performance-critical proxy
  between gRPC calls and Docker API calls. Rust gives bounded latency and
  memory, no GC pauses, and first-class async via tokio. tonic is the
  mature gRPC stack and matches the protobuf tooling we'd use anyway.
- **bollard** — the Docker client in Rust. Mature, maintained, and streams
  attach/logs natively — which matters for `ExecStream`.

## Non-goals

- **Multi-host orchestration.** The MVP is a single-node daemon. If we
  outgrow one box we'll front multiple instances with a router, not build
  a cluster manager in this service.
- **Image build pipeline.** The sandbox image is built out-of-band by CI
  or by hand and referenced by tag. Scriptorium does not build or pull
  images on startup (it will pull on first-exec if missing, via the
  Docker daemon's own image cache).
- **Credential vault.** See the "Two kinds of state" section. This is
  intentional and a hard boundary.

## Open questions / phases

1. **Warm pool**: should we keep a small pool of pre-created stopped
   containers per image to shave the spawn cost for hot callers?
   Decision deferred until we measure actual spawn latency on OrbStack.
2. **GC policy**: automatic pruning of old workspaces. MVP exposes manual
   `DeleteWorkspace`; a scheduled sweeper is Phase 2.
3. **Multipart upload for >1 GiB artifacts**: current `UploadToOSS`
   reads the payload fully into memory before `put_object`. Move to the
   aws-sdk-s3 transfer manager when an actual >1 GiB artifact shows up.
4. **Cancellation on stream drop**: a dropped `ExecStream` client
   currently leaves the container running to its natural end or wall
   timeout. The container is still `AutoRemove=true` so it cleans up —
   but the wasted work is visible. Wire a CancellationToken into the
   stream future.
5. **mitmproxy integration** for container-side egress audit: Phase 3.
