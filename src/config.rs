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
    4000
}
fn default_memory_bytes() -> u64 {
    8 * 1024 * 1024 * 1024
}
fn default_pids() -> u32 {
    512
}
fn default_tmpfs_bytes() -> u64 {
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
        Ok(())
    }
}
