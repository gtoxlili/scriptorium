mod archive;
mod files;

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures::{Stream, StreamExt, stream};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Semaphore,
};
use tonic::{Request, Response, Status};

use crate::{
    config::{ConcurrencyConfig, FetchConfig},
    error::{Error, Result},
    fetch::fetch_to_file,
    oss::{OssClient, guess_content_type},
    pb::{
        CallToolRequest, CallToolResponse, DeleteWorkspaceRequest, DeleteWorkspaceResponse,
        ExecEvent, ExecFinished, ExecRequest, ExecResponse, ExecStarted,
        ExportWorkspaceObjectHeader, ExportWorkspaceObjectRequest, ExportWorkspaceObjectResponse,
        FetchRequest, FetchResponse, HealthRequest, HealthResponse,
        ImportWorkspaceObjectRequest, ImportWorkspaceObjectResponse, ListFilesRequest,
        ListFilesResponse, ListToolsRequest, ListToolsResponse, ResourceLimits, StderrChunk,
        StdoutChunk, UploadRequest, UploadResponse, WorkspaceObjectEncoding, exec_event,
        export_workspace_object_response, import_workspace_object_request, sandbox_server::Sandbox,
    },
    runtime::{DockerRuntime, ExecParams, StreamEvent},
    tools,
    workspace::WorkspaceManager,
};

use archive::{
    TempFileGuard, build_workspace_transfer_temp_path, replace_workspace_directory_from_archive_path,
    replace_workspace_file_from_staging, sha256_file, tar_gz_into_temp,
    WORKSPACE_TRANSFER_CHUNK_SIZE,
};
use files::collect_files;

#[derive(Debug)]
pub struct SandboxService {
    runtime: DockerRuntime,
    workspaces: WorkspaceManager,
    oss: OssClient,
    fetch_cfg: FetchConfig,
    exec_permits: Arc<Semaphore>,
    exec_queue_timeout: Option<Duration>,
    exec_permit_capacity: usize,
}

impl SandboxService {
    pub fn new(
        runtime: DockerRuntime,
        workspaces: WorkspaceManager,
        oss: OssClient,
        fetch_cfg: FetchConfig,
        concurrency: &ConcurrencyConfig,
    ) -> Self {
        let capacity = concurrency.effective_max();
        Self {
            runtime,
            workspaces,
            oss,
            fetch_cfg,
            exec_permits: Arc::new(Semaphore::new(capacity)),
            exec_queue_timeout: concurrency.effective_queue_timeout(),
            exec_permit_capacity: capacity,
        }
    }

    // ─── Permit machinery ─────────────────────────────────────────────────

    async fn acquire_exec_permit(&self) -> std::result::Result<OwnedExecPermit, Status> {
        let acquire = self.exec_permits.clone().acquire_owned();
        let permit = match self.exec_queue_timeout {
            Some(timeout) => tokio::time::timeout(timeout, acquire).await.map_err(|_| {
                Status::resource_exhausted(format!(
                    "exec queue full (cap={}); retry with backoff",
                    self.exec_permit_capacity
                ))
            })?,
            None => acquire.await,
        }
        .map_err(|_| Status::internal("exec semaphore closed"))?;
        Ok(OwnedExecPermit { _permit: permit })
    }

    fn exec_permits_available(&self) -> u32 {
        u32::try_from(self.exec_permits.available_permits()).unwrap_or(u32::MAX)
    }

    // ─── Core helpers ─────────────────────────────────────────────────────
    //
    // The three `do_*` functions are the implementation truth. Both the
    // primitive RPCs (Exec / FetchIntoWorkspace / UploadToOSS) and the
    // tool-layer RPCs (CallTool) call into the subset they expose, so the
    // behaviour cannot drift between the two surfaces.

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

    async fn do_exec_oneshot(&self, req: ExecRequest) -> std::result::Result<ExecResponse, Status> {
        let permit = self.acquire_exec_permit().await?;
        let params = self.build_exec_params(req).await?;
        let outcome = self.runtime.exec_oneshot(params).await?;
        drop(permit);
        Ok(ExecResponse {
            exit_code: outcome.exit_code,
            stdout: outcome.stdout.into(),
            stderr: outcome.stderr.into(),
            duration_ms: outcome.duration_ms,
            timed_out: outcome.timed_out,
        })
    }

    async fn do_fetch(
        &self,
        workspace_id: &str,
        url: &str,
        target_path: &str,
        headers: &HashMap<String, String>,
        timeout_seconds: u32,
    ) -> std::result::Result<FetchResponse, Status> {
        let sandbox = self.runtime.sandbox_cfg();
        self.workspaces
            .ensure_home(workspace_id, sandbox.agent_uid, sandbox.agent_gid)
            .await?;
        let target = self.workspaces.resolve_path(workspace_id, target_path)?;

        let timeout = if timeout_seconds == 0 {
            Duration::from_secs(self.fetch_cfg.timeout_seconds)
        } else {
            Duration::from_secs(u64::from(timeout_seconds))
        };

        let outcome = fetch_to_file(&self.fetch_cfg, url, &target, headers, timeout).await?;

        // Make the downloaded file world-readable (container UID needs
        // `o+r` since we can't chown on non-root hosts).
        fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .await
            .map_err(Error::from)?;

        Ok(FetchResponse {
            bytes_written: outcome.bytes_written,
            content_type: outcome.content_type,
            http_status: i32::from(outcome.http_status),
        })
    }

    async fn do_upload(
        &self,
        workspace_id: &str,
        tenant_id: &str,
        source_path: &str,
        compress: bool,
        label: &str,
    ) -> std::result::Result<UploadResponse, Status> {
        if workspace_id.is_empty() {
            return Err(Status::invalid_argument("workspace_id is required"));
        }
        let source = self.workspaces.resolve_path(workspace_id, source_path)?;

        let meta = fs::metadata(&source).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Status::not_found(format!("no such path: {source_path}"))
            } else {
                Status::from(Error::Io(e))
            }
        })?;

        // Directories always tar.gz; files gz if `compress`, raw otherwise.
        // The cleanup guard keeps the temp tarball around for the upload's
        // lifetime and removes it when we drop back out of this scope.
        let is_dir = meta.is_dir();
        let (payload_path, effective_basename, content_type, _cleanup) = if is_dir || compress {
            let basename = source.file_name().map_or_else(
                || "workspace".to_string(),
                |s| s.to_string_lossy().into_owned(),
            );
            let out_basename = format!("{basename}.tar.gz");
            let tmp = tar_gz_into_temp(&source, &out_basename).await?;
            (
                tmp.clone(),
                out_basename,
                "application/gzip",
                Some(TempFileGuard(tmp)),
            )
        } else {
            let basename = source.file_name().map_or_else(
                || "artifact".to_string(),
                |s| s.to_string_lossy().into_owned(),
            );
            let ct = guess_content_type(&source);
            (source.clone(), basename, ct, None)
        };

        let key = self
            .oss
            .build_key(tenant_id, workspace_id, &effective_basename);
        let opt_label = if label.is_empty() { None } else { Some(label) };
        let outcome = self
            .oss
            .upload_file(&key, &payload_path, content_type, opt_label)
            .await?;

        Ok(UploadResponse {
            object_key: outcome.key,
            size_bytes: outcome.size_bytes,
            content_type: outcome.content_type,
            sha256_hex: outcome.sha256_hex,
            basename: effective_basename,
        })
    }

    async fn do_import_workspace_object(
        &self,
        mut stream: tonic::Streaming<ImportWorkspaceObjectRequest>,
    ) -> std::result::Result<ImportWorkspaceObjectResponse, Status> {
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("import_workspace_object requires a header"))?;
        let header = match first.payload {
            Some(import_workspace_object_request::Payload::Header(header)) => header,
            Some(import_workspace_object_request::Payload::Chunk(_)) => {
                return Err(Status::invalid_argument(
                    "import_workspace_object header must be the first message",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "import_workspace_object first message is empty",
                ));
            }
        };

        let encoding = WorkspaceObjectEncoding::try_from(header.encoding).unwrap_or_default();
        let sandbox = self.runtime.sandbox_cfg();
        self.workspaces
            .ensure_home(&header.workspace_id, sandbox.agent_uid, sandbox.agent_gid)
            .await?;
        let target = self
            .workspaces
            .resolve_path(&header.workspace_id, &header.target_path)?;
        fs::create_dir_all(target.parent().unwrap_or_else(|| Path::new("."))).await?;

        let staging = build_workspace_transfer_temp_path(&target, "import");
        let mut file = fs::File::create(&staging).await?;
        let mut bytes_written = 0u64;

        while let Some(msg) = stream.message().await? {
            match msg.payload {
                Some(import_workspace_object_request::Payload::Chunk(chunk)) => {
                    file.write_all(&chunk).await?;
                    bytes_written = bytes_written
                        .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                }
                Some(import_workspace_object_request::Payload::Header(_)) => {
                    return Err(Status::invalid_argument(
                        "import_workspace_object header may only appear once",
                    ));
                }
                None => {
                    return Err(Status::invalid_argument(
                        "import_workspace_object received an empty message",
                    ));
                }
            }
        }
        file.flush().await?;
        drop(file);

        let import_result = match encoding {
            WorkspaceObjectEncoding::Raw => {
                replace_workspace_file_from_staging(staging.clone(), target.clone()).await?;
                ImportWorkspaceObjectResponse {
                    bytes_written,
                    content_type: if header.content_type.trim().is_empty() {
                        "application/octet-stream".to_string()
                    } else {
                        header.content_type
                    },
                }
            }
            WorkspaceObjectEncoding::TarGz => {
                replace_workspace_directory_from_archive_path(staging.clone(), target.clone())
                    .await?;
                ImportWorkspaceObjectResponse {
                    bytes_written,
                    content_type: "application/gzip".to_string(),
                }
            }
            WorkspaceObjectEncoding::Unspecified => {
                let _ = fs::remove_file(&staging).await;
                return Err(Status::invalid_argument(
                    "import_workspace_object encoding is required",
                ));
            }
        };

        let _ = fs::remove_file(staging).await;
        Ok(import_result)
    }

    async fn do_export_workspace_object(
        &self,
        req: ExportWorkspaceObjectRequest,
    ) -> std::result::Result<PreparedWorkspaceExport, Status> {
        if req.workspace_id.is_empty() {
            return Err(Status::invalid_argument("workspace_id is required"));
        }
        let source = self
            .workspaces
            .resolve_path(&req.workspace_id, &req.source_path)?;
        let meta = fs::metadata(&source).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Status::not_found(format!("no such path: {}", req.source_path))
            } else {
                Status::from(Error::Io(e))
            }
        })?;

        let encoding = WorkspaceObjectEncoding::try_from(req.encoding).unwrap_or_default();
        let (payload_path, basename, content_type, cleanup) = match encoding {
            WorkspaceObjectEncoding::Raw => {
                if meta.is_dir() {
                    return Err(Status::invalid_argument(
                        "export_workspace_object raw encoding expects a file",
                    ));
                }
                let basename = source.file_name().map_or_else(
                    || "artifact".to_string(),
                    |s| s.to_string_lossy().into_owned(),
                );
                (
                    source.clone(),
                    basename,
                    guess_content_type(&source).to_string(),
                    None,
                )
            }
            WorkspaceObjectEncoding::TarGz => {
                if !meta.is_dir() {
                    return Err(Status::invalid_argument(
                        "export_workspace_object tar_gz encoding expects a directory",
                    ));
                }
                let basename = source.file_name().map_or_else(
                    || "workspace".to_string(),
                    |s| s.to_string_lossy().into_owned(),
                );
                let out_basename = format!("{basename}.tar.gz");
                let tmp = tar_gz_into_temp(&source, &out_basename).await?;
                (
                    tmp.clone(),
                    out_basename,
                    "application/gzip".to_string(),
                    Some(TempFileGuard(tmp)),
                )
            }
            WorkspaceObjectEncoding::Unspecified => {
                return Err(Status::invalid_argument(
                    "export_workspace_object encoding is required",
                ));
            }
        };

        let size_bytes = fs::metadata(&payload_path).await?.len();
        let sha256_hex = sha256_file(&payload_path).await?;

        Ok(PreparedWorkspaceExport {
            header: ExportWorkspaceObjectHeader {
                source_path: req.source_path,
                basename,
                encoding: encoding as i32,
                content_type,
                size_bytes,
                sha256_hex,
            },
            payload_path,
            cleanup,
        })
    }
}

/// Holds a semaphore permit for the lifetime of an exec call. Dropped at
/// the end of the handler scope.
struct OwnedExecPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

type ExecStreamOut =
    Pin<Box<dyn Stream<Item = std::result::Result<ExecEvent, Status>> + Send + 'static>>;
type ExportWorkspaceObjectOut = Pin<
    Box<
        dyn Stream<Item = std::result::Result<ExportWorkspaceObjectResponse, Status>>
            + Send
            + 'static,
    >,
>;

struct PreparedWorkspaceExport {
    header: ExportWorkspaceObjectHeader,
    payload_path: PathBuf,
    cleanup: Option<TempFileGuard>,
}

#[tonic::async_trait]
impl Sandbox for SandboxService {
    #[tracing::instrument(
        skip_all,
        name = "rpc.exec",
        fields(workspace_id = %req.get_ref().workspace_id, tenant_id = %req.get_ref().tenant_id),
    )]
    async fn exec(
        &self,
        req: Request<ExecRequest>,
    ) -> std::result::Result<Response<ExecResponse>, Status> {
        let resp = self.do_exec_oneshot(req.into_inner()).await?;
        Ok(Response::new(resp))
    }

    type ExecStreamStream = ExecStreamOut;
    #[tracing::instrument(
        skip_all,
        name = "rpc.exec_stream",
        fields(workspace_id = %req.get_ref().workspace_id, tenant_id = %req.get_ref().tenant_id),
    )]
    async fn exec_stream(
        &self,
        req: Request<ExecRequest>,
    ) -> std::result::Result<Response<Self::ExecStreamStream>, Status> {
        let permit = self.acquire_exec_permit().await?;
        let params = self.build_exec_params(req.into_inner()).await?;
        let (container_id, events) = self.runtime.exec_stream(params).await?;

        let started = stream::once(async move {
            Ok(ExecEvent {
                event: Some(exec_event::Event::Started(ExecStarted { container_id })),
            })
        });
        // Move the permit into the stream so it's released only when the
        // final Finished event is drained.
        let rest = events.map(move |ev| {
            let _keep_permit_alive = &permit;
            Ok(match ev {
                StreamEvent::Stdout(data) => ExecEvent {
                    event: Some(exec_event::Event::Stdout(StdoutChunk { data })),
                },
                StreamEvent::Stderr(data) => ExecEvent {
                    event: Some(exec_event::Event::Stderr(StderrChunk { data })),
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

    #[tracing::instrument(
        skip_all,
        name = "rpc.fetch_into_workspace",
        fields(workspace_id = %req.get_ref().workspace_id, target_path = %req.get_ref().target_path),
    )]
    async fn fetch_into_workspace(
        &self,
        req: Request<FetchRequest>,
    ) -> std::result::Result<Response<FetchResponse>, Status> {
        let inner = req.into_inner();
        let resp = self
            .do_fetch(
                &inner.workspace_id,
                &inner.url,
                &inner.target_path,
                &inner.headers,
                inner.timeout_seconds,
            )
            .await?;
        Ok(Response::new(resp))
    }

    #[tracing::instrument(
        skip_all,
        name = "rpc.upload_to_oss",
        fields(
            workspace_id = %req.get_ref().workspace_id,
            tenant_id = %req.get_ref().tenant_id,
            source_path = %req.get_ref().source_path,
            compress = req.get_ref().compress,
        ),
    )]
    async fn upload_to_oss(
        &self,
        req: Request<UploadRequest>,
    ) -> std::result::Result<Response<UploadResponse>, Status> {
        let inner = req.into_inner();
        let resp = self
            .do_upload(
                &inner.workspace_id,
                &inner.tenant_id,
                &inner.source_path,
                inner.compress,
                &inner.label,
            )
            .await?;
        Ok(Response::new(resp))
    }

    #[tracing::instrument(skip_all, name = "rpc.import_workspace_object")]
    async fn import_workspace_object(
        &self,
        req: Request<tonic::Streaming<ImportWorkspaceObjectRequest>>,
    ) -> std::result::Result<Response<ImportWorkspaceObjectResponse>, Status> {
        let resp = self.do_import_workspace_object(req.into_inner()).await?;
        Ok(Response::new(resp))
    }

    type ExportWorkspaceObjectStream = ExportWorkspaceObjectOut;
    #[tracing::instrument(
        skip_all,
        name = "rpc.export_workspace_object",
        fields(
            workspace_id = %req.get_ref().workspace_id,
            source_path = %req.get_ref().source_path,
        ),
    )]
    async fn export_workspace_object(
        &self,
        req: Request<ExportWorkspaceObjectRequest>,
    ) -> std::result::Result<Response<Self::ExportWorkspaceObjectStream>, Status> {
        let prepared = self.do_export_workspace_object(req.into_inner()).await?;
        let PreparedWorkspaceExport {
            header,
            payload_path,
            cleanup,
        } = prepared;
        let stream = async_stream::try_stream! {
            yield ExportWorkspaceObjectResponse {
                payload: Some(export_workspace_object_response::Payload::Header(header)),
            };

            let mut file = fs::File::open(&payload_path).await?;
            let mut buf = vec![0u8; WORKSPACE_TRANSFER_CHUNK_SIZE];
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                yield ExportWorkspaceObjectResponse {
                    payload: Some(export_workspace_object_response::Payload::Chunk(
                        bytes::Bytes::copy_from_slice(&buf[..n]),
                    )),
                };
            }

            drop(cleanup);
        };
        Ok(Response::new(Box::pin(stream) as ExportWorkspaceObjectOut))
    }

    #[tracing::instrument(
        skip_all,
        name = "rpc.list_files",
        fields(workspace_id = %req.get_ref().workspace_id, path = %req.get_ref().path),
    )]
    async fn list_files(
        &self,
        req: Request<ListFilesRequest>,
    ) -> std::result::Result<Response<ListFilesResponse>, Status> {
        let inner = req.into_inner();
        let base = self
            .workspaces
            .resolve_path(&inner.workspace_id, &inner.path)?;

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

    #[tracing::instrument(
        skip_all,
        name = "rpc.delete_workspace",
        fields(workspace_id = %req.get_ref().workspace_id),
    )]
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

    #[tracing::instrument(skip_all, name = "rpc.health")]
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
            exec_permits_available: self.exec_permits_available(),
        }))
    }

    #[tracing::instrument(skip_all, name = "rpc.list_tools")]
    async fn list_tools(
        &self,
        _req: Request<ListToolsRequest>,
    ) -> std::result::Result<Response<ListToolsResponse>, Status> {
        Ok(Response::new(ListToolsResponse {
            tools: tools::descriptors(),
        }))
    }

    #[tracing::instrument(
        skip_all,
        name = "rpc.call_tool",
        fields(
            workspace_id = %req.get_ref().workspace_id,
            tenant_id = %req.get_ref().tenant_id,
            tool = %req.get_ref().tool_name,
        ),
    )]
    async fn call_tool(
        &self,
        req: Request<CallToolRequest>,
    ) -> std::result::Result<Response<CallToolResponse>, Status> {
        let inner = req.into_inner();
        let args_json = if inner.arguments_json.trim().is_empty() {
            "{}"
        } else {
            inner.arguments_json.as_str()
        };

        let result_json = match inner.tool_name.as_str() {
            tools::TOOL_EXECUTE_SHELL => {
                let args: tools::ExecuteShellArgs = serde_json::from_str(args_json)
                    .map_err(|e| Status::invalid_argument(format!("execute_shell args: {e}")))?;
                let timeout_seconds = if inner.timeout_seconds > 0 {
                    inner.timeout_seconds
                } else {
                    args.timeout_seconds
                };
                // `heavy` opts this one call into the fatter resource tier
                // from sandbox config. Light calls (default) get `limits =
                // None`, which picks up the server defaults.
                let limits = if args.heavy {
                    let sbx = self.runtime.sandbox_cfg();
                    Some(ResourceLimits {
                        cpu_millis: sbx.heavy_cpu_millis,
                        memory_bytes: sbx.heavy_memory_bytes,
                        pids: sbx.heavy_pids,
                        tmpfs_bytes: sbx.heavy_tmpfs_bytes,
                    })
                } else {
                    None
                };
                let exec_req = ExecRequest {
                    workspace_id: inner.workspace_id.clone(),
                    tenant_id: inner.tenant_id.clone(),
                    command: args.command,
                    timeout_seconds,
                    env: args.env,
                    image: String::new(),
                    limits,
                };
                let resp = self.do_exec_oneshot(exec_req).await?;
                let result = tools::ExecuteShellResult {
                    exit_code: resp.exit_code,
                    stdout: String::from_utf8_lossy(&resp.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&resp.stderr).into_owned(),
                    duration_ms: resp.duration_ms,
                    timed_out: resp.timed_out,
                };
                encode_result(&result)?
            }
            tools::TOOL_DELIVER => {
                let args: tools::DeliverArgs = serde_json::from_str(args_json)
                    .map_err(|e| Status::invalid_argument(format!("deliver args: {e}")))?;
                let resp = self
                    .do_upload(
                        &inner.workspace_id,
                        &inner.tenant_id,
                        &args.path,
                        args.compress,
                        &args.label,
                    )
                    .await?;
                let result = tools::DeliverResult {
                    object_key: resp.object_key,
                    basename: resp.basename,
                    size_bytes: resp.size_bytes,
                    content_type: resp.content_type,
                    sha256_hex: resp.sha256_hex,
                };
                encode_result(&result)?
            }
            tools::TOOL_COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX
            | tools::TOOL_COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX => {
                return Ok(Response::new(CallToolResponse {
                    result_json: String::new(),
                    is_error: true,
                    error_message: format!(
                        "tool {} requires a host bridge; direct scriptorium calls are unsupported",
                        inner.tool_name
                    ),
                }));
            }
            other => {
                return Ok(Response::new(CallToolResponse {
                    result_json: String::new(),
                    is_error: true,
                    error_message: format!("unknown tool: {other}"),
                }));
            }
        };

        Ok(Response::new(CallToolResponse {
            result_json,
            is_error: false,
            error_message: String::new(),
        }))
    }
}

fn encode_result<T: serde::Serialize>(v: &T) -> std::result::Result<String, Status> {
    serde_json::to_string(v).map_err(|e| Status::internal(format!("encode result: {e}")))
}
