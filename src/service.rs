use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use futures::{Stream, StreamExt, stream};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tonic::{Request, Response, Status, Streaming};

use crate::{
    error::{Error, Result},
    pb::{
        DeleteWorkspaceRequest, DeleteWorkspaceResponse, ExecEvent, ExecFinished, ExecRequest,
        ExecResponse, ExecStarted, FileInfo, GetFileChunk, GetFileRequest, HealthRequest,
        HealthResponse, ListFilesRequest, ListFilesResponse, PutFileRequest, PutFileResponse,
        ResourceLimits, StderrChunk, StdoutChunk, exec_event, get_file_chunk, put_file_request,
        sandbox_server::Sandbox,
    },
    runtime::{DockerRuntime, ExecParams, StreamEvent},
    workspace::WorkspaceManager,
};

// 64 KiB — keeps us well under the default gRPC max-message size (4 MiB)
// while avoiding tiny-chunk overhead.
const FILE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct SandboxService {
    runtime: DockerRuntime,
    workspaces: WorkspaceManager,
}

impl SandboxService {
    pub fn new(runtime: DockerRuntime, workspaces: WorkspaceManager) -> Self {
        Self {
            runtime,
            workspaces,
        }
    }

    /// Translate a gRPC `ExecRequest` into the runtime's internal
    /// `ExecParams`, applying server-side defaults for any zero/empty fields.
    async fn build_exec_params(&self, req: ExecRequest) -> Result<ExecParams> {
        let sandbox = self.runtime.sandbox_cfg();

        let workspace_id = req.workspace_id;
        let uid = sandbox.agent_uid;
        let gid = sandbox.agent_gid;
        let host_home_dir = self.workspaces.ensure_home(&workspace_id, uid, gid).await?;

        let timeout_secs = if req.timeout_seconds == 0 {
            sandbox.default_timeout_seconds
        } else {
            req.timeout_seconds
        };

        let limits = req.limits.unwrap_or(ResourceLimits {
            cpu_millis: 0,
            memory_bytes: 0,
            pids: 0,
            tmpfs_bytes: 0,
        });
        let cpu_millis = if limits.cpu_millis == 0 {
            sandbox.default_cpu_millis
        } else {
            limits.cpu_millis
        };
        let memory_bytes = if limits.memory_bytes == 0 {
            sandbox.default_memory_bytes
        } else {
            limits.memory_bytes
        };
        let pids = if limits.pids == 0 {
            sandbox.default_pids
        } else {
            limits.pids
        };
        let tmpfs_bytes = if limits.tmpfs_bytes == 0 {
            sandbox.default_tmpfs_bytes
        } else {
            limits.tmpfs_bytes
        };

        let image = if req.image.is_empty() {
            sandbox.default_image.clone()
        } else {
            req.image
        };

        Ok(ExecParams {
            workspace_id,
            tenant_id: req.tenant_id,
            command: req.command,
            timeout: Duration::from_secs(u64::from(timeout_secs)),
            env: req.env.into_iter().collect(),
            image,
            host_home_dir,
            cpu_millis,
            memory_bytes,
            pids,
            tmpfs_bytes,
        })
    }
}

type ExecStreamOut =
    Pin<Box<dyn Stream<Item = std::result::Result<ExecEvent, Status>> + Send + 'static>>;
type GetFileStreamOut =
    Pin<Box<dyn Stream<Item = std::result::Result<GetFileChunk, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Sandbox for SandboxService {
    async fn exec(
        &self,
        req: Request<ExecRequest>,
    ) -> std::result::Result<Response<ExecResponse>, Status> {
        let params = self.build_exec_params(req.into_inner()).await?;
        let outcome = self.runtime.exec_oneshot(params).await?;
        Ok(Response::new(ExecResponse {
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            duration_ms: outcome.duration_ms,
            timed_out: outcome.timed_out,
        }))
    }

    type ExecStreamStream = ExecStreamOut;
    async fn exec_stream(
        &self,
        req: Request<ExecRequest>,
    ) -> std::result::Result<Response<Self::ExecStreamStream>, Status> {
        let params = self.build_exec_params(req.into_inner()).await?;
        let (container_id, events) = self.runtime.exec_stream(params).await?;

        let started = stream::once(async move {
            Ok(ExecEvent {
                event: Some(exec_event::Event::Started(ExecStarted { container_id })),
            })
        });
        let rest = events.map(|ev| {
            Ok(match ev {
                StreamEvent::Stdout(data) => ExecEvent {
                    event: Some(exec_event::Event::Stdout(StdoutChunk {
                        data: data.to_vec(),
                    })),
                },
                StreamEvent::Stderr(data) => ExecEvent {
                    event: Some(exec_event::Event::Stderr(StderrChunk {
                        data: data.to_vec(),
                    })),
                },
                StreamEvent::Finished {
                    exit_code,
                    timed_out,
                    duration_ms,
                } => ExecEvent {
                    event: Some(exec_event::Event::Finished(ExecFinished {
                        exit_code,
                        duration_ms,
                        timed_out,
                    })),
                },
            })
        });
        Ok(Response::new(Box::pin(started.chain(rest)) as ExecStreamOut))
    }

    async fn put_file(
        &self,
        req: Request<Streaming<PutFileRequest>>,
    ) -> std::result::Result<Response<PutFileResponse>, Status> {
        let mut inbound = req.into_inner();

        // First message must carry the header.
        let header = match inbound.next().await {
            Some(Ok(PutFileRequest {
                payload: Some(put_file_request::Payload::Header(h)),
            })) => h,
            Some(Ok(_)) => {
                return Err(Status::invalid_argument(
                    "put_file: first message must be a header",
                ));
            }
            Some(Err(status)) => return Err(status),
            None => return Err(Status::invalid_argument("put_file: empty stream")),
        };

        let sandbox = self.runtime.sandbox_cfg();
        self.workspaces
            .ensure_home(&header.workspace_id, sandbox.agent_uid, sandbox.agent_gid)
            .await?;
        let target = self
            .workspaces
            .resolve_path(&header.workspace_id, &header.path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await.map_err(Error::from)?;
        }

        // Delete any pre-existing file so an entry owned by a previous
        // exec's container UID doesn't block the server (running as a
        // different UID) from recreating it.
        match fs::remove_file(&target).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::from(e).into()),
        }

        let file = fs::File::create(&target).await.map_err(Error::from)?;
        let mut writer = BufWriter::new(file);
        let mut bytes_written: u64 = 0;

        while let Some(chunk) = inbound.next().await {
            let chunk = chunk?;
            match chunk.payload {
                Some(put_file_request::Payload::Chunk(data)) => {
                    writer.write_all(&data).await.map_err(Error::from)?;
                    bytes_written += data.len() as u64;
                }
                Some(put_file_request::Payload::Header(_)) => {
                    return Err(Status::invalid_argument("put_file: header sent twice"));
                }
                None => {}
            }
        }
        writer.flush().await.map_err(Error::from)?;
        writer.into_inner().sync_all().await.map_err(Error::from)?;

        // Permissions are set to a mode that lets the container user read
        // (`o+r`) — we can't chown to the container UID on non-root hosts,
        // so we rely on the "other" bit. 0644 is the default.
        let mode = if header.mode == 0 {
            0o644
        } else {
            header.mode & 0o777
        };
        fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(Error::from)?;
        // `sandbox` kept as an explicit binding so future work can reach
        // back to config without re-plumbing — currently unused here.
        let _ = sandbox;

        Ok(Response::new(PutFileResponse { bytes_written }))
    }

    type GetFileStream = GetFileStreamOut;
    async fn get_file(
        &self,
        req: Request<GetFileRequest>,
    ) -> std::result::Result<Response<Self::GetFileStream>, Status> {
        let inner = req.into_inner();
        let target = self
            .workspaces
            .resolve_path(&inner.workspace_id, &inner.path)?;

        let meta = fs::metadata(&target).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Status::not_found(format!("no such file: {}", inner.path))
            } else {
                Status::from(Error::Io(e))
            }
        })?;
        if meta.is_dir() {
            return Err(Status::invalid_argument(format!(
                "{} is a directory",
                inner.path
            )));
        }

        let size_bytes = meta.len();
        let mode = meta.permissions().mode() & 0o7777;
        let header_chunk = GetFileChunk {
            payload: Some(get_file_chunk::Payload::Header(crate::pb::GetFileHeader {
                size_bytes,
                mode,
            })),
        };

        let file = fs::File::open(&target).await.map_err(Error::from)?;
        let reader = BufReader::new(file);

        let body = stream_file_chunks(reader);
        let full = stream::once(async move { Ok(header_chunk) }).chain(body);
        Ok(Response::new(Box::pin(full) as GetFileStreamOut))
    }

    async fn list_files(
        &self,
        req: Request<ListFilesRequest>,
    ) -> std::result::Result<Response<ListFilesResponse>, Status> {
        let inner = req.into_inner();
        let base = self
            .workspaces
            .resolve_path(&inner.workspace_id, &inner.path)?;

        // If base is missing, return empty rather than NotFound — an empty
        // workspace is a valid state.
        match fs::metadata(&base).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Response::new(ListFilesResponse { files: vec![] }));
            }
            Err(e) => return Err(Status::from(Error::Io(e))),
        }

        let root = self.workspaces.resolve_path(&inner.workspace_id, "")?;
        let mut files = Vec::new();
        collect_files(&root, &base, inner.recursive, &mut files).await?;
        Ok(Response::new(ListFilesResponse { files }))
    }

    async fn delete_workspace(
        &self,
        req: Request<DeleteWorkspaceRequest>,
    ) -> std::result::Result<Response<DeleteWorkspaceResponse>, Status> {
        let existed = self
            .workspaces
            .delete(&req.into_inner().workspace_id)
            .await?;
        Ok(Response::new(DeleteWorkspaceResponse { existed }))
    }

    async fn health(
        &self,
        _req: Request<HealthRequest>,
    ) -> std::result::Result<Response<HealthResponse>, Status> {
        let docker_version = self.runtime.version_string().await;
        let docker_reachable = self.runtime.docker().ping().await.is_ok();
        Ok(Response::new(HealthResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            docker_version,
            docker_reachable,
        }))
    }
}

/// Read a file in chunked payloads until EOF.
fn stream_file_chunks(
    reader: BufReader<fs::File>,
) -> Pin<Box<dyn Stream<Item = std::result::Result<GetFileChunk, Status>> + Send + 'static>> {
    Box::pin(async_stream::stream! {
        let mut reader = reader;
        let mut buf = vec![0u8; FILE_CHUNK_BYTES];
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    yield Err(Status::from(Error::Io(e)));
                    return;
                }
            };
            if n == 0 {
                break;
            }
            yield Ok(GetFileChunk {
                payload: Some(get_file_chunk::Payload::Chunk(buf[..n].to_vec())),
            });
        }
    })
}

/// Recursive (or single-level) directory walk, yielding `FileInfo` entries
/// with paths relative to the workspace root.
async fn collect_files(
    workspace_root: &Path,
    base: &Path,
    recursive: bool,
    out: &mut Vec<FileInfo>,
) -> Result<()> {
    let mut stack: Vec<PathBuf> = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            let is_dir = meta.is_dir();
            let rel = path
                .strip_prefix(workspace_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0i64, |d| d.as_secs() as i64);
            out.push(FileInfo {
                path: rel,
                size_bytes: meta.len(),
                mode: meta.permissions().mode() & 0o7777,
                is_dir,
                modified_unix,
            });
            if is_dir && recursive {
                stack.push(path);
            }
        }
    }
    Ok(())
}
