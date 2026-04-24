use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub docker: DockerConfig,
    pub sandbox: SandboxConfig,
    pub workspace: WorkspaceConfig,
    pub tos: TosConfig,
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,
    #[serde(default)]
    pub fetch: FetchConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen: SocketAddr,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DockerConfig {
    /// Unix socket to the Docker daemon. On macOS with OrbStack this is
    /// typically `~/.orbstack/run/docker.sock`.
    pub socket: PathBuf,
    /// Timeout for control-plane calls (image pull, container create, etc.).
    #[serde(default = "default_docker_timeout")]
    pub control_timeout_seconds: u64,
}

fn default_docker_timeout() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    /// Default image tag used when the request does not override it.
    pub default_image: String,
    /// Default per-exec timeout.
    #[serde(default = "default_exec_timeout")]
    pub default_timeout_seconds: u32,
    /// Default CPU quota in milli-cores (1000 = 1 core).
    #[serde(default = "default_cpu_millis")]
    pub default_cpu_millis: u32,
    /// Default memory cap in bytes.
    #[serde(default = "default_memory_bytes")]
    pub default_memory_bytes: u64,
    /// Default PID limit.
    #[serde(default = "default_pids")]
    pub default_pids: u32,
    /// Default /tmp tmpfs size in bytes.
    #[serde(default = "default_tmpfs_bytes")]
    pub default_tmpfs_bytes: u64,
    /// Heavy-mode CPU quota. Used when `execute_shell` is invoked with
    /// `heavy = true`, i.e. the LLM has flagged this specific call as
    /// needing more than the default budget (Chromium with many tabs,
    /// large pandas, video encoding, …). Meant to be ≥ default_cpu_millis.
    #[serde(default = "default_heavy_cpu_millis")]
    pub heavy_cpu_millis: u32,
    /// Heavy-mode memory cap (bytes). See `heavy_cpu_millis`.
    #[serde(default = "default_heavy_memory_bytes")]
    pub heavy_memory_bytes: u64,
    /// Heavy-mode PID limit.
    #[serde(default = "default_heavy_pids")]
    pub heavy_pids: u32,
    /// Heavy-mode /tmp tmpfs size.
    #[serde(default = "default_heavy_tmpfs_bytes")]
    pub heavy_tmpfs_bytes: u64,
    /// UID that the in-container non-root user has. The workspace directory
    /// is chowned to this uid on first access so bind-mount writes land with
    /// the correct ownership.
    #[serde(default = "default_agent_uid")]
    pub agent_uid: u32,
    #[serde(default = "default_agent_gid")]
    pub agent_gid: u32,
    /// Absolute path inside the container where the workspace is mounted.
    #[serde(default = "default_workspace_mount")]
    pub workspace_mount: PathBuf,
}

fn default_exec_timeout() -> u32 {
    300
}
fn default_cpu_millis() -> u32 {
    2000
}
fn default_memory_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}
fn default_pids() -> u32 {
    256
}
fn default_tmpfs_bytes() -> u64 {
    1024 * 1024 * 1024
}
fn default_heavy_cpu_millis() -> u32 {
    4000
}
fn default_heavy_memory_bytes() -> u64 {
    8 * 1024 * 1024 * 1024
}
fn default_heavy_pids() -> u32 {
    512
}
fn default_heavy_tmpfs_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}
fn default_agent_uid() -> u32 {
    1000
}
fn default_agent_gid() -> u32 {
    1000
}
fn default_workspace_mount() -> PathBuf {
    PathBuf::from("/home/agent")
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    /// Root directory on the host where per-workspace state lives.
    /// Each workspace_id becomes a direct child of this directory.
    pub root: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TosConfig {
    /// S3-compatible endpoint. For Volcano Engine TOS this is typically
    /// `https://tos-s3-cn-{region}.volces.com`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Prefix prepended to all scriptorium-generated object keys. Keeps
    /// sandbox artifacts separate from other producers in the same bucket.
    #[serde(default = "default_tos_prefix")]
    pub key_prefix: String,
    /// Default lifetime for signed download URLs.
    #[serde(default = "default_signed_url_expires")]
    pub signed_url_expires_seconds: u32,
    /// Max signed URL lifetime; requests asking for more are clamped.
    #[serde(default = "default_signed_url_max")]
    pub signed_url_max_seconds: u32,
    /// Multipart upload chunk size. Matches aws-sdk-s3 default.
    #[serde(default = "default_part_size")]
    pub part_size_bytes: u64,
    /// File-size threshold above which uploads switch from a single
    /// `put_object` (reads whole file into RAM) to streaming multipart
    /// upload (peak RAM bounded by `part_size_bytes`). Default 64 MiB —
    /// smaller files don't gain anything from multipart's extra round
    /// trips, larger ones benefit from bounded memory use.
    #[serde(default = "default_multipart_threshold")]
    pub multipart_threshold_bytes: u64,
    /// Upload total-wall timeout.
    #[serde(default = "default_upload_timeout")]
    pub upload_timeout_seconds: u64,
}

fn default_tos_prefix() -> String {
    "sandbox/".to_string()
}
fn default_signed_url_expires() -> u32 {
    3600
}
fn default_signed_url_max() -> u32 {
    86400
}
fn default_part_size() -> u64 {
    8 * 1024 * 1024
}
fn default_multipart_threshold() -> u64 {
    64 * 1024 * 1024
}
fn default_upload_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConcurrencyConfig {
    /// Hard cap on simultaneously running containers spawned by `Exec` and
    /// `ExecStream`. Requests beyond this queue (up to `queue_timeout`).
    /// 0 = derive a default from available CPU/memory (currently: 10).
    #[serde(default)]
    pub max_concurrent_execs: u32,
    /// Time a queued exec will wait for a permit before returning
    /// `RESOURCE_EXHAUSTED`. 0 = queue indefinitely.
    #[serde(default)]
    pub exec_queue_timeout_seconds: u32,
}

impl ConcurrencyConfig {
    pub fn effective_max(&self) -> usize {
        if self.max_concurrent_execs == 0 {
            10
        } else {
            self.max_concurrent_execs as usize
        }
    }

    pub fn effective_queue_timeout(&self) -> Option<std::time::Duration> {
        if self.exec_queue_timeout_seconds == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(u64::from(
                self.exec_queue_timeout_seconds,
            )))
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FetchConfig {
    #[serde(default = "default_fetch_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_fetch_max_body")]
    pub max_body_bytes: u64,
    /// When true, URLs resolving to loopback / RFC1918 ranges are allowed.
    /// Default false — the service rejects them as SSRF.
    #[serde(default)]
    pub allow_private_network: bool,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: default_fetch_timeout(),
            max_body_bytes: default_fetch_max_body(),
            allow_private_network: false,
        }
    }
}

fn default_fetch_timeout() -> u64 {
    60
}
fn default_fetch_max_body() -> u64 {
    1024 * 1024 * 1024 // 1 GiB
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| Error::Other(format!("parse {}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if !self.workspace.root.is_absolute() {
            return Err(Error::Other(format!(
                "workspace.root must be an absolute path, got {}",
                self.workspace.root.display()
            )));
        }
        if self.sandbox.agent_uid == 0 {
            return Err(Error::Other(
                "sandbox.agent_uid must be non-zero — running as root inside the sandbox defeats isolation"
                    .into(),
            ));
        }
        if self.sandbox.default_image.is_empty() {
            return Err(Error::Other(
                "sandbox.default_image must not be empty".into(),
            ));
        }
        if !self.docker.socket.exists() {
            tracing::warn!(
                socket = %self.docker.socket.display(),
                "docker socket does not exist at startup — service will fail to reach the daemon"
            );
        }
        if self.tos.endpoint.is_empty() || self.tos.bucket.is_empty() {
            return Err(Error::Other(
                "tos.endpoint and tos.bucket are required".into(),
            ));
        }
        Ok(())
    }
}
