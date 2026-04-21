use std::{fmt, path::PathBuf, time::Duration};

use bollard::Docker;

use crate::{
    config::{DockerConfig, SandboxConfig},
    error::Result,
};

/// `DockerRuntime` is the Docker-backed implementation of the container
/// spawning port. It talks to a local Docker-compatible daemon (OrbStack,
/// Docker Desktop, Colima, etc.) via its Unix socket.
#[derive(Clone)]
pub struct DockerRuntime {
    pub(crate) docker: Docker,
    pub(crate) docker_cfg: DockerConfig,
    pub(crate) sandbox_cfg: SandboxConfig,
}

// Manual Debug — `bollard::Docker` does not derive `Debug`, and the config
// fields are more useful than a dump of the docker client handle anyway.
impl fmt::Debug for DockerRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DockerRuntime")
            .field("socket", &self.docker_cfg.socket)
            .field("default_image", &self.sandbox_cfg.default_image)
            .finish_non_exhaustive()
    }
}

impl DockerRuntime {
    pub async fn connect(docker_cfg: DockerConfig, sandbox_cfg: SandboxConfig) -> Result<Self> {
        let socket = docker_cfg.socket.clone();
        let docker = Docker::connect_with_unix(
            &socket.to_string_lossy(),
            docker_cfg.control_timeout_seconds,
            bollard::API_DEFAULT_VERSION,
        )?;
        docker.ping().await?;
        tracing::info!(socket = %socket.display(), "docker daemon reachable");
        Ok(Self {
            docker,
            docker_cfg,
            sandbox_cfg,
        })
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    pub async fn version_string(&self) -> String {
        match self.docker.version().await {
            Ok(v) => format!(
                "{} ({})",
                v.version.unwrap_or_default(),
                v.api_version.unwrap_or_default()
            ),
            Err(e) => format!("unavailable: {e}"),
        }
    }

    /// Spawn a container, run `command` inside it via `bash -lc`, and return
    /// the collected output. The container is removed when the call returns.
    ///
    /// This is the MVP one-shot path; streaming is wired in `ExecStream`.
    #[allow(clippy::unused_async)] // The real implementation will use async IO.
    pub async fn exec_oneshot(&self, _req: ExecParams) -> Result<ExecOutcome> {
        // TODO: implement via bollard create_container + start + attach + wait.
        Err(crate::error::Error::Other("not yet implemented".into()))
    }
}

/// Internal parameter bundle used by exec paths. Mirrors the gRPC
/// `ExecRequest` but is decoupled from the protobuf types so the runtime is
/// reusable if we ever add a non-gRPC interface.
#[derive(Debug)]
pub struct ExecParams {
    pub workspace_id: String,
    pub tenant_id: String,
    pub command: String,
    pub timeout: Duration,
    pub env: Vec<(String, String)>,
    pub image: String,
    pub host_home_dir: PathBuf,
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub pids: u32,
    pub tmpfs_bytes: u64,
}

#[derive(Debug)]
pub struct ExecOutcome {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
    pub timed_out: bool,
}
