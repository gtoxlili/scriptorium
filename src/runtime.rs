use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    pin::Pin,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bollard::{
    Docker,
    container::LogOutput,
    models::{ContainerCreateBody, HostConfig},
    query_parameters::{
        AttachContainerOptionsBuilder, CreateContainerOptionsBuilder, KillContainerOptionsBuilder,
        RemoveContainerOptionsBuilder, StartContainerOptions, WaitContainerOptions,
    },
};
use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::{
    config::{DockerConfig, SandboxConfig},
    error::{Error, Result},
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

    pub fn sandbox_cfg(&self) -> &SandboxConfig {
        &self.sandbox_cfg
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
    pub async fn exec_oneshot(&self, req: ExecParams) -> Result<ExecOutcome> {
        let start = Instant::now();
        let container_id = self.create_container(&req).await?;

        // Attach BEFORE start so we don't miss early output.
        let attach = self
            .docker
            .attach_container(
                &container_id,
                Some(
                    AttachContainerOptionsBuilder::default()
                        .stdout(true)
                        .stderr(true)
                        .stream(true)
                        .logs(false)
                        .stdin(false)
                        .build(),
                ),
            )
            .await?;

        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut attach_stream = attach.output;

        let docker = &self.docker;
        let cid = container_id.clone();

        let collect = async {
            while let Some(result) = attach_stream.next().await {
                match result {
                    Ok(LogOutput::StdOut { message }) => stdout.extend_from_slice(&message),
                    Ok(LogOutput::StdErr { message }) => stderr.extend_from_slice(&message),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        };

        let wait = async {
            let mut wait_stream = docker.wait_container(&cid, None::<WaitContainerOptions>);
            // bollard maps non-zero exit codes into a dedicated error variant,
            // so we unwrap it back into the numeric code we actually want.
            match wait_stream.next().await {
                Some(Ok(resp)) => resp.status_code,
                Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => code,
                _ => -1,
            }
        };

        let combined = async { tokio::join!(collect, wait) };

        let timeout_result = tokio::time::timeout(req.timeout, combined).await;
        let (exit_code, timed_out) = if let Ok(((), code)) = timeout_result {
            (i32::try_from(code).unwrap_or(-1), false)
        } else {
            // AutoRemove fires on successful exit; after KILL the container may
            // linger, so force-remove to be safe. Ignore errors — on some race
            // conditions the container is already gone.
            let _ = self
                .docker
                .kill_container(
                    &container_id,
                    Some(
                        KillContainerOptionsBuilder::default()
                            .signal("SIGKILL")
                            .build(),
                    ),
                )
                .await;
            let _ = self
                .docker
                .remove_container(
                    &container_id,
                    Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                )
                .await;
            (-1, true)
        };

        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(ExecOutcome {
            exit_code,
            stdout,
            stderr,
            duration_ms,
            timed_out,
        })
    }

    /// Streaming variant. Returns the container id (so the caller can emit a
    /// `Started` event) plus a stream that yields output chunks and a final
    /// `Finished` event.
    ///
    /// If the caller drops the stream before draining the final `Finished`
    /// event, `ContainerCleanupGuard` below spawns a best-effort
    /// kill+force-remove so the container doesn't keep running to its wall
    /// timeout. On natural completion or explicit timeout handling the guard
    /// is disarmed (`completed = true`) because Docker's `AutoRemove=true` or
    /// the explicit remove call already handled cleanup.
    pub async fn exec_stream(
        &self,
        req: ExecParams,
    ) -> Result<(
        String,
        Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>,
    )> {
        let container_id = self.create_container(&req).await?;
        let started_at = Instant::now();
        let timeout = req.timeout;

        let attach = self
            .docker
            .attach_container(
                &container_id,
                Some(
                    AttachContainerOptionsBuilder::default()
                        .stdout(true)
                        .stderr(true)
                        .stream(true)
                        .logs(false)
                        .stdin(false)
                        .build(),
                ),
            )
            .await?;

        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await?;

        let docker = self.docker.clone();
        let cid_for_stream = container_id.clone();

        let stream = async_stream::stream! {
            let mut cleanup_guard = ContainerCleanupGuard::new(docker.clone(), cid_for_stream.clone());
            let mut attach_stream = attach.output;
            // Set up wait *concurrently* with attach so we capture the exit
            // code before Docker's AutoRemove races to delete the container.
            let mut wait_stream = docker.wait_container(
                &cid_for_stream,
                None::<WaitContainerOptions>,
            );
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);

            let mut pending_exit: Option<i32> = None;
            let mut wait_done = false;
            let mut attach_done = false;
            let mut timed_out = false;

            while !(wait_done && attach_done) {
                tokio::select! {
                    () = &mut deadline => {
                        timed_out = true;
                        break;
                    }
                    w = wait_stream.next(), if !wait_done => {
                        wait_done = true;
                        match w {
                            Some(Ok(resp)) => {
                                pending_exit = Some(i32::try_from(resp.status_code).unwrap_or(-1));
                            }
                            Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => {
                                pending_exit = Some(i32::try_from(code).unwrap_or(-1));
                            }
                            _ => {}
                        }
                    }
                    a = attach_stream.next(), if !attach_done => match a {
                        Some(Ok(LogOutput::StdOut { message })) => yield StreamEvent::Stdout(message),
                        Some(Ok(LogOutput::StdErr { message })) => yield StreamEvent::Stderr(message),
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => attach_done = true,
                    }
                }
            }

            if timed_out {
                // Explicit cleanup for the timeout path — the container would
                // otherwise linger until the wall-clock kicks in inside it.
                cleanup_guard.disarm();
                let _ = docker
                    .kill_container(
                        &cid_for_stream,
                        Some(KillContainerOptionsBuilder::default().signal("SIGKILL").build()),
                    )
                    .await;
                let _ = docker
                    .remove_container(
                        &cid_for_stream,
                        Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                    )
                    .await;
                let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                yield StreamEvent::Finished { exit_code: -1, timed_out: true, duration_ms };
                return;
            }

            // Natural completion: AutoRemove handles cleanup.
            cleanup_guard.disarm();
            let exit_code = pending_exit.unwrap_or(-1);
            let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            yield StreamEvent::Finished { exit_code, timed_out: false, duration_ms };
        };

        Ok((container_id, Box::pin(stream)))
    }

    async fn create_container(&self, req: &ExecParams) -> Result<String> {
        let host_path = req
            .host_home_dir
            .to_str()
            .ok_or_else(|| {
                Error::Other(format!(
                    "non-UTF8 host home path: {}",
                    req.host_home_dir.display()
                ))
            })?
            .to_string();
        let container_path = self
            .sandbox_cfg
            .workspace_mount
            .to_string_lossy()
            .into_owned();

        let mut tmpfs: HashMap<String, String> = HashMap::new();
        tmpfs.insert("/tmp".into(), format!("rw,size={}", req.tmpfs_bytes));

        let host_config = HostConfig {
            auto_remove: Some(true),
            binds: Some(vec![format!("{host_path}:{container_path}")]),
            memory: Some(i64::try_from(req.memory_bytes).unwrap_or(i64::MAX)),
            nano_cpus: Some(i64::from(req.cpu_millis) * 1_000_000),
            pids_limit: Some(i64::from(req.pids)),
            readonly_rootfs: Some(true),
            tmpfs: Some(tmpfs),
            network_mode: Some("bridge".into()),
            ..Default::default()
        };

        let env: Vec<String> = req.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

        let uid = self.sandbox_cfg.agent_uid;
        let gid = self.sandbox_cfg.agent_gid;

        let config = ContainerCreateBody {
            image: Some(req.image.clone()),
            user: Some(format!("{uid}:{gid}")),
            working_dir: Some(container_path.clone()),
            cmd: Some(vec![
                "bash".to_string(),
                "-lc".to_string(),
                req.command.clone(),
            ]),
            env: Some(env),
            host_config: Some(host_config),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(false),
            ..Default::default()
        };

        let name = container_name(&req.workspace_id);
        let options = Some(CreateContainerOptionsBuilder::default().name(&name).build());

        let response = self.docker.create_container(options, config).await?;
        Ok(response.id)
    }
}

fn container_name(workspace_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u64, |d| d.as_nanos() as u64);
    format!("scriptorium-{workspace_id}-{nanos:x}")
}

/// Internal parameter bundle used by exec paths. Mirrors the gRPC
/// `ExecRequest` but is decoupled from the protobuf types so the runtime is
/// reusable if we ever add a non-gRPC interface.
#[derive(Debug, Clone)]
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

/// Events produced by `exec_stream`, in order: zero or more Stdout/Stderr
/// chunks, then exactly one Finished.
#[derive(Debug)]
pub enum StreamEvent {
    Stdout(Bytes),
    Stderr(Bytes),
    Finished {
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
    },
}

/// Drop-based cleanup for a running exec container. If the gRPC client
/// drops `ExecStream` mid-flight, `async_stream`'s generator future is
/// dropped — which drops this guard — which fires a best-effort
/// `docker kill` + `force remove` so the container doesn't keep burning
/// CPU/memory until its wall timeout.
///
/// Set `completed = true` (via `disarm`) when normal completion or the
/// explicit timeout path has already handled cleanup, otherwise we'd
/// race against Docker's own AutoRemove with no useful effect.
struct ContainerCleanupGuard {
    docker: Docker,
    container_id: String,
    disarmed: bool,
}

impl ContainerCleanupGuard {
    fn new(docker: Docker, container_id: String) -> Self {
        Self {
            docker,
            container_id,
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        // Can't .await in Drop — spawn the cleanup. Errors are expected if
        // the container already exited (AutoRemove), so they're ignored.
        let docker = self.docker.clone();
        let cid = std::mem::take(&mut self.container_id);
        tokio::spawn(async move {
            tracing::debug!(container = %cid, "ExecStream dropped; cleaning up container");
            let _ = docker
                .kill_container(
                    &cid,
                    Some(
                        KillContainerOptionsBuilder::default()
                            .signal("SIGKILL")
                            .build(),
                    ),
                )
                .await;
            let _ = docker
                .remove_container(
                    &cid,
                    Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                )
                .await;
        });
    }
}
