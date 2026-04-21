use std::pin::Pin;

use futures::Stream;
use tonic::{Request, Response, Status, Streaming};

use crate::{
    pb::{
        DeleteWorkspaceRequest, DeleteWorkspaceResponse, ExecEvent, ExecRequest, ExecResponse,
        GetFileChunk, GetFileRequest, HealthRequest, HealthResponse, ListFilesRequest,
        ListFilesResponse, PutFileRequest, PutFileResponse, sandbox_server::Sandbox,
    },
    runtime::DockerRuntime,
    workspace::WorkspaceManager,
};

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
}

type ExecStreamOut = Pin<Box<dyn Stream<Item = Result<ExecEvent, Status>> + Send + 'static>>;
type GetFileStreamOut = Pin<Box<dyn Stream<Item = Result<GetFileChunk, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Sandbox for SandboxService {
    async fn exec(&self, _req: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        // TODO: wire to runtime.exec_oneshot once the bollard plumbing is in place.
        Err(Status::unimplemented(
            "exec: pending runtime implementation",
        ))
    }

    type ExecStreamStream = ExecStreamOut;
    async fn exec_stream(
        &self,
        _req: Request<ExecRequest>,
    ) -> Result<Response<Self::ExecStreamStream>, Status> {
        Err(Status::unimplemented(
            "exec_stream: pending runtime implementation",
        ))
    }

    async fn put_file(
        &self,
        _req: Request<Streaming<PutFileRequest>>,
    ) -> Result<Response<PutFileResponse>, Status> {
        Err(Status::unimplemented("put_file: pending"))
    }

    type GetFileStream = GetFileStreamOut;
    async fn get_file(
        &self,
        _req: Request<GetFileRequest>,
    ) -> Result<Response<Self::GetFileStream>, Status> {
        Err(Status::unimplemented("get_file: pending"))
    }

    async fn list_files(
        &self,
        _req: Request<ListFilesRequest>,
    ) -> Result<Response<ListFilesResponse>, Status> {
        Err(Status::unimplemented("list_files: pending"))
    }

    async fn delete_workspace(
        &self,
        req: Request<DeleteWorkspaceRequest>,
    ) -> Result<Response<DeleteWorkspaceResponse>, Status> {
        let existed = self
            .workspaces
            .delete(&req.into_inner().workspace_id)
            .await?;
        Ok(Response::new(DeleteWorkspaceResponse { existed }))
    }

    async fn health(
        &self,
        _req: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        let docker_version = self.runtime.version_string().await;
        let docker_reachable = self.runtime.docker().ping().await.is_ok();
        Ok(Response::new(HealthResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            docker_version,
            docker_reachable,
        }))
    }
}
